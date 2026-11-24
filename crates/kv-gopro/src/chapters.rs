//! Grouping of chaptered GoPro recordings by file name.
//!
//! GoPro cameras split one long recording into several "chapter" files
//! (about every 4 GB). The naming scheme (guess: from GoPro's public camera
//! file naming article, not yet verified on real cards; see ADR 0006) is:
//!
//! - HERO5 and older: first chapter `GOPRxxxx.MP4`, later chapters
//!   `GP01xxxx.MP4`, `GP02xxxx.MP4`, ... (prefix family `"GOPR/GP"`).
//! - HERO6 and newer: `GHzzxxxx.MP4` (AVC) or `GXzzxxxx.MP4` (HEVC), where
//!   `zz` is the chapter (01, 02, ...) and `xxxx` the recording number.
//!
//! [`group_chapters`] is a pure function of the given paths: it never touches
//! the file system.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Prefix family of HERO6+ AVC (H.264) recordings.
pub const PREFIX_GH: &str = "GH";
/// Prefix family of HERO6+ HEVC (H.265) recordings.
pub const PREFIX_GX: &str = "GX";
/// Prefix family of HERO5-and-older recordings (`GOPRxxxx`, `GPzzxxxx`).
pub const PREFIX_GOPR: &str = "GOPR/GP";

/// One recording: its chapter files in order, or a single non-GoPro file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterGroup {
    /// The GoPro recording number (`xxxx`), or `None` for a file whose name
    /// is not a recognised GoPro video name (a single-file group).
    pub recording: Option<u32>,
    /// The prefix family: [`PREFIX_GH`], [`PREFIX_GX`] or [`PREFIX_GOPR`];
    /// empty for non-GoPro files.
    pub prefix: String,
    /// The files of the recording, in chapter order.
    pub files: Vec<PathBuf>,
}

impl ChapterGroup {
    /// True when a chapter is missing: the chapter numbers of [`Self::files`]
    /// do not run without holes from the first chapter (`GOPR` or `01`) to
    /// the last one present. Always false for non-GoPro groups.
    pub fn has_gaps(&self) -> bool {
        if self.recording.is_none() {
            return false;
        }
        let mut chapters: Vec<u32> = self
            .files
            .iter()
            .filter_map(|f| parse_name(f).map(|n| n.chapter))
            .collect();
        chapters.sort_unstable();
        chapters.dedup();
        let first = if self.prefix == PREFIX_GOPR { 0 } else { 1 };
        chapters
            .iter()
            .enumerate()
            .any(|(i, &c)| c != first + i as u32)
    }
}

/// Group file paths into GoPro recordings with their chapters in order.
///
/// - Matching is case-insensitive on the file name and the `.mp4` extension,
///   and only files in the **same directory** (same [`Path::parent`], compared
///   as given) with the same prefix family and recording number are grouped.
///   AVC (`GH`) and HEVC (`GX`) files with the same number are separate groups.
/// - `.LRV` (low-res proxy) and `.THM` (thumbnail) inputs are ignored and not
///   returned, whatever their name.
/// - Every other file (non-GoPro names, photos, looping/timelapse variants
///   such as `GL`/`GG`, other extensions) becomes a single-file group with
///   `recording: None` and an empty prefix.
/// - Output order is deterministic: by directory; within a directory GoPro
///   recordings by number (then prefix family), then non-GoPro files by name.
///   Within a group, files are in chapter order.
///
/// Pure: works on the given paths only, no file system access.
pub fn group_chapters<I, P>(paths: I) -> Vec<ChapterGroup>
where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
{
    let mut gopro: Vec<Recording> = Vec::new();
    let mut others: Vec<PathBuf> = Vec::new();

    for path in paths.into_iter().map(Into::into) {
        if is_proxy(&path) {
            continue;
        }
        let Some(name) = parse_name(&path) else {
            others.push(path);
            continue;
        };
        let dir = parent(&path).to_path_buf();
        match gopro
            .iter_mut()
            .find(|(d, r, p, _)| *d == dir && *r == name.recording && *p == name.prefix)
        {
            Some((_, _, _, files)) => files.push((name.chapter, path)),
            None => gopro.push((dir, name.recording, name.prefix, vec![(name.chapter, path)])),
        }
    }

    // Sort key: directory, then 0 for GoPro groups / 1 for others, then
    // (recording, prefix) or file name.
    let mut groups: Vec<(PathBuf, ChapterGroup)> = Vec::new();
    for (dir, recording, prefix, mut files) in gopro {
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        groups.push((
            dir,
            ChapterGroup {
                recording: Some(recording),
                prefix: prefix.to_owned(),
                files: files.into_iter().map(|(_, p)| p).collect(),
            },
        ));
    }
    for path in others {
        groups.push((
            parent(&path).to_path_buf(),
            ChapterGroup {
                recording: None,
                prefix: String::new(),
                files: vec![path],
            },
        ));
    }
    groups.sort_by(|(da, a), (db, b)| da.cmp(db).then_with(|| group_order(a, b)));
    groups.into_iter().map(|(_, g)| g).collect()
}

/// A GoPro recording being collected: directory, recording number, prefix
/// family and its `(chapter, path)` files.
type Recording = (PathBuf, u32, &'static str, Vec<(u32, PathBuf)>);

/// Order of two groups in the same directory.
fn group_order(a: &ChapterGroup, b: &ChapterGroup) -> Ordering {
    match (a.recording, b.recording) {
        (Some(ra), Some(rb)) => ra.cmp(&rb).then_with(|| a.prefix.cmp(&b.prefix)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => {
            let name = |g: &ChapterGroup| {
                g.files
                    .first()
                    .and_then(|f| f.file_name())
                    .map(OsStr::to_owned)
            };
            name(a).cmp(&name(b)).then_with(|| a.files.cmp(&b.files))
        }
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

fn is_proxy(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("lrv") || e.eq_ignore_ascii_case("thm"))
}

/// A recognised GoPro chapter file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChapterName {
    prefix: &'static str,
    /// 0 for `GOPRxxxx`, otherwise the `zz` chapter number (1..=99).
    chapter: u32,
    recording: u32,
}

/// Parse `GOPRxxxx.MP4`, `GPzzxxxx.MP4`, `GHzzxxxx.MP4` or `GXzzxxxx.MP4`
/// (case-insensitive). Anything else is `None`.
fn parse_name(path: &Path) -> Option<ChapterName> {
    let ext = path.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("mp4") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.to_ascii_uppercase();
    if stem.len() != 8 || !stem.is_ascii() {
        return None;
    }
    let (head, recording) = stem.split_at(4);
    let recording = digits(recording)?;
    if head == "GOPR" {
        return Some(ChapterName {
            prefix: PREFIX_GOPR,
            chapter: 0,
            recording,
        });
    }
    let (family, chapter) = head.split_at(2);
    let prefix = match family {
        "GP" => PREFIX_GOPR,
        "GH" => PREFIX_GH,
        "GX" => PREFIX_GX,
        _ => return None,
    };
    let chapter = digits(chapter)?;
    if chapter == 0 {
        return None;
    }
    Some(ChapterName {
        prefix,
        chapter,
        recording,
    })
}

fn digits(s: &str) -> Option<u32> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn g(recording: Option<u32>, prefix: &str, files: &[&str]) -> ChapterGroup {
        ChapterGroup {
            recording,
            prefix: prefix.to_owned(),
            files: files.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn parses_each_naming_scheme() {
        type Parsed = Option<(&'static str, u32, u32)>;
        let cases: &[(&str, Parsed)] = &[
            ("GOPR0042.MP4", Some((PREFIX_GOPR, 0, 42))),
            ("GP010042.MP4", Some((PREFIX_GOPR, 1, 42))),
            ("GP120042.MP4", Some((PREFIX_GOPR, 12, 42))),
            ("GH010007.MP4", Some((PREFIX_GH, 1, 7))),
            ("GX039999.MP4", Some((PREFIX_GX, 3, 9999))),
            ("gx010001.mp4", Some((PREFIX_GX, 1, 1))),
            ("Gh020001.Mp4", Some((PREFIX_GH, 2, 1))),
            ("gopr0001.mp4", Some((PREFIX_GOPR, 0, 1))),
            ("/card/DCIM/100GOPRO/GX010001.MP4", Some((PREFIX_GX, 1, 1))),
            // Not recognised.
            ("GX000001.MP4", None),
            ("GL010001.MP4", None),
            ("GG010001.MP4", None),
            ("G0010001.JPG", None),
            ("GOPR0001.JPG", None),
            ("GX010001.MOV", None),
            ("GX01001.MP4", None),
            ("GX0100012.MP4", None),
            ("GXA10001.MP4", None),
            ("GX01000A.MP4", None),
            ("GX01+001.MP4", None),
            ("GX010001", None),
            ("holiday.mp4", None),
        ];
        for (name, want) in cases {
            let got = parse_name(Path::new(name)).map(|n| (n.prefix, n.chapter, n.recording));
            assert_eq!(got, *want, "{name}");
        }
    }

    #[test]
    fn groups_table() {
        struct Case {
            name: &'static str,
            input: &'static [&'static str],
            want: Vec<ChapterGroup>,
        }
        let cases = [
            Case {
                name: "HERO5 and older, chapters out of order",
                input: &["d/GP020001.MP4", "d/GOPR0001.MP4", "d/GP010001.MP4"],
                want: vec![g(
                    Some(1),
                    PREFIX_GOPR,
                    &["d/GOPR0001.MP4", "d/GP010001.MP4", "d/GP020001.MP4"],
                )],
            },
            Case {
                name: "HERO6+ HEVC",
                input: &["d/GX030005.MP4", "d/GX010005.MP4", "d/GX020005.MP4"],
                want: vec![g(
                    Some(5),
                    PREFIX_GX,
                    &["d/GX010005.MP4", "d/GX020005.MP4", "d/GX030005.MP4"],
                )],
            },
            Case {
                name: "HERO6+ AVC",
                input: &["d/GH020005.MP4", "d/GH010005.MP4"],
                want: vec![g(Some(5), PREFIX_GH, &["d/GH010005.MP4", "d/GH020005.MP4"])],
            },
            Case {
                name: "mixed case names and extensions",
                input: &["d/gx020001.mp4", "d/GX010001.Mp4", "d/Gx030001.MP4"],
                want: vec![g(
                    Some(1),
                    PREFIX_GX,
                    &["d/GX010001.Mp4", "d/gx020001.mp4", "d/Gx030001.MP4"],
                )],
            },
            Case {
                name: "AVC and HEVC with the same number are separate groups",
                input: &["d/GX010003.MP4", "d/GH010003.MP4", "d/GH020003.MP4"],
                want: vec![
                    g(Some(3), PREFIX_GH, &["d/GH010003.MP4", "d/GH020003.MP4"]),
                    g(Some(3), PREFIX_GX, &["d/GX010003.MP4"]),
                ],
            },
            Case {
                name: "same recording in different directories is not grouped",
                input: &["b/GX020001.MP4", "a/GX010001.MP4", "b/GX010001.MP4"],
                want: vec![
                    g(Some(1), PREFIX_GX, &["a/GX010001.MP4"]),
                    g(Some(1), PREFIX_GX, &["b/GX010001.MP4", "b/GX020001.MP4"]),
                ],
            },
            Case {
                name: "proxies and thumbnails are ignored",
                input: &[
                    "d/GX010001.MP4",
                    "d/GL010001.LRV",
                    "d/GX010001.THM",
                    "d/GX010001.lrv",
                    "d/other.thm",
                ],
                want: vec![g(Some(1), PREFIX_GX, &["d/GX010001.MP4"])],
            },
            Case {
                name: "non-GoPro files are single groups after recordings, by name",
                input: &[
                    "d/zebra.mp4",
                    "d/GX010009.MP4",
                    "d/G0010001.JPG",
                    "d/GL010001.MP4",
                    "d/GX010002.MP4",
                    "d/apple.mov",
                ],
                want: vec![
                    g(Some(2), PREFIX_GX, &["d/GX010002.MP4"]),
                    g(Some(9), PREFIX_GX, &["d/GX010009.MP4"]),
                    g(None, "", &["d/G0010001.JPG"]),
                    g(None, "", &["d/GL010001.MP4"]),
                    g(None, "", &["d/apple.mov"]),
                    g(None, "", &["d/zebra.mp4"]),
                ],
            },
            Case {
                name: "ordered by directory first",
                input: &[
                    "b/GX010001.MP4",
                    "a/notes.mp4",
                    "a/GX010002.MP4",
                    "song.mp3",
                ],
                want: vec![
                    g(None, "", &["song.mp3"]),
                    g(Some(2), PREFIX_GX, &["a/GX010002.MP4"]),
                    g(None, "", &["a/notes.mp4"]),
                    g(Some(1), PREFIX_GX, &["b/GX010001.MP4"]),
                ],
            },
            Case {
                name: "empty input",
                input: &[],
                want: vec![],
            },
        ];
        for case in cases {
            assert_eq!(
                group_chapters(case.input.iter().copied()),
                case.want,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn gaps_are_grouped_and_reported() {
        let cases: &[(&[&str], bool)] = &[
            (&["GX010001.MP4", "GX030001.MP4"], true),
            (&["GX020001.MP4", "GX030001.MP4"], true),
            (&["GX010001.MP4", "GX020001.MP4", "GX030001.MP4"], false),
            (&["GX010001.MP4"], false),
            (&["GOPR0001.MP4", "GP020001.MP4"], true),
            (&["GP010001.MP4", "GP020001.MP4"], true),
            (&["GOPR0001.MP4", "GP010001.MP4"], false),
            (&["GOPR0001.MP4"], false),
            (&["holiday.mp4"], false),
        ];
        for (input, want) in cases {
            let groups = group_chapters(input.iter().copied());
            assert_eq!(groups.len(), 1, "{input:?}");
            assert_eq!(groups[0].files.len(), input.len(), "{input:?}");
            assert_eq!(groups[0].has_gaps(), *want, "{input:?}");
        }
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let input = [
            "d/GX020001.MP4",
            "e/x.mp4",
            "d/GH010001.MP4",
            "d/GX010001.MP4",
            "d/a.mp4",
        ];
        let mut reversed = input;
        reversed.reverse();
        assert_eq!(group_chapters(input), group_chapters(reversed));
    }

    fn arb_path() -> impl Strategy<Value = PathBuf> {
        let dir = prop::sample::select(vec!["", "a", "b", "/card/DCIM/100GOPRO"]);
        let head = prop::sample::select(vec![
            "GOPR", "gopr", "GP01", "GP02", "gp03", "GH01", "GH02", "gh03", "GX01", "GX02", "gx04",
            "GL01", "GX00", "VID_",
        ]);
        let rec = prop::sample::select(vec!["0001", "0002", "0010", "abcd"]);
        let ext = prop::sample::select(vec!["MP4", "mp4", "LRV", "lrv", "THM", "JPG", "MOV"]);
        (dir, head, rec, ext).prop_map(|(d, h, r, e)| Path::new(d).join(format!("{h}{r}.{e}")))
    }

    proptest! {
        #[test]
        fn grouping_partitions_non_proxy_inputs(paths in prop::collection::vec(arb_path(), 0..40)) {
            let groups = group_chapters(paths.clone());
            let mut want: Vec<PathBuf> = paths.into_iter().filter(|p| !is_proxy(p)).collect();
            let mut got: Vec<PathBuf> = groups.iter().flat_map(|g| g.files.clone()).collect();
            want.sort();
            got.sort();
            prop_assert_eq!(got, want);
            for group in &groups {
                prop_assert!(!group.files.is_empty());
                let dir = parent(&group.files[0]);
                prop_assert!(group.files.iter().all(|f| parent(f) == dir));
                match group.recording {
                    None => prop_assert_eq!(group.files.len(), 1),
                    Some(r) => {
                        for f in &group.files {
                            let n = parse_name(f).expect("GoPro group holds GoPro names");
                            prop_assert_eq!(n.recording, r);
                            prop_assert_eq!(n.prefix, group.prefix.as_str());
                        }
                    }
                }
            }
        }
    }
}
