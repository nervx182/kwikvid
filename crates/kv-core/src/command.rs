//! The `Command` API: every user-facing action on a project.
//!
//! The desktop app, the CLI and the MCP server all build [`Command`] values and
//! hand them to [`Project::apply`] (usually through a
//! [`History`](crate::History) for undo). Commands and events serialise to JSON
//! with a `"type"` tag, e.g. `{"type":"set_seed","section":1,"seed":7}`.

use crate::error::CoreError;
use crate::model::{
    AspectRatio, Edl, MediaId, MediaItem, MusicRef, Project, RangeRef, Section, SectionId, Segment,
    Speed, StylePreset, TargetLength, TitleCard, Transition,
};
use crate::Micros;
use serde::{Deserialize, Serialize};

/// A user-facing action on a [`Project`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Add a clip or photo to the library. The caller picks the id (see
    /// [`Project::next_media_id`]).
    AddMedia {
        item: MediaItem,
    },
    /// Remove a media item, along with every pin, exclude and EDL segment that
    /// refers to it (affected EDLs are repacked).
    RemoveMedia {
        media_id: MediaId,
    },
    /// Set or clear a section's music.
    SetMusic {
        section: SectionId,
        music: Option<MusicRef>,
    },
    SetTargetLength {
        section: SectionId,
        target: TargetLength,
    },
    /// Force a range into the edit. Rejected if it overlaps an exclude.
    PinRange {
        section: SectionId,
        range: RangeRef,
    },
    /// Remove a pin; the range must match an existing pin exactly.
    UnpinRange {
        section: SectionId,
        range: RangeRef,
    },
    /// Force a range (or a whole clip, `0..duration`) out of the edit.
    /// Rejected if it overlaps a pin.
    ExcludeRange {
        section: SectionId,
        range: RangeRef,
    },
    /// Remove an exclude; the range must match an existing exclude exactly.
    UnexcludeRange {
        section: SectionId,
        range: RangeRef,
    },
    /// Move the segment at index `from` so it ends up at index `to`, then
    /// repack the timeline. Transitions stay with the slot, not the segment:
    /// the transition into index `k` is the same before and after, and every
    /// segment keeps the footage it shows between its cut points (the middle
    /// of each crossfade), so the cut points stay where they were (on the
    /// beat) and only the order of what plays between them changes. Each
    /// moved segment's crossfade handles (`src_in` / `src_out` extended by
    /// half a crossfade) are rebuilt for its new slot; a crossfade that would
    /// need footage outside the media, or whose half is longer than either
    /// neighbour's footage between its cut points, is shortened (to a cut if
    /// nothing is left). Edits with only cuts are reordered as they are.
    MoveSegment {
        section: SectionId,
        from: usize,
        to: usize,
    },
    SetSeed {
        section: SectionId,
        seed: u64,
    },
    /// Replace a section's whole edit (the auto-edit result, for example).
    ReplaceEdl {
        section: SectionId,
        edl: Edl,
    },
    /// Move the cut between segments `cut - 1` and `cut` (`1 <= cut < len`)
    /// to timeline time `to`: the previous segment's `src_out` and the next
    /// segment's `src_in` both shift by the timeline delta scaled by each
    /// segment's speed, then the timeline is repacked. Rejected if either
    /// segment would become shorter than [`Segment::MIN_LEN`] or extend
    /// outside its media. The core knows nothing about beats: front ends
    /// compute `to` from the beat grid ("nudge by one beat").
    MoveCut {
        section: SectionId,
        cut: usize,
        to: Micros,
    },
    /// Turn a clip's own audio on or off in the edit.
    SetClipAudio {
        media_id: MediaId,
        enabled: bool,
    },
    /// Set the music volume, 0..=100 percent.
    SetMusicVolume {
        percent: u8,
    },
    /// Set the output aspect ratio.
    SetAspectRatio {
        aspect: AspectRatio,
    },
    /// Set a section's style preset.
    SetStyle {
        section: SectionId,
        style: StylePreset,
    },
    /// Set or clear a section's title card.
    SetTitleCard {
        section: SectionId,
        title: Option<TitleCard>,
    },
}

/// What an applied [`Command`] changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    MediaAdded {
        media_id: MediaId,
    },
    MediaRemoved {
        media_id: MediaId,
        pins_removed: usize,
        excludes_removed: usize,
        segments_removed: usize,
    },
    MusicSet {
        section: SectionId,
    },
    TargetLengthSet {
        section: SectionId,
    },
    RangePinned {
        section: SectionId,
        range: RangeRef,
    },
    RangeUnpinned {
        section: SectionId,
        range: RangeRef,
    },
    RangeExcluded {
        section: SectionId,
        range: RangeRef,
    },
    RangeUnexcluded {
        section: SectionId,
        range: RangeRef,
    },
    SegmentMoved {
        section: SectionId,
        from: usize,
        to: usize,
    },
    SeedSet {
        section: SectionId,
        seed: u64,
    },
    EdlReplaced {
        section: SectionId,
        segments: usize,
    },
    CutMoved {
        section: SectionId,
        cut: usize,
        /// Where the cut actually landed after scaling by speed and
        /// repacking (equal to the requested time at normal speed).
        at: Micros,
    },
    ClipAudioSet {
        media_id: MediaId,
        enabled: bool,
    },
    MusicVolumeSet {
        percent: u8,
    },
    AspectRatioSet {
        aspect: AspectRatio,
    },
    StyleSet {
        section: SectionId,
        style: StylePreset,
    },
    TitleCardSet {
        section: SectionId,
        /// False when the title card was cleared.
        present: bool,
    },
}

impl Project {
    /// Validate and apply a command. On error the project is unchanged.
    pub fn apply(&mut self, cmd: Command) -> Result<Event, CoreError> {
        match cmd {
            Command::AddMedia { item } => {
                if self.media_item(item.id).is_some() {
                    return Err(CoreError::DuplicateMedia(item.id));
                }
                check_path(&item.path)?;
                check_duration(item.duration)?;
                check_chapters(&item)?;
                if let Some(&at) = item
                    .hilights
                    .iter()
                    .find(|h| h.0 < 0 || **h > item.duration)
                {
                    return Err(CoreError::HilightOutOfRange {
                        at,
                        duration: item.duration,
                    });
                }
                let media_id = item.id;
                self.media.push(item);
                Ok(Event::MediaAdded { media_id })
            }
            Command::RemoveMedia { media_id } => {
                let idx = self
                    .media
                    .iter()
                    .position(|m| m.id == media_id)
                    .ok_or(CoreError::UnknownMedia(media_id))?;
                self.media.remove(idx);
                let (mut pins, mut excludes, mut segments) = (0, 0, 0);
                for s in &mut self.sections {
                    pins += retain_count(&mut s.pins, |r| r.media_id != media_id);
                    excludes += retain_count(&mut s.excludes, |r| r.media_id != media_id);
                    let removed = retain_count(&mut s.edit.segments, |g| g.media_id != media_id);
                    if removed > 0 {
                        s.edit.repack();
                    }
                    segments += removed;
                }
                Ok(Event::MediaRemoved {
                    media_id,
                    pins_removed: pins,
                    excludes_removed: excludes,
                    segments_removed: segments,
                })
            }
            Command::SetMusic { section, music } => {
                if let Some(m) = &music {
                    check_path(&m.path)?;
                    check_duration(m.duration)?;
                }
                self.section_mut(section)?.music = music;
                Ok(Event::MusicSet { section })
            }
            Command::SetTargetLength { section, target } => {
                if let TargetLength::Fixed { length } = target {
                    if length.0 <= 0 {
                        return Err(CoreError::InvalidTargetLength(length));
                    }
                }
                self.section_mut(section)?.target_length = target;
                Ok(Event::TargetLengthSet { section })
            }
            Command::PinRange { section, range } => {
                self.check_range(&range)?;
                let s = self.section_mut(section)?;
                if s.pins.contains(&range) {
                    return Err(CoreError::AlreadyPinned(range));
                }
                if let Some(existing) = s.excludes.iter().find(|e| e.overlaps(&range)) {
                    return Err(CoreError::RangeConflict {
                        range,
                        existing: *existing,
                    });
                }
                s.pins.push(range);
                Ok(Event::RangePinned { section, range })
            }
            Command::UnpinRange { section, range } => {
                let s = self.section_mut(section)?;
                let idx = s
                    .pins
                    .iter()
                    .position(|p| *p == range)
                    .ok_or(CoreError::PinNotFound(range))?;
                s.pins.remove(idx);
                Ok(Event::RangeUnpinned { section, range })
            }
            Command::ExcludeRange { section, range } => {
                self.check_range(&range)?;
                let s = self.section_mut(section)?;
                if s.excludes.contains(&range) {
                    return Err(CoreError::AlreadyExcluded(range));
                }
                if let Some(existing) = s.pins.iter().find(|p| p.overlaps(&range)) {
                    return Err(CoreError::RangeConflict {
                        range,
                        existing: *existing,
                    });
                }
                s.excludes.push(range);
                Ok(Event::RangeExcluded { section, range })
            }
            Command::UnexcludeRange { section, range } => {
                let s = self.section_mut(section)?;
                let idx = s
                    .excludes
                    .iter()
                    .position(|p| *p == range)
                    .ok_or(CoreError::ExcludeNotFound(range))?;
                s.excludes.remove(idx);
                Ok(Event::RangeUnexcluded { section, range })
            }
            Command::MoveSegment { section, from, to } => {
                let s = self
                    .section(section)
                    .ok_or(CoreError::UnknownSection(section))?;
                let len = s.edit.segments.len();
                for index in [from, to] {
                    if index >= len {
                        return Err(CoreError::SegmentIndexOutOfRange { index, len });
                    }
                }
                let edl = move_segment(&s.edit, from, to, |id| self.duration_of(id))?;
                self.section_mut(section)?.edit = edl;
                Ok(Event::SegmentMoved { section, from, to })
            }
            Command::SetSeed { section, seed } => {
                self.section_mut(section)?.seed = seed;
                Ok(Event::SeedSet { section, seed })
            }
            Command::ReplaceEdl { section, edl } => {
                self.section_mut(section)?;
                self.check_edl(&edl)?;
                let segments = edl.segments.len();
                self.section_mut(section)?.edit = edl;
                Ok(Event::EdlReplaced { section, segments })
            }
            Command::MoveCut { section, cut, to } => {
                let edit = &self
                    .section(section)
                    .ok_or(CoreError::UnknownSection(section))?
                    .edit;
                let mut edl = move_cut(edit, cut, to, |id| self.duration_of(id))?;
                edl.repack();
                self.check_edl(&edl)?;
                let at = edl.segments[cut].timeline_start;
                self.section_mut(section)?.edit = edl;
                Ok(Event::CutMoved { section, cut, at })
            }
            Command::SetClipAudio { media_id, enabled } => {
                self.media
                    .iter_mut()
                    .find(|m| m.id == media_id)
                    .ok_or(CoreError::UnknownMedia(media_id))?
                    .clip_audio = enabled;
                Ok(Event::ClipAudioSet { media_id, enabled })
            }
            Command::SetMusicVolume { percent } => {
                if percent > 100 {
                    return Err(CoreError::InvalidMusicVolume(percent));
                }
                self.settings.music_volume_percent = percent;
                Ok(Event::MusicVolumeSet { percent })
            }
            Command::SetAspectRatio { aspect } => {
                self.settings.aspect_ratio = aspect;
                Ok(Event::AspectRatioSet { aspect })
            }
            Command::SetStyle { section, style } => {
                self.section_mut(section)?.style = style;
                Ok(Event::StyleSet { section, style })
            }
            Command::SetTitleCard { section, title } => {
                if let Some(t) = &title {
                    check_title(t)?;
                }
                let present = title.is_some();
                self.section_mut(section)?.title = title;
                Ok(Event::TitleCardSet { section, present })
            }
        }
    }

    fn section_mut(&mut self, id: SectionId) -> Result<&mut Section, CoreError> {
        self.sections
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(CoreError::UnknownSection(id))
    }

    /// Media duration, or an error if the id is unknown.
    fn duration_of(&self, id: MediaId) -> Result<Micros, CoreError> {
        self.media_item(id)
            .map(|m| m.duration)
            .ok_or(CoreError::UnknownMedia(id))
    }

    fn check_range(&self, r: &RangeRef) -> Result<(), CoreError> {
        let duration = self.duration_of(r.media_id)?;
        if r.start.0 < 0 || r.start >= r.end || r.end > duration {
            return Err(CoreError::InvalidRange {
                start: r.start,
                end: r.end,
                duration,
            });
        }
        Ok(())
    }

    fn check_edl(&self, edl: &Edl) -> Result<(), CoreError> {
        let mut prev_start = Micros(0);
        for (index, seg) in edl.segments.iter().enumerate() {
            let bad = |reason| Err(CoreError::InvalidSegment { index, reason });
            let duration = self.duration_of(seg.media_id)?;
            if seg.src_in.0 < 0 || seg.src_in >= seg.src_out || seg.src_out > duration {
                return bad("source range is empty or outside the media duration");
            }
            if seg.speed.0 == 0 {
                return bad("speed must be positive");
            }
            if seg.timeline_start < prev_start {
                return bad("timeline_start must be non-negative and non-decreasing");
            }
            if let Transition::Crossfade { duration } = seg.transition {
                if duration.0 <= 0 || duration > seg.timeline_len() {
                    return bad("crossfade must be positive and no longer than the segment");
                }
            }
            prev_start = seg.timeline_start;
        }
        Ok(())
    }
}

/// The EDL with cut `cut` moved to `to`, not yet repacked.
fn move_cut(
    edl: &Edl,
    cut: usize,
    to: Micros,
    duration_of: impl Fn(MediaId) -> Result<Micros, CoreError>,
) -> Result<Edl, CoreError> {
    let len = edl.segments.len();
    if cut == 0 || cut >= len {
        return Err(CoreError::CutIndexOutOfRange { cut, len });
    }
    let delta = i128::from(to.0) - i128::from(edl.segments[cut].timeline_start.0);
    let mut edl = edl.clone();
    for (index, sign) in [(cut - 1, 1), (cut, -1)] {
        let seg = &mut edl.segments[index];
        let src_delta = scale_by_speed(delta, seg.speed);
        let outside = CoreError::CutOutsideMedia { cut, to, index };
        let shifted = |t: Micros| {
            i64::try_from(i128::from(t.0) + src_delta)
                .map(Micros)
                .map_err(|_| outside.clone())
        };
        // The previous segment's end and the next segment's start both
        // follow the cut.
        if sign > 0 {
            seg.src_out = shifted(seg.src_out)?;
        } else {
            seg.src_in = shifted(seg.src_in)?;
        }
        if seg.src_in.0 < 0 || seg.src_out > duration_of(seg.media_id)? {
            return Err(outside);
        }
        if seg.src_out <= seg.src_in || seg.timeline_len() < Segment::MIN_LEN {
            return Err(CoreError::CutTooShort {
                cut,
                to,
                index,
                min: Segment::MIN_LEN,
            });
        }
    }
    Ok(edl)
}

/// [`Command::MoveSegment`] on a copy of `edl` (indices already checked).
fn move_segment(
    edl: &Edl,
    from: usize,
    to: usize,
    duration_of: impl Fn(MediaId) -> Result<Micros, CoreError>,
) -> Result<Edl, CoreError> {
    let segs = &edl.segments;
    let n = segs.len();
    // Effective overlap into each slot, as `Edl::repack` computes it.
    let overlap: Vec<i64> = (0..n)
        .map(|i| match i {
            0 => 0,
            _ => segs[i]
                .transition
                .overlap()
                .0
                .min(segs[i - 1].timeline_len().0),
        })
        .collect();
    // Half a crossfade belongs to each side; an odd microsecond goes to the
    // outgoing segment, so the cut point is `timeline_start + overlap / 2`.
    let lead = |d: i64| d / 2;
    let trail = |d: i64| d - d / 2;
    // Each segment's footage between its cut points, in source time.
    let mut cores: Vec<(Segment, i64, i64)> = segs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let out = overlap.get(i + 1).copied().unwrap_or(0);
            let core_in = s.src_in.0 + src_of(lead(overlap[i]), s.speed);
            let core_out = s.src_out.0 - src_of(trail(out), s.speed);
            (s.clone(), core_in, core_out)
        })
        .collect();
    let moved = cores.remove(from);
    cores.insert(to, moved);

    // Room each segment has on the timeline before and after its core:
    // media bounds, and the core itself (a fade never outlasts it).
    let mut room_before = Vec::with_capacity(n);
    let mut room_after = Vec::with_capacity(n);
    for (s, core_in, core_out) in &cores {
        let core_len = timeline_of(core_out - core_in, s.speed).max(0);
        let dur = duration_of(s.media_id)?.0;
        room_before.push(timeline_of(*core_in, s.speed).clamp(0, core_len));
        room_after.push(timeline_of(dur - core_out, s.speed).clamp(0, core_len));
    }
    let mut fade = vec![0i64; n + 1];
    for k in 1..n {
        let room = room_after[k - 1].min(room_before[k]);
        fade[k] = overlap[k].min(room.saturating_mul(2));
    }

    let mut out = edl.clone();
    for (k, (seg, (mut s, core_in, core_out))) in out.segments.iter_mut().zip(cores).enumerate() {
        s.src_in = Micros(core_in - src_of(lead(fade[k]), s.speed));
        s.src_out = Micros(core_out + src_of(trail(fade[k + 1]), s.speed));
        // The slot's transition, shortened if it had to be.
        s.transition = match segs[k].transition {
            _ if k == 0 || fade[k] == overlap[k] => segs[k].transition,
            _ if fade[k] == 0 => Transition::Cut,
            _ => Transition::Crossfade {
                duration: Micros(fade[k]),
            },
        };
        *seg = s;
    }
    out.repack();
    Ok(out)
}

/// [`scale_by_speed`] for `i64` values.
fn src_of(timeline: i64, speed: Speed) -> i64 {
    i64::try_from(scale_by_speed(i128::from(timeline), speed)).unwrap_or(i64::MAX)
}

/// A source duration converted to timeline time at `speed` (rounded toward
/// zero).
fn timeline_of(src: i64, speed: Speed) -> i64 {
    let t = i128::from(src) * i128::from(Speed::NORMAL.0) / i128::from(speed.0.max(1));
    i64::try_from(t).unwrap_or(i64::MAX)
}

/// A timeline duration converted to source time at `speed` (rounded toward
/// zero).
fn scale_by_speed(timeline: i128, speed: Speed) -> i128 {
    timeline * i128::from(speed.0.max(1)) / i128::from(Speed::NORMAL.0)
}

fn check_title(t: &TitleCard) -> Result<(), CoreError> {
    let chars = t.text.chars().count();
    if chars == 0 || chars > TitleCard::MAX_CHARS || t.text.trim().is_empty() {
        return Err(CoreError::InvalidTitleText {
            chars,
            max: TitleCard::MAX_CHARS,
        });
    }
    if t.duration < TitleCard::MIN_DURATION || t.duration > TitleCard::MAX_DURATION {
        return Err(CoreError::InvalidTitleDuration {
            duration: t.duration,
            min: TitleCard::MIN_DURATION,
            max: TitleCard::MAX_DURATION,
        });
    }
    Ok(())
}

fn check_path(path: &str) -> Result<(), CoreError> {
    let b = path.as_bytes();
    let absolute = path.starts_with('/')
        || path.starts_with('\\')
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    if path.is_empty() || absolute {
        return Err(CoreError::InvalidPath(path.to_owned()));
    }
    Ok(())
}

/// A chaptered item's chapters must have valid paths and durations, the
/// item's path must be the first chapter's and its duration their sum.
fn check_chapters(item: &MediaItem) -> Result<(), CoreError> {
    let Some(first) = item.chapters.first() else {
        return Ok(());
    };
    let mut sum: i64 = 0;
    for c in &item.chapters {
        check_path(&c.path)?;
        check_duration(c.duration)?;
        sum = sum.saturating_add(c.duration.0);
    }
    if item.path != first.path {
        return Err(CoreError::ChapterPathMismatch {
            path: item.path.clone(),
            first: first.path.clone(),
        });
    }
    if item.duration.0 != sum {
        return Err(CoreError::ChapterDurationMismatch {
            duration: item.duration,
            sum: Micros(sum),
        });
    }
    Ok(())
}

fn check_duration(d: Micros) -> Result<(), CoreError> {
    if d.0 <= 0 {
        return Err(CoreError::InvalidDuration(d));
    }
    Ok(())
}

/// `Vec::retain` that returns how many elements were removed.
fn retain_count<T>(v: &mut Vec<T>, keep: impl FnMut(&T) -> bool) -> usize {
    let before = v.len();
    v.retain(keep);
    before - v.len()
}
