//! Pure, lexical path arithmetic between absolute file paths and project
//! paths (`/`-separated, relative to the media root).
//!
//! The string functions never touch the file system: `.` and `..`
//! components are folded lexically. Both Unix and Windows path syntax are
//! implemented as plain string parsing so that each is testable on every OS.
//!
//! The `*_native` helpers behind the public [`Session`](crate::Session)
//! methods use the host's [`Style::NATIVE`] and add one file system check:
//! the OS resolves `..` *physically* (after following symlinks), so when the
//! root is reached through a symlink a lexical `..` can point somewhere else.
//! When the files exist they verify the lexical answer against the real
//! (canonical) paths and fall back to arithmetic on those (issue #139).

use crate::SessionError;
use std::path::{Path, PathBuf};

/// Which OS path syntax to parse.
// Only the host's style is used outside tests; both are tested everywhere.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    /// `/` separator, absolute paths start with `/`, case-sensitive.
    Unix,
    /// `\` or `/` separators; absolute paths start with a drive (`C:\`) or a
    /// UNC share (`\\server\share\`), optionally behind a `\\?\` verbatim
    /// prefix; comparison ignores case.
    Windows,
}

impl Style {
    #[cfg(windows)]
    pub(crate) const NATIVE: Style = Style::Windows;
    #[cfg(not(windows))]
    pub(crate) const NATIVE: Style = Style::Unix;

    fn is_sep(self, c: char) -> bool {
        match self {
            Style::Unix => c == '/',
            Style::Windows => c == '/' || c == '\\',
        }
    }

    fn same(self, a: &str, b: &str) -> bool {
        match self {
            Style::Unix => a == b,
            Style::Windows => a.to_lowercase() == b.to_lowercase(),
        }
    }
}

/// An absolute path split into its root prefix and normalised components.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AbsPath<'a> {
    /// `""` for Unix, `C:` for a drive, `\\server\share` for UNC.
    prefix: String,
    components: Vec<&'a str>,
}

/// Parse an absolute path, or `None` if it is not absolute in `style`.
fn parse_abs(path: &str, style: Style) -> Option<AbsPath<'_>> {
    let (prefix, rest) = match style {
        Style::Unix => (String::new(), path.strip_prefix('/')?),
        Style::Windows => split_windows_prefix(path)?,
    };
    Some(AbsPath {
        prefix,
        components: normalise(rest.split(|c| style.is_sep(c)), true),
    })
}

/// Split `C:\rest` or `\\server\share\rest` (optionally verbatim) into a
/// prefix and the rest. `None` for relative, drive-relative (`C:foo`) and
/// rooted-without-drive (`\foo`) paths, which are not absolute on Windows.
fn split_windows_prefix(path: &str) -> Option<(String, &str)> {
    let sep = |c: char| c == '/' || c == '\\';
    let (path, verbatim) = match path.strip_prefix(r"\\?\") {
        Some(p) => (p, true),
        None => (path, false),
    };
    // Verbatim UNC: \\?\UNC\server\share\...
    let unc = if verbatim {
        path.strip_prefix("UNC\\")
            .or_else(|| path.strip_prefix("unc\\"))
    } else {
        path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//"))
    };
    if let Some(unc) = unc {
        let mut parts = unc.splitn(3, sep);
        let server = parts.next().filter(|s| !s.is_empty())?;
        let share = parts.next().filter(|s| !s.is_empty())?;
        let rest = parts.next().unwrap_or("");
        return Some((format!(r"\\{server}\{share}"), rest));
    }
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2] as char) {
        return Some((path[..2].to_owned(), &path[3..]));
    }
    // A bare drive root without the trailing separator ("C:") is relative to
    // the drive's current directory; only accept it as verbatim.
    if verbatim && b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some((path.to_owned(), ""));
    }
    None
}

/// Drop empty and `.` components and fold `..` into its parent. When
/// `rooted`, `..` at the root stays at the root; otherwise leading `..` are
/// kept.
fn normalise<'a>(components: impl Iterator<Item = &'a str>, rooted: bool) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for c in components {
        match c {
            "" | "." => {}
            ".." => match out.last() {
                Some(&last) if last != ".." => {
                    out.pop();
                }
                _ if rooted => {}
                _ => out.push(".."),
            },
            c => out.push(c),
        }
    }
    out
}

/// `path` (absolute) as a `/`-separated path relative to `root` (absolute),
/// with `..` steps when `path` is not under `root`.
pub(crate) fn to_project_path(
    root: &str,
    path: &str,
    style: Style,
) -> Result<String, SessionError> {
    let root_abs =
        parse_abs(root, style).ok_or_else(|| SessionError::RootNotAbsolute(root.into()))?;
    let path_abs =
        parse_abs(path, style).ok_or_else(|| SessionError::PathNotAbsolute(path.into()))?;
    if !style.same(&root_abs.prefix, &path_abs.prefix) {
        return Err(SessionError::OtherVolume(path.into()));
    }
    let common = root_abs
        .components
        .iter()
        .zip(&path_abs.components)
        .take_while(|(a, b)| style.same(a, b))
        .count();
    let ups = root_abs.components.len() - common;
    let parts: Vec<&str> = std::iter::repeat_n("..", ups)
        .chain(path_abs.components[common..].iter().copied())
        .collect();
    if parts.is_empty() {
        return Err(SessionError::PathIsRoot(path.into()));
    }
    Ok(parts.join("/"))
}

/// The absolute path a project path refers to: `root` joined with
/// `project_path` (`/`-separated), `..` folded lexically. A `..` above the
/// file system root stays at the root.
pub(crate) fn resolve(root: &str, project_path: &str, style: Style) -> String {
    let (prefix, mut components) = match parse_abs(root, style) {
        Some(abs) => (Some(abs.prefix), abs.components),
        // Not absolute (should not happen for a session): keep it as a
        // relative base and join onto it.
        None => (None, normalise(root.split(|c| style.is_sep(c)), false)),
    };
    let rooted = prefix.is_some();
    components.extend(project_path.split('/'));
    let components = normalise(components.into_iter(), rooted);
    match (style, prefix) {
        (Style::Unix, Some(_)) => format!("/{}", components.join("/")),
        (Style::Windows, Some(p)) => format!(r"{p}\{}", components.join(r"\")),
        (Style::Unix, None) => components.join("/"),
        (Style::Windows, None) => components.join(r"\"),
    }
}

/// [`to_project_path`] for host paths.
///
/// The lexical answer is kept when it also holds physically (or the files
/// cannot be checked). When `root` is reached through a symlink and the
/// lexical `..` steps would land elsewhere on disk, the path is computed
/// between the canonical root and file instead, so that it resolves from
/// `root` both physically and through [`resolve_native`].
pub(crate) fn to_project_path_native(root: &Path, path: &Path) -> Result<String, SessionError> {
    let lexical = to_project_path(utf8(root)?, utf8(path)?, Style::NATIVE)?;
    let (Ok(real_root), Ok(real_path)) = (root.canonicalize(), path.canonicalize()) else {
        return Ok(lexical);
    };
    if root.join(&lexical).canonicalize().ok().as_ref() == Some(&real_path) {
        return Ok(lexical);
    }
    match (real_root.to_str(), real_path.to_str()) {
        (Some(r), Some(p)) => Ok(to_project_path(r, p, Style::NATIVE).unwrap_or(lexical)),
        _ => Ok(lexical),
    }
}

pub(crate) fn utf8(path: &Path) -> Result<&str, SessionError> {
    path.to_str()
        .ok_or_else(|| SessionError::NonUtf8Path(path.to_owned()))
}

/// [`resolve`] for host paths. A non-UTF-8 root (which cannot have produced
/// any project path) is joined without normalisation.
///
/// The lexical answer is kept unless the file the OS finds at
/// `root/project_path` (resolving `..` physically) is a different one; then
/// the path is resolved from the canonical root instead.
pub(crate) fn resolve_native(root: &Path, project_path: &str) -> PathBuf {
    let Some(r) = root.to_str() else {
        return project_path
            .split('/')
            .fold(root.to_owned(), |p, c| p.join(c));
    };
    let lexical = PathBuf::from(resolve(r, project_path, Style::NATIVE));
    let joined = project_path
        .split('/')
        .fold(root.to_owned(), |p, c| p.join(c));
    let Ok(physical) = joined.canonicalize() else {
        return lexical;
    };
    if lexical.canonicalize().ok().as_ref() == Some(&physical) {
        return lexical;
    }
    match root.canonicalize().ok().as_ref().and_then(|r| r.to_str()) {
        Some(real_root) => PathBuf::from(resolve(real_root, project_path, Style::NATIVE)),
        None => lexical,
    }
}

#[cfg(test)]
mod tests {
    use super::Style::{Unix, Windows};
    use super::*;

    fn rel(root: &str, path: &str, style: Style) -> Result<String, SessionError> {
        to_project_path(root, path, style)
    }

    #[test]
    fn unix_inside_root() {
        let root = "/home/ana/Movies";
        assert_eq!(
            rel(root, "/home/ana/Movies/GX01.MP4", Unix).unwrap(),
            "GX01.MP4"
        );
        assert_eq!(
            rel(root, "/home/ana/Movies/trip/day 1/GX01.MP4", Unix).unwrap(),
            "trip/day 1/GX01.MP4"
        );
        // Trailing separator on the root, `.` and `..` in the input.
        assert_eq!(
            rel("/home/ana/Movies/", "/home/ana/Movies/./x/../a.mp4", Unix).unwrap(),
            "a.mp4"
        );
    }

    #[test]
    fn unix_outside_root_uses_dot_dot() {
        let root = "/home/ana/Movies/project";
        assert_eq!(
            rel(root, "/home/ana/Music/song.mp3", Unix).unwrap(),
            "../../Music/song.mp3"
        );
        assert_eq!(
            rel(root, "/home/ana/Movies/a.mp4", Unix).unwrap(),
            "../a.mp4"
        );
        assert_eq!(rel(root, "/x.mp4", Unix).unwrap(), "../../../../x.mp4");
        // A sibling whose name starts with the root's name is not inside it.
        assert_eq!(
            rel(root, "/home/ana/Movies/project2/a.mp4", Unix).unwrap(),
            "../project2/a.mp4"
        );
        // An ancestor of the root.
        assert_eq!(rel(root, "/home/ana", Unix).unwrap(), "../..");
    }

    #[test]
    fn unix_root_at_filesystem_root() {
        assert_eq!(rel("/", "/a/b.mp4", Unix).unwrap(), "a/b.mp4");
    }

    #[test]
    fn unix_is_case_sensitive_and_backslash_is_a_name_char() {
        assert_eq!(
            rel("/Users/ana", "/users/ana/a.mp4", Unix).unwrap(),
            "../../users/ana/a.mp4"
        );
        assert_eq!(rel("/r", r"/r/a\b.mp4", Unix).unwrap(), r"a\b.mp4");
    }

    #[test]
    fn unix_errors() {
        assert_eq!(
            rel("/r", "/r", Unix),
            Err(SessionError::PathIsRoot("/r".into()))
        );
        assert_eq!(
            rel("/r", "/r/sub/..", Unix),
            Err(SessionError::PathIsRoot("/r/sub/..".into()))
        );
        assert_eq!(
            rel("/r", "a.mp4", Unix),
            Err(SessionError::PathNotAbsolute("a.mp4".into()))
        );
        assert_eq!(
            rel("r", "/r/a.mp4", Unix),
            Err(SessionError::RootNotAbsolute("r".into()))
        );
        // Windows syntax is not absolute on Unix.
        assert!(matches!(
            rel("/r", r"C:\r\a.mp4", Unix),
            Err(SessionError::PathNotAbsolute(_))
        ));
    }

    #[test]
    fn windows_inside_and_outside_root() {
        let root = r"C:\Users\Ana\Videos\project";
        assert_eq!(
            rel(root, r"C:\Users\Ana\Videos\project\clips\GX01.MP4", Windows).unwrap(),
            "clips/GX01.MP4"
        );
        assert_eq!(
            rel(root, r"C:\Users\Ana\Music\song.mp3", Windows).unwrap(),
            "../../Music/song.mp3"
        );
        // Mixed separators, case-insensitive drive and folders; output keeps
        // the file's own spelling.
        assert_eq!(
            rel(root, "c:/users/ana/videos/PROJECT/A.mp4", Windows).unwrap(),
            "A.mp4"
        );
        assert_eq!(rel(r"C:\", r"C:\a\b.mp4", Windows).unwrap(), "a/b.mp4");
    }

    #[test]
    fn windows_unc_and_verbatim() {
        let root = r"\\nas\media\project";
        assert_eq!(
            rel(root, r"\\nas\media\project\a.mp4", Windows).unwrap(),
            "a.mp4"
        );
        assert_eq!(
            rel(root, r"\\NAS\Media\music\b.mp3", Windows).unwrap(),
            "../music/b.mp3"
        );
        assert_eq!(rel(r"C:\p", r"\\?\C:\p\a.mp4", Windows).unwrap(), "a.mp4");
        assert_eq!(
            rel(root, r"\\?\UNC\nas\media\project\a.mp4", Windows).unwrap(),
            "a.mp4"
        );
    }

    #[test]
    fn windows_errors() {
        let root = r"C:\p";
        assert!(matches!(
            rel(root, r"D:\p\a.mp4", Windows),
            Err(SessionError::OtherVolume(_))
        ));
        assert!(matches!(
            rel(root, r"\\nas\share\a.mp4", Windows),
            Err(SessionError::OtherVolume(_))
        ));
        assert!(matches!(
            rel(root, r"C:\p\", Windows),
            Err(SessionError::PathIsRoot(_))
        ));
        assert!(matches!(
            rel(root, r"c:/P", Windows),
            Err(SessionError::PathIsRoot(_))
        ));
        // Relative, drive-relative and drive-less rooted paths.
        for p in [r"p\a.mp4", r"C:a.mp4", r"\p\a.mp4", "/p/a.mp4", r"\\nas"] {
            assert!(
                matches!(rel(root, p, Windows), Err(SessionError::PathNotAbsolute(_))),
                "{p}"
            );
        }
        assert!(matches!(
            rel("/home/ana", r"C:\a.mp4", Windows),
            Err(SessionError::RootNotAbsolute(_))
        ));
    }

    #[test]
    fn resolve_unix() {
        let root = "/home/ana/Movies/project";
        assert_eq!(
            resolve(root, "clips/a.mp4", Unix),
            "/home/ana/Movies/project/clips/a.mp4"
        );
        assert_eq!(
            resolve(root, "../../Music/s.mp3", Unix),
            "/home/ana/Music/s.mp3"
        );
        assert_eq!(resolve("/", "a.mp4", Unix), "/a.mp4");
        // `..` above the file system root stays at the root.
        assert_eq!(resolve("/a", "../../../b", Unix), "/b");
    }

    #[test]
    fn resolve_windows() {
        let root = r"C:\Users\Ana\project";
        assert_eq!(
            resolve(root, "clips/a.mp4", Windows),
            r"C:\Users\Ana\project\clips\a.mp4"
        );
        assert_eq!(
            resolve(root, "../Music/s.mp3", Windows),
            r"C:\Users\Ana\Music\s.mp3"
        );
        assert_eq!(
            resolve(r"\\nas\media\p", "../a.mp4", Windows),
            r"\\nas\media\a.mp4"
        );
        assert_eq!(resolve(r"C:\", "a.mp4", Windows), r"C:\a.mp4");
    }

    #[test]
    fn round_trips() {
        let cases: &[(&str, &str, Style)] = &[
            ("/home/ana/p", "/home/ana/p/a/b.mp4", Unix),
            ("/home/ana/p", "/home/ana/q/b.mp4", Unix),
            ("/home/ana/p", "/b.mp4", Unix),
            (r"C:\Users\Ana\p", r"C:\Users\Ana\p\a\b.mp4", Windows),
            (r"C:\Users\Ana\p", r"C:\Other\b.mp4", Windows),
            (r"\\nas\media\p", r"\\nas\media\x\b.mp4", Windows),
        ];
        for &(root, path, style) in cases {
            let p = to_project_path(root, path, style).unwrap();
            assert_eq!(resolve(root, &p, style), path, "{root} {path}");
        }
    }

    #[test]
    fn native_helpers() {
        #[cfg(not(windows))]
        let (root, file) = ("/home/ana/p", "/home/ana/music/s.mp3");
        #[cfg(windows)]
        let (root, file) = (r"C:\Users\ana\p", r"C:\Users\ana\music\s.mp3");
        let p = to_project_path_native(Path::new(root), Path::new(file)).unwrap();
        assert_eq!(p, "../music/s.mp3");
        assert_eq!(resolve_native(Path::new(root), &p), PathBuf::from(file));
    }
}
