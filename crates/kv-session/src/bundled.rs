//! Bundled music: the tracks shipped in the app's resource folder (ADR 0013).
//!
//! The app tells the session where that folder is
//! ([`Session::set_bundled_music_dir`]). A bundled track is referenced in
//! place while the project is being edited, like any other song; saving the
//! project copies it into the project's [`MUSIC_DIR`] folder and points the
//! `MusicRef` at the copy, so a saved project does not depend on where the
//! app is installed (see [`Session::save_as`]).
//!
//! When a bundled track cannot be referenced in place (on Windows, the app
//! is on another drive than the project), picking it still works: its
//! `MusicRef.path` is already the path the copy will have,
//! `music/<file name>`, and the session resolves that path to the bundled
//! file until the next save makes the copy. Nothing here touches the file
//! system.

use std::path::{Path, PathBuf};

use crate::paths::{self, Style};
use crate::{Session, SessionError};

/// The folder, inside the project folder, that saved bundled tracks are
/// copied to.
pub const MUSIC_DIR: &str = "music";

/// The session's bundled music state.
#[derive(Debug, Clone, Default)]
pub(crate) struct Bundled {
    /// The app's bundled music folder, if the app has one.
    pub(crate) dir: Option<PathBuf>,
    /// Bundled tracks picked but not reachable in place: project path
    /// (`music/<file name>`) to the bundled file, until the next save.
    pub(crate) pending: Vec<(String, PathBuf)>,
}

/// How a picked music file is named in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MusicPath {
    /// An ordinary project path to the file where it is.
    InPlace(String),
    /// A bundled track that cannot be referenced in place: the path its
    /// copy will have after the next save.
    Pending(String),
}

/// The file name of `path` when it is inside the folder `dir` (lexically;
/// both absolute), else `None`.
pub(crate) fn file_in(dir: &str, path: &str, style: Style) -> Option<String> {
    let rel = paths::to_project_path(dir, path, style).ok()?;
    if rel.split('/').any(|c| c == "..") {
        return None;
    }
    rel.rsplit('/').next().map(str::to_owned)
}

/// How the music file `path` (absolute) is named in a project whose media
/// root is `root`, given the bundled folder `bundled`: in place when it can
/// be, otherwise, for a bundled track, the path of its future copy.
pub(crate) fn music_path(
    root: &str,
    bundled: Option<&str>,
    path: &str,
    style: Style,
) -> Result<MusicPath, SessionError> {
    match paths::to_project_path(root, path, style) {
        Ok(p) => Ok(MusicPath::InPlace(p)),
        Err(e) => match bundled.and_then(|dir| file_in(dir, path, style)) {
            Some(name) => Ok(MusicPath::Pending(format!("{MUSIC_DIR}/{name}"))),
            None => Err(e),
        },
    }
}

fn utf8(path: &Path) -> Result<&str, SessionError> {
    path.to_str()
        .ok_or_else(|| SessionError::NonUtf8Path(path.to_owned()))
}

impl Session {
    /// Tell the session where the app's bundled music folder is (`None`:
    /// no bundled music). Music under it is copied into the project folder
    /// on save.
    pub fn set_bundled_music_dir(&mut self, dir: Option<PathBuf>) {
        self.bundled.dir = dir;
    }

    /// The bundled music folder (see [`Session::set_bundled_music_dir`]).
    pub fn bundled_music_dir(&self) -> Option<&Path> {
        self.bundled.dir.as_deref()
    }

    fn plan_music_path(&self, path: &Path) -> Result<MusicPath, SessionError> {
        let bundled = self.bundled.dir.as_deref().and_then(Path::to_str);
        music_path(utf8(&self.media_root)?, bundled, utf8(path)?, Style::NATIVE)
    }

    /// Check that the music file `path` (absolute) can be used in this
    /// project (see [`Session::music_project_path`]) without changing
    /// anything.
    pub fn check_music_path(&self, path: &Path) -> Result<(), SessionError> {
        self.plan_music_path(path).map(|_| ())
    }

    /// The project path to store in a `MusicRef` for the music file `path`
    /// (absolute). Like [`Session::to_project_path`], except that a bundled
    /// track that cannot be referenced in place (another drive) gets the
    /// path its copy will have, `music/<file name>`, which
    /// [`Session::resolve`] maps to the bundled file until the next save.
    pub fn music_project_path(&mut self, path: &Path) -> Result<String, SessionError> {
        match self.plan_music_path(path)? {
            MusicPath::InPlace(p) => Ok(p),
            MusicPath::Pending(p) => {
                self.bundled.pending.retain(|(k, _)| *k != p);
                self.bundled.pending.push((p.clone(), path.to_owned()));
                Ok(p)
            }
        }
    }

    /// The bundled file a pending project path stands for.
    pub(crate) fn pending_music(&self, project_path: &str) -> Option<&Path> {
        self.bundled
            .pending
            .iter()
            .find(|(k, _)| k == project_path)
            .map(|(_, v)| v.as_path())
    }

    /// The file name of `path` (absolute) when it is a bundled track.
    pub(crate) fn bundled_file_name(&self, path: &Path) -> Option<String> {
        let dir = self.bundled.dir.as_deref()?.to_str()?;
        file_in(dir, path.to_str()?, Style::NATIVE)
    }
}

#[cfg(test)]
mod tests {
    use super::Style::{Unix, Windows};
    use super::*;

    #[test]
    fn file_in_folder() {
        let res = "/Applications/kwikvid.app/Contents/Resources/music";
        assert_eq!(
            file_in(res, &format!("{res}/arpent.mp3"), Unix).as_deref(),
            Some("arpent.mp3")
        );
        assert_eq!(file_in(res, "/home/ana/arpent.mp3", Unix), None);
        assert_eq!(file_in(res, res, Unix), None);
        assert_eq!(
            file_in(r"C:\kwikvid\music", r"c:\KWIKVID\Music\a.mp3", Windows).as_deref(),
            Some("a.mp3")
        );
        assert_eq!(file_in(r"C:\kwikvid\music", r"D:\a.mp3", Windows), None);
    }

    #[test]
    fn bundled_track_in_place_when_reachable() {
        assert_eq!(
            music_path(
                "/home/ana",
                Some("/opt/kwikvid/music"),
                "/opt/kwikvid/music/a.mp3",
                Unix
            )
            .unwrap(),
            MusicPath::InPlace("../../opt/kwikvid/music/a.mp3".into())
        );
    }

    #[test]
    fn bundled_track_on_another_drive_is_pending() {
        let bundled = Some(r"C:\Program Files\kwikvid\music");
        let track = r"C:\Program Files\kwikvid\music\arpent.mp3";
        assert_eq!(
            music_path(r"D:\trips\p", bundled, track, Windows).unwrap(),
            MusicPath::Pending("music/arpent.mp3".into())
        );
        // Not bundled: refused as before.
        assert!(matches!(
            music_path(r"D:\trips\p", bundled, r"C:\Users\ana\s.mp3", Windows),
            Err(SessionError::OtherVolume(_))
        ));
        assert!(matches!(
            music_path(r"D:\trips\p", None, track, Windows),
            Err(SessionError::OtherVolume(_))
        ));
    }
}
