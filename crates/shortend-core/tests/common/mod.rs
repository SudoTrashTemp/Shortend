//! Shared helpers for engine integration tests.
#![allow(dead_code, unsafe_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::SystemTime;

use shortend_core::{
    CancelToken, Event, HashAlgorithm, JobReport, JobSettings, JobSpec, ProductionInfo,
};

/// Points the engine's state directory at a temp dir, once per test binary.
pub fn init_state_dir() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        // SAFETY: set once, before any engine call in this process reads it; std serialises
        // its own environment access.
        unsafe { std::env::set_var("SHORTEND_STATE_DIR", &dir) };
        dir
    });
}

pub fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// Deterministic pseudo-random bytes (xorshift), so files differ and compress badly.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A card-like tree: nested clips, sidecars, an empty file, an empty folder, Unicode names
/// and a batch of small "frames".
pub fn make_card(root: &Path) {
    write_file(
        &root.join("CLIP/A001C001_240101_R1AB.mxf"),
        &noise(5_000_000, 1),
    );
    write_file(
        &root.join("CLIP/A001C002_240101_R1AB.mxf"),
        &noise(3_333_333, 2),
    );
    write_file(&root.join("CLIP/A001C001_240101_R1AB.xml"), b"<clip/>");
    write_file(&root.join("SUB/Ünïcödé — 名前.wav"), &noise(100_000, 3));
    write_file(&root.join("empty.bin"), b"");
    write_file(&root.join(".DS_Store"), b"mac junk is copied as-is");
    std::fs::create_dir_all(root.join("EMPTY_DIR/nested")).unwrap();
    for i in 0..60 {
        write_file(
            &root.join(format!("DNG/frame_{i:05}.dng")),
            &noise(10_000 + i * 7, 100 + i as u64),
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FileState {
    pub xxh64: String,
    pub size: u64,
    pub mtime: SystemTime,
}

/// Content checksum, size and mtime of every regular file under `root` (relative paths).
pub fn tree_state(root: &Path, skip_top: &[&str]) -> BTreeMap<String, FileState> {
    let mut out = BTreeMap::new();
    for e in walk(root) {
        let rel = e
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if skip_top
            .iter()
            .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
        {
            continue;
        }
        let m = std::fs::symlink_metadata(&e).unwrap();
        if m.is_file() {
            out.insert(
                rel,
                FileState {
                    xxh64: xxh64_file(&e),
                    size: m.len(),
                    mtime: m.modified().unwrap(),
                },
            );
        }
    }
    out
}

pub fn dirs(root: &Path, skip_top: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = walk(root)
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| {
            p.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .filter(|rel| {
            !skip_top
                .iter()
                .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
        })
        .collect();
    v.sort();
    v
}

pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out
}

pub fn xxh64_file(path: &Path) -> String {
    let mut h = shortend_core::hash::Hasher::new(HashAlgorithm::Xxh64);
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finish().value()
}

pub fn spec(source: &Path, dests: &[&Path]) -> JobSpec {
    JobSpec {
        source: source.to_path_buf(),
        card_label: None,
        destinations: dests
            .iter()
            .enumerate()
            .map(|(i, d)| (format!("D{}", i + 1), d.to_path_buf()))
            .collect(),
        folder: None,
        settings: JobSettings {
            chunk_size: 1 << 20,
            ..JobSettings::default()
        },
        production: ProductionInfo::default(),
    }
}

/// Runs `f` with an event channel drained on another thread; returns the result and events.
pub fn with_events<T>(f: impl FnOnce(&mpsc::SyncSender<Event>) -> T) -> (T, Vec<Event>) {
    let (tx, rx) = mpsc::sync_channel(64);
    let drain = std::thread::spawn(move || rx.into_iter().collect::<Vec<_>>());
    let out = f(&tx);
    drop(tx);
    (out, drain.join().unwrap())
}

pub fn run(spec: JobSpec) -> (JobReport, Vec<Event>) {
    init_state_dir();
    let job = shortend_core::plan(spec).expect("plan");
    let (r, ev) = with_events(|tx| shortend_core::run(&job, tx, &CancelToken::new()));
    (r.expect("run"), ev)
}

pub fn resume(id: &str) -> (shortend_core::Result<JobReport>, Vec<Event>) {
    init_state_dir();
    with_events(|tx| shortend_core::resume(id, tx, &CancelToken::new()))
}

#[cfg(unix)]
pub fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}
