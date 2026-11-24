//! Property tests: undo/redo restores state exactly, and projects round-trip
//! through JSON.

use kv_core::*;
use proptest::prelude::*;

const S: SectionId = Project::DEFAULT_SECTION;

/// Raw randomness for one command; turned into a valid command against the
/// current project state by [`build_command`].
type Raw = (u8, u64, u64, u64);

fn pick<T>(items: &[T], r: u64) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        items.get((r % items.len() as u64) as usize)
    }
}

fn random_range(p: &Project, a: u64, b: u64, c: u64) -> Option<RangeRef> {
    let m = pick(&p.media, a)?;
    let dur = m.duration.0 as u64;
    let start = b % dur;
    let end = start + 1 + c % (dur - start);
    Some(RangeRef {
        media_id: m.id,
        start: Micros(start as i64),
        end: Micros(end as i64),
    })
}

fn random_edl(p: &Project, a: u64, b: u64) -> Edl {
    let mut segments = Vec::new();
    let mut start = 0i64;
    for i in 0..(a % 5) {
        let Some(m) = pick(&p.media, b.wrapping_add(i * 7919)) else {
            break;
        };
        let dur = m.duration.0;
        let src_in = (b.wrapping_mul(i + 1) % dur as u64) as i64;
        let src_out = src_in + 1 + ((a ^ i) % (dur - src_in) as u64) as i64;
        let speed = Speed([50, 100, 200][((a >> 8) + i) as usize % 3]);
        let mut seg = Segment {
            media_id: m.id,
            src_in: Micros(src_in),
            src_out: Micros(src_out),
            timeline_start: Micros(start),
            transition: Transition::Cut,
            speed,
        };
        let len = seg.timeline_len().0;
        if i > 0 && len > 1 && b & 1 == 1 {
            seg.transition = Transition::Crossfade {
                duration: Micros(1 + (b as i64 & 0xffff) % len),
            };
        }
        start += len;
        segments.push(seg);
    }
    Edl { segments }
}

/// Build a command from raw randomness; may be invalid for the current state.
fn candidate(p: &Project, (kind, a, b, c): Raw) -> Option<Command> {
    let s = p.section(S)?;
    Some(match kind % 17 {
        0 => Command::AddMedia {
            item: MediaItem {
                id: p.next_media_id(),
                path: format!("clips/{a}.mp4"),
                duration: Micros(1_000 + (a % 30_000_000) as i64),
                kind: if b % 4 == 0 {
                    MediaKind::Photo
                } else {
                    MediaKind::Video
                },
                hilights: vec![Micros(0)],
                clip_audio: c % 2 == 0,
                chapters: Vec::new(),
            },
        },
        1 => Command::RemoveMedia {
            media_id: pick(&p.media, a)?.id,
        },
        2 => Command::SetMusic {
            section: S,
            music: (b % 3 != 0).then(|| MusicRef {
                path: format!("music/{a}.mp3"),
                duration: Micros(1 + (a % 300_000_000) as i64),
            }),
        },
        3 => Command::SetTargetLength {
            section: S,
            target: if b % 2 == 0 {
                TargetLength::FullSong
            } else {
                TargetLength::Fixed {
                    length: Micros(1 + (a % 120_000_000) as i64),
                }
            },
        },
        4 => Command::PinRange {
            section: S,
            range: random_range(p, a, b, c)?,
        },
        5 => Command::UnpinRange {
            section: S,
            range: *pick(&s.pins, a)?,
        },
        6 => Command::ExcludeRange {
            section: S,
            range: random_range(p, a, b, c)?,
        },
        7 => Command::UnexcludeRange {
            section: S,
            range: *pick(&s.excludes, a)?,
        },
        8 => {
            let len = s.edit.segments.len() as u64;
            if len == 0 {
                return None;
            }
            Command::MoveSegment {
                section: S,
                from: (a % len) as usize,
                to: (b % len) as usize,
            }
        }
        9 => Command::SetSeed {
            section: S,
            seed: a,
        },
        10 => Command::ReplaceEdl {
            section: S,
            edl: random_edl(p, a, b),
        },
        11 => {
            let segs = &s.edit.segments;
            if segs.len() < 2 {
                return None;
            }
            let cut = 1 + (a % (segs.len() as u64 - 1)) as usize;
            // Within +-2 s of the current cut: often valid, sometimes not.
            let offset = (b % 4_000_001) as i64 - 2_000_000;
            Command::MoveCut {
                section: S,
                cut,
                to: Micros(segs[cut].timeline_start.0 + offset),
            }
        }
        12 => Command::SetClipAudio {
            media_id: pick(&p.media, a)?.id,
            enabled: b % 2 == 0,
        },
        13 => Command::SetMusicVolume {
            percent: (a % 101) as u8,
        },
        14 => Command::SetAspectRatio {
            aspect: [
                AspectRatio::Landscape16x9,
                AspectRatio::Portrait9x16,
                AspectRatio::Square1x1,
            ][(a % 3) as usize],
        },
        15 => Command::SetStyle {
            section: S,
            style: [StylePreset::Cut, StylePreset::Smooth, StylePreset::Action][(a % 3) as usize],
        },
        _ => Command::SetTitleCard {
            section: S,
            title: (b % 3 != 0).then(|| TitleCard {
                text: format!("Title {a}"),
                duration: Micros(500_000 + (c % 4_500_001) as i64),
            }),
        },
    })
}

/// A command guaranteed valid for `p`: the candidate if it applies, else
/// `SetSeed` (always valid).
fn build_command(p: &Project, raw: Raw) -> Command {
    candidate(p, raw)
        .filter(|cmd| p.clone().apply(cmd.clone()).is_ok())
        .unwrap_or(Command::SetSeed {
            section: S,
            seed: raw.1,
        })
}

fn start_project(raws: &[Raw]) -> Project {
    let mut p = Project::new(ProjectId(7));
    for &raw in raws {
        p.apply(build_command(&p, raw)).unwrap();
    }
    p
}

fn raws(max: usize) -> impl Strategy<Value = Vec<Raw>> {
    prop::collection::vec(any::<Raw>(), 0..max)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn undo_all_restores_original(setup in raws(12), ops in raws(40)) {
        let original = start_project(&setup);
        let mut h = History::new(original.clone());
        for &raw in &ops {
            let cmd = build_command(h.project(), raw);
            prop_assert!(h.apply(cmd).is_ok());
        }
        let edited = h.project().clone();
        for _ in 0..ops.len() {
            prop_assert!(h.undo().is_some());
        }
        prop_assert!(!h.can_undo());
        prop_assert_eq!(h.project(), &original);
        for _ in 0..ops.len() {
            prop_assert!(h.redo().is_some());
        }
        prop_assert_eq!(h.project(), &edited);
    }

    #[test]
    fn project_json_round_trips(setup in raws(40)) {
        let p = start_project(&setup);
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, p);
    }

    #[test]
    fn command_json_round_trips(setup in raws(20), raw in any::<Raw>()) {
        let p = start_project(&setup);
        let cmd = build_command(&p, raw);
        let json = serde_json::to_string(&cmd).unwrap();
        prop_assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }
}
