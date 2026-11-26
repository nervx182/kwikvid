//! `kwikvid-mcp`: the kwikvid MCP server over stdio.
//!
//! Usage: `kwikvid-mcp [--root <dir>] [--cache <dir>]`. Media paths are
//! relative to the `--root` directory (default: the working directory); the
//! server never reads or writes outside it. The analysis cache is
//! `cache.sqlite` in the `--cache` directory (default: `kwikvid/` in the
//! platform cache directory; in memory when it cannot be opened). Logs go to
//! stderr (`RUST_LOG`), never stdout, which carries the MCP protocol.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use kv_cache::Cache;
use kv_mcp::{KwikvidServer, NoMediaBackend};
use kv_media::MediaBackend;
use kv_session::{AudioAnalyzer, JobContext};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: kwikvid-mcp [--root <dir>] [--cache <dir>]";

struct Args {
    root: PathBuf,
    cache: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut root = None;
    let mut cache = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(args.next().context("--root needs a value")?)),
            "--cache" => cache = Some(PathBuf::from(args.next().context("--cache needs a value")?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}; {USAGE}"),
        }
    }
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir().context("cannot read the working directory")?,
    };
    let root = std::path::absolute(&root).with_context(|| format!("bad --root {root:?}"))?;
    // Guess: the platform cache dir (`~/Library/Caches`, `$XDG_CACHE_HOME`,
    // `%LOCALAPPDATA%`) under `kwikvid/`.
    let cache = cache.or_else(|| dirs::cache_dir().map(|d| d.join("kwikvid")));
    Ok(Args { root, cache })
}

/// The real backends (see `real_context` in `crates/kv-app/src/library.rs`).
/// Without FFmpeg the server still starts; tools that need it say so.
fn real_context(cache_dir: Option<PathBuf>) -> Result<JobContext> {
    let media: Arc<dyn MediaBackend> = match kv_media::FfmpegBackend::new() {
        Ok(b) => Arc::new(b),
        Err(e) => {
            tracing::warn!("FFmpeg unavailable, media_add will need duration_us: {e}");
            Arc::new(NoMediaBackend::new(e.to_string()))
        }
    };
    let on_disk = cache_dir.and_then(|dir| {
        std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| Cache::open(dir.join("cache.sqlite")).map_err(|e| e.to_string()))
            .inspect_err(|e| tracing::warn!(?dir, error = %e, "no cache file; caching in memory"))
            .ok()
    });
    let cache = match on_disk {
        Some(c) => c,
        None => Cache::in_memory().context("cannot create an in-memory cache")?,
    };
    Ok(JobContext::new(
        media,
        Arc::new(AudioAnalyzer),
        Arc::new(cache),
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = parse_args()?;
    let ctx = real_context(args.cache)?;
    tracing::info!(root = ?args.root, "kwikvid-mcp starting on stdio");

    let service = KwikvidServer::new(args.root, ctx)
        .serve(rmcp::transport::stdio())
        .await
        .context("MCP initialisation failed")?;
    service.waiting().await.context("MCP server task failed")?;
    Ok(())
}
