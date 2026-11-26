//! Project files: save, save as, open, and the recent projects list.
//!
//! A project file is the [`kv_core::Project`] as pretty-printed JSON with the
//! extension [`PROJECT_EXTENSION`] (`.kwikvid`; a reversible decision). Every
//! media and music path in it is relative to the **folder that contains the
//! file**, so a project folder moved or copied together with its media still
//! opens. Files are written atomically: to a temporary file in the same
//! folder, flushed, then renamed over the target, so a crash mid-save never
//! leaves a half-written project.
//!
//! This is the only module of the crate that touches the file system.

use crate::paths;
use crate::{Job, Session, SessionError, Snapshot};
use kv_core::{FormatVersion, History, MediaId, Project, SectionId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// File extension of kwikvid project files, without the dot.
pub const PROJECT_EXTENSION: &str = "kwikvid";

/// Why saving or opening a project (or the recent list) failed.
///
/// `Display` is a short, user-facing sentence; the file and the underlying
/// error stay reachable through the fields and
/// [`std::error::Error::source`].
#[derive(Debug, Error)]
pub enum ProjectFileError {
    /// Reading or writing the file failed.
    #[error("The project file couldn't be read or written.")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The file is not valid JSON or not a kwikvid project.
    #[error("That file is damaged or is not a kwikvid project.")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The file was written in a project format this build cannot read.
    #[error("That project was made with a different version of kwikvid (format {found}, this version reads format {}).", FormatVersion::CURRENT)]
    UnsupportedVersion { path: PathBuf, found: String },

    /// The path has no file name or no parent folder (for example `/`).
    #[error("That isn't a valid location for a project file.")]
    InvalidLocation(PathBuf),

    /// [`Session::save`] on a project that was never saved.
    #[error("Choose where to save the project first.")]
    Untitled,

    /// A media path could not be rebased onto the new project folder (for
    /// example the media is on another drive), or a path is not UTF-8.
    #[error("{0}")]
    Path(#[from] SessionError),

    /// The data could not be encoded as JSON (a path that is not UTF-8).
    #[error("The project couldn't be saved.")]
    Encode(#[source] serde_json::Error),
}

/// A media or music file referenced by an opened project that does not exist
/// on disk, for the UI to show (relinking is a separate feature).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingFile {
    pub kind: MissingKind,
    /// The path as stored in the project (relative to the project folder).
    pub project_path: String,
    /// Where kwikvid looked for it.
    pub resolved: PathBuf,
}

/// What a [`MissingFile`] is used as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MissingKind {
    /// A clip or photo in the media library.
    Media { media_id: MediaId },
    /// The music of a section.
    Music { section: SectionId },
}

/// `path` (a project path relative to `from_root`) as a project path relative
/// to `to_root`: `/`-separated on every OS, with `..` steps when the file is
/// outside `to_root`. Symlinked roots are handled like
/// [`Session::to_project_path`] and [`Session::resolve`] handle them.
///
/// Errors when the file cannot be reached relatively from `to_root` (another
/// drive or share), a root is not absolute, or the file is `to_root` itself.
pub fn rebase(path: &str, from_root: &Path, to_root: &Path) -> Result<String, SessionError> {
    let absolute = paths::resolve_native(from_root, path);
    paths::to_project_path_native(to_root, &absolute)
}

impl Session {
    /// The project file this session was opened from or last saved to;
    /// `None` for an untitled project.
    pub fn file_path(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Write the project to `path` and make it the session's file.
    ///
    /// `.kwikvid` is appended when `path` has another extension or none.
    /// Every media and music path is rebased from the current media root to
    /// the file's folder (see [`rebase`]), the media root becomes that
    /// folder, and the project is marked saved. The revision is bumped
    /// because the project's paths and media root changed.
    ///
    /// Bundled music (a file under [`Session::bundled_music_dir`]) is
    /// copied into the project's [`MUSIC_DIR`](crate::MUSIC_DIR) folder,
    /// as `music/<file name>`, and its path points at the copy, so the
    /// project no longer depends on where the app is installed. A file of
    /// that name with the same contents is reused; one with other contents
    /// is kept and the copy gets a numbered name (`arpent-2.mp3`).
    ///
    /// The rebase is **not** an undoable command: it is not an edit (every
    /// path still points at the same file, or a copy of it), so there is
    /// nothing to undo. Instead the whole history (every undo and redo
    /// state) is rebased with it, so undo and redo keep working against the
    /// new media root.
    ///
    /// On error nothing changes and no file is left behind.
    pub fn save_as(&mut self, path: &Path) -> Result<Snapshot, ProjectFileError> {
        let file = with_project_extension(absolute(path)?);
        let dir = parent_dir(&file)?;
        let plan = self.plan_save(&dir)?;
        write_project(&file, &plan)?;

        tracing::info!(file = %file.display(), copied = plan.copies.len(), "project saved as");
        self.thumbnails.rebase(&self.media_root, &dir);
        self.finish_save(plan, &dir);
        self.media_root = dir;
        self.file = Some(file);
        self.revision += 1;
        Ok(self.snapshot())
    }

    /// Write the project to its current file and mark it saved.
    /// [`ProjectFileError::Untitled`] when it has no file yet (use
    /// [`Session::save_as`]). Bundled music is copied into the project
    /// folder as in [`Session::save_as`]; the revision is bumped only when
    /// that changed a path. A second save copies nothing.
    pub fn save(&mut self) -> Result<Snapshot, ProjectFileError> {
        let file = self.file.clone().ok_or(ProjectFileError::Untitled)?;
        let dir = self.media_root.clone();
        let plan = self.plan_save(&dir)?;
        write_project(&file, &plan)?;
        tracing::info!(file = %file.display(), copied = plan.copies.len(), "project saved");
        if plan.changed {
            self.revision += 1;
        }
        self.finish_save(plan, &dir);
        Ok(self.snapshot())
    }

    /// The history as it will be saved in the folder `dir`, and the
    /// bundled music to copy there. Reads the existing files in `dir`'s
    /// music folder; writes nothing.
    fn plan_save(&self, dir: &Path) -> Result<SavePlan, ProjectFileError> {
        let mut history: History = self.history.clone();
        let mut renamed: HashMap<String, String> = HashMap::new();
        let mut targets: HashMap<PathBuf, String> = HashMap::new();
        let mut copies: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut changed = false;
        history.try_map_paths(|p| -> Result<String, ProjectFileError> {
            if let Some(new) = renamed.get(p) {
                return Ok(new.clone());
            }
            let source = self.resolve(p);
            let new = match self.bundled_file_name(&source) {
                None => rebase(p, &self.media_root, dir)?,
                Some(name) => {
                    changed = true;
                    match targets.get(&source) {
                        Some(t) => t.clone(),
                        None => {
                            let (target, copy) = copy_target(&source, &name, dir, &targets)?;
                            if copy {
                                copies.push((source.clone(), paths::resolve_native(dir, &target)));
                            }
                            targets.insert(source, target.clone());
                            target
                        }
                    }
                }
            };
            renamed.insert(p.to_owned(), new.clone());
            Ok(new)
        })?;
        Ok(SavePlan {
            history,
            renamed,
            copies,
            changed,
        })
    }

    /// Adopt a written [`SavePlan`] whose paths are relative to `dir`.
    fn finish_save(&mut self, plan: SavePlan, dir: &Path) {
        let from = self.media_root.clone();
        let renamed = plan.renamed;
        self.music.remap(|k| {
            renamed
                .get(k)
                .cloned()
                .or_else(|| rebase(k, &from, dir).ok())
        });
        self.bundled.pending.clear();
        self.history = plan.history;
        self.saved = self.history.project().clone();
    }

    /// Open a project file. The media root is the file's folder. Returns the
    /// session (clean, no undo history) and the media and music files that
    /// do not exist, for the UI to report.
    pub fn open(path: &Path) -> Result<(Session, Vec<MissingFile>), ProjectFileError> {
        let file = absolute(path)?;
        let dir = parent_dir(&file)?;
        let text = fs::read_to_string(&file).map_err(|source| ProjectFileError::Io {
            path: file.clone(),
            source,
        })?;
        let project = parse_project(&file, &text)?;
        let session = Session {
            saved: project.clone(),
            history: History::new(project),
            media_root: dir,
            revision: 0,
            file: Some(file),
            music: Default::default(),
            thumbnails: Default::default(),
            bundled: Default::default(),
        };
        let missing = session.missing_files();
        tracing::info!(
            file = %path.display(),
            missing = missing.len(),
            "project opened"
        );
        Ok((session, missing))
    }

    /// Media and music files of the current project that do not exist at
    /// their resolved path.
    pub fn missing_files(&self) -> Vec<MissingFile> {
        let project = self.project();
        let media = project
            .media
            .iter()
            .map(|m| (MissingKind::Media { media_id: m.id }, m.path.as_str()));
        let music = project.sections.iter().filter_map(|s| {
            s.music
                .as_ref()
                .map(|m| (MissingKind::Music { section: s.id }, m.path.as_str()))
        });
        media
            .chain(music)
            .filter_map(|(kind, project_path)| {
                let resolved = self.resolve(project_path);
                (!resolved.exists()).then(|| MissingFile {
                    kind,
                    project_path: project_path.to_owned(),
                    resolved,
                })
            })
            .collect()
    }
}

impl Session {
    /// The background jobs to run after opening a project: music analysis
    /// for every section's music, and probe plus HiLights for every media
    /// item, skipping files that do not exist (see
    /// [`Session::missing_files`]). Every job checks `kv-cache` first, so
    /// reopening an unchanged project is served from the cache.
    pub fn reopen_jobs(&self) -> Vec<Job> {
        let project = self.project();
        let music = project
            .sections
            .iter()
            .filter_map(|s| s.music.as_ref())
            .map(|m| self.resolve(&m.path))
            .filter(|p| p.exists())
            .map(Job::AnalyzeMusic);
        let media = project
            .media
            .iter()
            .map(|m| self.resolve(&m.path))
            .filter(|p| p.exists())
            .flat_map(|p| [Job::ProbeMedia(p.clone()), Job::ReadHilights(p)]);
        let mut jobs: Vec<Job> = Vec::new();
        for job in music.chain(media) {
            // Two sections may share a song.
            if !jobs.contains(&job) {
                jobs.push(job);
            }
        }
        jobs
    }
}

/// Parse a project file, reporting an unsupported `version` by itself
/// rather than as a generic parse error.
fn parse_project(file: &Path, text: &str) -> Result<Project, ProjectFileError> {
    let corrupt = |source| ProjectFileError::Corrupt {
        path: file.to_owned(),
        source,
    };
    let value: serde_json::Value = serde_json::from_str(text).map_err(corrupt)?;
    if let Some(version) = value.get("version") {
        if version.as_u64() != Some(u64::from(FormatVersion::CURRENT)) {
            return Err(ProjectFileError::UnsupportedVersion {
                path: file.to_owned(),
                found: version.to_string(),
            });
        }
    }
    serde_json::from_value(value).map_err(corrupt)
}

/// The most recently opened or saved project files, newest first, at most
/// [`RecentProjects::LIMIT`]. Persisted as JSON in a config folder chosen by
/// the caller (the app passes its platform config dir).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProjects {
    projects: Vec<PathBuf>,
}

/// On-disk form of [`RecentProjects`].
#[derive(Serialize, Deserialize)]
struct RecentFile {
    version: u32,
    projects: Vec<PathBuf>,
}

impl RecentProjects {
    /// How many projects the list keeps.
    pub const LIMIT: usize = 10;
    /// The list's file name inside the config folder.
    pub const FILE_NAME: &'static str = "recent-projects.json";
    const VERSION: u32 = 1;

    /// Load the list from `config_dir`. A missing file is an empty list; a
    /// damaged one is an error (callers may fall back to an empty list).
    pub fn load(config_dir: &Path) -> Result<RecentProjects, ProjectFileError> {
        let file = config_dir.join(Self::FILE_NAME);
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ProjectFileError::Io { path: file, source }),
        };
        let parsed: RecentFile =
            serde_json::from_str(&text).map_err(|source| ProjectFileError::Corrupt {
                path: file.clone(),
                source,
            })?;
        if parsed.version != Self::VERSION {
            return Err(ProjectFileError::UnsupportedVersion {
                path: file,
                found: parsed.version.to_string(),
            });
        }
        let mut list = Self::default();
        // Oldest first, so the first entry ends up newest; also dedups and
        // enforces the limit on a hand-edited file.
        for p in parsed.projects.iter().rev() {
            list.add(p);
        }
        Ok(list)
    }

    /// Write the list to `config_dir` (created if needed), atomically.
    pub fn save(&self, config_dir: &Path) -> Result<(), ProjectFileError> {
        fs::create_dir_all(config_dir).map_err(|source| ProjectFileError::Io {
            path: config_dir.to_owned(),
            source,
        })?;
        let data = RecentFile {
            version: Self::VERSION,
            projects: self.projects.clone(),
        };
        write_json_atomic(&config_dir.join(Self::FILE_NAME), &data)
    }

    /// Put `path` at the top, removing an older entry for it and dropping
    /// the oldest entries beyond [`RecentProjects::LIMIT`].
    pub fn add(&mut self, path: &Path) {
        self.remove(path);
        self.projects.insert(0, path.to_owned());
        self.projects.truncate(Self::LIMIT);
    }

    /// Remove `path` (for example a project that no longer exists).
    pub fn remove(&mut self, path: &Path) {
        self.projects.retain(|p| p != path);
    }

    /// The projects, newest first.
    pub fn paths(&self) -> &[PathBuf] {
        &self.projects
    }
}

/// `path` made absolute against the current directory (lexically; the file
/// need not exist).
fn absolute(path: &Path) -> Result<PathBuf, ProjectFileError> {
    std::path::absolute(path).map_err(|source| ProjectFileError::Io {
        path: path.to_owned(),
        source,
    })
}

/// Append `.kwikvid` unless the path already has that extension (any case).
fn with_project_extension(path: PathBuf) -> PathBuf {
    let has_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION));
    if has_ext {
        return path;
    }
    let mut s = OsString::from(path);
    s.push(".");
    s.push(PROJECT_EXTENSION);
    PathBuf::from(s)
}

/// The folder that contains `file`.
fn parent_dir(file: &Path) -> Result<PathBuf, ProjectFileError> {
    match (file.parent(), file.file_name()) {
        (Some(dir), Some(_)) => Ok(dir.to_owned()),
        _ => Err(ProjectFileError::InvalidLocation(file.to_owned())),
    }
}

/// A save worked out by [`Session::plan_save`].
struct SavePlan {
    /// The history with every path relative to the new project folder.
    history: History,
    /// Old project path to new, for every path in the history.
    renamed: HashMap<String, String>,
    /// Bundled files to copy: (bundled file, destination).
    copies: Vec<(PathBuf, PathBuf)>,
    /// Some bundled music was (or is about to be) moved to a copy.
    changed: bool,
}

/// Where the bundled file `source` named `name` goes in the project folder
/// `dir`: the first of `music/<name>`, `music/<stem>-2.<ext>`, ... that no
/// other planned copy (`taken`) uses and that is free or already holds the
/// same contents. Returns the project path and whether it must be copied.
fn copy_target(
    source: &Path,
    name: &str,
    dir: &Path,
    taken: &HashMap<PathBuf, String>,
) -> Result<(String, bool), ProjectFileError> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    for n in 1.. {
        let candidate = match (n, ext) {
            (1, _) => name.to_owned(),
            (n, Some(ext)) => format!("{stem}-{n}.{ext}"),
            (n, None) => format!("{stem}-{n}"),
        };
        let target = format!("{}/{candidate}", crate::MUSIC_DIR);
        if taken.values().any(|t| *t == target) {
            continue;
        }
        let dest = paths::resolve_native(dir, &target);
        match fs::symlink_metadata(&dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((target, true)),
            Err(source) => return Err(ProjectFileError::Io { path: dest, source }),
            Ok(_) if same_contents(source, &dest)? => return Ok((target, false)),
            Ok(_) => {}
        }
    }
    unreachable!("an unbounded range always yields a free name")
}

/// Whether the files `a` and `b` have the same bytes.
fn same_contents(a: &Path, b: &Path) -> Result<bool, ProjectFileError> {
    let read = |p: &Path| {
        fs::read(p).map_err(|source| ProjectFileError::Io {
            path: p.to_owned(),
            source,
        })
    };
    let (ma, mb) = (fs::metadata(a), fs::metadata(b));
    if let (Ok(ma), Ok(mb)) = (&ma, &mb) {
        if !mb.is_file() || ma.len() != mb.len() {
            return Ok(false);
        }
    }
    Ok(read(a)? == read(b)?)
}

/// Copy the plan's bundled files, then write its project to `file`. On
/// error, the copies made so far are removed and `file` is untouched.
fn write_project(file: &Path, plan: &SavePlan) -> Result<(), ProjectFileError> {
    let mut made: Vec<&Path> = Vec::new();
    let result = (|| {
        for (source, dest) in &plan.copies {
            copy_atomic(source, dest)?;
            made.push(dest);
        }
        write_json_atomic(file, plan.history.project())
    })();
    if result.is_err() {
        for dest in made {
            // Best effort: the project file still points at the bundled file.
            let _ = fs::remove_file(dest);
        }
    }
    result
}

/// Copy `source` to `dest` (creating its folder) through a temporary file
/// in the same folder, so `dest` is never half written.
fn copy_atomic(source: &Path, dest: &Path) -> Result<(), ProjectFileError> {
    let dir = parent_dir(dest)?;
    let io_err = |path: &Path| {
        let path = path.to_owned();
        move |source| ProjectFileError::Io { path, source }
    };
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    let tmp = temp_path(dest)?;
    let copy = || -> io::Result<()> {
        fs::copy(source, &tmp)?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, dest)
    };
    copy().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(dest)(e)
    })?;
    tracing::info!(from = %source.display(), to = %dest.display(), "bundled music copied");
    Ok(())
}

/// The hidden temporary file next to `path` that an atomic write uses.
fn temp_path(path: &Path) -> Result<PathBuf, ProjectFileError> {
    let dir = parent_dir(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| ProjectFileError::InvalidLocation(path.to_owned()))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    Ok(dir.join(tmp_name))
}

/// Write `value` as pretty JSON to `path` atomically: to a temporary file in
/// the same folder, flushed to disk, then renamed over `path`. On error the
/// temporary file is removed and `path` is untouched.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ProjectFileError> {
    let mut json = serde_json::to_vec_pretty(value).map_err(ProjectFileError::Encode)?;
    json.push(b'\n');
    let tmp = temp_path(path)?;

    let write = || -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    };
    write().map_err(|source| {
        // Best effort: the temp file may not exist if create failed.
        let _ = fs::remove_file(&tmp);
        ProjectFileError::Io {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_between_roots() {
        #[cfg(not(windows))]
        let (home, trip, other) = ("/home/ana", "/home/ana/Movies/trip", "/home/bob");
        #[cfg(windows)]
        let (home, trip, other) = (
            r"C:\Users\ana",
            r"C:\Users\ana\Movies\trip",
            r"C:\Users\bob",
        );
        let (home, trip, other) = (Path::new(home), Path::new(trip), Path::new(other));
        assert_eq!(rebase("Movies/trip/a.mp4", home, trip).unwrap(), "a.mp4");
        assert_eq!(
            rebase("Music/s.mp3", home, trip).unwrap(),
            "../../Music/s.mp3"
        );
        assert_eq!(
            rebase("../../Music/s.mp3", trip, home).unwrap(),
            "Music/s.mp3"
        );
        assert_eq!(rebase("a/b.mp4", home, other).unwrap(), "../ana/a/b.mp4");
        assert_eq!(rebase("a/b.mp4", home, home).unwrap(), "a/b.mp4");
        assert!(matches!(
            rebase("x.mp4", Path::new("relative"), trip),
            Err(SessionError::PathNotAbsolute(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rebase_to_other_drive_fails() {
        assert!(matches!(
            rebase("a.mp4", Path::new(r"C:\p"), Path::new(r"D:\q")),
            Err(SessionError::OtherVolume(_))
        ));
    }

    /// A bundled track picked from another drive (simulated: the pending
    /// entry `music_project_path` makes on Windows) resolves to the bundled
    /// file until saved, and the save copies it.
    #[test]
    fn pending_bundled_track_is_copied_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled = tmp.path().join("app").join("music");
        fs::create_dir_all(&bundled).unwrap();
        let track = bundled.join("arpent.mp3");
        fs::write(&track, b"audio").unwrap();
        let project = tmp.path().join("p");
        fs::create_dir_all(&project).unwrap();

        let mut s = Session::new(project.clone());
        s.set_bundled_music_dir(Some(bundled));
        s.bundled
            .pending
            .push(("music/arpent.mp3".into(), track.clone()));
        s.apply(kv_core::Command::SetMusic {
            section: Project::DEFAULT_SECTION,
            music: Some(kv_core::MusicRef {
                path: "music/arpent.mp3".into(),
                duration: kv_core::Micros(1),
            }),
        })
        .unwrap();
        assert_eq!(s.resolve("music/arpent.mp3"), track);
        assert_eq!(s.missing_files(), []);

        s.save_as(&project.join("p.kwikvid")).unwrap();
        let copy = project.join("music").join("arpent.mp3");
        assert_eq!(fs::read(&copy).unwrap(), b"audio");
        assert_eq!(s.resolve("music/arpent.mp3"), copy);
        assert!(s.bundled.pending.is_empty());
    }

    #[test]
    fn project_extension_is_appended_once() {
        let p = |s: &str| with_project_extension(PathBuf::from(s));
        assert_eq!(p("trip.kwikvid"), PathBuf::from("trip.kwikvid"));
        assert_eq!(p("trip.KWIKVID"), PathBuf::from("trip.KWIKVID"));
        assert_eq!(p("trip"), PathBuf::from("trip.kwikvid"));
        assert_eq!(p("trip.v2"), PathBuf::from("trip.v2.kwikvid"));
    }

    #[test]
    fn unsupported_version_is_readable() {
        let err = parse_project(Path::new("p.kwikvid"), r#"{"version":2,"id":1}"#).unwrap_err();
        assert!(matches!(
            &err,
            ProjectFileError::UnsupportedVersion { found, .. } if found == "2"
        ));
        assert_eq!(
            err.to_string(),
            "That project was made with a different version of kwikvid \
             (format 2, this version reads format 1)."
        );
    }
}
