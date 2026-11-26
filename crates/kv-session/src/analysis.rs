//! Music analysis: the [`MusicAnalyzer`] backend trait and the session's
//! in-memory map of finished analyses.
//!
//! Analysis runs on [`JobRunner`](crate::JobRunner) workers. When a job
//! finishes, the caller hands its [`JobMessage`] to
//! [`Session::handle_job_message`], which keeps the result so that
//! regenerate can read it synchronously through [`Session::music_analysis`].

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use kv_audio::MusicAnalysis;
use kv_core::MusicRef;

use crate::jobs::{Job, JobEvent, JobMessage, JobResult};
use crate::Session;

/// Analyses a music file. Implementations run on worker threads.
///
/// Injected into [`JobRunner`](crate::JobRunner) so tests can use fakes.
/// [`AudioAnalyzer`] is the real one.
pub trait MusicAnalyzer: Send + Sync {
    /// Decode and analyse the file at `path`. The error's `Display` ends up
    /// in [`JobEvent::Failed`], so it should be readable.
    fn analyze(&self, path: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>>;
}

/// [`MusicAnalyzer`] backed by [`kv_audio::analyze`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioAnalyzer;

impl MusicAnalyzer for AudioAnalyzer {
    fn analyze(&self, path: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
        Ok(kv_audio::analyze(path)?)
    }
}

/// Finished music analyses, keyed by project path (`MusicRef::path`).
#[derive(Debug, Clone, Default)]
pub(crate) struct MusicAnalyses(HashMap<String, Arc<MusicAnalysis>>);

impl MusicAnalyses {
    /// Re-key every entry with `f`; entries it maps to `None` are dropped.
    pub(crate) fn remap(&mut self, f: impl Fn(&str) -> Option<String>) {
        self.0 = std::mem::take(&mut self.0)
            .into_iter()
            .filter_map(|(k, v)| f(&k).map(|k| (k, v)))
            .collect();
    }
}

impl Session {
    /// The analysis of a music track, if a finished job delivered one (see
    /// [`Session::handle_job_message`]).
    pub fn music_analysis(&self, music: &MusicRef) -> Option<&MusicAnalysis> {
        self.music.0.get(&music.path).map(|a| &**a)
    }

    /// Store the analysis of the music file at `project_path` (a project
    /// path, see [`Session::to_project_path`]), replacing any previous one.
    pub fn set_music_analysis(&mut self, project_path: String, analysis: MusicAnalysis) {
        self.music.0.insert(project_path, Arc::new(analysis));
    }

    /// Every finished analysis, keyed by project path, as an
    /// [`AnalysisSource`](crate::regen::AnalysisSource) detached from the
    /// session. Cheap (the analyses are shared, not copied). Pass it to
    /// [`Session::regenerate`], [`Session::reroll`] or
    /// [`Session::apply_and_regenerate`], which borrow the session mutably.
    pub fn analyses(&self) -> HashMap<String, Arc<MusicAnalysis>> {
        self.music.0.clone()
    }

    /// Feed a job event into the session. A finished
    /// [`Job::AnalyzeMusic`] is stored for [`Session::music_analysis`], a
    /// finished [`Job::Filmstrip`] for [`Session::thumbnail`]; every other
    /// message is ignored. Returns whether something was stored.
    ///
    /// A file whose host path has no project path (another drive, say) is
    /// ignored, since no `MusicRef` or media item could name it.
    pub fn handle_job_message(&mut self, msg: &JobMessage) -> bool {
        if let (
            Job::Filmstrip { .. },
            JobEvent::Done {
                result: JobResult::Filmstrip(thumbs),
            },
        ) = (&msg.job, &msg.event)
        {
            return self.store_thumbnails(msg.job.path(), thumbs.clone());
        }
        let (
            Job::AnalyzeMusic(path),
            JobEvent::Done {
                result: JobResult::MusicAnalysis(analysis),
            },
        ) = (&msg.job, &msg.event)
        else {
            return false;
        };
        match self.music_project_path(path) {
            Ok(project_path) => {
                self.set_music_analysis(project_path, analysis.clone());
                true
            }
            Err(e) => {
                tracing::debug!(?path, error = ?e, "music analysis for a path outside the project");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis() -> Arc<MusicAnalysis> {
        Arc::new(MusicAnalysis {
            duration: kv_core::Micros(1_000_000),
            tempo_bpm: 120.0,
            beats: Vec::new(),
            downbeats: Vec::new(),
            onsets: Vec::new(),
            energy: Vec::new(),
        })
    }

    #[test]
    fn remap_rekeys_and_drops_what_cannot_be_rebased() {
        let (home, trip) = if cfg!(windows) {
            (r"C:\home\ana", r"C:\home\ana\trip")
        } else {
            ("/home/ana", "/home/ana/trip")
        };
        let a = analysis();
        let mut m = MusicAnalyses(HashMap::from([("music/s.mp3".to_string(), a.clone())]));
        m.remap(|k| crate::rebase(k, Path::new(home), Path::new(trip)).ok());
        assert_eq!(m.0.keys().collect::<Vec<_>>(), ["../music/s.mp3"]);
        assert!(Arc::ptr_eq(&m.0["../music/s.mp3"], &a));

        // A relative root can't be resolved, so the entry is dropped.
        m.remap(|k| crate::rebase(k, Path::new("relative"), Path::new(trip)).ok());
        assert!(m.0.is_empty());
    }
}
