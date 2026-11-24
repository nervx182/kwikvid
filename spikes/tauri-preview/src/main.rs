//! kwikvid preview spike (issue #5): which way of getting video frames on
//! screen in Tauri 2 sustains 60 fps at 1080p? Throwaway quality on purpose.
//!
//! Paths compared (see README.md):
//! - `native-wgpu`: wgpu renders into a Metal layer under a transparent webview (macOS).
//! - `raw-1080` / `raw-540`: raw RGBA frames over a custom protocol into a WebGL canvas.
//! - `jpeg-1080`: JPEG frames over a custom protocol, decoded by the webview.
//! - `baseline`: requestAnimationFrame only, to show the display's own rate.
//!
//! `--bench` runs every path for `--seconds` each, writes `--log`, and exits.

mod native;
mod pattern;
mod stats;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::http::{Request, Response};
use tauri::{Manager, State, UriSchemeContext, UriSchemeResponder};

use stats::{PathResult, Samples};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    bench: bool,
    seconds: f64,
    log_path: String,
    /// Optional subset of path ids (`--only raw-1080,jpeg-1080`).
    only: Vec<String>,
    os: &'static str,
    arch: &'static str,
}

impl Config {
    fn from_args() -> Self {
        let mut cfg = Config {
            bench: std::env::var_os("KV_SPIKE_BENCH").is_some(),
            seconds: 10.0,
            // An app opened from Finder runs with cwd "/", so default to $HOME.
            log_path: std::env::var("KV_SPIKE_LOG").unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(|h| format!("{h}/kv-preview-bench.log"))
                    .unwrap_or_else(|_| "kv-preview-bench.log".into())
            }),
            only: Vec::new(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--bench" => cfg.bench = true,
                "--seconds" => {
                    if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                        cfg.seconds = v;
                    }
                }
                "--only" => {
                    if let Some(v) = args.next() {
                        cfg.only = v.split(',').map(|s| s.trim().to_string()).collect();
                    }
                }
                "--log" => {
                    if let Some(v) = args.next() {
                        cfg.log_path = v;
                    }
                }
                _ => {} // ignore e.g. macOS -psn_ arguments
            }
        }
        cfg
    }
}

struct AppState {
    cfg: Config,
    log: Mutex<File>,
    started: Instant,
    native: native::Native,
}

impl AppState {
    fn log(&self, line: &str) {
        println!("{line}");
        let mut f = self.log.lock().unwrap();
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

#[tauri::command]
fn bench_config(state: State<'_, AppState>) -> Config {
    state.cfg.clone()
}

#[tauri::command]
fn log_line(state: State<'_, AppState>, line: String) {
    state.log(&format!(
        "[{:>6.1}s] {line}",
        state.started.elapsed().as_secs_f64()
    ));
}

/// The webview reports a finished web path with its raw samples.
#[tauri::command]
fn report(
    state: State<'_, AppState>,
    path: String,
    seconds: f64,
    displayed: u64,
    produced: u64,
    samples: Samples,
    notes: String,
) -> PathResult {
    let r = PathResult::from_samples(&path, seconds, displayed, produced, &samples, notes);
    write_result(&state, &r);
    r
}

fn write_result(state: &AppState, r: &PathResult) {
    state.log(&format!("SUMMARY {}", r.summary_line()));
    state.log(&format!(
        "RESULT {}",
        serde_json::to_string(r).unwrap_or_default()
    ));
}

#[tauri::command]
async fn native_start(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    state.native.start(&window, (size.width, size.height))
}

#[tauri::command]
fn native_progress(state: State<'_, AppState>) -> (u64, f64) {
    state.native.progress()
}

#[tauri::command]
async fn native_stop(state: State<'_, AppState>) -> Result<PathResult, String> {
    let r = state.native.stop()?;
    write_result(&state, &r);
    Ok(r)
}

#[tauri::command]
fn finish(app: tauri::AppHandle, state: State<'_, AppState>, code: i32) {
    state.log(&format!("END code={code}"));
    app.exit(code);
}

/// `kvframe://localhost/<kind>/<w>/<h>/<n>` where kind is `raw` or `jpeg`.
/// Each request is served on its own thread, like a decoder pool would.
fn frame_protocol(
    _ctx: UriSchemeContext<'_, tauri::Wry>,
    req: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let path = req.uri().path().to_string();
    std::thread::spawn(move || {
        let resp = match serve_frame(&path) {
            Ok((mime, body, gen_ms)) => Response::builder()
                .header("Content-Type", mime)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Expose-Headers", "X-Gen-Ms")
                .header("Cache-Control", "no-store")
                .header("X-Gen-Ms", format!("{gen_ms:.3}"))
                .body(body),
            Err(e) => Response::builder()
                .status(400)
                .header("Access-Control-Allow-Origin", "*")
                .body(e.into_bytes()),
        };
        responder.respond(resp.expect("valid response"));
    });
}

fn serve_frame(path: &str) -> Result<(&'static str, Vec<u8>, f64), String> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let [kind, w, h, n] = parts[..] else {
        return Err(format!("bad frame path {path}"));
    };
    let num = |s: &str| s.parse::<u64>().map_err(|e| e.to_string());
    let (w, h, n) = (num(w)? as u32, num(h)? as u32, num(n)?);
    if !(16..=3840).contains(&w) || !(16..=2160).contains(&h) {
        return Err("size out of range".into());
    }
    let t = Instant::now();
    let mut buf = Vec::new();
    pattern::render(n, w, h, &mut buf);
    match kind {
        "raw" => Ok((
            "application/octet-stream",
            buf,
            t.elapsed().as_secs_f64() * 1e3,
        )),
        "jpeg" => {
            let rgb = pattern::rgba_to_rgb(&buf);
            let mut out = Vec::with_capacity(rgb.len() / 8);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| e.to_string())?;
            Ok(("image/jpeg", out, t.elapsed().as_secs_f64() * 1e3))
        }
        _ => Err(format!("unknown frame kind {kind}")),
    }
}

fn main() {
    let cfg = Config::from_args();
    let log_path = PathBuf::from(&cfg.log_path);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| panic!("cannot open log {}: {e}", log_path.display()));
    let state = AppState {
        cfg: cfg.clone(),
        log: Mutex::new(log),
        started: Instant::now(),
        native: native::Native::default(),
    };
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    state.log(&format!(
        "START kv-preview-spike unix={unix} os={} arch={} bench={} seconds={} log={}",
        cfg.os,
        cfg.arch,
        cfg.bench,
        cfg.seconds,
        log_path.display()
    ));

    if cfg.bench {
        // Watchdog: never hang a CI runner. 5 paths plus generous slack.
        let limit = Duration::from_secs_f64(cfg.seconds * 6.0 + 90.0);
        let path = log_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(limit);
            if let Ok(mut f) = OpenOptions::new().append(true).open(&path) {
                let _ = writeln!(
                    f,
                    "END code=3 watchdog: benchmark did not finish in {limit:?}"
                );
            }
            eprintln!("watchdog: benchmark did not finish in {limit:?}");
            std::process::exit(3);
        });
    }

    tauri::Builder::default()
        .manage(state)
        .register_asynchronous_uri_scheme_protocol("kvframe", frame_protocol)
        .invoke_handler(tauri::generate_handler![
            bench_config,
            log_line,
            report,
            native_start,
            native_progress,
            native_stop,
            finish
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            if let Some(w) = app.get_webview_window("main") {
                if let (Ok(size), Ok(scale)) = (w.inner_size(), w.scale_factor()) {
                    state.log(&format!(
                        "WINDOW inner={}x{} px scale={scale}",
                        size.width, size.height
                    ));
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_raw_and_jpeg() {
        let (mime, body, _) = serve_frame("/raw/64/32/7").unwrap();
        assert_eq!(mime, "application/octet-stream");
        assert_eq!(body.len(), 64 * 32 * 4);
        let (mime, body, _) = serve_frame("/jpeg/64/32/7").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&body[..2], &[0xFF, 0xD8]);
        assert!(serve_frame("/raw/1/1").is_err());
        assert!(serve_frame("/gif/64/32/1").is_err());
    }
}
