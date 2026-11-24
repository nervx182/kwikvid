//! `kwikvid-mcp`: the kwikvid MCP server over stdio.
//!
//! Usage: `kwikvid-mcp [--root <dir>]`. Media paths are relative to `<dir>`
//! (default: the working directory). Logs go to stderr (`RUST_LOG`), never
//! stdout, which carries the MCP protocol.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use kv_mcp::KwikvidServer;
use kv_media::MediaBackend;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

fn parse_root() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut root = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(args.next().context("--root needs a value")?)),
            "-h" | "--help" => {
                println!("usage: kwikvid-mcp [--root <dir>]");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}; usage: kwikvid-mcp [--root <dir>]"),
        }
    }
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir().context("cannot read the working directory")?,
    };
    std::path::absolute(&root).with_context(|| format!("bad --root {root:?}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let root = parse_root()?;
    let backend: Option<Arc<dyn MediaBackend>> = match kv_media::FfmpegBackend::new() {
        Ok(b) => Some(Arc::new(b)),
        Err(e) => {
            tracing::warn!("media probing unavailable, media_add will need duration_us: {e}");
            None
        }
    };
    tracing::info!(?root, "kwikvid-mcp starting on stdio");

    let service = KwikvidServer::new(root, backend)
        .serve(rmcp::transport::stdio())
        .await
        .context("MCP initialisation failed")?;
    service.waiting().await.context("MCP server task failed")?;
    Ok(())
}
