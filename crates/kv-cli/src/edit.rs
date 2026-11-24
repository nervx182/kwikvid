//! `kwikvid edit`: build the automatic edit for clips and a song.
//!
//! [`plan`] does the work and is independent of the output: it gathers clip
//! durations (probe or `--durations`), HiLights and the music analysis, builds
//! a [`Project`] through `kv-core` [`Command`]s, runs the auto-edit and stores
//! the result with [`Command::ReplaceEdl`]. [`run`] then prints the plan
//! (`--dry-run`); rendering it to a video file is a later output mode.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};
use kv_autoedit::{AutoEditError, EditInput, MediaSummary, MusicAnalysis};
use kv_core::{
    Command, MediaId, MediaItem, MediaKind, Micros, MusicRef, Project, ProjectId, RangeRef,
    TargetLength,
};
use kv_media::MediaBackend;
use serde::Serialize;

use crate::cli::EditArgs;
use crate::format::{print_json, print_text, round_bpm, secs};
use crate::spec::{Length, RangeSpec};

/// File extensions picked up when `--clips` names a directory.
const VIDEO_EXTENSIONS: [&str; 3] = ["mp4", "mov", "m4v"];

/// One input clip.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Path to read the file from.
    pub path: PathBuf,
    /// The path as the user gave it (or as found in a `--clips` directory).
    pub name: String,
}

/// A finished edit plan: the project (media, music, constraints and EDL, all
/// applied through `kv-core` commands) plus what the output needs.
#[derive(Debug)]
pub struct Plan {
    pub project: Project,
    /// Clip `i` has media id `i + 1` in the project.
    pub clips: Vec<Clip>,
    pub music_name: String,
    pub music: MusicAnalysis,
    pub target_length: Micros,
}

impl Plan {
    fn section(&self) -> &kv_core::Section {
        &self.project.sections[0]
    }

    fn clip_name(&self, id: MediaId) -> &str {
        self.clips
            .get((id.0 as usize).wrapping_sub(1))
            .map_or("?", |c| c.name.as_str())
    }

    /// End of the last segment on the timeline.
    pub fn total_length(&self) -> Micros {
        self.section()
            .edit
            .segments
            .iter()
            .map(|s| Micros(s.timeline_start.0 + s.timeline_len().0))
            .max()
            .unwrap_or(Micros(0))
    }
}

pub fn run(args: &EditArgs) -> anyhow::Result<()> {
    if !args.dry_run {
        bail!("rendering to a video file is not implemented yet; pass --dry-run to print the edit");
    }
    let plan = plan(args)?;
    if args.json {
        print_json(&json_report(&plan))
    } else {
        print_text(&table(&plan))
    }
}

/// Build the edit described by `args`.
pub fn plan(args: &EditArgs) -> anyhow::Result<Plan> {
    let clips = expand_clips(&args.clips)?;
    let base = std::env::current_dir().context("cannot read the current directory")?;

    let mut durations: Vec<Option<Micros>> = vec![None; clips.len()];
    for d in &args.durations {
        let i = find_clip(&clips, &d.clip).with_context(|| format!("--durations {}", d.clip))?;
        durations[i] = Some(d.duration);
    }
    let mut backend: Option<kv_media::FfmpegBackend> = None;
    let mut project = Project::new(ProjectId(1));
    let section = Project::DEFAULT_SECTION;

    for (i, clip) in clips.iter().enumerate() {
        let duration = match durations[i] {
            Some(d) => d,
            None => {
                let b = match backend {
                    Some(b) => b,
                    None => *backend.insert(kv_media::FfmpegBackend::new().context(
                        "cannot start the media prober; pass --durations to skip probing",
                    )?),
                };
                b.probe(&clip.path)
                    .with_context(|| format!("cannot probe clip {:?}", clip.name))?
                    .duration
            }
        };
        if duration.0 <= 0 {
            bail!("clip {:?} has no duration", clip.name);
        }
        // HiLights are optional: non-GoPro files simply have none.
        let mut hilights = kv_gopro::read_hilights(&clip.path).unwrap_or_default();
        hilights.retain(|h| h.0 >= 0 && *h <= duration);
        let item = MediaItem {
            id: MediaId(i as u64 + 1),
            path: project_path(&clip.path, &base)?,
            duration,
            kind: MediaKind::Video,
            hilights,
            clip_audio: false,
        };
        project
            .apply(Command::AddMedia { item })
            .with_context(|| format!("cannot add clip {:?}", clip.name))?;
    }

    let music_name = args.music.display().to_string();
    let music = kv_audio::analyze(&args.music)
        .with_context(|| format!("cannot analyse music {music_name:?}"))?;
    project.apply(Command::SetMusic {
        section,
        music: Some(MusicRef {
            path: project_path(&args.music, &base)?,
            duration: music.duration,
        }),
    })?;
    let (target, target_length) = match args.length {
        Length::Fixed(length) => (TargetLength::Fixed { length }, length),
        Length::FullSong => (TargetLength::FullSong, music.duration),
    };
    project.apply(Command::SetTargetLength { section, target })?;
    project.apply(Command::SetSeed {
        section,
        seed: args.seed,
    })?;
    for p in &args.pin {
        let range =
            resolve_range(&project, &clips, p).with_context(|| format!("--pin {}", p.text))?;
        project
            .apply(Command::PinRange { section, range })
            .with_context(|| format!("--pin {}", p.text))?;
    }
    for e in &args.exclude {
        let range =
            resolve_range(&project, &clips, e).with_context(|| format!("--exclude {}", e.text))?;
        project
            .apply(Command::ExcludeRange { section, range })
            .with_context(|| format!("--exclude {}", e.text))?;
    }

    let s = &project.sections[0];
    let input = EditInput {
        media: project
            .media
            .iter()
            .map(|m| MediaSummary {
                id: m.id,
                duration: m.duration,
                hilights: m.hilights.clone(),
                scores: None,
            })
            .collect(),
        music: music.clone(),
        target_length,
        pins: s.pins.clone(),
        excludes: s.excludes.clone(),
        seed: s.seed,
    };
    let edl = kv_autoedit::generate(&input)
        .map_err(|e| anyhow::anyhow!("cannot build the edit: {}", explain(&e, &clips)))?;
    project.apply(Command::ReplaceEdl { section, edl })?;

    Ok(Plan {
        project,
        clips,
        music_name,
        music,
        target_length,
    })
}

/// Expand `--clips`: files are kept as given, directories contribute their
/// video files in name order. Every clip must exist and appear once.
fn expand_clips(inputs: &[PathBuf]) -> anyhow::Result<Vec<Clip>> {
    let mut clips: Vec<Clip> = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(input)
                .with_context(|| format!("cannot read clip directory {:?}", input.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && is_video(p))
                .collect();
            if found.is_empty() {
                bail!(
                    "clip directory {:?} has no {} files",
                    input.display().to_string(),
                    VIDEO_EXTENSIONS.join("/")
                );
            }
            found.sort();
            clips.extend(found.into_iter().map(|path| Clip {
                name: path.display().to_string(),
                path,
            }));
        } else if input.is_file() {
            clips.push(Clip {
                path: input.clone(),
                name: input.display().to_string(),
            });
        } else {
            bail!("clip {:?} not found", input.display().to_string());
        }
    }
    for (i, c) in clips.iter().enumerate() {
        if clips[..i].iter().any(|o| o.path == c.path) {
            bail!("clip {:?} is listed more than once", c.name);
        }
    }
    Ok(clips)
}

fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.iter().any(|v| e.eq_ignore_ascii_case(v)))
}

/// Index of the clip a `--pin`/`--exclude`/`--durations` value names: the
/// path as given, or else a file name that matches exactly one clip.
fn find_clip(clips: &[Clip], name: &str) -> anyhow::Result<usize> {
    if let Some(i) = clips
        .iter()
        .position(|c| c.name == name || c.path == Path::new(name))
    {
        return Ok(i);
    }
    let by_file: Vec<usize> = (0..clips.len())
        .filter(|&i| clips[i].path.file_name().is_some_and(|f| f == name))
        .collect();
    match by_file[..] {
        [i] => Ok(i),
        [] => bail!("no clip named {name:?} among --clips"),
        _ => bail!("{name:?} matches several clips; use the clip's path as given to --clips"),
    }
}

/// The [`RangeRef`] for a pin or exclude (a whole clip when it has no range).
fn resolve_range(project: &Project, clips: &[Clip], spec: &RangeSpec) -> anyhow::Result<RangeRef> {
    let i = find_clip(clips, &spec.clip)?;
    let media = &project.media[i];
    let (start, end) = spec.range.unwrap_or((Micros(0), media.duration));
    if end > media.duration {
        bail!(
            "range ends at {} s but {:?} is only {} s long",
            secs(end),
            clips[i].name,
            secs(media.duration)
        );
    }
    Ok(RangeRef {
        media_id: media.id,
        start,
        end,
    })
}

/// `path` relative to `base`, `/`-separated, as `kv-core` stores media paths
/// (relative to the project file; for now the current directory stands in
/// for it).
fn project_path(path: &Path, base: &Path) -> anyhow::Result<String> {
    let abs = normalize(&std::path::absolute(path)?);
    let base = normalize(base);
    let common = abs.iter().zip(&base).take_while(|(a, b)| a == b).count();
    if common == 0 || (cfg!(windows) && common < 2) {
        bail!(
            "{:?} cannot be expressed relative to {:?}",
            path.display().to_string(),
            base.iter().collect::<PathBuf>().display().to_string()
        );
    }
    let parts: Vec<String> = std::iter::repeat_n("..".to_owned(), base.len() - common)
        .chain(
            abs[common..]
                .iter()
                .map(|c| c.to_string_lossy().into_owned()),
        )
        .collect();
    Ok(parts.join("/"))
}

/// Path components with `.` dropped and `..` applied lexically.
fn normalize(p: &Path) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str().to_owned()),
        }
    }
    out
}

/// A human-readable [`AutoEditError`], with clip names and seconds instead of
/// ids and microseconds.
fn explain(e: &AutoEditError, clips: &[Clip]) -> String {
    let name = |id: MediaId| {
        clips
            .get((id.0 as usize).wrapping_sub(1))
            .map_or_else(|| format!("{id:?}"), |c| c.name.clone())
    };
    let range = |r: &RangeRef| format!("{}@{}-{}", name(r.media_id), secs(r.start), secs(r.end));
    match e {
        AutoEditError::InvalidTargetLength(t) => {
            format!(
                "target length must be positive and at most 1 hour, got {} s",
                secs(*t)
            )
        }
        AutoEditError::PinLongerThanTarget { pin, target } => format!(
            "pin {} is longer than the target length {} s",
            range(pin),
            secs(*target)
        ),
        AutoEditError::PinsExceedTarget { target } => {
            format!(
                "the pins together do not fit in the target length {} s",
                secs(*target)
            )
        }
        AutoEditError::PinRoomTooShort { pin } => {
            format!(
                "pin {} has less footage around it than one beat",
                range(pin)
            )
        }
        AutoEditError::NotEnoughMedia { target } => format!(
            "not enough usable footage to fill the target length {} s",
            secs(*target)
        ),
        other => other.to_string(),
    }
}

/// JSON output of `edit --dry-run --json`. Times are microseconds; the
/// project is the `kv-core` project file format.
#[derive(Debug, Serialize)]
struct JsonReport<'a> {
    music: JsonMusic<'a>,
    target_length: Micros,
    total_length: Micros,
    project: &'a Project,
}

#[derive(Debug, Serialize)]
struct JsonMusic<'a> {
    path: &'a str,
    duration: Micros,
    tempo_bpm: f64,
    beat_count: usize,
}

fn json_report(plan: &Plan) -> JsonReport<'_> {
    JsonReport {
        music: JsonMusic {
            path: &plan.music_name,
            duration: plan.music.duration,
            tempo_bpm: round_bpm(plan.music.tempo_bpm),
            beat_count: plan.music.beats.len(),
        },
        target_length: plan.target_length,
        total_length: plan.total_length(),
        project: &plan.project,
    }
}

/// The edit as a readable table, in seconds.
fn table(plan: &Plan) -> String {
    let s = plan.section();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "music   {} ({:.2} BPM, {} beats, {} s)",
        plan.music_name,
        plan.music.tempo_bpm,
        plan.music.beats.len(),
        secs(plan.music.duration)
    );
    let _ = writeln!(
        out,
        "target  {} s, seed {}",
        secs(plan.target_length),
        s.seed
    );
    let _ = writeln!(
        out,
        "clips   {}, {} pinned, {} excluded",
        plan.clips.len(),
        s.pins.len(),
        s.excludes.len()
    );
    let _ = writeln!(out);

    let w = s
        .edit
        .segments
        .iter()
        .map(|g| plan.clip_name(g.media_id).len())
        .max()
        .unwrap_or(0)
        .max("clip".len());
    let _ = writeln!(
        out,
        "{:>3}  {:<w$}  {:>9}  {:>9}  {:>9}  {:>9}  {:>7}",
        "#", "clip", "src in", "src out", "start", "end", "length"
    );
    for (i, g) in s.edit.segments.iter().enumerate() {
        let len = g.timeline_len();
        let pinned = s
            .pins
            .iter()
            .any(|p| p.media_id == g.media_id && g.src_in <= p.start && p.end <= g.src_out);
        let _ = writeln!(
            out,
            "{:>3}  {:<w$}  {:>9}  {:>9}  {:>9}  {:>9}  {:>7}{}",
            i + 1,
            plan.clip_name(g.media_id),
            secs(g.src_in),
            secs(g.src_out),
            secs(g.timeline_start),
            secs(Micros(g.timeline_start.0 + len.0)),
            secs(len),
            if pinned { "  pinned" } else { "" }
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "total   {} s in {} segments",
        secs(plan.total_length()),
        s.edit.segments.len()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clips(names: &[&str]) -> Vec<Clip> {
        names
            .iter()
            .map(|n| Clip {
                path: PathBuf::from(n),
                name: (*n).to_owned(),
            })
            .collect()
    }

    #[test]
    fn find_clip_by_path_or_unique_file_name() {
        let c = clips(&["a/x.mp4", "b/x.mp4", "b/y.mp4"]);
        assert_eq!(find_clip(&c, "a/x.mp4").unwrap(), 0);
        assert_eq!(find_clip(&c, "y.mp4").unwrap(), 2);
        let err = find_clip(&c, "x.mp4").unwrap_err().to_string();
        assert!(err.contains("several clips"), "{err}");
        let err = find_clip(&c, "z.mp4").unwrap_err().to_string();
        assert!(err.contains("no clip named"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn project_paths_are_relative() {
        let base = Path::new("/home/me/work");
        let rel = |p: &str| project_path(Path::new(p), base).unwrap();
        assert_eq!(rel("/home/me/work/clips/a.mp4"), "clips/a.mp4");
        assert_eq!(rel("/home/me/music/song.mp3"), "../music/song.mp3");
        assert_eq!(rel("/home/me/work/./x/../a.mp4"), "a.mp4");
        assert_eq!(rel("/tmp/a.mp4"), "../../../tmp/a.mp4");
    }
}
