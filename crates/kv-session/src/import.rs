//! Import: files and folders into library items (MVP items 1-2).
//!
//! [`Session::import`] scans folders (not recursively) for `.mp4`/`.mov`,
//! groups GoPro chapter files into one recording with
//! [`kv_gopro::group_chapters`], probes each file and reads its HiLights
//! (cache first, like [`Job::ProbeMedia`] and [`Job::ReadHilights`]), and
//! adds one [`MediaItem`] per recording as a single undo step. Thumbnails
//! come afterwards: the caller submits [`Session::filmstrip_job`] for each
//! added item to its [`JobRunner`](crate::JobRunner), feeds the events to
//! [`Session::handle_job_message`], and reads them back with
//! [`Session::thumbnail`].
//!
//! Decisions (reversible):
//! - Probing and HiLights run on the calling thread, one file after
//!   another: both only read headers, and they are cached. The app calls
//!   `import` off the UI thread.
//! - Only `.mp4` and `.mov` files are imported, also when given one by one;
//!   photos are skipped in the MVP (reported in [`ImportReport::skipped`]).
//! - A recording with a missing chapter is imported with the chapters
//!   present (see [`kv_gopro::media_item_from_group`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use kv_core::{Command, MediaId, MediaItem, MediaKind, Micros};
use kv_gopro::ProbedFile;
use serde::{Deserialize, Serialize};

use crate::jobs::{hilights_cached, probe_cached};
use crate::{Job, JobContext, Session, SessionError, Snapshot};

/// File extensions [`Session::import`] picks up (case-insensitive).
pub const IMPORT_EXTENSIONS: [&str; 2] = ["mp4", "mov"];

/// What [`Session::import`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// The new library items, in import order.
    pub added: Vec<MediaId>,
    /// Files that were not added, with a short, readable reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Thumbnails delivered by finished filmstrip jobs, by item project path.
#[derive(Debug, Clone, Default)]
pub(crate) struct Thumbnails(HashMap<String, Vec<Vec<u8>>>);

impl Thumbnails {
    /// Re-key from project paths relative to `from` to ones relative to `to`
    /// (see [`crate::rebase`]); entries that can't be rebased are dropped.
    pub(crate) fn rebase(&mut self, from: &Path, to: &Path) {
        self.0 = std::mem::take(&mut self.0)
            .into_iter()
            .filter_map(|(k, v)| crate::rebase(&k, from, to).ok().map(|k| (k, v)))
            .collect();
    }
}

impl Session {
    /// Import `paths` (files and folders, absolute) into the library. See the
    /// [module docs](crate::import) for what is picked up and how.
    ///
    /// Everything added is one undo step; nothing changes (and no step is
    /// recorded) when nothing is added. Files already in the library (same
    /// resolved path, as an item or as a chapter of one) are skipped, as are
    /// files that fail to probe or have no video stream.
    pub fn import(
        &mut self,
        paths: &[PathBuf],
        ctx: &JobContext,
    ) -> Result<(ImportReport, Snapshot), SessionError> {
        let mut report = ImportReport::default();
        let files = self.collect_files(paths, &mut report);

        let mut next_id = self.project().next_media_id().0;
        let mut items = Vec::new();
        for group in kv_gopro::group_chapters(files) {
            let mut probed = Vec::with_capacity(group.files.len());
            let mut failed = false;
            for file in &group.files {
                match self.probe_one(file, ctx) {
                    Ok(p) => probed.push(p),
                    Err(reason) => {
                        report.skipped.push((file.clone(), reason));
                        failed = true;
                    }
                }
            }
            if failed && group.files.len() > 1 {
                tracing::warn!(files = ?group.files, "a chapter failed to import; importing the rest");
            }
            if group.has_gaps() {
                tracing::warn!(files = ?group.files, "chaptered recording is missing a chapter");
            }
            if let Some(item) =
                kv_gopro::media_item_from_files(MediaId(next_id), MediaKind::Video, probed)
            {
                next_id += 1;
                report.added.push(item.id);
                items.push(item);
            }
        }

        let snapshot = if items.is_empty() {
            self.snapshot()
        } else {
            let cmds = items
                .into_iter()
                .map(|item| Command::AddMedia { item })
                .collect();
            self.apply_group(cmds)?
        };
        tracing::info!(
            added = report.added.len(),
            skipped = report.skipped.len(),
            "import finished"
        );
        Ok((report, snapshot))
    }

    /// The import candidates among `paths`: folders expanded (not
    /// recursively, sorted), sidecars and other extensions dropped,
    /// duplicates and files already in the library reported as skipped.
    fn collect_files(&self, paths: &[PathBuf], report: &mut ImportReport) -> Vec<PathBuf> {
        let mut known: HashSet<String> = self
            .project()
            .media
            .iter()
            .flat_map(|m| {
                std::iter::once(m.path.clone()).chain(m.chapters.iter().map(|c| c.path.clone()))
            })
            .collect();
        let mut out = Vec::new();
        for path in paths {
            let candidates = if path.is_dir() {
                match list_dir(path) {
                    Ok(files) => files,
                    Err(e) => {
                        report
                            .skipped
                            .push((path.clone(), format!("Couldn't read the folder: {e}")));
                        continue;
                    }
                }
            } else if is_sidecar(path) {
                continue;
            } else if !has_import_extension(path) {
                report.skipped.push((
                    path.clone(),
                    "Only MP4 and MOV videos can be imported for now.".into(),
                ));
                continue;
            } else {
                vec![path.clone()]
            };
            for file in candidates {
                let project_path = match self.to_project_path(&file) {
                    Ok(p) => p,
                    Err(e) => {
                        report.skipped.push((file, e.to_string()));
                        continue;
                    }
                };
                if !known.insert(project_path) {
                    report
                        .skipped
                        .push((file, "Already in the library.".into()));
                    continue;
                }
                out.push(file);
            }
        }
        out
    }

    /// Probe and HiLights of one file, or why it can't be imported.
    fn probe_one(&self, file: &Path, ctx: &JobContext) -> Result<ProbedFile, String> {
        let info = probe_cached(ctx, file, &mut |_| {})?;
        if info.video.is_none() {
            return Err("It has no video.".into());
        }
        if info.duration <= Micros(0) {
            return Err("It has no duration (a photo?).".into());
        }
        let hilights = hilights_cached(ctx, file, &mut |_| {})?
            .into_iter()
            .filter(|h| (Micros(0)..=info.duration).contains(h))
            .collect();
        Ok(ProbedFile {
            path: self.to_project_path(file).map_err(|e| e.to_string())?,
            duration: info.duration,
            hilights,
        })
    }

    /// The [`Job::Filmstrip`] for a library item, with its files resolved to
    /// host paths; `None` for an unknown id or a photo.
    pub fn filmstrip_job(&self, media_id: MediaId) -> Option<Job> {
        let item = self.project().media_item(media_id)?;
        if item.kind != MediaKind::Video {
            return None;
        }
        Some(Job::Filmstrip {
            media_id,
            files: item_files(item)
                .into_iter()
                .map(|(p, d)| (self.resolve(&p), d))
                .collect(),
        })
    }

    /// JPEG thumbnail `index` of a library item's filmstrip, once its
    /// [`Job::Filmstrip`] finished (see [`Session::handle_job_message`]).
    pub fn thumbnail(&self, media_id: MediaId, index: usize) -> Option<Vec<u8>> {
        let item = self.project().media_item(media_id)?;
        self.thumbnails.0.get(&item.path)?.get(index).cloned()
    }

    /// Store finished thumbnails for the item whose (first) file is `first`.
    pub(crate) fn store_thumbnails(&mut self, first: &Path, thumbs: Vec<Vec<u8>>) -> bool {
        match self.to_project_path(first) {
            Ok(project_path) => {
                self.thumbnails.0.insert(project_path, thumbs);
                true
            }
            Err(e) => {
                tracing::debug!(?first, error = ?e, "thumbnails for a path outside the project");
                false
            }
        }
    }
}

/// An item's files (project paths) with durations: its chapters, or itself.
fn item_files(item: &MediaItem) -> Vec<(String, Micros)> {
    if item.chapters.is_empty() {
        vec![(item.path.clone(), item.duration)]
    } else {
        item.chapters
            .iter()
            .map(|c| (c.path.clone(), c.duration))
            .collect()
    }
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

pub(crate) fn has_import_extension(path: &Path) -> bool {
    extension_lower(path).is_some_and(|e| IMPORT_EXTENSIONS.contains(&e.as_str()))
}

/// GoPro `.LRV` proxies and `.THM` thumbnails.
fn is_sidecar(path: &Path) -> bool {
    extension_lower(path).is_some_and(|e| e == "lrv" || e == "thm")
}

/// The importable files directly inside `dir`, sorted by path.
fn list_dir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && has_import_extension(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}
