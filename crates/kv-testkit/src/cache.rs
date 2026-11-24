//! On-disk fixture cache under `<workspace>/fixtures/generated/`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Directory that holds generated fixtures: `<workspace>/fixtures/generated/`.
pub fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<workspace>/crates/kv-testkit`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("kv-testkit lives two levels below the workspace root")
        .join("fixtures")
        .join("generated")
}

/// Return `<fixtures_dir>/<name>`, rendering it with `render` only if it does
/// not exist yet.
pub(crate) fn ensure_fixture(name: &str, render: impl FnOnce() -> Vec<u8>) -> PathBuf {
    ensure_fixture_in(&fixtures_dir(), name, render)
}

/// Like [`ensure_fixture`] but in an explicit directory (used by tests).
pub(crate) fn ensure_fixture_in(
    dir: &Path,
    name: &str,
    render: impl FnOnce() -> Vec<u8>,
) -> PathBuf {
    let path = dir.join(name);
    if path.is_file() {
        return path;
    }
    fs::create_dir_all(dir)
        .unwrap_or_else(|e| panic!("cannot create fixture dir {}: {e}", dir.display()));

    let bytes = render();

    // Write to a unique temporary file, then rename into place, so a reader in
    // another test process never observes a half-written fixture.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = fs::File::create(&tmp)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", tmp.display()));
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", tmp.display()));
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        // On Windows rename fails if another process won the race; the
        // content is identical, so keep theirs.
        let _ = fs::remove_file(&tmp);
        if !path.is_file() {
            panic!("cannot move fixture into {}: {e}", path.display());
        }
    }
    path
}

/// Format a positive float for use in a file name: `120` -> `120`,
/// `8.5` -> `8p5`.
pub(crate) fn name_num(x: f64) -> String {
    format!("{x}").replace('.', "p")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "kv-testkit-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn fixtures_dir_is_under_workspace_root() {
        let dir = fixtures_dir();
        assert!(dir.ends_with("fixtures/generated"));
        let root = dir.parent().unwrap().parent().unwrap();
        assert!(root.join("Cargo.toml").is_file());
        assert!(root.join("crates").join("kv-testkit").is_dir());
    }

    #[test]
    fn existing_fixture_is_reused_not_rerendered() {
        let dir = unique_temp_dir("reuse");
        let calls = Cell::new(0);
        let render = || {
            calls.set(calls.get() + 1);
            b"first".to_vec()
        };
        let p1 = ensure_fixture_in(&dir, "x.bin", render);
        assert_eq!(fs::read(&p1).unwrap(), b"first");
        assert_eq!(calls.get(), 1);

        let p2 = ensure_fixture_in(&dir, "x.bin", || {
            calls.set(calls.get() + 1);
            b"second".to_vec()
        });
        assert_eq!(p1, p2);
        assert_eq!(calls.get(), 1, "render must not run for a cached fixture");
        assert_eq!(fs::read(&p2).unwrap(), b"first");

        // No temporary files left behind.
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn name_num_formats() {
        assert_eq!(name_num(120.0), "120");
        assert_eq!(name_num(8.5), "8p5");
    }
}
