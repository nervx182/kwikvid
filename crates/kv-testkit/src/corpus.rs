//! Regression corpus: real camera footage listed in `fixtures/corpus.toml`.
//!
//! The footage itself lives in Git LFS under `fixtures/corpus/` (ADR 0008)
//! and is only fetched by the full regression workflow. Everywhere else
//! (the merge gate, cloud sessions without the LFS objects) the files are
//! absent, and [`corpus_file`] reports that so the test can skip itself:
//!
//! ```no_run
//! #[test]
//! #[ignore = "regression"]
//! fn hilights_from_real_hero12_clip() {
//!     let path = kv_testkit::corpus_file_or_skip!("hero12-hilights.mp4");
//!     // ... open `path` and assert on it ...
//! }
//! ```
//!
//! Environment variables:
//! - `KV_CORPUS_DIR`: read the files from this directory instead of
//!   `<workspace>/fixtures/corpus/` (e.g. a local copy of the footage).
//! - `KV_REQUIRE_CORPUS=1`: a missing file is a test failure instead of a
//!   skip. Set it in the regression workflow once the corpus is populated.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Parsed `fixtures/corpus.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusManifest {
    /// Manifest format version. Currently 1.
    pub version: u32,
    /// Corpus files, in manifest order.
    #[serde(default, rename = "file")]
    pub files: Vec<CorpusEntry>,
}

/// One file of the regression corpus.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusEntry {
    /// File name under the corpus directory (no path separators).
    pub name: String,
    /// Lower-case hex SHA-256 of the file contents.
    pub sha256: String,
    /// Camera model that recorded it, e.g. "HERO12 Black".
    pub camera: String,
    /// Video codec and profile, e.g. "HEVC Main10 4K60".
    pub codec: String,
    /// What the file is good for (HiLights, chapters, telemetry, ...).
    #[serde(default)]
    pub notes: String,
}

impl CorpusManifest {
    /// Parse and validate manifest text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Look up an entry by file name.
    pub fn entry(&self, name: &str) -> Option<&CorpusEntry> {
        self.files.iter().find(|e| e.name == name)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("unsupported manifest version {}", self.version));
        }
        let mut seen = HashSet::new();
        for e in &self.files {
            if e.name.is_empty() || e.name.contains(['/', '\\']) || e.name.starts_with('.') {
                return Err(format!("invalid corpus file name {:?}", e.name));
            }
            if !seen.insert(e.name.as_str()) {
                return Err(format!("duplicate corpus file name {:?}", e.name));
            }
            if e.sha256.len() != 64
                || !e
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(format!(
                    "{}: sha256 must be 64 lower-case hex digits",
                    e.name
                ));
            }
        }
        Ok(())
    }
}

/// Path of the manifest: `<workspace>/fixtures/corpus.toml`.
pub fn corpus_manifest_path() -> PathBuf {
    workspace_fixtures().join("corpus.toml")
}

/// Directory that holds the corpus files: `$KV_CORPUS_DIR` if set, else
/// `<workspace>/fixtures/corpus/`.
pub fn corpus_dir() -> PathBuf {
    match std::env::var_os("KV_CORPUS_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => workspace_fixtures().join("corpus"),
    }
}

/// The parsed manifest, loaded once per process. Panics if it is invalid.
pub fn corpus_manifest() -> &'static CorpusManifest {
    static MANIFEST: OnceLock<CorpusManifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let path = corpus_manifest_path();
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        CorpusManifest::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    })
}

/// Path of corpus file `name`, or `None` (after printing why) when it is not
/// available, so the calling test can skip itself.
///
/// Panics when `name` is not listed in `fixtures/corpus.toml` (a typo), when
/// the file is present but its SHA-256 does not match the manifest, and when
/// the file is missing while `KV_REQUIRE_CORPUS=1` is set. The checksum is
/// verified once per file per process.
///
/// Most tests want [`corpus_file_or_skip!`](crate::corpus_file_or_skip).
pub fn corpus_file(name: &str) -> Option<PathBuf> {
    let require = std::env::var("KV_REQUIRE_CORPUS").is_ok_and(|v| v == "1");
    match resolve(corpus_manifest(), &corpus_dir(), name, &VERIFIED) {
        Ok(path) => Some(path),
        Err(reason) if require => panic!("{reason} (KV_REQUIRE_CORPUS=1)"),
        Err(reason) => {
            eprintln!("SKIPPED: {reason}");
            None
        }
    }
}

/// Return the path of corpus file `name`, or return early from the calling
/// test (which then passes) when the corpus is absent. See
/// [`corpus_file`](crate::corpus_file).
#[macro_export]
macro_rules! corpus_file_or_skip {
    ($name:expr) => {
        match $crate::corpus_file($name) {
            Some(path) => path,
            None => return,
        }
    };
}

static VERIFIED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// First bytes of a Git LFS pointer file (what a checkout without the LFS
/// objects contains instead of the media).
const LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/";

/// Core of [`corpus_file`]: `Ok(path)` when usable, `Err(reason)` when the
/// file is not available. Panics on an unknown name or checksum mismatch.
fn resolve(
    manifest: &CorpusManifest,
    dir: &Path,
    name: &str,
    verified: &Mutex<Option<HashSet<PathBuf>>>,
) -> Result<PathBuf, String> {
    let entry = manifest
        .entry(name)
        .unwrap_or_else(|| panic!("corpus file {name:?} is not listed in fixtures/corpus.toml"));
    let path = dir.join(&entry.name);
    let hint = "the regression corpus is not checked out \
                (run `git lfs pull --include \"fixtures/corpus/**\"` or set KV_CORPUS_DIR)";
    if !path.is_file() {
        return Err(format!(
            "corpus file {} is absent at {}; {hint}",
            entry.name,
            path.display()
        ));
    }
    if is_lfs_pointer(&path) {
        return Err(format!(
            "corpus file {} is a Git LFS pointer, not the media; {hint}",
            entry.name
        ));
    }

    let mut guard = verified.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.contains(&path) {
        let actual = sha256_file(&path);
        assert_eq!(
            actual,
            entry.sha256,
            "corpus file {} does not match the sha256 in fixtures/corpus.toml",
            path.display()
        );
        set.insert(path.clone());
    }
    Ok(path)
}

fn is_lfs_pointer(path: &Path) -> bool {
    let mut head = [0u8; LFS_POINTER_PREFIX.len()];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == LFS_POINTER_PREFIX)
}

/// Lower-case hex SHA-256 of a file, streamed so large footage is fine.
pub fn sha256_file(path: &Path) -> String {
    let mut file =
        fs::File::open(path).unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn workspace_fixtures() -> PathBuf {
    crate::fixtures_dir()
        .parent()
        .expect("fixtures_dir has a parent")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn temp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kv-testkit-corpus-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest(sha: &str) -> CorpusManifest {
        CorpusManifest::parse(&format!(
            r#"
            version = 1
            [[file]]
            name = "clip.mp4"
            sha256 = "{sha}"
            camera = "HERO12 Black"
            codec = "HEVC"
            notes = "test"
            "#
        ))
        .unwrap()
    }

    fn fresh() -> Mutex<Option<HashSet<PathBuf>>> {
        Mutex::new(None)
    }

    #[test]
    fn checked_in_manifest_is_valid() {
        let m = corpus_manifest();
        assert_eq!(m.version, 1);
    }

    #[test]
    fn present_file_with_matching_hash_resolves() {
        let dir = temp_dir();
        fs::write(dir.join("clip.mp4"), b"hello").unwrap();
        let path = resolve(&manifest(HELLO_SHA), &dir, "clip.mp4", &fresh()).unwrap();
        assert_eq!(path, dir.join("clip.mp4"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_skips_with_clear_message() {
        let dir = temp_dir();
        let reason = resolve(&manifest(HELLO_SHA), &dir, "clip.mp4", &fresh()).unwrap_err();
        assert!(reason.contains("clip.mp4 is absent"), "{reason}");
        assert!(reason.contains("git lfs pull"), "{reason}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lfs_pointer_skips_instead_of_failing_hash() {
        let dir = temp_dir();
        fs::write(
            dir.join("clip.mp4"),
            "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 5\n",
        )
        .unwrap();
        let reason = resolve(&manifest(HELLO_SHA), &dir, "clip.mp4", &fresh()).unwrap_err();
        assert!(reason.contains("Git LFS pointer"), "{reason}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[should_panic(expected = "does not match the sha256")]
    fn hash_mismatch_panics() {
        let dir = temp_dir();
        fs::write(dir.join("clip.mp4"), b"not hello").unwrap();
        let _ = resolve(&manifest(HELLO_SHA), &dir, "clip.mp4", &fresh());
    }

    #[test]
    #[should_panic(expected = "not listed in fixtures/corpus.toml")]
    fn unknown_name_panics() {
        let _ = resolve(&manifest(HELLO_SHA), &temp_dir(), "typo.mp4", &fresh());
    }

    #[test]
    fn real_corpus_file_skips_cleanly_when_unlisted_or_absent() {
        // Whatever the checked-in manifest lists, files that are not checked
        // out must produce `None`, never a panic (unless required by env).
        if std::env::var("KV_REQUIRE_CORPUS").is_ok_and(|v| v == "1") {
            return;
        }
        for e in &corpus_manifest().files {
            let path = corpus_dir().join(&e.name);
            if !path.is_file() || is_lfs_pointer(&path) {
                assert_eq!(corpus_file(&e.name), None);
            }
        }
    }

    #[test]
    fn manifest_validation_rejects_bad_entries() {
        let entry = |name: &str, sha: &str| {
            format!(
                "version = 1\n[[file]]\nname = {name:?}\nsha256 = {sha:?}\ncamera = \"c\"\ncodec = \"h264\"\n"
            )
        };
        assert!(CorpusManifest::parse(&entry("a.mp4", HELLO_SHA)).is_ok());
        assert!(CorpusManifest::parse(&entry("../a.mp4", HELLO_SHA)).is_err());
        assert!(CorpusManifest::parse(&entry("a.mp4", "ABC")).is_err());
        assert!(CorpusManifest::parse(&entry("a.mp4", &HELLO_SHA.to_uppercase())).is_err());
        let dup = format!(
            "{}[[file]]\nname = \"a.mp4\"\nsha256 = {HELLO_SHA:?}\ncamera = \"c\"\ncodec = \"h\"\n",
            entry("a.mp4", HELLO_SHA)
        );
        assert!(CorpusManifest::parse(&dup)
            .unwrap_err()
            .contains("duplicate"));
        assert!(CorpusManifest::parse("version = 2").is_err());
        assert!(CorpusManifest::parse("version = 1\nbogus = 3").is_err());
    }
}
