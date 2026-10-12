//! End-to-end tests of the `shortend` binary: exit codes, injected corruption, SIGKILL +
//! resume, and bounded memory.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shortend_core::HashAlgorithm;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_shortend"))
}

struct Env {
    tmp: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
        }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.tmp.path().join(p)
    }
    fn cmd(&self) -> Command {
        let mut c = bin();
        c.env("SHORTEND_STATE_DIR", self.path("state"))
            .env_remove("SHORTEND_FAULT");
        c
    }
    fn mkdirs(&self, names: &[&str]) {
        for n in names {
            std::fs::create_dir_all(self.path(n)).unwrap();
        }
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
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

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn xxh64(path: &Path) -> String {
    let mut h = shortend_core::hash::Hasher::new(HashAlgorithm::Xxh64);
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            return h.finish().value();
        }
        h.update(&buf[..n]);
    }
}

/// rel path → (checksum, size) for regular files, ignoring the report folder.
fn tree(root: &Path) -> BTreeMap<String, (String, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "_ShortEnd" {
                continue;
            }
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                out.insert(rel, (xxh64(&p), m.len()));
            }
        }
    }
    out
}

fn ok(o: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn preflight_refusal_exits_2() {
    let env = Env::new();
    env.mkdirs(&["dest"]);
    let o = env
        .cmd()
        .args(["copy", "--source"])
        .arg(env.path("nope"))
        .arg("--dest")
        .arg(env.path("dest"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{}", ok(&o));
    assert!(String::from_utf8_lossy(&o.stderr).contains("job refused"));
}

#[test]
fn successful_copy_exits_0_and_lists_job() {
    let env = Env::new();
    write(&env.path("CARD/a.mov"), &noise(300_000, 1));
    write(&env.path("CARD/sub/b.wav"), &noise(10_000, 2));
    env.mkdirs(&["d1", "d2"]);
    let o = env
        .cmd()
        .args(["copy", "-s"])
        .arg(env.path("CARD"))
        .arg("-d")
        .arg(format!("RAID={}", env.path("d1").display()))
        .arg("-d")
        .arg(env.path("d2"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", ok(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("PASS") && out.contains("RAID"), "{out}");
    assert_eq!(tree(&env.path("d1/CARD")), tree(&env.path("CARD")));
    assert_eq!(tree(&env.path("d2/CARD")), tree(&env.path("CARD")));

    let o = env.cmd().arg("jobs").output().unwrap();
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("[completed]"),
        "{}",
        ok(&o)
    );
}

#[test]
fn corruption_on_one_destination_is_detected_and_exits_1() {
    let env = Env::new();
    for i in 0..5 {
        write(
            &env.path(&format!("CARD/c{i}.mov")),
            &noise(200_000 + i, i as u64),
        );
    }
    env.mkdirs(&["good", "bad"]);
    let o = env
        .cmd()
        .env("SHORTEND_FAULT", "corrupt-dest=1")
        .args(["copy", "-s"])
        .arg(env.path("CARD"))
        .arg("-d")
        .arg(env.path("good"))
        .arg("-d")
        .arg(env.path("bad"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", ok(&o));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(
        tree(&env.path("good/CARD")),
        tree(&env.path("CARD")),
        "healthy destination unaffected"
    );
    assert!(
        tree(&env.path("bad/CARD")).is_empty(),
        "no unverified file may reach its final path"
    );
    assert!(!env.path("bad/.shortend-staging").exists());

    let report_dir = env.path("good/CARD/_ShortEnd");
    let csv = std::fs::read_dir(&report_dir)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().unwrap() == "csv")
        .unwrap();
    let csv = std::fs::read_to_string(csv.path()).unwrap();
    assert_eq!(
        csv.lines()
            .filter(|l| l.contains(r#",failed,"checksum mismatch"#))
            .count(),
        5,
        "{csv}"
    );
    let txt = std::fs::read_dir(&report_dir)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().unwrap() == "txt")
        .unwrap();
    assert!(
        std::fs::read_to_string(txt.path())
            .unwrap()
            .contains("Result:       FAIL")
    );
}

/// Kills `shortend copy` with SIGKILL at seeded random moments, then resumes. After every
/// resume both destinations must match the source exactly, with nothing left in staging.
#[test]
fn sigkill_at_random_points_then_resume() {
    let iterations: u64 = std::env::var("SHORTEND_KILL_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let env = Env::new();
    for i in 0..24u64 {
        write(
            &env.path(&format!("CARD/CLIP/C{i:03}.mov")),
            &noise(64_000 + (i as usize * 97_531) % 2_000_000, i),
        );
    }
    write(&env.path("CARD/BIG.mxf"), &noise(12_000_000, 99));
    write(&env.path("CARD/empty.txt"), b"");
    let source = tree(&env.path("CARD"));

    let copy = |d1: &Path, d2: &Path| {
        let mut c = env.cmd();
        c.args(["copy", "--chunk-mib", "1", "-s"])
            .arg(env.path("CARD"))
            .arg("-d")
            .arg(d1)
            .arg("-d")
            .arg(d2);
        c
    };

    // Baseline duration bounds the kill delays.
    env.mkdirs(&["base1", "base2"]);
    let t0 = Instant::now();
    let o = copy(&env.path("base1"), &env.path("base2"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", ok(&o));
    let full = t0.elapsed().as_millis().max(50) as u64;

    let mut mid_copy = 0;
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    for it in 0..iterations {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // Mostly within the copy itself (planning is the first ~10 %), some just past the end.
        let delay = full / 10 + rng % full;
        let (d1, d2) = (env.path(&format!("k{it}/a")), env.path(&format!("k{it}/b")));
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();

        let mut child = copy(&d1, &d2)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(delay));
        let _ = child.kill(); // SIGKILL on Unix
        let status = child.wait().unwrap();
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();

        let Some(id) = stdout
            .lines()
            .find_map(|l| l.strip_prefix("Job "))
            .map(str::to_owned)
        else {
            // Killed while planning: planning must leave nothing behind.
            assert!(
                !d1.join("CARD").exists() && !d1.join(".shortend-staging").exists(),
                "iteration {it}"
            );
            continue;
        };
        if status.success() {
            assert_eq!(tree(&d1.join("CARD")), source);
            continue;
        }
        let partial = tree(&d1.join("CARD"));
        // Whatever reached a final path before the kill must already be complete and correct.
        for (rel, v) in &partial {
            assert_eq!(
                Some(v),
                source.get(rel),
                "iteration {it}: {rel} is wrong after kill"
            );
        }
        if !partial.is_empty() && partial.len() < source.len() {
            mid_copy += 1;
        }

        let o = env.cmd().args(["resume", &id]).output().unwrap();
        let text = ok(&o);
        if o.status.code() == Some(2) && text.contains("no job matches") {
            // Killed between printing the plan and persisting the job: nothing was copied.
            assert!(partial.is_empty(), "iteration {it}");
            continue;
        }
        assert_eq!(
            o.status.code(),
            Some(0),
            "iteration {it} (killed after {delay} ms): {text}"
        );
        assert_eq!(tree(&d1.join("CARD")), source, "iteration {it}");
        assert_eq!(tree(&d2.join("CARD")), source, "iteration {it}");
        assert!(!d1.join(".shortend-staging").exists() && !d2.join(".shortend-staging").exists());
    }
    let msg = format!("{mid_copy}/{iterations} kills landed mid-copy (baseline {full} ms)");
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        println!("\n::notice title=sigkill-resume::{msg}");
    }
    eprintln!("{msg}");
    assert!(
        mid_copy * 5 >= iterations,
        "too few kills landed mid-copy ({mid_copy}/{iterations}); timing is off"
    );
}

/// Peak RSS stays bounded for a large single file. Size from SHORTEND_RSS_TEST_GB (default 4).
/// Ignored by default (it writes 2 × size to disk); CI runs it explicitly.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "writes several GB; run with --ignored"]
fn peak_rss_is_bounded_for_huge_files() {
    let gb: u64 = std::env::var("SHORTEND_RSS_TEST_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let env = Env::new();
    std::fs::create_dir_all(env.path("CARD")).unwrap();
    // Sparse source: reads cost no disk, writes are real.
    std::fs::File::create(env.path("CARD/huge.mxf"))
        .unwrap()
        .set_len(gb << 30)
        .unwrap();
    env.mkdirs(&["d1", "d2"]);
    let mut child = env
        .cmd()
        .args(["copy", "-s"])
        .arg(env.path("CARD"))
        .arg("-d")
        .arg(env.path("d1"))
        .arg("-d")
        .arg(env.path("d2"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status_path = format!("/proc/{}/status", child.id());
    let mut peak_kb = 0u64;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            assert!(st.success(), "copy failed: {st:?}");
            break;
        }
        if let Ok(s) = std::fs::read_to_string(&status_path)
            && let Some(line) = s.lines().find(|l| l.starts_with("VmHWM:"))
        {
            let kb: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            peak_kb = peak_kb.max(kb);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let msg = format!(
        "peak RSS {} MB for a {gb} GB file to 2 destinations",
        peak_kb / 1024
    );
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        println!("\n::notice title=memory-bound::{msg}");
    }
    eprintln!("{msg}");
    assert!(peak_kb > 0, "never sampled");
    assert!(peak_kb < 100 * 1024, "peak RSS {peak_kb} kB exceeds 100 MB");
}
