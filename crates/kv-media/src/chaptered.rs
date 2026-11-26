//! [`ChapteredReader`]: one [`VideoReader`] over the chapter files of a
//! chaptered recording (a GoPro recording the camera split into
//! `GX01xxxx.MP4`, `GX02xxxx.MP4`, ...), so preview and export see one clip
//! whose time runs continuously across the files.
//!
//! Item time `t` maps to chapter `i` with `start(i) <= t < start(i + 1)`,
//! where `start(i)` is the sum of the durations of the chapters before `i`,
//! and to local time `t - start(i)` in that file. Frames come out with
//! `pts` in item time (local pts plus `start(i)`). A frame of a chapter
//! that is not the last one whose local pts is at or past the chapter's
//! duration is dropped: from there on the next chapter is displayed, so a
//! slightly long file never yields a duplicate or out-of-order frame.

use std::path::{Path, PathBuf};

use kv_core::{ChapterRef, Micros};

use crate::{DecodeOptions, MediaBackend, MediaError, Result, VideoFrame, VideoInfo, VideoReader};

/// How many chapter files a [`ChapteredReader`] keeps open at once: the
/// current one and the one used before it, so scrubbing back and forth over
/// a chapter boundary does not reopen files.
const MAX_OPEN_CHAPTERS: usize = 2;

/// One chapter file, with its path resolved for the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub path: PathBuf,
    /// Duration of the file: item time advances by this much across it.
    pub duration: Micros,
}

impl Chapter {
    /// The chapters of a [`kv_core::MediaItem`], with each project-relative
    /// path turned into a file path by `resolve`.
    pub fn from_refs(refs: &[ChapterRef], resolve: impl Fn(&str) -> PathBuf) -> Vec<Chapter> {
        refs.iter()
            .map(|c| Chapter {
                path: resolve(&c.path),
                duration: c.duration,
            })
            .collect()
    }
}

/// Opens the reader of one chapter file.
type Opener<'a> = Box<dyn FnMut(&Path) -> Result<Box<dyn VideoReader + 'a>> + Send + 'a>;

/// A [`VideoReader`] over the chapter files of one recording; see the
/// module docs for the time mapping. Chapter files are opened on first use
/// and at most two stay open.
pub struct ChapteredReader<'a> {
    chapters: Vec<Chapter>,
    /// Item time at which each chapter starts.
    starts: Vec<Micros>,
    open: Opener<'a>,
    /// Open readers by chapter index, most recently used last.
    readers: Vec<(usize, Box<dyn VideoReader + 'a>)>,
    /// Chapter that [`VideoReader::next_frame`] reads from.
    cur: usize,
    info: VideoInfo,
}

impl std::fmt::Debug for ChapteredReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChapteredReader")
            .field("chapters", &self.chapters)
            .field("cur", &self.cur)
            .field(
                "open",
                &self.readers.iter().map(|r| r.0).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl<'a> ChapteredReader<'a> {
    /// A reader over `chapters`, opening each file with `backend` and
    /// `opts`. Opens the first chapter now (its [`VideoInfo`] is the
    /// reader's); the others when first needed.
    pub fn open(
        backend: &'a dyn MediaBackend,
        chapters: Vec<Chapter>,
        opts: &DecodeOptions,
    ) -> Result<Self> {
        let opts = *opts;
        Self::new(chapters, move |path| backend.open_video(path, &opts))
    }

    /// A reader over `chapters`, opening each file with `open`; for callers
    /// that own their backend (e.g. through an `Arc`). Fails with
    /// [`MediaError::InvalidChapters`] when `chapters` is empty or a
    /// duration is not positive, or with `open`'s error for the first file.
    pub fn new(
        chapters: Vec<Chapter>,
        open: impl FnMut(&Path) -> Result<Box<dyn VideoReader + 'a>> + Send + 'a,
    ) -> Result<Self> {
        if chapters.is_empty() {
            return Err(MediaError::InvalidChapters("no chapters".into()));
        }
        if let Some(c) = chapters.iter().find(|c| c.duration.0 <= 0) {
            return Err(MediaError::InvalidChapters(format!(
                "chapter {:?} has duration {} us",
                c.path, c.duration.0
            )));
        }
        let mut starts = Vec::with_capacity(chapters.len());
        let mut acc = 0i64;
        for c in &chapters {
            starts.push(Micros(acc));
            acc = acc
                .checked_add(c.duration.0)
                .ok_or_else(|| MediaError::InvalidChapters("total duration overflows".into()))?;
        }
        let mut open: Opener<'a> = Box::new(open);
        let first = open(&chapters[0].path)?;
        let info = first.info().clone();
        Ok(ChapteredReader {
            chapters,
            starts,
            open,
            readers: vec![(0, first)],
            cur: 0,
            info,
        })
    }

    /// The chapter files, in order.
    pub fn chapters(&self) -> &[Chapter] {
        &self.chapters
    }

    /// Index of the chapter displayed at item time `t` (the first one
    /// before it, the last one past the end).
    pub fn chapter_at(&self, t: Micros) -> usize {
        self.starts.partition_point(|s| *s <= t).saturating_sub(1)
    }

    /// The reader of chapter `i`, opening it if needed, and whether it was
    /// just opened (and so is at its start).
    fn reader(&mut self, i: usize) -> Result<(&mut (dyn VideoReader + 'a), bool)> {
        let fresh = match self.readers.iter().position(|(c, _)| *c == i) {
            Some(k) => {
                let entry = self.readers.remove(k);
                self.readers.push(entry);
                false
            }
            None => {
                if self.readers.len() >= MAX_OPEN_CHAPTERS {
                    self.readers.remove(0);
                }
                let r = (self.open)(&self.chapters[i].path)?;
                self.readers.push((i, r));
                true
            }
        };
        let r = &mut self.readers.last_mut().expect("just pushed").1;
        Ok((r.as_mut(), fresh))
    }

    fn shift(&self, i: usize, mut f: VideoFrame) -> VideoFrame {
        f.pts = Micros(f.pts.0 + self.starts[i].0);
        f
    }

    /// The frame at item time `t` read with `get(reader, local time)`;
    /// when chapter `i` has nothing displayed at its local time (its first
    /// frame starts late), the last frame of an earlier chapter.
    fn frame_with(
        &mut self,
        t: Micros,
        mut get: impl FnMut(&mut (dyn VideoReader + 'a), Micros) -> Result<Option<VideoFrame>>,
    ) -> Result<Option<VideoFrame>> {
        let i = self.chapter_at(t);
        self.cur = i;
        let local = Micros(t.0 - self.starts[i].0);
        if let Some(f) = get(self.reader(i)?.0, local)? {
            return Ok(Some(self.shift(i, f)));
        }
        for j in (0..i).rev() {
            self.cur = j;
            // The last frame before the chapter's end.
            let end = Micros(self.chapters[j].duration.0 - 1);
            if let Some(f) = self.reader(j)?.0.frame_at(end)? {
                return Ok(Some(self.shift(j, f)));
            }
        }
        Ok(None)
    }
}

impl VideoReader for ChapteredReader<'_> {
    /// The first chapter's stream info.
    fn info(&self) -> &VideoInfo {
        &self.info
    }

    fn seek(&mut self, t: Micros) -> Result<()> {
        let i = self.chapter_at(t);
        self.cur = i;
        let local = Micros(t.0 - self.starts[i].0);
        self.reader(i)?.0.seek(local)
    }

    fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
        loop {
            let i = self.cur;
            let last = i + 1 == self.chapters.len();
            let duration = self.chapters[i].duration;
            match self.reader(i)?.0.next_frame()? {
                Some(f) if last || f.pts < duration => return Ok(Some(self.shift(i, f))),
                _ if last => return Ok(None),
                // End of this file (or a frame past its duration): go on
                // with the next one from its start.
                _ => {
                    self.cur = i + 1;
                    let (r, fresh) = self.reader(i + 1)?;
                    if !fresh {
                        r.seek(Micros(0))?;
                    }
                }
            }
        }
    }

    fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>> {
        self.frame_with(t, |r, local| r.frame_at(local))
    }

    fn frame_near(&mut self, t: Micros, tolerance: Micros) -> Result<Option<VideoFrame>> {
        self.frame_with(t, |r, local| r.frame_near(local, tolerance))
    }
}

/// Open the video of a media item: a plain reader for `path` when
/// `chapters` is empty (a normal single-file item), otherwise a
/// [`ChapteredReader`] over `chapters` (`path` is then the first chapter's).
pub fn open_item_video<'a>(
    backend: &'a dyn MediaBackend,
    path: &Path,
    chapters: &[Chapter],
    opts: &DecodeOptions,
) -> Result<Box<dyn VideoReader + 'a>> {
    if chapters.is_empty() {
        backend.open_video(path, opts)
    } else {
        Ok(Box::new(ChapteredReader::open(
            backend,
            chapters.to_vec(),
            opts,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{MediaInfo, Rational};

    /// A reader over frames at known local pts; frame `k` of chapter `c` is
    /// a 1x1 pixel `[c, k, 0, 255]`.
    struct FakeReader {
        chapter: u8,
        pts: Vec<i64>,
        /// Index of the frame `next_frame` returns.
        pos: usize,
        info: VideoInfo,
    }

    fn info() -> VideoInfo {
        VideoInfo {
            codec: "fake".into(),
            width: 1,
            height: 1,
            fps: Rational::new(10, 1),
            bit_depth: 8,
            rotation: 0,
        }
    }

    impl FakeReader {
        fn frame(&self, k: usize) -> VideoFrame {
            VideoFrame::solid(Micros(self.pts[k]), 1, 1, [self.chapter, k as u8, 0, 255])
        }
        /// Index of the frame displayed at `t`.
        fn index(&self, t: i64) -> Option<usize> {
            self.pts.iter().rposition(|&p| p <= t)
        }
    }

    impl VideoReader for FakeReader {
        fn info(&self) -> &VideoInfo {
            &self.info
        }
        fn seek(&mut self, t: Micros) -> Result<()> {
            self.pos = self.index(t.0).unwrap_or(0);
            Ok(())
        }
        fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
            if self.pos >= self.pts.len() {
                return Ok(None);
            }
            self.pos += 1;
            Ok(Some(self.frame(self.pos - 1)))
        }
        fn frame_at(&mut self, t: Micros) -> Result<Option<VideoFrame>> {
            Ok(self.index(t.0).map(|k| {
                self.pos = k + 1;
                self.frame(k)
            }))
        }
    }

    /// Backend whose file `"c<n>"` is chapter `n` with the given pts.
    #[derive(Default)]
    struct FakeMedia {
        files: Vec<Vec<i64>>,
        opens: Mutex<Vec<PathBuf>>,
    }

    impl MediaBackend for FakeMedia {
        fn probe(&self, _: &Path) -> Result<MediaInfo> {
            unimplemented!()
        }
        fn open_video(&self, path: &Path, _: &DecodeOptions) -> Result<Box<dyn VideoReader>> {
            self.opens.lock().unwrap().push(path.to_path_buf());
            let n: usize = path.to_str().unwrap()[1..].parse().unwrap();
            Ok(Box::new(FakeReader {
                chapter: n as u8,
                pts: self.files[n].clone(),
                pos: 0,
                info: info(),
            }))
        }
    }

    fn chapters(durations: &[i64]) -> Vec<Chapter> {
        durations
            .iter()
            .enumerate()
            .map(|(i, &d)| Chapter {
                path: PathBuf::from(format!("c{i}")),
                duration: Micros(d),
            })
            .collect()
    }

    /// Frames every 100 us from `first` while below `end`.
    fn every_100(first: i64, end: i64) -> Vec<i64> {
        (first..end).step_by(100).collect()
    }

    /// (chapter, frame index, item pts) of a frame.
    fn id(f: &VideoFrame) -> (u8, u8, i64) {
        (f.data[0], f.data[1], f.pts.0)
    }

    /// Three chapters of 1000, 500 and 1000 us with a frame every 100 us.
    fn three() -> FakeMedia {
        FakeMedia {
            files: vec![every_100(0, 1000), every_100(0, 500), every_100(0, 1000)],
            ..FakeMedia::default()
        }
    }

    /// Every frame of `three()` in item time, in order.
    fn three_frames() -> Vec<(u8, u8, i64)> {
        let mut v = Vec::new();
        for (c, (start, n)) in [(0, 10), (1000, 5), (1500, 10)].into_iter().enumerate() {
            for k in 0..n {
                v.push((c as u8, k as u8, start + 100 * k as i64));
            }
        }
        v
    }

    /// The frame of `frames` displayed at `t`.
    fn expected_at(frames: &[(u8, u8, i64)], t: i64) -> Option<(u8, u8, i64)> {
        frames.iter().rev().find(|f| f.2 <= t).copied()
    }

    #[test]
    fn chapter_at_maps_item_time() {
        let media = three();
        let r = ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
            .unwrap();
        for (t, c) in [
            (-5, 0),
            (0, 0),
            (999, 0),
            (1000, 1),
            (1499, 1),
            (1500, 2),
            (9999, 2),
        ] {
            assert_eq!(r.chapter_at(Micros(t)), c, "t = {t}");
        }
    }

    #[test]
    fn frame_at_is_continuous_across_boundaries() {
        let media = three();
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
                .unwrap();
        let frames = three_frames();
        for t in (-50..2700).step_by(25) {
            let got = r.frame_at(Micros(t)).unwrap().map(|f| id(&f));
            assert_eq!(got, expected_at(&frames, t), "t = {t}");
        }
        // Backwards and jumping around, including exact boundaries.
        for t in [
            2499, 1500, 1499, 999, 1000, 0, 2000, 1001, 999, 1500, 10_000,
        ] {
            let got = r.frame_at(Micros(t)).unwrap().map(|f| id(&f));
            assert_eq!(got, expected_at(&frames, t), "t = {t}");
        }
    }

    #[test]
    fn next_frame_runs_through_every_chapter_with_offset_pts() {
        let media = three();
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
                .unwrap();
        let mut got = Vec::new();
        while let Some(f) = r.next_frame().unwrap() {
            got.push(id(&f));
        }
        assert_eq!(got, three_frames(), "no gap, no duplicate");
        assert!(r.next_frame().unwrap().is_none(), "stays at the end");
    }

    #[test]
    fn seek_into_a_later_chapter_then_next_frame() {
        let media = three();
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
                .unwrap();
        r.seek(Micros(1250)).unwrap();
        let got: Vec<_> = std::iter::from_fn(|| r.next_frame().unwrap())
            .map(|f| id(&f))
            .collect();
        let all = three_frames();
        let from = all.iter().position(|f| f.2 == 1200).unwrap();
        assert_eq!(got, all[from..]);

        // Back into the first chapter: it continues into the second again.
        r.seek(Micros(950)).unwrap();
        let got: Vec<_> = (0..3)
            .map(|_| id(&r.next_frame().unwrap().unwrap()))
            .collect();
        assert_eq!(got, vec![(0, 9, 900), (1, 0, 1000), (1, 1, 1100)]);
    }

    #[test]
    fn frames_past_a_chapters_duration_are_dropped() {
        // Chapter 0 holds a frame at 1000 although it lasts 1000 us: chapter
        // 1's first frame is shown there instead.
        let media = FakeMedia {
            files: vec![every_100(0, 1100), every_100(0, 500)],
            ..FakeMedia::default()
        };
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500]), &Default::default()).unwrap();
        let pts: Vec<i64> = std::iter::from_fn(|| r.next_frame().unwrap())
            .map(|f| f.pts.0)
            .collect();
        assert_eq!(pts, every_100(0, 1500));
        assert_eq!(
            id(&r.frame_at(Micros(1000)).unwrap().unwrap()),
            (1, 0, 1000)
        );
        assert_eq!(id(&r.frame_at(Micros(999)).unwrap().unwrap()), (0, 9, 900));
    }

    #[test]
    fn a_chapter_starting_late_shows_the_previous_last_frame() {
        let media = FakeMedia {
            files: vec![every_100(0, 1000), every_100(50, 500)],
            ..FakeMedia::default()
        };
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500]), &Default::default()).unwrap();
        assert_eq!(id(&r.frame_at(Micros(1020)).unwrap().unwrap()), (0, 9, 900));
        assert_eq!(
            id(&r.frame_at(Micros(1050)).unwrap().unwrap()),
            (1, 0, 1050)
        );
        assert!(r.frame_at(Micros(-1)).unwrap().is_none());
    }

    #[test]
    fn frame_near_maps_like_frame_at() {
        let media = three();
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
                .unwrap();
        let got = r.frame_near(Micros(1730), Micros(1000)).unwrap().unwrap();
        assert_eq!(id(&got), (2, 2, 1700));
    }

    #[test]
    fn opens_lazily_and_keeps_two_chapters_open() {
        let media = three();
        let mut r =
            ChapteredReader::open(&media, chapters(&[1000, 500, 1000]), &Default::default())
                .unwrap();
        assert_eq!(*media.opens.lock().unwrap(), vec![PathBuf::from("c0")]);
        // Scrubbing over the first boundary reuses both readers.
        for t in [900, 1100, 900, 1100] {
            r.frame_at(Micros(t)).unwrap();
        }
        assert_eq!(media.opens.lock().unwrap().len(), 2);
        r.frame_at(Micros(2000)).unwrap();
        assert_eq!(r.readers.len(), MAX_OPEN_CHAPTERS);
        assert_eq!(media.opens.lock().unwrap().len(), 3);
    }

    #[test]
    fn works_with_an_owned_backend() {
        let media = Arc::new(three());
        let m = Arc::clone(&media);
        let mut r: Box<dyn VideoReader> = Box::new(
            ChapteredReader::new(chapters(&[1000, 500, 1000]), move |p| {
                m.open_video(p, &DecodeOptions::default())
            })
            .unwrap(),
        );
        assert_eq!(
            id(&r.frame_at(Micros(1600)).unwrap().unwrap()),
            (2, 1, 1600)
        );
        assert_eq!(r.info(), &info());
    }

    #[test]
    fn rejects_empty_and_non_positive_chapters() {
        let media = three();
        for c in [vec![], chapters(&[1000, 0]), chapters(&[-1])] {
            let err = ChapteredReader::open(&media, c, &Default::default()).unwrap_err();
            assert!(matches!(err, MediaError::InvalidChapters(_)), "{err}");
        }
        assert!(media.opens.lock().unwrap().is_empty());
    }

    #[test]
    fn open_item_video_uses_the_plain_reader_without_chapters() {
        let media = three();
        let mut r = open_item_video(&media, Path::new("c1"), &[], &Default::default()).unwrap();
        // Chapter 1's file on its own: its time is not offset.
        assert_eq!(id(&r.frame_at(Micros(1200)).unwrap().unwrap()), (1, 4, 400));

        let mut r = open_item_video(
            &media,
            Path::new("c0"),
            &chapters(&[1000, 500]),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            id(&r.frame_at(Micros(1200)).unwrap().unwrap()),
            (1, 2, 1200)
        );
    }

    #[test]
    fn from_refs_resolves_paths() {
        let refs = [
            ChapterRef {
                path: "a/GX010042.MP4".into(),
                duration: Micros(5),
            },
            ChapterRef {
                path: "a/GX020042.MP4".into(),
                duration: Micros(6),
            },
        ];
        let c = Chapter::from_refs(&refs, |p| Path::new("/root").join(p));
        assert_eq!(
            c,
            vec![
                Chapter {
                    path: "/root/a/GX010042.MP4".into(),
                    duration: Micros(5)
                },
                Chapter {
                    path: "/root/a/GX020042.MP4".into(),
                    duration: Micros(6)
                },
            ]
        );
    }
}
