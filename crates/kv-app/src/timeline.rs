//! IPC behind the timeline view (`ui/js/timeline.js`, `ui/js/tweaks.js`):
//! the beat grid of a section's music and nudging a cut by beats. Seeking is
//! the transport's (`transport.rs`).
//!
//! Beats are fetched with their own command instead of riding in every
//! [`kv_session::Snapshot`]: a song has hundreds of beats that never change
//! while the user edits, so the UI asks once per music track (and again
//! while the analysis is still running).

use kv_core::{Micros, SectionId};
use kv_session::{Session, Snapshot};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::ipc::{AppState, IpcError};

/// The beat grid of a section's music, for drawing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Beats {
    /// The `MusicRef.path` these beats belong to, so the UI can drop an
    /// answer that arrives after the music changed.
    pub music: String,
    /// Length of the analysed audio.
    pub duration: Micros,
    pub tempo_bpm: f64,
    /// Beat times from the start of the music, increasing.
    pub beats: Vec<Micros>,
    /// The bar starts, a subset of `beats`.
    pub downbeats: Vec<Micros>,
}

/// The beats of `section`'s music: `None` when the section has no music or
/// its analysis has not finished. Errors when the section does not exist.
pub fn beats(session: &Session, section: SectionId) -> Result<Option<Beats>, IpcError> {
    let sec = session.project().section(section).ok_or_else(|| IpcError {
        message: "That part of the project no longer exists.".into(),
    })?;
    let Some(music) = &sec.music else {
        return Ok(None);
    };
    Ok(session.music_analysis(music).map(|a| Beats {
        music: music.path.clone(),
        duration: a.duration,
        tempo_bpm: a.tempo_bpm,
        beats: a.beats.clone(),
        downbeats: a.downbeats.clone(),
    }))
}

#[tauri::command]
pub fn get_beats(
    state: State<'_, AppState>,
    section: SectionId,
) -> Result<Option<Beats>, IpcError> {
    beats(&*state.session()?, section)
}

/// Move cut `cut` of `section` by `beats` beats on the music's beat grid
/// (see [`Session::nudge_cut`]). A manual tweak: never rebuilds the edit.
pub fn nudge(
    session: &mut Session,
    section: SectionId,
    cut: usize,
    beats: i32,
) -> Result<Snapshot, IpcError> {
    Ok(session.nudge_cut(section, cut, beats)?)
}

#[tauri::command]
pub fn nudge_cut(
    app: AppHandle,
    state: State<'_, AppState>,
    section: SectionId,
    cut: usize,
    beats: i32,
) -> Result<Snapshot, IpcError> {
    let result = nudge(&mut *state.session()?, section, cut, beats);
    if let Ok(snapshot) = &result {
        crate::emit_state(&app, snapshot);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_audio::MusicAnalysis;
    use kv_core::{Command, MusicRef, Project};
    use std::path::PathBuf;

    const S: SectionId = Project::DEFAULT_SECTION;

    fn session_with_music() -> Session {
        let mut s = Session::new(PathBuf::from("/home/ana"));
        s.apply(Command::SetMusic {
            section: S,
            music: Some(MusicRef {
                path: "Music/song.mp3".into(),
                duration: Micros(2_000_000),
            }),
        })
        .unwrap();
        s
    }

    fn analysis() -> MusicAnalysis {
        MusicAnalysis {
            duration: Micros(2_000_000),
            tempo_bpm: 120.0,
            beats: (0..4).map(|i| Micros(i * 500_000)).collect(),
            downbeats: vec![Micros(0)],
            onsets: Vec::new(),
            energy: Vec::new(),
        }
    }

    #[test]
    fn no_music_means_no_beats() {
        let s = Session::new(PathBuf::from("/home/ana"));
        assert_eq!(beats(&s, S).unwrap(), None);
    }

    #[test]
    fn music_without_finished_analysis_has_no_beats_yet() {
        assert_eq!(beats(&session_with_music(), S).unwrap(), None);
    }

    #[test]
    fn finished_analysis_gives_the_beat_grid() {
        let mut s = session_with_music();
        s.set_music_analysis("Music/song.mp3".into(), analysis());
        let b = beats(&s, S).unwrap().expect("beats");
        assert_eq!(b.music, "Music/song.mp3");
        assert_eq!(b.duration, Micros(2_000_000));
        assert_eq!(b.beats.len(), 4);
        assert_eq!(b.downbeats, vec![Micros(0)]);
        // What the UI receives: plain microsecond numbers, snake_case keys.
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["beats"][1], 500_000);
        assert_eq!(json["tempo_bpm"], 120.0);
    }

    #[test]
    fn unknown_section_is_an_error_message() {
        let s = Session::new(PathBuf::from("/home/ana"));
        let e = beats(&s, SectionId(99)).unwrap_err();
        assert_eq!(e.message, "That part of the project no longer exists.");
    }

    #[test]
    fn nudge_moves_a_cut_one_beat_and_reports_errors_as_messages() {
        use kv_core::{Edl, MediaId, MediaItem, MediaKind, Segment, Speed, Transition};
        let mut s = session_with_music();
        s.set_music_analysis("Music/song.mp3".into(), analysis());
        s.apply(Command::AddMedia {
            item: MediaItem {
                id: MediaId(1),
                path: "a.mp4".into(),
                duration: Micros(10_000_000),
                kind: MediaKind::Video,
                hilights: Vec::new(),
                clip_audio: false,
                chapters: Vec::new(),
            },
        })
        .unwrap();
        let seg = |src_in: i64, src_out: i64| Segment {
            media_id: MediaId(1),
            src_in: Micros(src_in),
            src_out: Micros(src_out),
            timeline_start: Micros(0),
            transition: Transition::Cut,
            speed: Speed(100),
        };
        let mut edl = Edl {
            segments: vec![seg(0, 1_000_000), seg(5_000_000, 6_000_000)],
        };
        edl.repack();
        s.apply(Command::ReplaceEdl { section: S, edl }).unwrap();
        let snap = nudge(&mut s, S, 1, -1).unwrap();
        assert_eq!(
            snap.project.sections[0].edit.segments[1].timeline_start,
            Micros(500_000)
        );
        // 0.5 s is the first beat.
        let e = nudge(&mut s, S, 1, -1).unwrap_err();
        assert_eq!(e.message, "There is no beat there to move the cut to.");
    }

    /// The UI tests' `beats-song.json` (generated by `tests/ui_fixtures.rs`)
    /// is exactly what this module answers for that song.
    #[test]
    fn ui_fixture_matches_get_beats() {
        let mut s = Session::new(PathBuf::from("/home/ana"));
        s.apply(Command::SetMusic {
            section: S,
            music: Some(MusicRef {
                path: "Music/song.mp3".into(),
                duration: Micros(8_000_000),
            }),
        })
        .unwrap();
        let grid: Vec<Micros> = (0..16).map(|i| Micros(i * 500_000)).collect();
        s.set_music_analysis(
            "Music/song.mp3".into(),
            MusicAnalysis {
                duration: Micros(8_000_000),
                tempo_bpm: 120.0,
                downbeats: grid.iter().step_by(4).copied().collect(),
                beats: grid,
                onsets: Vec::new(),
                energy: Vec::new(),
            },
        );
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../ui-tests/fixtures/beats-song.json")).unwrap();
        let ours = serde_json::to_value(beats(&s, S).unwrap()).unwrap();
        assert_eq!(ours, fixture);
    }
}
