//! `kwikvid edit --out FILE`: render the edit to an MP4.
//!
//! The CLI only gathers settings, reports progress and manages the output
//! file; all rendering is [`kv_render::export`] with [`FfmpegBackend`] as
//! both the [`kv_media::MediaBackend`] and the [`kv_media::Encoder`].
//!
//! The video is written to a hidden temporary file next to `--out` and
//! renamed into place only when the export succeeds, so a failed run never
//! leaves a partial file and never destroys a file `--overwrite` would have
//! replaced.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use kv_core::Project;
use kv_media::{ExportSettings, FfmpegBackend};
use kv_render::Progress;

use crate::cli::EditArgs;
use crate::edit;
use crate::format::secs;

/// `--out` exists and `--overwrite` was not given. `main` maps it to exit
/// code [`OutputExists::EXIT_CODE`].
#[derive(Debug, thiserror::Error)]
#[error("{} already exists; pass --overwrite to replace it", .0.display())]
pub struct OutputExists(pub PathBuf);

impl OutputExists {
    pub const EXIT_CODE: u8 = 2;
}

/// Without a terminal, print a progress line every this many percent.
const LINE_STEP_PERCENT: u64 = 10;

pub fn run(args: &EditArgs) -> anyhow::Result<()> {
    let out = args
        .out
        .as_deref()
        .context("--out is required unless --dry-run is given")?;
    if out.exists() && !args.overwrite {
        return Err(OutputExists(out.to_path_buf()).into());
    }
    let settings = settings(args);
    settings
        .validate()
        .context("invalid --size or --fps for export")?;

    let plan = edit::plan(args)?;
    let backend = FfmpegBackend::new().context("cannot start FFmpeg")?;
    let tmp = temp_path(out)?;
    let mut reporter = Reporter::new(args.quiet);
    let result = kv_render::export(
        &plan.project,
        Project::DEFAULT_SECTION,
        &plan.root,
        &backend,
        &backend,
        &tmp,
        &settings,
        &mut |p| reporter.update(p),
    );
    reporter.finish();
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            remove_partial(&tmp);
            return Err(e).with_context(|| format!("cannot render {}", out.display()));
        }
    };
    if let Err(e) = std::fs::rename(&tmp, out) {
        remove_partial(&tmp);
        return Err(e).with_context(|| format!("cannot write {}", out.display()));
    }
    if !args.quiet {
        eprintln!(
            "wrote {}: {} s, encoder {}, {:.1} s elapsed",
            out.display(),
            secs(report.duration),
            report.encoder_name,
            report.elapsed.as_secs_f64()
        );
    }
    Ok(())
}

/// Export settings from `--size` and `--fps`, the rest from
/// [`ExportSettings::default`]; the bitrate follows the size.
fn settings(args: &EditArgs) -> ExportSettings {
    let mut s = ExportSettings::default();
    if let Some((w, h)) = args.size {
        s.width = w;
        s.height = h;
        s.video_bitrate = ExportSettings::default_bitrate(w, h, s.video_codec);
    }
    if let Some(fps) = args.fps {
        s.fps = fps;
    }
    s
}

/// `.<name>.partial` in the directory of `out`.
fn temp_path(out: &Path) -> anyhow::Result<PathBuf> {
    let name = out
        .file_name()
        .with_context(|| format!("--out {} is not a file name", out.display()))?;
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(name);
    tmp.push(".partial");
    Ok(out.with_file_name(tmp))
}

/// Best effort: the export error matters more than a failed cleanup.
fn remove_partial(tmp: &Path) {
    if tmp.exists() {
        if let Err(e) = std::fs::remove_file(tmp) {
            tracing::warn!(path = %tmp.display(), error = %e, "cannot remove partial output");
        }
    }
}

/// Export progress on stderr: one updating line on a terminal, a line every
/// [`LINE_STEP_PERCENT`] percent otherwise, nothing with `--quiet`.
struct Reporter {
    mode: Mode,
    started: Instant,
    /// Last percentage printed.
    shown: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Quiet,
    Terminal,
    Lines,
}

impl Reporter {
    fn new(quiet: bool) -> Self {
        let mode = if quiet {
            Mode::Quiet
        } else if std::io::stderr().is_terminal() {
            Mode::Terminal
        } else {
            Mode::Lines
        };
        Reporter {
            mode,
            started: Instant::now(),
            shown: None,
        }
    }

    fn update(&mut self, p: Progress) {
        if let Some(line) = self.line(p) {
            let mut err = std::io::stderr().lock();
            let _ = match self.mode {
                Mode::Terminal => write!(err, "\r{line}"),
                _ => writeln!(err, "{line}"),
            };
            let _ = err.flush();
        }
    }

    /// The progress text to print for `p`, if any.
    fn line(&mut self, p: Progress) -> Option<String> {
        let percent = (p.frame * 100).checked_div(p.total_frames).unwrap_or(100);
        let due = match self.mode {
            Mode::Quiet => false,
            Mode::Terminal => self.shown != Some(percent),
            Mode::Lines => {
                let step = percent / LINE_STEP_PERCENT;
                self.shown.is_none_or(|s| s / LINE_STEP_PERCENT != step)
            }
        };
        if !due {
            return None;
        }
        self.shown = Some(percent);
        Some(format!(
            "rendering {percent:>3}% ({}/{} frames, {})",
            p.frame,
            p.total_frames,
            elapsed(self.started.elapsed())
        ))
    }

    /// End the updating terminal line.
    fn finish(&mut self) {
        if self.mode == Mode::Terminal && self.shown.is_some() {
            eprintln!();
        }
    }
}

fn elapsed(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reporter(mode: Mode) -> Reporter {
        Reporter {
            mode,
            started: Instant::now(),
            shown: None,
        }
    }

    fn printed(r: &mut Reporter, total: u64) -> Vec<u64> {
        (1..=total)
            .filter(|&frame| {
                r.line(Progress {
                    frame,
                    total_frames: total,
                })
                .is_some()
            })
            .collect()
    }

    #[test]
    fn lines_mode_prints_every_ten_percent() {
        let frames = printed(&mut reporter(Mode::Lines), 200);
        assert_eq!(
            frames,
            [1, 20, 40, 60, 80, 100, 120, 140, 160, 180, 200],
            "first frame, then each 10%"
        );
    }

    #[test]
    fn terminal_mode_prints_each_new_percent_and_quiet_nothing() {
        assert_eq!(printed(&mut reporter(Mode::Terminal), 1000).len(), 101);
        assert!(printed(&mut reporter(Mode::Quiet), 50).is_empty());
    }

    #[test]
    fn temp_file_is_hidden_next_to_the_output() {
        assert_eq!(
            temp_path(Path::new("dir/out.mp4")).unwrap(),
            Path::new("dir/.out.mp4.partial")
        );
        assert!(temp_path(Path::new("..")).is_err());
    }
}
