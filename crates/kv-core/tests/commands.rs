//! Unit tests for every `Command`: valid and invalid inputs.

use kv_core::*;

const S: SectionId = Project::DEFAULT_SECTION;

fn secs(s: i64) -> Micros {
    Micros(s * 1_000_000)
}

fn clip(id: u64, dur_secs: i64) -> MediaItem {
    MediaItem {
        id: MediaId(id),
        path: format!("clips/GX0{id}.MP4"),
        duration: secs(dur_secs),
        kind: MediaKind::Video,
        hilights: vec![secs(1)],
        clip_audio: false,
        chapters: Vec::new(),
    }
}

fn range(id: u64, start: i64, end: i64) -> RangeRef {
    RangeRef {
        media_id: MediaId(id),
        start: secs(start),
        end: secs(end),
    }
}

fn seg(id: u64, src_in: i64, src_out: i64, start: i64) -> Segment {
    Segment {
        media_id: MediaId(id),
        src_in: secs(src_in),
        src_out: secs(src_out),
        timeline_start: secs(start),
        transition: Transition::Cut,
        speed: Speed::NORMAL,
    }
}

/// A project with clips 1 (10 s) and 2 (20 s).
fn project() -> Project {
    let mut p = Project::new(ProjectId(1));
    p.apply(Command::AddMedia { item: clip(1, 10) }).unwrap();
    p.apply(Command::AddMedia { item: clip(2, 20) }).unwrap();
    p
}

/// Apply a command expected to fail and check the project is unchanged.
fn rejects(p: &Project, cmd: Command) -> CoreError {
    let mut q = p.clone();
    let err = q.apply(cmd).expect_err("command should be rejected");
    assert_eq!(&q, p, "rejected command must not change the project");
    err
}

fn section(p: &Project) -> &Section {
    p.section(S).unwrap()
}

#[test]
fn add_media() {
    let mut p = Project::new(ProjectId(1));
    assert_eq!(p.next_media_id(), MediaId(1));
    let ev = p.apply(Command::AddMedia { item: clip(1, 10) }).unwrap();
    assert_eq!(
        ev,
        Event::MediaAdded {
            media_id: MediaId(1)
        }
    );
    assert_eq!(p.media.len(), 1);
    assert_eq!(p.next_media_id(), MediaId(2));
}

#[test]
fn add_media_invalid() {
    let p = project();
    assert_eq!(
        rejects(&p, Command::AddMedia { item: clip(1, 5) }),
        CoreError::DuplicateMedia(MediaId(1))
    );
    let mut item = clip(3, 0);
    assert_eq!(
        rejects(&p, Command::AddMedia { item: item.clone() }),
        CoreError::InvalidDuration(Micros(0))
    );
    item.duration = secs(5);
    item.hilights = vec![secs(6)];
    assert!(matches!(
        rejects(&p, Command::AddMedia { item: item.clone() }),
        CoreError::HilightOutOfRange { .. }
    ));
    item.hilights.clear();
    for bad in ["", "/abs/clip.mp4", "C:\\clips\\a.mp4", "\\\\server\\a.mp4"] {
        item.path = bad.into();
        assert_eq!(
            rejects(&p, Command::AddMedia { item: item.clone() }),
            CoreError::InvalidPath(bad.into())
        );
    }
}

#[test]
fn remove_media_cascades() {
    let mut p = project();
    p.apply(Command::PinRange {
        section: S,
        range: range(1, 0, 2),
    })
    .unwrap();
    p.apply(Command::ExcludeRange {
        section: S,
        range: range(1, 5, 6),
    })
    .unwrap();
    p.apply(Command::PinRange {
        section: S,
        range: range(2, 0, 2),
    })
    .unwrap();
    p.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(1, 0, 2, 0), seg(2, 0, 3, 2), seg(1, 3, 4, 5)],
        },
    })
    .unwrap();

    let ev = p
        .apply(Command::RemoveMedia {
            media_id: MediaId(1),
        })
        .unwrap();
    assert_eq!(
        ev,
        Event::MediaRemoved {
            media_id: MediaId(1),
            pins_removed: 1,
            excludes_removed: 1,
            segments_removed: 2,
        }
    );
    assert!(p.media_item(MediaId(1)).is_none());
    let s = section(&p);
    assert_eq!(s.pins, vec![range(2, 0, 2)]);
    assert!(s.excludes.is_empty());
    // The remaining segment is repacked to the start of the timeline.
    assert_eq!(s.edit.segments, vec![seg(2, 0, 3, 0)]);
}

#[test]
fn remove_media_unknown() {
    assert_eq!(
        rejects(
            &project(),
            Command::RemoveMedia {
                media_id: MediaId(9)
            }
        ),
        CoreError::UnknownMedia(MediaId(9))
    );
}

#[test]
fn set_music() {
    let mut p = project();
    let music = MusicRef {
        path: "music/song.mp3".into(),
        duration: secs(180),
    };
    p.apply(Command::SetMusic {
        section: S,
        music: Some(music.clone()),
    })
    .unwrap();
    assert_eq!(section(&p).music, Some(music));
    p.apply(Command::SetMusic {
        section: S,
        music: None,
    })
    .unwrap();
    assert_eq!(section(&p).music, None);
}

#[test]
fn set_music_invalid() {
    let p = project();
    let music = MusicRef {
        path: "/abs/song.mp3".into(),
        duration: secs(1),
    };
    assert!(matches!(
        rejects(
            &p,
            Command::SetMusic {
                section: S,
                music: Some(music.clone())
            }
        ),
        CoreError::InvalidPath(_)
    ));
    let music = MusicRef {
        path: "song.mp3".into(),
        duration: Micros(-1),
    };
    assert_eq!(
        rejects(
            &p,
            Command::SetMusic {
                section: S,
                music: Some(music)
            }
        ),
        CoreError::InvalidDuration(Micros(-1))
    );
    assert_eq!(
        rejects(
            &p,
            Command::SetMusic {
                section: SectionId(7),
                music: None
            }
        ),
        CoreError::UnknownSection(SectionId(7))
    );
}

#[test]
fn set_target_length() {
    let mut p = project();
    let target = TargetLength::Fixed { length: secs(30) };
    p.apply(Command::SetTargetLength { section: S, target })
        .unwrap();
    assert_eq!(section(&p).target_length, target);
    assert_eq!(
        rejects(
            &p,
            Command::SetTargetLength {
                section: S,
                target: TargetLength::Fixed { length: Micros(0) }
            }
        ),
        CoreError::InvalidTargetLength(Micros(0))
    );
    assert_eq!(
        rejects(
            &p,
            Command::SetTargetLength {
                section: SectionId(2),
                target: TargetLength::FullSong
            }
        ),
        CoreError::UnknownSection(SectionId(2))
    );
}

#[test]
fn pin_and_unpin() {
    let mut p = project();
    let r = range(1, 2, 4);
    let ev = p
        .apply(Command::PinRange {
            section: S,
            range: r,
        })
        .unwrap();
    assert_eq!(
        ev,
        Event::RangePinned {
            section: S,
            range: r
        }
    );
    assert_eq!(section(&p).pins, vec![r]);
    p.apply(Command::UnpinRange {
        section: S,
        range: r,
    })
    .unwrap();
    assert!(section(&p).pins.is_empty());
}

#[test]
fn pin_invalid() {
    let mut p = project();
    let pin = |range| Command::PinRange { section: S, range };
    for r in [
        range(1, 4, 4),
        range(1, 5, 4),
        range(1, -1, 2),
        range(1, 9, 11),
    ] {
        assert!(matches!(
            rejects(&p, pin(r)),
            CoreError::InvalidRange { .. }
        ));
    }
    assert_eq!(
        rejects(&p, pin(range(9, 0, 1))),
        CoreError::UnknownMedia(MediaId(9))
    );
    p.apply(pin(range(1, 0, 2))).unwrap();
    assert_eq!(
        rejects(&p, pin(range(1, 0, 2))),
        CoreError::AlreadyPinned(range(1, 0, 2))
    );
    p.apply(Command::ExcludeRange {
        section: S,
        range: range(1, 5, 8),
    })
    .unwrap();
    assert_eq!(
        rejects(&p, pin(range(1, 7, 9))),
        CoreError::RangeConflict {
            range: range(1, 7, 9),
            existing: range(1, 5, 8)
        }
    );
    // Touching but not overlapping, or another clip: allowed.
    p.apply(pin(range(1, 8, 9))).unwrap();
    p.apply(pin(range(2, 5, 8))).unwrap();
    assert_eq!(
        rejects(
            &p,
            Command::UnpinRange {
                section: S,
                range: range(1, 0, 3)
            }
        ),
        CoreError::PinNotFound(range(1, 0, 3))
    );
}

#[test]
fn exclude_and_unexclude() {
    let mut p = project();
    let whole_clip = range(2, 0, 20);
    p.apply(Command::ExcludeRange {
        section: S,
        range: whole_clip,
    })
    .unwrap();
    assert_eq!(section(&p).excludes, vec![whole_clip]);
    let ev = p
        .apply(Command::UnexcludeRange {
            section: S,
            range: whole_clip,
        })
        .unwrap();
    assert_eq!(
        ev,
        Event::RangeUnexcluded {
            section: S,
            range: whole_clip
        }
    );
    assert!(section(&p).excludes.is_empty());
}

#[test]
fn exclude_invalid() {
    let mut p = project();
    let exclude = |range| Command::ExcludeRange { section: S, range };
    assert!(matches!(
        rejects(&p, exclude(range(1, 0, 11))),
        CoreError::InvalidRange { .. }
    ));
    p.apply(exclude(range(1, 0, 1))).unwrap();
    assert_eq!(
        rejects(&p, exclude(range(1, 0, 1))),
        CoreError::AlreadyExcluded(range(1, 0, 1))
    );
    p.apply(Command::PinRange {
        section: S,
        range: range(1, 3, 5),
    })
    .unwrap();
    assert!(matches!(
        rejects(&p, exclude(range(1, 4, 6))),
        CoreError::RangeConflict { .. }
    ));
    assert_eq!(
        rejects(
            &p,
            Command::UnexcludeRange {
                section: S,
                range: range(1, 0, 2)
            }
        ),
        CoreError::ExcludeNotFound(range(1, 0, 2))
    );
    assert_eq!(
        rejects(
            &p,
            Command::ExcludeRange {
                section: SectionId(3),
                range: range(1, 6, 7)
            }
        ),
        CoreError::UnknownSection(SectionId(3))
    );
}

#[test]
fn move_segment_reorders_and_repacks() {
    let mut p = project();
    let mut crossfade = seg(1, 5, 7, 4);
    crossfade.transition = Transition::Crossfade {
        duration: Micros(500_000),
    };
    let mut slow = seg(2, 0, 1, 6);
    slow.speed = Speed(50); // 1 s of source plays for 2 s.
    p.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(1, 0, 4, 0), crossfade.clone(), slow.clone()],
        },
    })
    .unwrap();

    let ev = p
        .apply(Command::MoveSegment {
            section: S,
            from: 0,
            to: 2,
        })
        .unwrap();
    assert_eq!(
        ev,
        Event::SegmentMoved {
            section: S,
            from: 0,
            to: 2
        }
    );
    let segs = &section(&p).edit.segments;
    // Order: crossfade (1.75 s), slow (2 s on the timeline), first (3.75 s).
    // The transition stays with the slot: slot 1 had the 0.5 s crossfade,
    // but the slow clip starts at its media start, so there is no footage
    // for the fade's first half and it becomes a cut. Each segment keeps
    // what it showed between its cut points (the crossfade halves go).
    assert_eq!(segs[0].src_in, Micros(5_250_000));
    assert_eq!(segs[0].src_out, secs(7));
    assert_eq!(segs[0].timeline_start, Micros(0));
    assert_eq!(segs[1].media_id, MediaId(2));
    assert_eq!(segs[1].transition, Transition::Cut);
    assert_eq!(segs[1].timeline_start, Micros(1_750_000));
    assert_eq!(segs[2].src_in, secs(0));
    assert_eq!(segs[2].src_out, Micros(3_750_000));
    assert_eq!(segs[2].transition, Transition::Cut);
    assert_eq!(segs[2].timeline_start, Micros(3_750_000));

    p.apply(Command::MoveSegment {
        section: S,
        from: 0,
        to: 1,
    })
    .unwrap();
    let segs = &section(&p).edit.segments;
    assert_eq!(segs[0].media_id, MediaId(2));
    assert_eq!(segs[1].timeline_start, secs(2));
}

/// A smooth-style edit: one beat per second, crossfades of 200, 250 and
/// 300 ms centred on the beats at 1, 2 and 3 s (`src` ranges carry half a
/// fade either side, as the auto-edit's style pass leaves them).
fn crossfaded_edl() -> Edl {
    let ms = |m: i64| Micros(m * 1_000);
    let fade = |m: i64| Transition::Crossfade { duration: ms(m) };
    let s = |id: u64, src_in: i64, src_out: i64, transition: Transition| Segment {
        media_id: MediaId(id),
        src_in: ms(src_in),
        src_out: ms(src_out),
        timeline_start: Micros(0),
        transition,
        speed: Speed::NORMAL,
    };
    let mut edl = Edl {
        segments: vec![
            s(1, 1_000, 2_100, Transition::Cut),
            s(2, 4_900, 6_125, fade(200)),
            s(1, 3_875, 5_150, fade(250)),
            s(2, 9_850, 11_000, fade(300)),
        ],
    };
    edl.repack();
    edl
}

/// Cut points (`timeline_start + overlap / 2`) after the first segment.
fn cut_points(segs: &[Segment]) -> Vec<i64> {
    segs[1..]
        .iter()
        .map(|s| s.timeline_start.0 + s.transition.overlap().0 / 2)
        .collect()
}

#[test]
fn move_segment_keeps_crossfaded_cuts_on_the_beat() {
    let mut p = project();
    let edl = crossfaded_edl();
    assert_eq!(cut_points(&edl.segments), [1_000_000, 2_000_000, 3_000_000]);
    let fades: Vec<Transition> = edl.segments.iter().map(|s| s.transition).collect();
    p.apply(Command::ReplaceEdl { section: S, edl }).unwrap();

    p.apply(Command::MoveSegment {
        section: S,
        from: 0,
        to: 2,
    })
    .unwrap();
    let segs = &section(&p).edit.segments;
    for (cut, beat) in cut_points(segs).into_iter().zip([1, 2, 3]) {
        assert!(
            (cut - secs(beat).0).abs() <= 1_000,
            "cut at {cut} µs is off the beat at {beat} s: {segs:?}"
        );
    }
    // Transitions stay with the slot.
    let now: Vec<Transition> = segs.iter().map(|s| s.transition).collect();
    assert_eq!(now, fades);
    // Order B, C, A, D; every segment shows the same beat of footage, with
    // handles for the crossfades of its new slot.
    let src: Vec<(u64, i64, i64)> = segs
        .iter()
        .map(|s| (s.media_id.0, s.src_in.0 / 1_000, s.src_out.0 / 1_000))
        .collect();
    assert_eq!(
        src,
        [
            (2, 5_000, 6_100),
            (1, 3_900, 5_125),
            (1, 875, 2_150),
            (2, 9_850, 11_000),
        ]
    );
    // The edit is as long as before.
    let last = segs.last().unwrap();
    assert_eq!(last.timeline_start.0 + last.timeline_len().0, 4_000_000);

    // Moving it back restores the original edit.
    p.apply(Command::MoveSegment {
        section: S,
        from: 2,
        to: 0,
    })
    .unwrap();
    assert_eq!(section(&p).edit, crossfaded_edl());
}

#[test]
fn move_segment_shortens_a_crossfade_without_footage() {
    let mut p = project();
    let mut edl = crossfaded_edl();
    // The first segment now starts at its media's start: in a crossfaded
    // slot it has no footage for the fade's first half.
    edl.segments[0].src_in = Micros(0);
    edl.segments[0].src_out = Micros(1_100_000);
    edl.repack();
    p.apply(Command::ReplaceEdl { section: S, edl }).unwrap();
    p.apply(Command::MoveSegment {
        section: S,
        from: 0,
        to: 2,
    })
    .unwrap();
    let segs = &section(&p).edit.segments;
    assert_eq!(segs[2].src_in, Micros(0));
    assert_eq!(segs[2].transition, Transition::Cut);
    assert_eq!(segs[1].src_out, Micros(5_000_000));
    assert_eq!(segs[3].transition.overlap(), Micros(300_000));
    assert_eq!(cut_points(segs), [1_000_000, 2_000_000, 3_000_000]);
}

#[test]
fn move_segment_invalid() {
    let mut p = project();
    p.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(1, 0, 1, 0)],
        },
    })
    .unwrap();
    assert_eq!(
        rejects(
            &p,
            Command::MoveSegment {
                section: S,
                from: 0,
                to: 1
            }
        ),
        CoreError::SegmentIndexOutOfRange { index: 1, len: 1 }
    );
    assert_eq!(
        rejects(
            &p,
            Command::MoveSegment {
                section: S,
                from: 3,
                to: 0
            }
        ),
        CoreError::SegmentIndexOutOfRange { index: 3, len: 1 }
    );
}

#[test]
fn set_seed() {
    let mut p = project();
    p.apply(Command::SetSeed {
        section: S,
        seed: 42,
    })
    .unwrap();
    assert_eq!(section(&p).seed, 42);
    assert_eq!(
        rejects(
            &p,
            Command::SetSeed {
                section: SectionId(5),
                seed: 1
            }
        ),
        CoreError::UnknownSection(SectionId(5))
    );
}

#[test]
fn replace_edl() {
    let mut p = project();
    let edl = Edl {
        segments: vec![seg(1, 0, 2, 0), seg(2, 4, 6, 2)],
    };
    let ev = p
        .apply(Command::ReplaceEdl {
            section: S,
            edl: edl.clone(),
        })
        .unwrap();
    assert_eq!(
        ev,
        Event::EdlReplaced {
            section: S,
            segments: 2
        }
    );
    assert_eq!(section(&p).edit, edl);
}

#[test]
fn replace_edl_invalid() {
    let p = project();
    let replace = |segments| Command::ReplaceEdl {
        section: S,
        edl: Edl { segments },
    };
    let invalid = |segments| {
        assert!(matches!(
            rejects(&p, replace(segments)),
            CoreError::InvalidSegment { .. }
        ))
    };
    invalid(vec![seg(1, 3, 3, 0)]); // empty source range
    invalid(vec![seg(1, 0, 11, 0)]); // past the media end
    invalid(vec![seg(1, 0, 1, -1)]); // negative timeline start
    invalid(vec![seg(1, 0, 1, 5), seg(2, 0, 1, 4)]); // out of order
    let mut zero_speed = seg(1, 0, 1, 0);
    zero_speed.speed = Speed(0);
    invalid(vec![zero_speed]);
    let mut long_fade = seg(1, 0, 1, 0);
    long_fade.transition = Transition::Crossfade { duration: secs(2) };
    invalid(vec![long_fade]);
    assert_eq!(
        rejects(&p, replace(vec![seg(9, 0, 1, 0)])),
        CoreError::UnknownMedia(MediaId(9))
    );
    assert_eq!(
        rejects(
            &p,
            Command::ReplaceEdl {
                section: SectionId(4),
                edl: Edl::default()
            }
        ),
        CoreError::UnknownSection(SectionId(4))
    );
}

#[test]
fn history_undo_redo() {
    let mut h = History::new(project());
    let original = h.project().clone();
    assert!(!h.can_undo());
    assert!(h.undo().is_none());
    let cmd = Command::SetSeed {
        section: S,
        seed: 3,
    };
    h.apply(cmd.clone()).unwrap();
    let after = h.project().clone();
    assert!(h
        .apply(Command::RemoveMedia {
            media_id: MediaId(9)
        })
        .is_err());
    assert_eq!(h.project(), &after, "failed command is not recorded");
    assert_eq!(h.undo(), Some(&cmd));
    assert_eq!(h.project(), &original);
    assert!(h.can_redo());
    assert_eq!(h.redo(), Some(&cmd));
    assert_eq!(h.project(), &after);
    assert!(h.redo().is_none());

    // A new command clears the redo stack.
    h.undo();
    h.apply(Command::SetSeed {
        section: S,
        seed: 4,
    })
    .unwrap();
    assert!(!h.can_redo());
    assert_eq!(h.into_project().section(S).unwrap().seed, 4);
}

#[test]
fn command_json_is_tagged() {
    let cmd = Command::SetSeed {
        section: S,
        seed: 7,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert_eq!(json, r#"{"type":"set_seed","section":1,"seed":7}"#);
    assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
}

#[test]
fn project_json_has_version_1() {
    let p = project();
    let value: serde_json::Value = serde_json::to_value(&p).unwrap();
    assert_eq!(value["version"], 1);
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.starts_with(r#"{"version":1,"#), "{json}");
    assert_eq!(serde_json::from_str::<Project>(&json).unwrap(), p);

    let bumped = json.replacen(r#""version":1"#, r#""version":2"#, 1);
    let err = serde_json::from_str::<Project>(&bumped).unwrap_err();
    assert!(err.to_string().contains("unsupported project version 2"));
    let missing = json.replacen(r#""version":1,"#, "", 1);
    assert!(serde_json::from_str::<Project>(&missing).is_err());
}

fn ms(v: i64) -> Micros {
    Micros(v * 1_000)
}

/// Project with an EDL: clip 1 0..4 s, clip 2 2..6 s at 50% speed (8 s on the
/// timeline), clip 1 6..9 s.
fn project_with_edl() -> Project {
    let mut p = project();
    let mut slow = seg(2, 2, 6, 4);
    slow.speed = Speed(50);
    p.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(1, 0, 4, 0), slow, seg(1, 6, 9, 12)],
        },
    })
    .unwrap();
    p
}

fn move_cut(cut: usize, to: Micros) -> Command {
    Command::MoveCut {
        section: S,
        cut,
        to,
    }
}

#[test]
fn move_cut_later_trims_next_and_extends_previous() {
    let mut p = project_with_edl();
    let ev = p.apply(move_cut(1, ms(4_500))).unwrap();
    assert_eq!(
        ev,
        Event::CutMoved {
            section: S,
            cut: 1,
            at: ms(4_500)
        }
    );
    let segs = &section(&p).edit.segments;
    assert_eq!((segs[0].src_in, segs[0].src_out), (secs(0), ms(4_500)));
    // 0.5 s of timeline at 50% speed is 0.25 s of source.
    assert_eq!((segs[1].src_in, segs[1].src_out), (ms(2_250), secs(6)));
    assert_eq!(segs[1].timeline_start, ms(4_500));
    // Repacked: segment 1 is now 7.5 s long, so the total length is unchanged.
    assert_eq!(segs[2].timeline_start, secs(12));
}

#[test]
fn move_cut_earlier_scales_by_speed() {
    let mut p = project_with_edl();
    // Cut 2 sits at 12 s, between the slow segment and clip 1 6..9 s.
    p.apply(move_cut(2, secs(10))).unwrap();
    let segs = &section(&p).edit.segments;
    // Slow segment loses 2 s of timeline = 1 s of source.
    assert_eq!(segs[1].src_out, secs(5));
    // Next segment gains 2 s of source at normal speed.
    assert_eq!(segs[2].src_in, secs(4));
    assert_eq!(segs[2].timeline_start, secs(10));
    assert_eq!(segs[2].timeline_len(), secs(5));
}

#[test]
fn move_cut_with_crossfade() {
    let mut p = project();
    let mut b = seg(2, 2, 6, 3);
    b.transition = Transition::Crossfade { duration: secs(1) };
    p.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(1, 0, 4, 0), b],
        },
    })
    .unwrap();
    let ev = p.apply(move_cut(1, ms(3_500))).unwrap();
    assert_eq!(
        ev,
        Event::CutMoved {
            section: S,
            cut: 1,
            at: ms(3_500)
        }
    );
    let segs = &section(&p).edit.segments;
    assert_eq!(segs[0].src_out, ms(4_500));
    assert_eq!(segs[1].src_in, ms(2_500));
    // A cut that would leave the crossfade longer than its segment is
    // rejected by the EDL check.
    assert!(matches!(
        rejects(&p, move_cut(1, ms(6_200))),
        CoreError::InvalidSegment { index: 1, .. }
    ));
}

#[test]
fn move_cut_invalid() {
    let p = project_with_edl();
    for cut in [0, 3, 9] {
        assert_eq!(
            rejects(&p, move_cut(cut, secs(1))),
            CoreError::CutIndexOutOfRange { cut, len: 3 }
        );
    }
    assert_eq!(
        rejects(
            &p,
            Command::MoveCut {
                section: SectionId(8),
                cut: 1,
                to: secs(1)
            }
        ),
        CoreError::UnknownSection(SectionId(8))
    );
    // Previous segment would be 99 ms long.
    assert_eq!(
        rejects(&p, move_cut(1, ms(99))),
        CoreError::CutTooShort {
            cut: 1,
            to: ms(99),
            index: 0,
            min: Segment::MIN_LEN
        }
    );
    // Exactly 100 ms is allowed.
    p.clone().apply(move_cut(1, ms(100))).unwrap();
    // Cut 2 (at 12 s) moved to 14.95 s leaves clip 1 6..9 s with 50 ms.
    assert_eq!(
        rejects(&p, move_cut(2, ms(14_950))),
        CoreError::CutTooShort {
            cut: 2,
            to: ms(14_950),
            index: 2,
            min: Segment::MIN_LEN
        }
    );
    // Earlier than the start of the timeline, previous segment vanishes.
    assert!(matches!(
        rejects(&p, move_cut(1, secs(-1))),
        CoreError::CutTooShort { index: 0, .. }
    ));
    // Clip 1 is 10 s long: extending 0..4 s by 7 s runs past its end.
    // (Next segment is long enough: 8 s of timeline minus 7 s.)
    assert_eq!(
        rejects(&p, move_cut(1, secs(11))),
        CoreError::CutOutsideMedia {
            cut: 1,
            to: secs(11),
            index: 0
        }
    );
    // Moving a cut 2 s earlier pulls the next segment's src_in (1 s) before
    // the start of its media.
    let mut q = project();
    q.apply(Command::ReplaceEdl {
        section: S,
        edl: Edl {
            segments: vec![seg(2, 0, 10, 0), seg(1, 1, 3, 10)],
        },
    })
    .unwrap();
    assert_eq!(
        rejects(&q, move_cut(1, secs(8))),
        CoreError::CutOutsideMedia {
            cut: 1,
            to: secs(8),
            index: 1
        }
    );
    let err = CoreError::CutTooShort {
        cut: 1,
        to: ms(99),
        index: 0,
        min: Segment::MIN_LEN,
    };
    assert!(err.to_string().contains("shorter than the minimum"));
}

#[test]
fn move_cut_undo() {
    let mut h = History::new(project_with_edl());
    let original = h.project().clone();
    h.apply(move_cut(1, ms(4_500))).unwrap();
    assert_ne!(h.project(), &original);
    h.undo();
    assert_eq!(h.project(), &original);
}

#[test]
fn set_clip_audio() {
    let mut p = project();
    assert!(!p.media_item(MediaId(1)).unwrap().clip_audio);
    let cmd = Command::SetClipAudio {
        media_id: MediaId(1),
        enabled: true,
    };
    let mut h = History::new(p.clone());
    assert_eq!(
        h.apply(cmd.clone()).unwrap(),
        Event::ClipAudioSet {
            media_id: MediaId(1),
            enabled: true
        }
    );
    assert!(h.project().media_item(MediaId(1)).unwrap().clip_audio);
    assert!(!h.project().media_item(MediaId(2)).unwrap().clip_audio);
    h.undo();
    assert_eq!(h.project(), &p);
    p.apply(cmd).unwrap();
    assert_eq!(
        rejects(
            &p,
            Command::SetClipAudio {
                media_id: MediaId(9),
                enabled: false
            }
        ),
        CoreError::UnknownMedia(MediaId(9))
    );
}

#[test]
fn set_music_volume() {
    let p = project();
    let mut h = History::new(p.clone());
    for percent in [0, 37, 100] {
        assert_eq!(
            h.apply(Command::SetMusicVolume { percent }).unwrap(),
            Event::MusicVolumeSet { percent }
        );
        assert_eq!(h.project().settings.music_volume_percent, percent);
    }
    for percent in [101, 255] {
        assert_eq!(
            rejects(h.project(), Command::SetMusicVolume { percent }),
            CoreError::InvalidMusicVolume(percent)
        );
    }
    while h.undo().is_some() {}
    assert_eq!(h.project(), &p);
    assert_eq!(p.settings.music_volume_percent, 100);
}

#[test]
fn set_aspect_ratio() {
    let p = project();
    let mut h = History::new(p.clone());
    let aspect = AspectRatio::Portrait9x16;
    assert_eq!(
        h.apply(Command::SetAspectRatio { aspect }).unwrap(),
        Event::AspectRatioSet { aspect }
    );
    assert_eq!(h.project().settings.aspect_ratio, aspect);
    h.undo();
    assert_eq!(h.project(), &p);
    let json = serde_json::to_string(&Command::SetAspectRatio {
        aspect: AspectRatio::Square1x1,
    })
    .unwrap();
    assert_eq!(json, r#"{"type":"set_aspect_ratio","aspect":"square1x1"}"#);
}

#[test]
fn set_style() {
    let p = project();
    assert_eq!(section(&p).style, StylePreset::Cut);
    let mut h = History::new(p.clone());
    for style in [StylePreset::Smooth, StylePreset::Action, StylePreset::Cut] {
        assert_eq!(
            h.apply(Command::SetStyle { section: S, style }).unwrap(),
            Event::StyleSet { section: S, style }
        );
        assert_eq!(section(h.project()).style, style);
    }
    while h.undo().is_some() {}
    assert_eq!(h.project(), &p);
    assert_eq!(
        rejects(
            &p,
            Command::SetStyle {
                section: SectionId(2),
                style: StylePreset::Smooth
            }
        ),
        CoreError::UnknownSection(SectionId(2))
    );
    let json = serde_json::to_string(&Command::SetStyle {
        section: S,
        style: StylePreset::Action,
    })
    .unwrap();
    assert_eq!(json, r#"{"type":"set_style","section":1,"style":"action"}"#);
}

fn title(text: &str, duration: Micros) -> Option<TitleCard> {
    Some(TitleCard {
        text: text.into(),
        duration,
    })
}

#[test]
fn set_title_card() {
    let p = project();
    assert_eq!(section(&p).title, None);
    let mut h = History::new(p.clone());
    let t = title("Summer in Lisbon", secs(2));
    assert_eq!(
        h.apply(Command::SetTitleCard {
            section: S,
            title: t.clone()
        })
        .unwrap(),
        Event::TitleCardSet {
            section: S,
            present: true
        }
    );
    assert_eq!(section(h.project()).title, t);
    let edge = [
        title("a", TitleCard::MIN_DURATION),
        title(&"é".repeat(80), TitleCard::MAX_DURATION),
    ];
    for t in edge {
        h.apply(Command::SetTitleCard {
            section: S,
            title: t,
        })
        .unwrap();
    }
    assert_eq!(
        h.apply(Command::SetTitleCard {
            section: S,
            title: None
        })
        .unwrap(),
        Event::TitleCardSet {
            section: S,
            present: false
        }
    );
    assert_eq!(section(h.project()).title, None);
    while h.undo().is_some() {}
    assert_eq!(h.project(), &p);
}

#[test]
fn set_title_card_invalid() {
    let p = project();
    let set = |title| Command::SetTitleCard { section: S, title };
    for (text, chars) in [("", 0), ("   ", 3), (&*"x".repeat(81), 81)] {
        assert_eq!(
            rejects(&p, set(title(text, secs(2)))),
            CoreError::InvalidTitleText { chars, max: 80 }
        );
    }
    for duration in [ms(499), ms(5_001), Micros(0), secs(-1)] {
        assert_eq!(
            rejects(&p, set(title("Hi", duration))),
            CoreError::InvalidTitleDuration {
                duration,
                min: TitleCard::MIN_DURATION,
                max: TitleCard::MAX_DURATION
            }
        );
    }
    assert_eq!(
        rejects(
            &p,
            Command::SetTitleCard {
                section: SectionId(3),
                title: None
            }
        ),
        CoreError::UnknownSection(SectionId(3))
    );
}

/// A project file written before clip audio, styles and title cards existed.
const PRE_M2_PROJECT: &str = r#"{
  "version": 1,
  "id": 3,
  "media": [
    {"id": 1, "path": "clips/GX010001.MP4", "duration": 10000000,
     "kind": "video", "hilights": [1000000]}
  ],
  "sections": [
    {"id": 1, "music": {"path": "music/song.mp3", "duration": 60000000},
     "target_length": {"type": "fixed", "length": 30000000},
     "pins": [], "excludes": [], "seed": 5,
     "edit": {"segments": [
       {"media_id": 1, "src_in": 0, "src_out": 2000000, "timeline_start": 0,
        "transition": {"type": "cut"}, "speed": 100}
     ]}}
  ],
  "settings": {"aspect_ratio": "portrait9x16", "music_volume_percent": 80}
}"#;

#[test]
fn pre_m2_project_file_loads_with_defaults() {
    let p: Project = serde_json::from_str(PRE_M2_PROJECT).unwrap();
    assert!(!p.media[0].clip_audio);
    let s = p.section(S).unwrap();
    assert_eq!(s.style, StylePreset::Cut);
    assert_eq!(s.title, None);
    assert_eq!(s.seed, 5);
    assert_eq!(p.settings.aspect_ratio, AspectRatio::Portrait9x16);
    assert_eq!(p.settings.music_volume_percent, 80);
    // Saving again still writes version 1, now with the new fields.
    let value = serde_json::to_value(&p).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["media"][0]["clip_audio"], false);
    assert_eq!(value["sections"][0]["style"], "cut");
    assert!(value["sections"][0]["title"].is_null());
}

#[test]
fn new_fields_round_trip_through_json() {
    let mut p = project_with_edl();
    for cmd in [
        Command::SetClipAudio {
            media_id: MediaId(2),
            enabled: true,
        },
        Command::SetStyle {
            section: S,
            style: StylePreset::Smooth,
        },
        Command::SetTitleCard {
            section: S,
            title: title("Ride", secs(3)),
        },
        Command::SetMusicVolume { percent: 40 },
        Command::SetAspectRatio {
            aspect: AspectRatio::Square1x1,
        },
        move_cut(1, ms(4_500)),
    ] {
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
        let ev = p.apply(cmd).unwrap();
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Project>(&json).unwrap(), p);
    assert_eq!(
        serde_json::to_string(&move_cut(2, Micros(7))).unwrap(),
        r#"{"type":"move_cut","section":1,"cut":2,"to":7}"#
    );
}

#[test]
fn history_try_map_paths_rewrites_every_state_without_new_steps() {
    let music = MusicRef {
        path: "music/song.mp3".into(),
        duration: secs(30),
    };
    let mut h = History::new(Project::new(ProjectId(1)));
    h.apply(Command::AddMedia { item: clip(1, 10) }).unwrap();
    h.apply(Command::SetMusic {
        section: S,
        music: Some(music),
    })
    .unwrap();
    h.apply(Command::AddMedia { item: clip(2, 10) }).unwrap();
    h.undo().unwrap();

    h.try_map_paths(|p| Ok::<_, ()>(format!("../{p}"))).unwrap();
    assert_eq!(h.project().media[0].path, "../clips/GX01.MP4");
    assert_eq!(
        h.project().section(S).unwrap().music.as_ref().unwrap().path,
        "../music/song.mp3"
    );
    // Redo restores a rewritten state and returns the rewritten command.
    match h.redo().unwrap() {
        Command::AddMedia { item } => assert_eq!(item.path, "../clips/GX02.MP4"),
        c => panic!("unexpected {c:?}"),
    }
    assert_eq!(h.project().media[1].path, "../clips/GX02.MP4");
    // Undo the music: the earlier state was rewritten too, and no step was added.
    h.undo().unwrap();
    h.undo().unwrap();
    assert_eq!(h.project().media[0].path, "../clips/GX01.MP4");
    assert!(h.project().section(S).unwrap().music.is_none());
    h.undo().unwrap();
    assert!(!h.can_undo());

    // Errors propagate.
    assert_eq!(h.try_map_paths(|_| Err("no")), Err("no"));
}

fn chapter(path: &str, dur_secs: i64) -> ChapterRef {
    ChapterRef {
        path: path.into(),
        duration: secs(dur_secs),
    }
}

/// A two-chapter recording: 10 s + 4 s.
fn chaptered(id: u64) -> MediaItem {
    MediaItem {
        path: "clips/GX010042.MP4".into(),
        duration: secs(14),
        hilights: vec![secs(12)],
        chapters: vec![
            chapter("clips/GX010042.MP4", 10),
            chapter("clips/GX020042.MP4", 4),
        ],
        ..clip(id, 14)
    }
}

#[test]
fn add_chaptered_media() {
    let mut p = Project::new(ProjectId(1));
    let ev = p.apply(Command::AddMedia { item: chaptered(1) }).unwrap();
    assert_eq!(
        ev,
        Event::MediaAdded {
            media_id: MediaId(1)
        }
    );
    let item = p.media_item(MediaId(1)).unwrap();
    assert_eq!(item.chapters.len(), 2);
    // Ranges span the chapter boundary like any other time in the item.
    p.apply(Command::PinRange {
        section: S,
        range: range(1, 9, 11),
    })
    .unwrap();
}

#[test]
fn add_chaptered_media_invalid() {
    let p = Project::new(ProjectId(1));
    let cases: Vec<(MediaItem, CoreError)> = vec![
        (
            MediaItem {
                path: "clips/GX020042.MP4".into(),
                ..chaptered(1)
            },
            CoreError::ChapterPathMismatch {
                path: "clips/GX020042.MP4".into(),
                first: "clips/GX010042.MP4".into(),
            },
        ),
        (
            MediaItem {
                duration: secs(13),
                hilights: Vec::new(),
                ..chaptered(1)
            },
            CoreError::ChapterDurationMismatch {
                duration: secs(13),
                sum: secs(14),
            },
        ),
        (
            MediaItem {
                chapters: vec![
                    chapter("clips/GX010042.MP4", 10),
                    chapter("/card/GX020042.MP4", 4),
                ],
                ..chaptered(1)
            },
            CoreError::InvalidPath("/card/GX020042.MP4".into()),
        ),
        (
            MediaItem {
                chapters: vec![
                    chapter("clips/GX010042.MP4", 14),
                    chapter("clips/GX020042.MP4", 0),
                ],
                ..chaptered(1)
            },
            CoreError::InvalidDuration(Micros(0)),
        ),
        (
            MediaItem {
                chapters: vec![
                    chapter("clips/GX010042.MP4", 16),
                    chapter("clips/GX020042.MP4", -2),
                ],
                ..chaptered(1)
            },
            CoreError::InvalidDuration(secs(-2)),
        ),
    ];
    for (item, want) in cases {
        assert_eq!(rejects(&p, Command::AddMedia { item }), want);
    }
}

#[test]
fn chaptered_media_round_trips_and_single_files_serialise_as_before() {
    let mut p = Project::new(ProjectId(1));
    p.apply(Command::AddMedia { item: chaptered(1) }).unwrap();
    p.apply(Command::AddMedia { item: clip(2, 5) }).unwrap();
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Project>(&json).unwrap(), p);

    let value = serde_json::to_value(&p).unwrap();
    assert_eq!(
        value["media"][0]["chapters"],
        serde_json::json!([
            {"path": "clips/GX010042.MP4", "duration": 10_000_000},
            {"path": "clips/GX020042.MP4", "duration": 4_000_000},
        ])
    );
    // A single-file item has no `chapters` key at all.
    assert!(value["media"][1]
        .as_object()
        .unwrap()
        .get("chapters")
        .is_none());
}

#[test]
fn project_without_chapters_field_loads() {
    let p: Project = serde_json::from_str(PRE_M2_PROJECT).unwrap();
    assert!(p.media.iter().all(|m| m.chapters.is_empty()));
}

#[test]
fn try_map_paths_rewrites_chapter_paths() {
    let mut p = Project::new(ProjectId(1));
    p.apply(Command::AddMedia { item: chaptered(1) }).unwrap();
    p.try_map_paths(|s| Ok::<_, ()>(format!("../{s}"))).unwrap();
    let item = p.media_item(MediaId(1)).unwrap();
    assert_eq!(item.path, "../clips/GX010042.MP4");
    assert_eq!(item.chapters[0].path, "../clips/GX010042.MP4");
    assert_eq!(item.chapters[1].path, "../clips/GX020042.MP4");
}
