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
pub(crate) struct MusicAnalyses(HashMap<String, MusicAnalysis>);

impl Session {
    /// The analysis of a music track, if a finished job delivered one (see
    /// [`Session::handle_job_message`]).
    pub fn music_analysis(&self, music: &MusicRef) -> Option<&MusicAnalysis> {
        self.music.0.get(&music.path)
    }

    /// Store the analysis of the music file at `project_path` (a project
    /// path, see [`Session::to_project_path`]), replacing any previous one.
    pub fn set_music_analysis(&mut self, project_path: String, analysis: MusicAnalysis) {
        self.music.0.insert(project_path, analysis);
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
        match self.to_project_path(path) {
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
