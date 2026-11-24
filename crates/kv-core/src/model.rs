//! Project model: media library, sections and the edit decision list (EDL).
//!
//! Everything here is plain data, serialisable to JSON, with no I/O. Time
//! values are [`Micros`]; persisted state avoids floats.

use crate::Micros;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Identifies a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub u64);

/// Identifies a [`MediaItem`] within a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MediaId(pub u64);

/// Identifies a [`Section`] within a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SectionId(pub u64);

/// Project file format version. Serialises as the number `1`; any other value
/// is rejected when deserialising.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatVersion;

impl FormatVersion {
    /// The only project format version this build reads and writes.
    pub const CURRENT: u32 = 1;
}

impl Serialize for FormatVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(Self::CURRENT)
    }
}

impl<'de> Deserialize<'de> for FormatVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = u32::deserialize(d)?;
        if v == Self::CURRENT {
            Ok(FormatVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported project version {v}, expected {}",
                Self::CURRENT
            )))
        }
    }
}

/// A kwikvid project. Mutate it only through [`Project::apply`] (or a
/// [`History`](crate::History)), never field by field from a front end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Always `1` in JSON; kept first so it is the first key in the file.
    pub version: FormatVersion,
    pub id: ProjectId,
    pub media: Vec<MediaItem>,
    /// Exactly one section in the MVP; several in Roadmap II story projects.
    pub sections: Vec<Section>,
    pub settings: ProjectSettings,
}

impl Project {
    /// Id of the section created by [`Project::new`].
    pub const DEFAULT_SECTION: SectionId = SectionId(1);

    /// An empty project with one empty section ([`Project::DEFAULT_SECTION`]).
    pub fn new(id: ProjectId) -> Self {
        Project {
            version: FormatVersion,
            id,
            media: Vec::new(),
            sections: vec![Section::new(Self::DEFAULT_SECTION)],
            settings: ProjectSettings::default(),
        }
    }

    /// Look up a media item by id.
    pub fn media_item(&self, id: MediaId) -> Option<&MediaItem> {
        self.media.iter().find(|m| m.id == id)
    }

    /// Look up a section by id.
    pub fn section(&self, id: SectionId) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }

    /// The smallest media id greater than every id in use; handy for front
    /// ends building an `AddMedia` command.
    pub fn next_media_id(&self) -> MediaId {
        MediaId(self.media.iter().map(|m| m.id.0 + 1).max().unwrap_or(1))
    }
}

/// Project-wide output settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSettings {
    pub aspect_ratio: AspectRatio,
    /// Music volume, 0..=100.
    pub music_volume_percent: u8,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        ProjectSettings {
            aspect_ratio: AspectRatio::Landscape16x9,
            music_volume_percent: 100,
        }
    }
}

/// Output frame shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectRatio {
    Landscape16x9,
    Portrait9x16,
    Square1x1,
}

/// What kind of file a media item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Photo,
}

/// A clip or photo in the project library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: MediaId,
    /// Path relative to the project file, `/`-separated.
    pub path: String,
    pub duration: Micros,
    pub kind: MediaKind,
    /// HiLight tags, as offsets from the start of the media.
    pub hilights: Vec<Micros>,
}

/// The music track of a section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MusicRef {
    /// Path relative to the project file, `/`-separated.
    pub path: String,
    pub duration: Micros,
}

/// How long the auto-edit of a section should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetLength {
    /// A fixed length (for example 15, 30 or 60 seconds).
    Fixed { length: Micros },
    /// As long as the section's music.
    FullSong,
}

/// A half-open range `[start, end)` within one media item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RangeRef {
    pub media_id: MediaId,
    pub start: Micros,
    pub end: Micros,
}

impl RangeRef {
    /// True when both ranges are in the same media and share some time.
    pub fn overlaps(&self, other: &RangeRef) -> bool {
        self.media_id == other.media_id && self.start < other.end && other.start < self.end
    }
}

/// One part of a project with its own music, constraints and edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub id: SectionId,
    pub music: Option<MusicRef>,
    pub target_length: TargetLength,
    /// Ranges that must appear in the edit.
    pub pins: Vec<RangeRef>,
    /// Ranges that must not appear in the edit.
    pub excludes: Vec<RangeRef>,
    /// Seed for the deterministic auto-edit.
    pub seed: u64,
    pub edit: Edl,
}

impl Section {
    /// An empty section: no music, full-song length, seed 0, empty edit.
    pub fn new(id: SectionId) -> Self {
        Section {
            id,
            music: None,
            target_length: TargetLength::FullSong,
            pins: Vec::new(),
            excludes: Vec::new(),
            seed: 0,
            edit: Edl::default(),
        }
    }
}

/// Edit decision list: the ordered segments of a section's edit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edl {
    pub segments: Vec<Segment>,
}

impl Edl {
    /// Lay the segments out back to back from zero, in their current order.
    /// A crossfade on a segment overlaps it with the previous one by the
    /// crossfade duration (clamped to the previous segment's length).
    pub fn repack(&mut self) {
        let mut cursor = Micros(0);
        let mut prev_len = Micros(0);
        for (i, seg) in self.segments.iter_mut().enumerate() {
            let overlap = if i == 0 {
                Micros(0)
            } else {
                seg.transition.overlap().min(prev_len)
            };
            seg.timeline_start = Micros(cursor.0 - overlap.0);
            prev_len = seg.timeline_len();
            cursor = Micros(seg.timeline_start.0 + prev_len.0);
        }
    }
}

/// One cut of a media item placed on the section timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub media_id: MediaId,
    /// Source in point (inclusive), from the start of the media.
    pub src_in: Micros,
    /// Source out point (exclusive).
    pub src_out: Micros,
    pub timeline_start: Micros,
    /// Transition from the previous segment into this one.
    pub transition: Transition,
    pub speed: Speed,
}

impl Segment {
    /// Length on the timeline: source length divided by speed.
    pub fn timeline_len(&self) -> Micros {
        let src = i128::from(self.src_out.0 - self.src_in.0);
        let pct = i128::from(self.speed.0.max(1));
        let len = src * i128::from(Speed::NORMAL.0) / pct;
        Micros(i64::try_from(len).unwrap_or(i64::MAX))
    }
}

/// Transition into a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transition {
    Cut,
    Crossfade { duration: Micros },
}

impl Transition {
    /// How long this transition overlaps the previous segment.
    pub fn overlap(&self) -> Micros {
        match self {
            Transition::Cut => Micros(0),
            Transition::Crossfade { duration } => *duration,
        }
    }
}

/// Playback speed in percent: 100 is normal, 50 is 2x slow motion, 200 is
/// double speed. An integer so persisted state has no floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Speed(pub u32);

impl Speed {
    pub const NORMAL: Speed = Speed(100);
}

impl Default for Speed {
    fn default() -> Self {
        Speed::NORMAL
    }
}
