//! Manual tweaks that need the session's knowledge of the music: nudging a
//! cut by whole beats.
//!
//! `kv-core` knows nothing about beats, so [`Session::nudge_cut`] turns "one
//! beat later" into a timeline time from the section's [`MusicAnalysis`] and
//! applies a plain [`Command::MoveCut`]. Like the other manual tweaks
//! (`MoveSegment`, `SetClipAudio`, `SetMusicVolume`) it never regenerates
//! the edit (ADR 0010).
//!
//! The beat grid mirrors the auto-edit's (`kv_autoedit`'s grid): beats at or
//! before the timeline start are ignored, a beat closer to the start than
//! half the median beat interval is dropped, and past the song's last beat
//! the grid continues at the median tempo, so cuts beyond the end of the
//! music can still be nudged.

use crate::{Session, SessionError, Snapshot};
use kv_audio::MusicAnalysis;
use kv_core::{Command, CoreError, Micros, SectionId};

/// The beat positions a cut can be nudged to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BeatGrid {
    /// Song beats after the timeline start, increasing, at least two.
    beats: Vec<i64>,
    /// Median beat interval, the tempo past the last beat.
    median_iv: i64,
}

impl BeatGrid {
    /// The grid of `music`, or `None` when it has fewer than two beats.
    pub(crate) fn new(music: &MusicAnalysis) -> Option<BeatGrid> {
        let mut beats: Vec<i64> = music.beats.iter().map(|b| b.0).filter(|&b| b > 0).collect();
        beats.sort_unstable();
        beats.dedup();
        if beats.len() < 2 {
            return None;
        }
        let mut ivs: Vec<i64> = beats.windows(2).map(|w| w[1] - w[0]).collect();
        ivs.sort_unstable();
        let median_iv = ivs[ivs.len() / 2];
        // Same rule as the auto-edit: a beat right after the start would
        // make a uselessly short first slot.
        while beats.len() > 1 && beats[0] < median_iv / 2 {
            beats.remove(0);
        }
        Some(BeatGrid { beats, median_iv })
    }

    /// Beat `i` (0 = the first beat); past the song it continues at the
    /// median tempo. `None` before the first beat or on overflow.
    fn beat(&self, i: i64) -> Option<i64> {
        let len = self.beats.len() as i64;
        if i < 0 {
            None
        } else if i < len {
            Some(self.beats[i as usize])
        } else {
            let last = *self.beats.last().expect("at least two beats");
            (i - len + 1)
                .checked_mul(self.median_iv)
                .and_then(|d| last.checked_add(d))
        }
    }

    /// The index of the beat nearest to `t` (ties go to the earlier beat).
    fn nearest(&self, t: i64) -> i64 {
        let last = *self.beats.last().expect("at least two beats");
        let len = self.beats.len() as i64;
        if t >= last {
            let past = t - last;
            let k = past / self.median_iv;
            let rem = past % self.median_iv;
            let k = if 2 * rem > self.median_iv { k + 1 } else { k };
            return len - 1 + k;
        }
        let i = self.beats.partition_point(|&b| b < t);
        if i == 0 {
            return 0;
        }
        let (before, after) = (self.beats[i - 1], self.beats[i]);
        if t - before <= after - t {
            i as i64 - 1
        } else {
            i as i64
        }
    }

    /// Where a cut at `at` lands when nudged by `beats` beats from the beat
    /// nearest to it. `None` when that is before the first beat.
    pub(crate) fn nudge(&self, at: Micros, beats: i32) -> Option<Micros> {
        let i = self.nearest(at.0).checked_add(i64::from(beats))?;
        self.beat(i).map(Micros)
    }
}

impl Session {
    /// Move cut `cut` of `section` (the boundary between segments `cut - 1`
    /// and `cut`) by `beats` beats: find the beat nearest the cut's current
    /// timeline time (the middle of a crossfade) and apply
    /// [`Command::MoveCut`] so it lands on the beat `beats` later (negative:
    /// earlier); a crossfade stays centred on the beat. `0` snaps the cut to its nearest beat. One
    /// undo step; never regenerates the edit.
    ///
    /// Errors (nothing changes): unknown section or cut, no music or its
    /// analysis not finished, no beat there (before the first beat), or the
    /// move would make a segment too short or run past its clip.
    pub fn nudge_cut(
        &mut self,
        section: SectionId,
        cut: usize,
        beats: i32,
    ) -> Result<Snapshot, SessionError> {
        let sec = self
            .project()
            .section(section)
            .ok_or(CoreError::UnknownSection(section))?;
        let len = sec.edit.segments.len();
        if cut == 0 || cut >= len {
            return Err(CoreError::CutIndexOutOfRange { cut, len }.into());
        }
        // A crossfade sits centred on its beat (as the auto-edit places
        // it), so its middle is the cut's time; `MoveCut` takes the incoming
        // segment's start, half the overlap earlier.
        let half_fade = Micros(sec.edit.segments[cut].transition.overlap().0 / 2);
        let at = Micros(sec.edit.segments[cut].timeline_start.0 + half_fade.0);
        let music = sec.music.as_ref().ok_or(SessionError::NoMusic)?;
        let analysis = self
            .music_analysis(music)
            .ok_or(SessionError::MusicNotAnalysed)?;
        let grid = BeatGrid::new(analysis).ok_or(SessionError::NoBeatThere)?;
        let to = grid.nudge(at, beats).ok_or(SessionError::NoBeatThere)?;
        tracing::debug!(?section, cut, beats, from = at.0, to = to.0, "nudge cut");
        self.apply(Command::MoveCut {
            section,
            cut,
            to: Micros(to.0 - half_fade.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regen::is_regenerate_trigger;
    use kv_core::{
        Edl, MediaId, MediaItem, MediaKind, MusicRef, Project, Segment, Speed, Transition,
    };
    use std::path::PathBuf;

    const S: SectionId = Project::DEFAULT_SECTION;
    const MS: i64 = 1_000;
    const SEC: i64 = 1_000_000;

    /// A beat every 0.5 s from 0 to 3.5 s (8 beats, 0 is ignored).
    fn analysis() -> MusicAnalysis {
        let beats: Vec<Micros> = (0..8).map(|i| Micros(i * 500 * MS)).collect();
        MusicAnalysis {
            duration: Micros(4 * SEC),
            tempo_bpm: 120.0,
            downbeats: beats.iter().step_by(4).copied().collect(),
            beats,
            onsets: Vec::new(),
            energy: Vec::new(),
        }
    }

    fn seg(media: u64, src_in_ms: i64, src_out_ms: i64) -> Segment {
        Segment {
            media_id: MediaId(media),
            src_in: Micros(src_in_ms * MS),
            src_out: Micros(src_out_ms * MS),
            timeline_start: Micros(0),
            transition: Transition::Cut,
            speed: Speed(100),
        }
    }

    /// Two 20 s clips and an edit of 1 s + 0.5 s + 1.5 s + 3 s segments:
    /// cuts at 1.0, 1.5 and 3.0 s (the last one at the song's last beat).
    fn session_with(segments: Vec<Segment>, analysed: bool) -> Session {
        let mut s = Session::new(PathBuf::from("/home/ana"));
        for id in [1, 2] {
            s.apply(Command::AddMedia {
                item: MediaItem {
                    id: MediaId(id),
                    path: format!("c{id}.mp4"),
                    duration: Micros(20 * SEC),
                    kind: MediaKind::Video,
                    hilights: Vec::new(),
                    clip_audio: false,
                    chapters: Vec::new(),
                },
            })
            .unwrap();
        }
        s.apply(Command::SetMusic {
            section: S,
            music: Some(MusicRef {
                path: "song.mp3".into(),
                duration: Micros(4 * SEC),
            }),
        })
        .unwrap();
        if analysed {
            s.set_music_analysis("song.mp3".into(), analysis());
        }
        let mut edl = Edl { segments };
        edl.repack();
        s.apply(Command::ReplaceEdl { section: S, edl }).unwrap();
        s
    }

    fn session() -> Session {
        session_with(
            vec![
                seg(1, 5_000, 6_000),
                seg(2, 2_000, 2_500),
                seg(1, 8_000, 9_500),
                seg(2, 10_000, 13_000),
            ],
            true,
        )
    }

    fn cuts(s: &Session) -> Vec<i64> {
        let segs = &s.project().sections[0].edit.segments;
        segs[1..].iter().map(|g| g.timeline_start.0 / MS).collect()
    }

    #[test]
    fn nudge_later_moves_the_cut_one_beat_and_shifts_both_segments() {
        let mut s = session();
        let before = s.revision();
        let snap = s.nudge_cut(S, 2, 1).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 2_000, 3_000]);
        assert_eq!(snap.revision, before + 1);
        let segs = &s.project().sections[0].edit.segments;
        // The segment before the cut got 0.5 s longer at its end, the one
        // after it 0.5 s shorter at its start.
        assert_eq!(segs[1].src_out, Micros(3_000 * MS));
        assert_eq!(segs[2].src_in, Micros(8_500 * MS));
        // One undo step brings the cut back.
        s.undo().unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 3_000]);
    }

    #[test]
    fn nudge_keeps_a_crossfade_centred_on_the_beat() {
        let mut fade = seg(1, 8_000, 9_700);
        fade.transition = Transition::Crossfade {
            duration: Micros(200 * MS),
        };
        // Cuts: 1.0 s, then a 0.2 s crossfade centred on 1.5 s.
        let mut s = session_with(vec![seg(1, 5_000, 6_000), seg(2, 2_000, 2_600), fade], true);
        let start = |s: &Session| s.project().sections[0].edit.segments[2].timeline_start;
        assert_eq!(start(&s), Micros(1_400 * MS));
        s.nudge_cut(S, 2, 1).unwrap();
        // Centred on 2.0 s: it starts 0.1 s before.
        assert_eq!(start(&s), Micros(1_900 * MS));
        s.nudge_cut(S, 2, 0).unwrap();
        assert_eq!(start(&s), Micros(1_900 * MS), "snap keeps it on its beat");
    }

    #[test]
    fn nudge_earlier_and_by_several_beats() {
        let mut s = session();
        s.nudge_cut(S, 3, -2).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 2_000]);
        s.nudge_cut(S, 3, 3).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 3_500]);
    }

    #[test]
    fn first_cut_moves_both_ways_but_not_before_the_first_beat() {
        let mut s = session();
        s.nudge_cut(S, 1, -1).unwrap();
        assert_eq!(cuts(&s), vec![500, 1_500, 3_000]);
        // 0.5 s is the first beat (0 is the timeline start, not a beat).
        assert_eq!(s.nudge_cut(S, 1, -1), Err(SessionError::NoBeatThere));
        s.nudge_cut(S, 1, 1).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 3_000]);
    }

    #[test]
    fn last_cut_continues_past_the_song_at_the_median_tempo() {
        let mut s = session();
        // The song's last beat is 3.5 s; 4.0 and 4.5 s are on the grid
        // continued at 0.5 s per beat.
        s.nudge_cut(S, 3, 3).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 4_500]);
        s.nudge_cut(S, 3, -1).unwrap();
        assert_eq!(cuts(&s), vec![1_000, 1_500, 4_000]);
    }

    #[test]
    fn rejected_when_a_segment_would_become_too_short() {
        let mut s = session();
        let before = s.snapshot();
        // Segment 1 is one beat long: moving its start or end into it by a
        // beat would leave nothing.
        let e = s.nudge_cut(S, 1, 1).unwrap_err();
        assert!(matches!(
            e,
            SessionError::Core(CoreError::CutTooShort { .. })
        ));
        assert_eq!(
            e.to_string(),
            "Moving that cut would make a clip too short."
        );
        assert!(s.nudge_cut(S, 2, -1).is_err());
        assert_eq!(s.snapshot(), before, "a rejected nudge changes nothing");
    }

    #[test]
    fn off_beat_cut_moves_from_its_nearest_beat() {
        let mut s = session_with(vec![seg(1, 5_000, 6_200), seg(2, 2_000, 5_000)], true);
        assert_eq!(cuts(&s), vec![1_200]);
        // Nearest beat is 1.0 s, so one beat later is 1.5 s.
        s.nudge_cut(S, 1, 1).unwrap();
        assert_eq!(cuts(&s), vec![1_500]);
        let mut s = session_with(vec![seg(1, 5_000, 6_300), seg(2, 2_000, 5_000)], true);
        // 1.3 s is nearest 1.5 s; zero beats snaps to it.
        s.nudge_cut(S, 1, 0).unwrap();
        assert_eq!(cuts(&s), vec![1_500]);
    }

    #[test]
    fn needs_analysed_music_and_a_real_cut() {
        let mut s = session_with(vec![seg(1, 5_000, 6_000), seg(2, 2_000, 4_000)], false);
        assert_eq!(s.nudge_cut(S, 1, 1), Err(SessionError::MusicNotAnalysed));
        let mut s = session();
        for cut in [0, 4] {
            assert!(matches!(
                s.nudge_cut(S, cut, 1),
                Err(SessionError::Core(CoreError::CutIndexOutOfRange { .. }))
            ));
        }
        assert!(matches!(
            s.nudge_cut(SectionId(9), 1, 1),
            Err(SessionError::Core(CoreError::UnknownSection(_)))
        ));
        s.apply(Command::SetMusic {
            section: S,
            music: None,
        })
        .unwrap();
        assert_eq!(s.nudge_cut(S, 2, 1), Err(SessionError::NoMusic));
    }

    #[test]
    fn grid_matches_the_auto_edit_rules() {
        let mut a = analysis();
        // A beat 10 ms after the start is dropped, like the auto-edit does.
        a.beats.insert(1, Micros(10 * MS));
        let g = BeatGrid::new(&a).unwrap();
        assert_eq!(g.beats[0], 500 * MS);
        assert_eq!(g.median_iv, 500 * MS);
        assert_eq!(g.nearest(740 * MS), 0);
        assert_eq!(g.nearest(760 * MS), 1);
        assert_eq!(g.nearest(4_240 * MS), 7);
        assert_eq!(g.nearest(4_260 * MS), 8);
        assert_eq!(g.nudge(Micros(4_260 * MS), 1), Some(Micros(5_000 * MS)));
        a.beats.truncate(2);
        assert_eq!(BeatGrid::new(&a), None);
    }

    #[test]
    fn manual_tweaks_never_regenerate() {
        for cmd in [
            Command::MoveSegment {
                section: S,
                from: 0,
                to: 1,
            },
            Command::MoveCut {
                section: S,
                cut: 1,
                to: Micros(SEC),
            },
            Command::SetClipAudio {
                media_id: MediaId(1),
                enabled: true,
            },
            Command::SetMusicVolume { percent: 40 },
        ] {
            assert!(!is_regenerate_trigger(&cmd), "{cmd:?}");
        }
        // nudge_cut is a plain MoveCut: the edit keeps the tweak although
        // the auto-edit would build something else.
        let mut s = session();
        let analyses = s.analyses();
        s.nudge_cut(S, 2, 1).unwrap();
        let snap = s
            .apply_and_regenerate(Command::SetMusicVolume { percent: 40 }, &analyses)
            .unwrap();
        assert!(!snap.manual_edits_replaced);
        assert_eq!(cuts(&s), vec![1_000, 2_000, 3_000]);
    }
}
