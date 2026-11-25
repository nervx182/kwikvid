//! Turning a chaptered recording into one [`MediaItem`].
//!
//! Import paths (the CLI's directory input, the desktop import pipeline) call
//! [`group_chapters`](crate::group_chapters) on the files they were given,
//! then [`media_item_from_group`] once per group with a closure that probes a
//! file. Everything here is pure; the probing (and so all I/O) stays with the
//! caller.

use std::path::Path;

use kv_core::{ChapterRef, MediaId, MediaItem, MediaKind, Micros};

use crate::ChapterGroup;

/// How a chapter file's HiLight offsets are measured.
///
/// Which one GoPro cameras use is not yet verified on real footage (#15);
/// [`merge_chapter_hilights`] assumes [`HilightOffsets::ChapterRelative`]
/// (decision, reversible: if real files say otherwise, only the default
/// changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HilightOffsets {
    /// Offsets count from the start of their own chapter file.
    ChapterRelative,
    /// Offsets count from the start of the whole recording.
    RecordingRelative,
}

/// Merge the HiLights of a chaptered recording into offsets from the start of
/// the recording, assuming each chapter's offsets are relative to that
/// chapter (see [`HilightOffsets`]).
///
/// `per_chapter` holds, in chapter order, each chapter's duration and
/// HiLight offsets. Offsets outside their chapter are dropped; the result is
/// sorted and free of duplicates.
pub fn merge_chapter_hilights(per_chapter: &[(Micros, Vec<Micros>)]) -> Vec<Micros> {
    merge_chapter_hilights_as(per_chapter, HilightOffsets::ChapterRelative)
}

/// [`merge_chapter_hilights`] with an explicit offset convention.
///
/// With [`HilightOffsets::RecordingRelative`] the offsets are kept as they
/// are (a camera that repeats every HiLight in every chapter yields each one
/// once); offsets outside the recording are dropped.
pub fn merge_chapter_hilights_as(
    per_chapter: &[(Micros, Vec<Micros>)],
    convention: HilightOffsets,
) -> Vec<Micros> {
    let total = per_chapter
        .iter()
        .fold(0i64, |sum, (d, _)| sum.saturating_add(d.0));
    let mut start = 0i64;
    let mut merged = Vec::new();
    for (duration, hilights) in per_chapter {
        for h in hilights {
            let at = match convention {
                HilightOffsets::ChapterRelative if (0..=duration.0).contains(&h.0) => {
                    start.saturating_add(h.0)
                }
                HilightOffsets::RecordingRelative if (0..=total).contains(&h.0) => h.0,
                _ => continue,
            };
            merged.push(Micros(at));
        }
        start = start.saturating_add(duration.0);
    }
    merged.sort_unstable();
    merged.dedup();
    merged
}

/// What an import path learned about one file by probing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedFile {
    /// Path relative to the project file, `/`-separated (as stored in
    /// [`MediaItem::path`]).
    pub path: String,
    pub duration: Micros,
    /// HiLight offsets from the start of this file (empty for non-GoPro
    /// files).
    pub hilights: Vec<Micros>,
}

/// Build the [`MediaItem`] for one recording: a normal item for a single
/// file, or a chaptered item whose duration is the sum of the chapters' and
/// whose HiLights are merged with [`merge_chapter_hilights`].
///
/// `probe` is called once per file of the group, in chapter order; its first
/// error is returned. A group with a missing chapter
/// ([`ChapterGroup::has_gaps`]) is imported anyway with a warning (decision,
/// reversible). Returns `Ok(None)` for a group without files.
pub fn media_item_from_group<E>(
    id: MediaId,
    kind: MediaKind,
    group: &ChapterGroup,
    mut probe: impl FnMut(&Path) -> Result<ProbedFile, E>,
) -> Result<Option<MediaItem>, E> {
    if group.has_gaps() {
        tracing::warn!(
            recording = ?group.recording,
            files = ?group.files,
            "chaptered recording is missing a chapter; importing the chapters present"
        );
    }
    let files = group
        .files
        .iter()
        .map(|f| probe(f))
        .collect::<Result<Vec<_>, E>>()?;
    Ok(media_item_from_files(id, kind, files))
}

/// [`media_item_from_group`] on already-probed files, in chapter order.
/// Returns `None` when `files` is empty.
pub fn media_item_from_files(
    id: MediaId,
    kind: MediaKind,
    files: Vec<ProbedFile>,
) -> Option<MediaItem> {
    let first = files.first()?.path.clone();
    let per_chapter: Vec<(Micros, Vec<Micros>)> = files
        .iter()
        .map(|f| (f.duration, f.hilights.clone()))
        .collect();
    let hilights = merge_chapter_hilights(&per_chapter);
    let duration = Micros(
        files
            .iter()
            .fold(0i64, |sum, f| sum.saturating_add(f.duration.0)),
    );
    let chapters = if files.len() > 1 {
        files
            .into_iter()
            .map(|f| ChapterRef {
                path: f.path,
                duration: f.duration,
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(MediaItem {
        id,
        path: first,
        duration,
        kind,
        hilights,
        clip_audio: false,
        chapters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group_chapters;
    use kv_core::{Command, Project, ProjectId};

    fn s(secs: i64) -> Micros {
        Micros(secs * 1_000_000)
    }

    fn ms(v: &[i64]) -> Vec<Micros> {
        v.iter().map(|&x| Micros(x * 1_000)).collect()
    }

    #[test]
    fn chapter_relative_offsets_get_the_previous_chapters_added() {
        let per_chapter = [
            (s(10), ms(&[2_000, 9_500])),
            (s(10), ms(&[0, 1_000])),
            (s(5), ms(&[4_000, 5_000])),
        ];
        assert_eq!(
            merge_chapter_hilights(&per_chapter),
            ms(&[2_000, 9_500, 10_000, 11_000, 24_000, 25_000])
        );
    }

    #[test]
    fn chapter_relative_drops_offsets_outside_their_chapter_and_sorts() {
        let per_chapter = [
            (s(10), ms(&[9_000, 3_000, 10_001, -1])),
            (s(10), ms(&[500, 500])),
        ];
        assert_eq!(
            merge_chapter_hilights(&per_chapter),
            ms(&[3_000, 9_000, 10_500])
        );
    }

    #[test]
    fn recording_relative_offsets_are_kept_and_deduplicated() {
        // A camera that stores recording offsets and repeats them per chapter.
        let per_chapter = [
            (s(10), ms(&[2_000, 15_000])),
            (s(10), ms(&[2_000, 15_000, 21_000])),
        ];
        assert_eq!(
            merge_chapter_hilights_as(&per_chapter, HilightOffsets::RecordingRelative),
            ms(&[2_000, 15_000])
        );
    }

    #[test]
    fn merge_of_nothing_is_empty() {
        assert_eq!(merge_chapter_hilights(&[]), Vec::<Micros>::new());
        assert_eq!(merge_chapter_hilights(&[(s(3), vec![])]), vec![]);
    }

    fn fake_probe(p: &Path) -> Result<ProbedFile, String> {
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let (duration, hilights) = match name.as_str() {
            "GX010042.MP4" => (s(10), ms(&[4_000])),
            "GX020042.MP4" => (s(10), ms(&[1_000])),
            "GX030042.MP4" => (s(3), vec![]),
            "holiday.mp4" => (s(7), vec![]),
            _ => return Err(format!("cannot probe {name}")),
        };
        Ok(ProbedFile {
            path: format!("clips/{name}"),
            duration,
            hilights,
        })
    }

    #[test]
    fn chaptered_group_becomes_one_valid_item() {
        let groups = group_chapters(["card/GX020042.MP4", "card/GX010042.MP4"]);
        assert_eq!(groups.len(), 1);
        let item = media_item_from_group(MediaId(1), MediaKind::Video, &groups[0], fake_probe)
            .unwrap()
            .unwrap();
        assert_eq!(item.path, "clips/GX010042.MP4");
        assert_eq!(item.duration, s(20));
        assert_eq!(item.hilights, ms(&[4_000, 11_000]));
        assert_eq!(
            item.chapters,
            vec![
                ChapterRef {
                    path: "clips/GX010042.MP4".into(),
                    duration: s(10)
                },
                ChapterRef {
                    path: "clips/GX020042.MP4".into(),
                    duration: s(10)
                },
            ]
        );
        // The core accepts it.
        Project::new(ProjectId(1))
            .apply(Command::AddMedia { item })
            .unwrap();
    }

    #[test]
    fn single_file_group_has_no_chapters() {
        let groups = group_chapters(["card/holiday.mp4"]);
        let item = media_item_from_group(MediaId(2), MediaKind::Video, &groups[0], fake_probe)
            .unwrap()
            .unwrap();
        assert_eq!(item.path, "clips/holiday.mp4");
        assert_eq!(item.duration, s(7));
        assert!(item.chapters.is_empty());
    }

    #[test]
    fn group_with_a_missing_chapter_is_imported_anyway() {
        let groups = group_chapters(["card/GX010042.MP4", "card/GX030042.MP4"]);
        assert!(groups[0].has_gaps());
        let item = media_item_from_group(MediaId(1), MediaKind::Video, &groups[0], fake_probe)
            .unwrap()
            .unwrap();
        assert_eq!(item.duration, s(13));
        assert_eq!(item.chapters.len(), 2);
    }

    #[test]
    fn probe_errors_propagate() {
        let groups = group_chapters(["card/GX010042.MP4", "card/GX040042.MP4"]);
        assert_eq!(
            media_item_from_group(MediaId(1), MediaKind::Video, &groups[0], fake_probe),
            Err("cannot probe GX040042.MP4".to_owned())
        );
    }

    #[test]
    fn empty_input_gives_no_item() {
        assert_eq!(
            media_item_from_files(MediaId(1), MediaKind::Video, vec![]),
            None
        );
    }
}
