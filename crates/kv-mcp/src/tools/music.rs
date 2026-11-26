//! Music and auto-edit tools: `music_set`, `music_clear`, `music_beats`,
//! `edit_set_target_length`, `edit_regenerate`, `edit_reroll`.
//!
//! `music_set` mirrors the desktop app's analyse-then-set flow
//! (`crates/kv-app/src/music.rs`): it submits [`Job::AnalyzeMusic`] to a
//! [`JobRunner`] built from the server's [`kv_session::JobContext`] (so the
//! `kv-cache` cache is used), waits for the final event, feeds it to
//! [`kv_session::Session::handle_job_message`], then applies `SetMusic`
//! through `apply_and_regenerate`. A failed analysis is a tool error and
//! changes nothing.

use kv_core::{Command, Micros, MusicRef, Project, SectionId, TargetLength};
use kv_session::{Job, JobEvent, JobResult, JobRunner};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::{Deserialize, Serialize};

use super::{json_result, relative_path, tool_error, WriteResult};
use crate::KwikvidServer;

/// The user-facing message for a missing section (as `NotReady`).
const NO_SECTION: &str = "That part of the project no longer exists.";

/// Input of the tools that only name a section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SectionParams {
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `music_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MusicSetParams {
    /// Path of the song (MP3, AAC/M4A, WAV or FLAC): relative to the project
    /// root (the server's `--root`), or absolute inside it.
    pub path: String,
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
}

/// Input of `edit_set_target_length`: exactly one of `seconds` and
/// `full_song: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetLengthParams {
    /// Section id; defaults to the project's only section (1).
    #[serde(default)]
    pub section: Option<u64>,
    /// A fixed edit length in seconds (not microseconds), above 0 and at
    /// most 3600, for example 15, 30 or 60. Give either this or
    /// `full_song: true`, not both.
    #[serde(default)]
    pub seconds: Option<f64>,
    /// `true`: the edit is as long as the section's music (at most one
    /// hour). Give either this or `seconds`, not both.
    #[serde(default)]
    pub full_song: Option<bool>,
}

/// What `music_set` returns: the analysis summary plus the write result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MusicSetResult {
    /// Dominant tempo in beats per minute (0 if no beat was found).
    pub tempo_bpm: f64,
    /// Number of beats found.
    pub beats: usize,
    /// Length of the song in microseconds.
    pub duration_us: i64,
    #[serde(flatten)]
    pub write: WriteResult,
}

/// What `music_beats` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MusicBeats {
    /// The section's music, as a project path.
    pub path: String,
    pub tempo_bpm: f64,
    pub duration_us: i64,
    /// Beat times in microseconds from the start of the song, increasing.
    pub beats_us: Vec<i64>,
    /// Bar starts (every 4th beat from the first), a subset of `beats_us`.
    pub downbeats_us: Vec<i64>,
}

fn section_id(section: Option<u64>) -> SectionId {
    section.map(SectionId).unwrap_or(Project::DEFAULT_SECTION)
}

fn micros(v: &[Micros]) -> Vec<i64> {
    v.iter().map(|m| m.0).collect()
}

impl TargetLengthParams {
    /// The target, or why the parameters are not exactly one choice.
    fn target(&self) -> Result<TargetLength, String> {
        match (self.seconds, self.full_song.unwrap_or(false)) {
            (Some(_), true) => Err("give either seconds or full_song: true, not both".into()),
            (None, false) => Err("give either seconds (e.g. 30) or full_song: true".into()),
            (None, true) => Ok(TargetLength::FullSong),
            (Some(s), false) => {
                if !s.is_finite() || s <= 0.0 || s > 3600.0 {
                    return Err(format!(
                        "seconds must be above 0 and at most 3600 (one hour), got {s}"
                    ));
                }
                let length = Micros((s * 1_000_000.0).round() as i64);
                if length.0 <= 0 {
                    return Err(format!("seconds must be at least one microsecond, got {s}"));
                }
                Ok(TargetLength::Fixed { length })
            }
        }
    }
}

impl KwikvidServer {
    pub(crate) fn music_set_impl(&self, p: MusicSetParams) -> Result<CallToolResult, ErrorData> {
        let section = section_id(p.section);
        let root = self.root();
        let file = match relative_path(&root, &p.path) {
            Ok(rel) => root.join(rel),
            Err(msg) => return Ok(tool_error(msg)),
        };
        {
            let session = self.lock();
            if session.project().section(section).is_none() {
                return Ok(tool_error(NO_SECTION));
            }
            if let Err(e) = session.check_music_path(&file) {
                return Ok(tool_error(format!(
                    "{:?} can't be used as music: {e}",
                    p.path
                )));
            }
        }

        // Analyse (cache first) without holding the session lock.
        let (runner, events) = JobRunner::with_workers(self.ctx.clone(), 1);
        let id = runner.submit(Job::AnalyzeMusic(file.clone()));
        let msg = loop {
            match events.recv() {
                Ok(msg) if msg.id == id && msg.event.is_final() => break msg,
                Ok(_) => {}
                Err(_) => return Ok(tool_error("the music analysis stopped unexpectedly")),
            }
        };
        drop(runner);
        let analysis = match &msg.event {
            JobEvent::Done {
                result: JobResult::MusicAnalysis(a),
            } => a,
            JobEvent::Failed { message } => return Ok(tool_error(message.clone())),
            other => {
                return Ok(tool_error(format!(
                    "the music analysis did not finish: {other:?}"
                )))
            }
        };

        let mut session = self.lock();
        session.handle_job_message(&msg);
        let path = match session.music_project_path(&file) {
            Ok(path) => path,
            Err(e) => return Ok(tool_error(e.to_string())),
        };
        let cmd = Command::SetMusic {
            section,
            music: Some(MusicRef {
                path,
                duration: analysis.duration,
            }),
        };
        tracing::debug!(?cmd, "setting music");
        let analyses = session.analyses();
        match session.apply_and_regenerate(cmd, &analyses) {
            Ok(r) => json_result(&MusicSetResult {
                tempo_bpm: analysis.tempo_bpm,
                beats: analysis.beats.len(),
                duration_us: analysis.duration.0,
                write: WriteResult::from_regen(&r, None),
            }),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    pub(crate) fn music_beats_impl(&self, p: SectionParams) -> Result<CallToolResult, ErrorData> {
        let session = self.lock();
        let Some(sec) = session.project().section(section_id(p.section)) else {
            return Ok(tool_error(NO_SECTION));
        };
        let Some(music) = &sec.music else {
            return Ok(tool_error(
                "This section has no music; call music_set first.",
            ));
        };
        let Some(a) = session.music_analysis(music) else {
            return Ok(tool_error(
                "The music has not been analysed in this session; call music_set again.",
            ));
        };
        json_result(&MusicBeats {
            path: music.path.clone(),
            tempo_bpm: a.tempo_bpm,
            duration_us: a.duration.0,
            beats_us: micros(&a.beats),
            downbeats_us: micros(&a.downbeats),
        })
    }

    /// `edit_regenerate` (`reroll: false`) or `edit_reroll`.
    pub(crate) fn rebuild(
        &self,
        p: SectionParams,
        reroll: bool,
    ) -> Result<CallToolResult, ErrorData> {
        let section = section_id(p.section);
        let mut session = self.lock();
        let analyses = session.analyses();
        let r = if reroll {
            session.reroll(section, &analyses)
        } else {
            session.regenerate(section, &analyses)
        };
        match r {
            Ok(r) => json_result(&WriteResult::from_regen(&r, None)),
            // `RegenError`'s Display is the user-facing message
            // (`auto_edit_message` for auto-edit failures).
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }
}

#[tool_router(router = music_router, vis = "pub(crate)")]
impl KwikvidServer {
    #[tool(
        description = "Set a section's music: analyse the song (tempo, beats; cached) and \
        apply kv-core SetMusic, rebuilding the edit. Returns tempo_bpm, beats (count), \
        duration_us and the write result. A song that cannot be analysed is an error and \
        changes nothing."
    )]
    fn music_set(
        &self,
        Parameters(p): Parameters<MusicSetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.music_set_impl(p)
    }

    #[tool(
        description = "Remove a section's music (kv-core SetMusic with no music). \
        The edit is kept until music is set again."
    )]
    fn music_clear(
        &self,
        Parameters(p): Parameters<SectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.apply(Command::SetMusic {
            section: section_id(p.section),
            music: None,
        })
    }

    #[tool(
        description = "Read the beats and downbeats (bar starts) of a section's analysed \
        music, in microseconds, with tempo_bpm and duration_us: where cuts can land."
    )]
    fn music_beats(
        &self,
        Parameters(p): Parameters<SectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.music_beats_impl(p)
    }

    #[tool(
        description = "Set how long a section's edit is (kv-core SetTargetLength) and rebuild \
        the edit. Give exactly one of: seconds (a fixed length in seconds, e.g. 30) or \
        full_song: true (as long as the music, at most one hour)."
    )]
    fn edit_set_target_length(
        &self,
        Parameters(p): Parameters<TargetLengthParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match p.target() {
            Ok(target) => self.apply(Command::SetTargetLength {
                section: section_id(p.section),
                target,
            }),
            Err(msg) => Ok(tool_error(msg)),
        }
    }

    #[tool(
        description = "Rebuild a section's auto-edit from its clips, music, pins, excludes \
        and seed (one undo step; replaces manual tweaks). An error with a readable reason \
        when it cannot be built (no clips, no music, ...)."
    )]
    fn edit_regenerate(
        &self,
        Parameters(p): Parameters<SectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.rebuild(p, false)
    }

    #[tool(
        description = "Re-roll a section's auto-edit: pick the next seed and rebuild, for a \
        different edit with the same pins and excludes (one undo step). An error with a \
        readable reason when it cannot be built."
    )]
    fn edit_reroll(
        &self,
        Parameters(p): Parameters<SectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.rebuild(p, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::{MediaAddParams, MediaKindParam};
    use kv_audio::MusicAnalysis;
    use kv_session::{JobContext, MusicAnalyzer};
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 120 bpm and 20 s for every file except `*bad*`; counts calls.
    #[derive(Default)]
    struct FixedAnalyzer(AtomicUsize);

    impl MusicAnalyzer for FixedAnalyzer {
        fn analyze(&self, path: &Path) -> Result<MusicAnalysis, Box<dyn Error + Send + Sync>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if path.to_string_lossy().contains("bad") {
                return Err("not a song".into());
            }
            Ok(music(20))
        }
    }

    /// A server whose root is a fresh temp dir holding `song.wav` and
    /// `bad.wav` (the cache keys need real files).
    fn music_server() -> (KwikvidServer, Arc<FixedAnalyzer>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["song.wav", "bad.wav"] {
            std::fs::write(dir.path().join(name), name).unwrap();
        }
        let analyzer = Arc::new(FixedAnalyzer::default());
        let ctx = JobContext::new(
            Arc::new(FakeBackend::default()),
            analyzer.clone(),
            Arc::new(kv_cache::Cache::in_memory().unwrap()),
        );
        let server = KwikvidServer::new(dir.path().to_owned(), ctx);
        (server, analyzer, dir)
    }

    fn add_clips(server: &KwikvidServer) {
        for name in ["a.mp4", "b.mp4", "c.mp4"] {
            ok(server.media_add_impl(MediaAddParams {
                path: name.into(),
                duration_us: Some(10 * SEC),
                kind: MediaKindParam::Video,
                hilights_us: vec![3 * SEC],
            }));
        }
    }

    fn set(path: &str) -> MusicSetParams {
        MusicSetParams {
            path: path.into(),
            section: None,
        }
    }

    fn length(seconds: Option<f64>, full_song: Option<bool>) -> Parameters<TargetLengthParams> {
        Parameters(TargetLengthParams {
            section: None,
            seconds,
            full_song,
        })
    }

    #[test]
    fn music_set_then_target_length_builds_an_edit() {
        let (server, analyzer, _dir) = music_server();
        add_clips(&server);
        let r = ok(server.music_set_impl(set("song.wav")));
        assert_eq!(r["tempo_bpm"], 120.0);
        assert_eq!(r["beats"], 40);
        assert_eq!(r["duration_us"], 20 * SEC);
        assert_eq!(r["regen_error"], serde_json::Value::Null, "{r}");
        assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");
        let music = server.project().sections[0].music.clone().unwrap();
        assert_eq!(
            music,
            MusicRef {
                path: "song.wav".into(),
                duration: Micros(20 * SEC)
            }
        );

        let r = ok(server.edit_set_target_length(length(Some(10.0), None)));
        assert_eq!(r["regen_error"], serde_json::Value::Null, "{r}");
        assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");
        let len = r["edit"]["length_us"].as_i64().unwrap();
        assert!((len - 10 * SEC).abs() <= SEC / 2, "{r}");
        assert_eq!(
            server.project().sections[0].target_length,
            TargetLength::Fixed {
                length: Micros(10 * SEC)
            }
        );
        let r = ok(server.edit_set_target_length(length(None, Some(true))));
        assert_eq!(
            server.project().sections[0].target_length,
            TargetLength::FullSong
        );
        assert!(r["edit"]["length_us"].as_i64().unwrap() > len, "{r}");

        // The second set of the same file is a cache hit.
        ok(server.music_set_impl(set("./song.wav")));
        assert_eq!(analyzer.0.load(Ordering::SeqCst), 1);

        // One undo reverts the target length and its rebuild.
        server.lock().undo().unwrap();
        assert_eq!(
            server.project().sections[0].target_length,
            TargetLength::FullSong
        );
    }

    #[test]
    fn failing_analysis_is_a_tool_error_and_changes_nothing() {
        let (server, _, dir) = music_server();
        add_clips(&server);
        let before = server.snapshot();
        let msg = err(server.music_set_impl(set("bad.wav")));
        assert!(msg.contains("not a song"), "{msg}");
        let msg = err(server.music_set_impl(set("missing.wav")));
        assert!(msg.contains("missing.wav"), "{msg}");
        let msg = err(server.music_set_impl(set("../up.wav")));
        assert!(msg.contains(".."), "{msg}");
        let outside: PathBuf = std::env::temp_dir().join("elsewhere.wav");
        err(server.music_set_impl(set(outside.to_str().unwrap())));
        let msg = err(server.music_set_impl(MusicSetParams {
            path: "song.wav".into(),
            section: Some(9),
        }));
        assert_eq!(msg, NO_SECTION);
        assert_eq!(server.snapshot(), before);
        drop(dir);
    }

    #[test]
    fn music_clear_and_beats() {
        let (server, _, _dir) = music_server();
        let msg = err(server.music_beats_impl(SectionParams::default()));
        assert!(msg.contains("no music"), "{msg}");
        ok(server.music_set_impl(set("song.wav")));
        let beats = ok(server.music_beats_impl(SectionParams::default()));
        assert_eq!(beats["path"], "song.wav");
        assert_eq!(beats["tempo_bpm"], 120.0);
        let b = beats["beats_us"].as_array().unwrap();
        assert_eq!(b.len(), 40);
        assert_eq!(b[1], SEC / 2);
        assert_eq!(beats["downbeats_us"][1], 2 * SEC);
        assert_eq!(
            err(server.music_beats_impl(SectionParams { section: Some(4) })),
            NO_SECTION
        );

        let r = ok(server.music_clear(Parameters(SectionParams::default())));
        assert_eq!(r["can_undo"], true);
        assert!(server.project().sections[0].music.is_none());
        err(server.music_beats_impl(SectionParams::default()));
    }

    #[test]
    fn target_length_needs_exactly_one_choice() {
        let server = server();
        let before = server.snapshot();
        for (s, f) in [
            (None, None),
            (None, Some(false)),
            (Some(10.0), Some(true)),
            (Some(0.0), None),
            (Some(-1.0), None),
            (Some(f64::NAN), None),
            (Some(3601.0), None),
        ] {
            err(server.edit_set_target_length(length(s, f)));
        }
        assert_eq!(server.snapshot(), before);
        ok(server.edit_set_target_length(length(Some(15.5), Some(false))));
        assert_eq!(
            server.project().sections[0].target_length,
            TargetLength::Fixed {
                length: Micros(15_500_000)
            }
        );
    }

    #[test]
    fn regenerate_and_reroll() {
        let (server, _, _dir) = music_server();
        // Not ready: the NotReady message, nothing changes.
        let before = server.snapshot();
        assert_eq!(
            err(server.edit_regenerate(Parameters(SectionParams::default()))),
            "Add clips to create an edit."
        );
        add_clips(&server);
        assert_eq!(
            err(server.edit_reroll(Parameters(SectionParams::default()))),
            "Add music to create an edit."
        );
        assert_eq!(
            err(server.edit_reroll(Parameters(SectionParams { section: Some(3) }))),
            NO_SECTION
        );
        assert_eq!(server.project().sections, before.project.sections);

        ok(server.music_set_impl(set("song.wav")));
        ok(server.edit_set_target_length(length(Some(10.0), None)));
        let edit = server.project().sections[0].edit.clone();
        // Nothing changed: regenerate is a no-op (no undo step).
        let rev = server.snapshot().revision;
        let r = ok(server.edit_regenerate(Parameters(SectionParams::default())));
        assert_eq!(r["revision"], rev);
        assert_eq!(server.project().sections[0].edit, edit);

        let seed = server.project().sections[0].seed;
        let r = ok(server.edit_reroll(Parameters(SectionParams::default())));
        assert!(r["edit"]["segments"].as_u64().unwrap() > 0, "{r}");
        let sec = &server.project().sections[0];
        assert_ne!(sec.seed, seed);
        assert_ne!(sec.edit, edit);

        // Auto-edit failure: its user-facing message.
        ok(server.apply(Command::PinRange {
            section: Project::DEFAULT_SECTION,
            range: kv_core::RangeRef {
                media_id: kv_core::MediaId(1),
                start: Micros(0),
                end: Micros(10 * SEC),
            },
        }));
        ok(server.apply(Command::PinRange {
            section: Project::DEFAULT_SECTION,
            range: kv_core::RangeRef {
                media_id: kv_core::MediaId(2),
                start: Micros(0),
                end: Micros(10 * SEC),
            },
        }));
        let msg = err(server.edit_regenerate(Parameters(SectionParams::default())));
        assert!(msg.contains("longer than the target length"), "{msg}");
    }
}
