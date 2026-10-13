//! Shared harness for the GUI tests: the real app in egui_kittest (headless, no GPU), with
//! state and preset folders in a temporary directory.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use shortend_gui::ShortEndApp;
use shortend_gui::form::DestRow;

pub fn dirs() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test starts an engine thread.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("SHORTEND_STATE_DIR", dir.join("state"));
            std::env::set_var("SHORTEND_CONFIG_DIR", dir.join("config"));
        }
        dir
    })
}

pub fn harness() -> Harness<'static, ShortEndApp> {
    dirs();
    Harness::builder()
        .with_size(shortend_gui::egui::vec2(1000.0, 800.0))
        .build_state(|ctx, app: &mut ShortEndApp| app.ui(ctx), ShortEndApp::new())
}

/// Steps frames (sleeping between them) until `done` holds, or panics after `secs`.
pub fn wait_for(
    h: &mut Harness<'_, ShortEndApp>,
    secs: u64,
    what: &str,
    mut done: impl FnMut(&ShortEndApp) -> bool,
) {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        h.step();
        if done(h.state()) {
            h.step();
            return;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

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

pub fn fill_form(app: &mut ShortEndApp, source: &Path, dests: &[&Path]) {
    app.form.source = source.display().to_string();
    app.form.dests = dests
        .iter()
        .enumerate()
        .map(|(i, d)| DestRow {
            label: format!("D{}", i + 1),
            root: d.display().to_string(),
        })
        .collect();
    app.form.pdf = false; // keeps the tests quick; the PDF has its own tests
}

/// Relative path → contents of every file under `root`, skipping reports and manifests.
pub fn tree(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "_ShortEnd" || rel == "ascmhl" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    out
}
