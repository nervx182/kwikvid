use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use kv_cache::{Cache, CacheError, SourceKey, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};

fn write_file(path: &Path, contents: &[u8]) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(contents).unwrap();
    f.sync_all().unwrap();
}

fn key(name: &str, size: u64) -> SourceKey {
    SourceKey {
        path: format!("/clips/{name}").into(),
        size,
        mtime_ns: 1_700_000_000_000_000_000,
    }
}

#[test]
fn put_get_round_trip() {
    let cache = Cache::in_memory().unwrap();
    let k = key("a.mp4", 10);
    assert_eq!(cache.get(&k, "filmstrip", "n=12,v=1").unwrap(), None);
    cache
        .put(&k, "filmstrip", "n=12,v=1", b"jpeg bytes")
        .unwrap();
    assert_eq!(
        cache.get(&k, "filmstrip", "n=12,v=1").unwrap().as_deref(),
        Some(&b"jpeg bytes"[..])
    );
    // Other kind or params miss.
    assert_eq!(cache.get(&k, "probe", "n=12,v=1").unwrap(), None);
    assert_eq!(cache.get(&k, "filmstrip", "n=12,v=2").unwrap(), None);
    // Overwrite.
    cache.put(&k, "filmstrip", "n=12,v=1", b"new").unwrap();
    assert_eq!(
        cache.get(&k, "filmstrip", "n=12,v=1").unwrap().as_deref(),
        Some(&b"new"[..])
    );
    assert_eq!(cache.total_bytes().unwrap(), 3);
}

#[test]
fn changed_file_size_is_a_miss_and_put_replaces_stale_rows() {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip.mp4");
    write_file(&clip, b"short");
    let old = SourceKey::from_file(&clip).unwrap();
    assert_eq!(old.size, 5);
    // Different spelling of the same path resolves to the same key.
    let dotted = dir.path().join(".").join("clip.mp4");
    assert_eq!(SourceKey::from_file(&dotted).unwrap(), old);

    let cache = Cache::in_memory().unwrap();
    cache.put(&old, "probe", "v=1", b"old-a").unwrap();
    cache.put(&old, "probe", "v=2", b"old-b").unwrap();
    cache.put(&old, "filmstrip", "v=1", b"strip").unwrap();

    write_file(&clip, b"a much longer file");
    let new = SourceKey::from_file(&clip).unwrap();
    assert_ne!(new.size, old.size);
    assert_eq!(cache.get(&new, "probe", "v=1").unwrap(), None);

    cache.put(&new, "probe", "v=1", b"new").unwrap();
    // Both stale probe rows are gone, other kinds are untouched.
    assert_eq!(cache.get(&old, "probe", "v=1").unwrap(), None);
    assert_eq!(cache.get(&old, "probe", "v=2").unwrap(), None);
    assert!(cache.get(&old, "filmstrip", "v=1").unwrap().is_some());
    assert_eq!(cache.total_bytes().unwrap(), 3 + 5);
}

#[test]
fn changed_mtime_is_a_miss() {
    let cache = Cache::in_memory().unwrap();
    let k = key("a.mp4", 10);
    cache.put(&k, "hilights", "v=1", b"x").unwrap();
    let touched = SourceKey {
        mtime_ns: k.mtime_ns + 1,
        ..k.clone()
    };
    assert_eq!(cache.get(&touched, "hilights", "v=1").unwrap(), None);
    cache.put(&touched, "hilights", "v=1", b"y").unwrap();
    assert_eq!(cache.get(&k, "hilights", "v=1").unwrap(), None);
    assert_eq!(cache.total_bytes().unwrap(), 1);
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Analysis {
    bpm: f64,
    beats: Vec<i64>,
}

#[test]
fn json_helpers_round_trip() {
    let cache = Cache::in_memory().unwrap();
    let k = key("song.mp3", 1234);
    let a = Analysis {
        bpm: 120.5,
        beats: vec![0, 500_000, 1_000_000],
    };
    assert_eq!(
        cache
            .get_json::<Analysis>(&k, "music-analysis", "v=1")
            .unwrap(),
        None
    );
    cache.put_json(&k, "music-analysis", "v=1", &a).unwrap();
    assert_eq!(
        cache
            .get_json::<Analysis>(&k, "music-analysis", "v=1")
            .unwrap(),
        Some(a)
    );
    // A blob that is not the expected JSON is an error, not a panic.
    cache.put(&k, "music-analysis", "v=2", b"not json").unwrap();
    assert!(matches!(
        cache.get_json::<Analysis>(&k, "music-analysis", "v=2"),
        Err(CacheError::Json(_))
    ));
}

#[test]
fn prune_evicts_least_recently_used_first() {
    let cache = Cache::in_memory().unwrap();
    let (a, b, c) = (key("a", 1), key("b", 1), key("c", 1));
    cache.put(&a, "filmstrip", "v=1", &[0; 100]).unwrap();
    cache.put(&b, "filmstrip", "v=1", &[0; 100]).unwrap();
    cache.put(&c, "filmstrip", "v=1", &[0; 100]).unwrap();
    // Touch `a`, so `b` is now the least recently used.
    assert!(cache.get(&a, "filmstrip", "v=1").unwrap().is_some());

    assert_eq!(cache.prune(300).unwrap(), 0);
    assert_eq!(cache.prune(250).unwrap(), 1);
    assert_eq!(cache.get(&b, "filmstrip", "v=1").unwrap(), None);
    assert_eq!(cache.total_bytes().unwrap(), 200);

    // c is now older than a (a was touched after c was written; c was not).
    assert_eq!(cache.prune(150).unwrap(), 1);
    assert_eq!(cache.get(&c, "filmstrip", "v=1").unwrap(), None);
    assert!(cache.get(&a, "filmstrip", "v=1").unwrap().is_some());

    assert_eq!(cache.prune(0).unwrap(), 1);
    assert_eq!(cache.total_bytes().unwrap(), 0);
}

#[test]
fn concurrent_writers_from_two_threads() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(Cache::open(dir.path().join("cache.sqlite")).unwrap());
    let handles: Vec<_> = (0..2)
        .map(|t| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                for i in 0..50u64 {
                    let k = key(&format!("t{t}-{i}.mp4"), i);
                    cache.put(&k, "probe", "v=1", &i.to_le_bytes()).unwrap();
                    assert!(cache.get(&k, "probe", "v=1").unwrap().is_some());
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(cache.total_bytes().unwrap(), 2 * 50 * 8);
    for t in 0..2 {
        for i in 0..50u64 {
            let k = key(&format!("t{t}-{i}.mp4"), i);
            assert_eq!(
                cache.get(&k, "probe", "v=1").unwrap(),
                Some(i.to_le_bytes().to_vec())
            );
        }
    }
}

#[test]
fn reopen_from_disk_keeps_data_and_lru_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.sqlite");
    let (a, b) = (key("a", 1), key("b", 1));
    {
        let cache = Cache::open(&db).unwrap();
        cache.put(&a, "filmstrip", "v=1", b"aaaa").unwrap();
        cache.put(&b, "filmstrip", "v=1", b"bbbb").unwrap();
    }
    let cache = Cache::open(&db).unwrap();
    assert_eq!(
        cache.get(&a, "filmstrip", "v=1").unwrap().as_deref(),
        Some(&b"aaaa"[..])
    );
    // The LRU clock resumed after the stored maximum, so the get above made
    // `a` newer than `b`.
    assert_eq!(cache.prune(4).unwrap(), 1);
    assert_eq!(cache.get(&b, "filmstrip", "v=1").unwrap(), None);
    assert!(cache.get(&a, "filmstrip", "v=1").unwrap().is_some());
}

fn set_schema_version(db: &Path, version: i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute("UPDATE schema_version SET version = ?1", [version])
        .unwrap();
}

#[test]
fn newer_schema_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.sqlite");
    drop(Cache::open(&db).unwrap());
    set_schema_version(&db, SCHEMA_VERSION + 1);
    match Cache::open(&db) {
        Err(CacheError::UnsupportedSchema { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 1);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("newer schema must be rejected"),
    }
}

#[test]
fn older_schema_is_wiped() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.sqlite");
    let k = key("a", 1);
    {
        let cache = Cache::open(&db).unwrap();
        cache.put(&k, "probe", "v=1", b"data").unwrap();
    }
    set_schema_version(&db, SCHEMA_VERSION - 1);
    let cache = Cache::open(&db).unwrap();
    assert_eq!(cache.get(&k, "probe", "v=1").unwrap(), None);
    assert_eq!(cache.total_bytes().unwrap(), 0);
    cache.put(&k, "probe", "v=1", b"data").unwrap();
    drop(cache);
    // Now current again: data survives a reopen.
    let cache = Cache::open(&db).unwrap();
    assert!(cache.get(&k, "probe", "v=1").unwrap().is_some());
}
