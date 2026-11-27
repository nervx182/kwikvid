//! Regenerate the auto-edit, re-roll it, and regenerate automatically when a
//! constraint changes.
//!
//! [`edit_input`] turns a project section plus its cached music analysis into
//! a [`kv_autoedit::EditInput`] (pure). The session methods run
//! [`kv_autoedit::generate`] on it and apply the result with
//! [`Command::ReplaceEdl`], after applying the section's style preset with
//! [`kv_autoedit::apply_style`]:
//!
//! - [`Session::regenerate`]: rebuild the edit from the current constraints.
//! - [`Session::reroll`]: pick the next seed and rebuild (pins stay covered,
//!   because `generate` treats them as hard constraints).
//! - [`Session::apply_and_regenerate`]: apply a command and, when it is one
//!   of the [auto-regenerate triggers](is_regenerate_trigger), rebuild the
//!   edit of every affected section.
//!
//! # Decisions
//!
//! - **Auto-regenerate** (reversible; Quik behaves like this): changing the
//!   music, target length, pins, excludes, style or the media library
//!   rebuilds the edit. Manual tweaks (`MoveSegment`, `MoveCut`) do not.
//!   A rebuild replaces manual tweaks; [`RegenSnapshot::manual_edits_replaced`]
//!   tells the UI so it can offer "Manual edits replaced (Undo)". Manual
//!   tweaks are detected statelessly: the section's edit differs from what
//!   the auto-edit produces for the constraints before the action. When those
//!   constraints could not produce an edit, the flag stays `false`.
//! - **One user action = one undo step**: the command and the `ReplaceEdl`s
//!   it causes are applied with [`kv_core::History::apply_group`], so undo
//!   restores both the constraint and the previous edit.
//! - **A failed rebuild never blocks the action.** If the edit cannot be
//!   rebuilt after a trigger (no music yet, analysis still running, pins too
//!   long, ...), the command is applied alone, the previous edit stays, and
//!   [`RegenSnapshot::regen_error`] carries a short message for the UI.
//!   Explicit [`Session::regenerate`] / [`Session::reroll`] return the error
//!   instead and change nothing.
//! - **Re-roll seeds** follow `seed + 1` (wrapping). If the next seed happens
//!   to give the same edit, the following seeds are tried, up to
//!   [`REROLL_ATTEMPTS`], so a re-roll visibly changes the edit whenever the
//!   footage allows it.
//! - `TargetLength::FullSong` is the analysed music duration, clamped to
//!   [`kv_autoedit::MAX_TARGET_LENGTH`].

use crate::{Session, SessionError, Snapshot};
use kv_autoedit::{AutoEditError, EditInput, MediaSummary, MusicAnalysis};
use kv_core::{Command, Edl, MediaItem, MusicRef, Project, SectionId, StylePreset, TargetLength};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// How many seeds [`Session::reroll`] tries before accepting an unchanged
/// edit.
pub const REROLL_ATTEMPTS: u64 = 8;

/// Where the session gets cached analysis results. The background job runner
/// implements this; the session never analyses anything itself.
pub trait AnalysisSource {
    /// The analysis of `music`, or `None` while it is not available (not
    /// started, running or failed).
    fn music_analysis(&self, music: &MusicRef) -> Option<Arc<MusicAnalysis>>;

    /// Highlight scores of a clip (see [`kv_autoedit::MediaSummary::scores`]).
    /// `None` in the MVP.
    fn media_scores(&self, _item: &MediaItem) -> Option<Vec<f32>> {
        None
    }
}

/// Analyses keyed by music path (`MusicRef.path`): the simplest source, handy
/// for tests and tools.
impl AnalysisSource for HashMap<String, Arc<MusicAnalysis>> {
    fn music_analysis(&self, music: &MusicRef) -> Option<Arc<MusicAnalysis>> {
        self.get(&music.path).cloned()
    }
}

/// Why a section's edit cannot be generated yet. `Display` is a short,
/// user-facing sentence.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NotReady {
    #[error("That part of the project no longer exists.")]
    UnknownSection(SectionId),
    #[error("Add clips to create an edit.")]
    NoMedia,
    #[error("Add music to create an edit.")]
    NoMusic,
    #[error("Still analysing the music…")]
    AnalysisPending,
}

/// Why regenerating or re-rolling failed. `Display` is a short, user-facing
/// sentence; the technical detail stays reachable through
/// [`std::error::Error::source`] and `Debug`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegenError {
    #[error(transparent)]
    NotReady(#[from] NotReady),
    #[error("{}", auto_edit_message(.0))]
    AutoEdit(#[source] AutoEditError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl From<AutoEditError> for RegenError {
    fn from(e: AutoEditError) -> Self {
        RegenError::AutoEdit(e)
    }
}

impl From<kv_core::CoreError> for RegenError {
    fn from(e: kv_core::CoreError) -> Self {
        RegenError::Session(e.into())
    }
}

/// A short, user-facing sentence for an auto-edit failure.
pub fn auto_edit_message(e: &AutoEditError) -> &'static str {
    match e {
        AutoEditError::InvalidTargetLength(_) => {
            "The edit length must be longer than zero and at most one hour."
        }
        AutoEditError::NotEnoughBeats => "No beat was found in the music. Try another song.",
        AutoEditError::DuplicateMedia(_) => "A clip is in the project twice.",
        AutoEditError::InvalidMediaDuration { .. } => "A clip has no playable length.",
        AutoEditError::UnknownMedia(_) => "A forced or excluded moment refers to a missing clip.",
        AutoEditError::InvalidRange(_) => "A forced or excluded moment is outside its clip.",
        AutoEditError::PinExcluded { .. } => "A forced-in moment overlaps an excluded one.",
        AutoEditError::PinLongerThanTarget { .. } => {
            "A pinned moment is longer than the target length."
        }
        AutoEditError::PinsExceedTarget { .. } => {
            "Pinned moments are longer than the target length."
        }
        AutoEditError::PinRoomTooShort { .. } => {
            "A pinned moment's clip is too short to cut on the beat."
        }
        AutoEditError::NotEnoughMedia { .. } => {
            "Not enough footage for this length. Add clips or pick a shorter length."
        }
        _ => "The edit couldn't be created.",
    }
}

/// The auto-edit input for `section`: media in library order (with
/// `scores(item)` as highlight scores), the target length resolved
/// (`FullSong` = `analysis.duration`, clamped to one hour), pins, excludes and
/// seed copied. `analysis` is the analysis of the section's music, `None`
/// while it is not available.
pub fn edit_input(
    project: &Project,
    section: SectionId,
    analysis: Option<&MusicAnalysis>,
    scores: impl Fn(&MediaItem) -> Option<Vec<f32>>,
) -> Result<EditInput, NotReady> {
    let sec = project
        .section(section)
        .ok_or(NotReady::UnknownSection(section))?;
    if project.media.is_empty() {
        return Err(NotReady::NoMedia);
    }
    if sec.music.is_none() {
        return Err(NotReady::NoMusic);
    }
    let music = analysis.ok_or(NotReady::AnalysisPending)?;
    let target_length = match sec.target_length {
        TargetLength::Fixed { length } => length,
        TargetLength::FullSong => music.duration.min(kv_autoedit::MAX_TARGET_LENGTH),
    };
    Ok(EditInput {
        media: project
            .media
            .iter()
            .map(|m| MediaSummary {
                id: m.id,
                duration: m.duration,
                hilights: m.hilights.clone(),
                scores: scores(m),
            })
            .collect(),
        music: music.clone(),
        target_length,
        pins: sec.pins.clone(),
        excludes: sec.excludes.clone(),
        seed: sec.seed,
    })
}

/// Whether applying `cmd` regenerates the edit (see the module docs).
pub fn is_regenerate_trigger(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::SetMusic { .. }
            | Command::SetTargetLength { .. }
            | Command::PinRange { .. }
            | Command::UnpinRange { .. }
            | Command::ExcludeRange { .. }
            | Command::UnexcludeRange { .. }
            | Command::SetStyle { .. }
            | Command::AddMedia { .. }
            | Command::RemoveMedia { .. }
    )
}

/// The sections whose edit `cmd` regenerates in `project`.
fn affected_sections(project: &Project, cmd: &Command) -> Vec<SectionId> {
    if !is_regenerate_trigger(cmd) {
        return Vec::new();
    }
    match cmd {
        Command::AddMedia { .. } | Command::RemoveMedia { .. } => {
            project.sections.iter().map(|s| s.id).collect()
        }
        Command::SetMusic { section, .. }
        | Command::SetTargetLength { section, .. }
        | Command::PinRange { section, .. }
        | Command::UnpinRange { section, .. }
        | Command::ExcludeRange { section, .. }
        | Command::UnexcludeRange { section, .. }
        | Command::SetStyle { section, .. } => vec![*section],
        _ => Vec::new(),
    }
}

/// The edit the auto-edit produces for `section` of `project`.
fn auto_edl(
    project: &Project,
    section: SectionId,
    source: &dyn AnalysisSource,
) -> Result<Edl, RegenError> {
    let sec = project
        .section(section)
        .ok_or(NotReady::UnknownSection(section))?;
    let analysis = sec.music.as_ref().and_then(|m| source.music_analysis(m));
    let input = edit_input(project, section, analysis.as_deref(), |m| {
        source.media_scores(m)
    })?;
    let edl = kv_autoedit::generate(&input)?;
    Ok(style_pass(edl, sec.style, &input))
}

/// Apply the section's style preset to a plain-cut edit from
/// [`kv_autoedit::generate`] for `input` (see [`kv_autoedit::style`]).
/// `StylePreset::Cut` returns the edit unchanged.
fn style_pass(edl: Edl, style: StylePreset, input: &EditInput) -> Edl {
    if style == StylePreset::Cut {
        return edl;
    }
    kv_autoedit::apply_style(
        &edl,
        style,
        &input.music,
        &input.media,
        &input.pins,
        &input.excludes,
    )
}

/// Whether `section`'s edit in `project` holds manual tweaks: it differs from
/// what the auto-edit produces for the same constraints. `false` when the
/// auto-edit cannot run for them or the edit is empty.
fn has_manual_edits(project: &Project, section: SectionId, source: &dyn AnalysisSource) -> bool {
    let Some(sec) = project.section(section) else {
        return false;
    };
    if sec.edit.segments.is_empty() {
        return false;
    }
    auto_edl(project, section, source).is_ok_and(|auto| auto != sec.edit)
}

/// A [`Snapshot`] plus what regeneration did. Serialises as the snapshot's
/// keys plus `manual_edits_replaced` and `regen_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegenSnapshot {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    /// The action replaced manual tweaks (`MoveSegment`, `MoveCut`) with a
    /// regenerated edit; the UI can offer "Manual edits replaced (Undo)".
    pub manual_edits_replaced: bool,
    /// Why an auto-regenerate trigger could not rebuild the edit (a short,
    /// user-facing sentence); the action itself was applied. `None` when the
    /// edit was rebuilt or no rebuild was due.
    pub regen_error: Option<String>,
}

impl RegenSnapshot {
    pub(crate) fn plain(snapshot: Snapshot) -> Self {
        RegenSnapshot {
            snapshot,
            manual_edits_replaced: false,
            regen_error: None,
        }
    }
}

impl Session {
    /// Apply several commands as one undo step and bump the revision.
    pub(crate) fn apply_group(&mut self, cmds: Vec<Command>) -> Result<Snapshot, SessionError> {
        self.history.apply_group(cmds)?;
        self.revision += 1;
        tracing::debug!(revision = self.revision, "command group applied");
        Ok(self.snapshot())
    }

    /// Rebuild `section`'s edit from the current constraints and seed. One
    /// undo step; nothing changes (and no undo step is recorded) when the
    /// rebuilt edit equals the current one. On error nothing changes.
    pub fn regenerate(
        &mut self,
        section: SectionId,
        source: &dyn AnalysisSource,
    ) -> Result<RegenSnapshot, RegenError> {
        let edl = auto_edl(self.project(), section, source)?;
        let current = &self.project().section(section).expect("checked").edit;
        if edl == *current {
            return Ok(RegenSnapshot::plain(self.snapshot()));
        }
        let manual = !current.segments.is_empty();
        let snapshot = self.apply_group(vec![Command::ReplaceEdl { section, edl }])?;
        // The current constraints rebuild a different edit, so a non-empty
        // current edit was tweaked by hand (or is stale).
        Ok(RegenSnapshot {
            snapshot,
            manual_edits_replaced: manual,
            regen_error: None,
        })
    }

    /// Pick the next seed (`seed + 1`, see the module docs) and rebuild
    /// `section`'s edit: [`Command::SetSeed`] plus [`Command::ReplaceEdl`] as
    /// one undo step. Pins stay in the edit. On error nothing changes.
    pub fn reroll(
        &mut self,
        section: SectionId,
        source: &dyn AnalysisSource,
    ) -> Result<RegenSnapshot, RegenError> {
        let sec = self
            .project()
            .section(section)
            .ok_or(NotReady::UnknownSection(section))?;
        let current = sec.edit.clone();
        let mut seed = sec.seed;
        let mut chosen = None;
        for _ in 0..REROLL_ATTEMPTS {
            seed = seed.wrapping_add(1);
            let mut next = self.project().clone();
            next.apply(Command::SetSeed { section, seed })?;
            let edl = auto_edl(&next, section, source)?;
            let differs = edl != current;
            chosen = Some((seed, edl));
            if differs {
                break;
            }
        }
        let (seed, edl) = chosen.expect("REROLL_ATTEMPTS > 0");
        let manual = edl != current && has_manual_edits(self.project(), section, source);
        let snapshot = self.apply_group(vec![
            Command::SetSeed { section, seed },
            Command::ReplaceEdl { section, edl },
        ])?;
        Ok(RegenSnapshot {
            snapshot,
            manual_edits_replaced: manual,
            regen_error: None,
        })
    }

    /// Apply `cmd`; when it is an [auto-regenerate
    /// trigger](is_regenerate_trigger), also rebuild the edit of every
    /// affected section, all as one undo step. If a rebuild fails, the
    /// command is still applied, that section keeps its edit and
    /// [`RegenSnapshot::regen_error`] says why. Errors only when the command
    /// itself is rejected; then nothing changes.
    pub fn apply_and_regenerate(
        &mut self,
        cmd: Command,
        source: &dyn AnalysisSource,
    ) -> Result<RegenSnapshot, SessionError> {
        if affected_sections(self.project(), &cmd).is_empty() {
            return self.apply(cmd).map(RegenSnapshot::plain);
        }
        self.apply_group_and_regenerate(vec![cmd], source)
    }

    /// Apply `cmds` and rebuild the edit of every section any of them
    /// affects, all as one undo step; like [`Session::apply_and_regenerate`]
    /// for several commands (e.g. the `AddMedia`s of an import). Errors only
    /// when a command is rejected; then nothing changes.
    pub(crate) fn apply_group_and_regenerate(
        &mut self,
        mut cmds: Vec<Command>,
        source: &dyn AnalysisSource,
    ) -> Result<RegenSnapshot, SessionError> {
        let mut sections = Vec::new();
        let mut next = self.project().clone();
        for cmd in &cmds {
            for section in affected_sections(&next, cmd) {
                if !sections.contains(&section) {
                    sections.push(section);
                }
            }
            next.apply(cmd.clone())?;
        }
        let mut manual = false;
        let mut regen_error = None;
        for section in sections {
            match auto_edl(&next, section, source) {
                Ok(edl) => {
                    if next.section(section).is_some_and(|s| s.edit != edl) {
                        manual |= has_manual_edits(self.project(), section, source);
                        cmds.push(Command::ReplaceEdl { section, edl });
                    }
                }
                Err(e) => {
                    tracing::debug!(?section, error = ?e, "edit not regenerated");
                    regen_error.get_or_insert_with(|| e.to_string());
                }
            }
        }
        let snapshot = self.apply_group(cmds)?;
        Ok(RegenSnapshot {
            snapshot,
            manual_edits_replaced: manual,
            regen_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::{MediaId, MediaKind, Micros, ProjectId, RangeRef};
    use std::error::Error as _;

    const S: SectionId = Project::DEFAULT_SECTION;
    const SEC: i64 = 1_000_000;

    fn music(secs: i64) -> MusicAnalysis {
        let beats: Vec<Micros> = (0..secs * 2).map(|i| Micros(i * SEC / 2)).collect();
        MusicAnalysis {
            duration: Micros(secs * SEC),
            tempo_bpm: 120.0,
            downbeats: beats.iter().step_by(4).copied().collect(),
            beats,
            onsets: Vec::new(),
            energy: vec![0.3; (secs * 10) as usize],
        }
    }

    fn project() -> Project {
        let mut p = Project::new(ProjectId(1));
        for id in [2, 1] {
            p.apply(Command::AddMedia {
                item: MediaItem {
                    id: MediaId(id),
                    path: format!("c{id}.mp4"),
                    duration: Micros(10 * SEC),
                    kind: MediaKind::Video,
                    hilights: vec![Micros(SEC)],
                    clip_audio: false,
                    chapters: Vec::new(),
                },
            })
            .unwrap();
        }
        p
    }

    fn with_music(mut p: Project) -> Project {
        p.apply(Command::SetMusic {
            section: S,
            music: Some(MusicRef {
                path: "song.mp3".into(),
                duration: Micros(40 * SEC),
            }),
        })
        .unwrap();
        p
    }

    #[test]
    fn edit_input_copies_project_in_library_order() {
        let mut p = with_music(project());
        let pin = RangeRef {
            media_id: MediaId(1),
            start: Micros(SEC),
            end: Micros(2 * SEC),
        };
        p.apply(Command::PinRange {
            section: S,
            range: pin,
        })
        .unwrap();
        p.apply(Command::SetSeed {
            section: S,
            seed: 9,
        })
        .unwrap();
        let m = music(37);
        let input = edit_input(&p, S, Some(&m), |_| None).unwrap();
        let ids: Vec<_> = input.media.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![MediaId(2), MediaId(1)]);
        assert_eq!(input.media[0].hilights, vec![Micros(SEC)]);
        // FullSong resolves to the analysed duration.
        assert_eq!(input.target_length, Micros(37 * SEC));
        assert_eq!(input.pins, vec![pin]);
        assert!(input.excludes.is_empty());
        assert_eq!(input.seed, 9);

        p.apply(Command::SetTargetLength {
            section: S,
            target: TargetLength::Fixed {
                length: Micros(15 * SEC),
            },
        })
        .unwrap();
        let input = edit_input(&p, S, Some(&m), |m| Some(vec![m.id.0 as f32])).unwrap();
        assert_eq!(input.target_length, Micros(15 * SEC));
        assert_eq!(input.media[0].scores, Some(vec![2.0]));
    }

    #[test]
    fn full_song_is_clamped_to_one_hour() {
        let p = with_music(project());
        let mut m = music(4);
        m.duration = Micros(2 * 3_600 * SEC);
        let input = edit_input(&p, S, Some(&m), |_| None).unwrap();
        assert_eq!(input.target_length, kv_autoedit::MAX_TARGET_LENGTH);
    }

    #[test]
    fn not_ready_errors() {
        let m = music(10);
        let empty = Project::new(ProjectId(1));
        assert_eq!(
            edit_input(&empty, S, Some(&m), |_| None),
            Err(NotReady::NoMedia)
        );
        assert_eq!(
            edit_input(&project(), S, Some(&m), |_| None),
            Err(NotReady::NoMusic)
        );
        assert_eq!(
            NotReady::NoMusic.to_string(),
            "Add music to create an edit."
        );
        assert_eq!(
            edit_input(&with_music(project()), S, None, |_| None),
            Err(NotReady::AnalysisPending)
        );
        assert_eq!(
            edit_input(&project(), SectionId(7), Some(&m), |_| None),
            Err(NotReady::UnknownSection(SectionId(7)))
        );
    }

    #[test]
    fn auto_edit_errors_map_to_short_messages() {
        let target = Micros(15 * SEC);
        let e = RegenError::from(AutoEditError::PinsExceedTarget { target });
        assert_eq!(
            e.to_string(),
            "Pinned moments are longer than the target length."
        );
        assert_eq!(
            e.source().unwrap().to_string(),
            AutoEditError::PinsExceedTarget { target }.to_string()
        );
        assert_eq!(
            RegenError::from(AutoEditError::NotEnoughMedia { target }).to_string(),
            "Not enough footage for this length. Add clips or pick a shorter length."
        );
        assert_eq!(
            RegenError::from(NotReady::NoMusic).to_string(),
            "Add music to create an edit."
        );
        // Every message is a short sentence.
        for e in [
            AutoEditError::InvalidTargetLength(target),
            AutoEditError::NotEnoughBeats,
            AutoEditError::DuplicateMedia(MediaId(1)),
            AutoEditError::InvalidMediaDuration {
                media_id: MediaId(1),
                duration: Micros(0),
            },
            AutoEditError::UnknownMedia(MediaId(1)),
            AutoEditError::NotEnoughMedia { target },
        ] {
            let msg = auto_edit_message(&e);
            assert!(msg.ends_with('.') && msg.len() < 80, "{msg}");
        }
    }

    #[test]
    fn triggers() {
        let p = project();
        let pin = RangeRef {
            media_id: MediaId(1),
            start: Micros(0),
            end: Micros(SEC),
        };
        assert_eq!(
            affected_sections(
                &p,
                &Command::PinRange {
                    section: S,
                    range: pin
                }
            ),
            vec![S]
        );
        assert_eq!(
            affected_sections(
                &p,
                &Command::RemoveMedia {
                    media_id: MediaId(1)
                }
            ),
            vec![S]
        );
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
            Command::SetSeed {
                section: S,
                seed: 1,
            },
            Command::ReplaceEdl {
                section: S,
                edl: Edl::default(),
            },
        ] {
            assert!(!is_regenerate_trigger(&cmd), "{cmd:?}");
            assert!(affected_sections(&p, &cmd).is_empty());
        }
    }
}
