//! Linux device-level integration tests: real filesystems on loop devices, simulated hot-unplug
//! (device-mapper `error` target, lazy unmount), full disks, read-only remounts and permission
//! edge cases.
//!
//! These need root and are skipped unless `SHORTEND_PRIVILEGED_TESTS=1`. A fixture that the
//! machine can't provide (no `dmsetup`, no exFAT driver, …) is skipped with a message, or fails
//! the test when `SHORTEND_REQUIRE_ALL_FS=1` (set in CI so nothing is skipped silently).
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::num::NonZeroU8;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use shortend_core::{
    CancelToken, Event, Failure, FileStatus, FsType, HashAlgorithm, JobReport, JobSettings,
    JobSpec, JobState, ProductionInfo,
};

// ------------------------------------------------------------------------------------------
// fixtures

fn enabled() -> bool {
    std::env::var("SHORTEND_PRIVILEGED_TESTS").as_deref() == Ok("1") && is_root()
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}

fn require_all() -> bool {
    std::env::var("SHORTEND_REQUIRE_ALL_FS").as_deref() == Ok("1")
}

macro_rules! privileged {
    () => {
        if !enabled() {
            assert!(
                !require_all(),
                "SHORTEND_REQUIRE_ALL_FS=1 but not running as root with SHORTEND_PRIVILEGED_TESTS=1"
            );
            eprintln!("skipped: needs root and SHORTEND_PRIVILEGED_TESTS=1");
            return;
        }
    };
}

/// Surfaces a line as a GitHub Actions annotation (visible via the checks API) and on stderr.
/// Records a result line. With `SHORTEND_RESULTS_FILE` set (CI), lines are appended there and
/// a workflow step posts them as one annotation (GitHub keeps only 10 per step).
fn notice(msg: &str) {
    if let Some(path) = std::env::var_os("SHORTEND_RESULTS_FILE") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }
    eprintln!("{msg}");
}

/// Device tests share loop/dm devices and global state; run them one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn state_dir() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once before any engine call reads it.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("SHORTEND_STATE_DIR", &d)
        };
        d
    });
}

fn run(cmd: &str, args: &[&str]) -> Result<Output, String> {
    let o = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() {
        Ok(o)
    } else {
        Err(format!(
            "{cmd} {args:?}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

fn skip_or_fail(what: &str, why: String) {
    if require_all() {
        panic!("{what} unavailable but SHORTEND_REQUIRE_ALL_FS=1: {why}");
    }
    eprintln!("skipped {what}: {why}");
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fs {
    Ext4,
    ExFat,
    Vfat,
    Ntfs,
    /// HFS+, not journaled, case-insensitive (the macOS default before journaling).
    Hfs,
    /// HFS+, journaled (the macOS default "Mac OS Extended (Journaled)").
    HfsJournaled,
    /// HFSX: HFS+, case-sensitive.
    Hfsx,
}

impl Fs {
    fn mkfs(self, dev: &str) -> Result<Output, String> {
        match self {
            Fs::Ext4 => run("mkfs.ext4", &["-q", "-F", dev]),
            Fs::ExFat => run("mkfs.exfat", &[dev]),
            Fs::Vfat => run("mkfs.vfat", &["-F", "32", dev]),
            Fs::Ntfs => run("mkfs.ntfs", &["-Q", "-F", dev]),
            Fs::Hfs => run("mkfs.hfsplus", &["-v", "CARD", dev]),
            Fs::HfsJournaled => run("mkfs.hfsplus", &["-J", "-v", "CARD", dev]),
            Fs::Hfsx => run("mkfs.hfsplus", &["-s", "-v", "CARD", dev]),
        }
    }
    fn mount_type(self) -> &'static str {
        match self {
            Fs::Ext4 => "ext4",
            Fs::ExFat => "exfat",
            Fs::Vfat => "vfat",
            Fs::Ntfs => "ntfs3",
            Fs::Hfs | Fs::HfsJournaled | Fs::Hfsx => "hfsplus",
        }
    }
    /// FUSE implementation to fall back to when the kernel has no driver.
    fn fuse_type(self) -> Option<&'static str> {
        match self {
            Fs::ExFat => Some("exfat-fuse"),
            Fs::Ntfs => Some("ntfs-3g"),
            _ => None,
        }
    }
    /// The type ShortEnd must report, kernel driver or FUSE (it looks behind FUSE mounts).
    fn expected(self) -> FsType {
        match self {
            Fs::Ext4 => FsType::Ext4,
            Fs::ExFat => FsType::ExFat,
            Fs::Vfat => FsType::Vfat,
            Fs::Ntfs => FsType::Ntfs,
            Fs::Hfs | Fs::HfsJournaled | Fs::Hfsx => FsType::HfsPlus,
        }
    }
}

/// A filesystem on a loop device, optionally behind a device-mapper target we can break.
struct Volume {
    dir: PathBuf,
    loop_dev: String,
    dm: Option<(String, u64)>, // (name, sectors)
    fs: Fs,
    /// Mounted through FUSE because the kernel has no driver for `fs`.
    fuse: bool,
}

impl Volume {
    fn create(tmp: &Path, name: &str, size_mb: u64, fs: Fs, dm: bool) -> Option<Self> {
        let img = tmp.join(format!("{name}.img"));
        std::fs::File::create(&img)
            .unwrap()
            .set_len(size_mb << 20)
            .unwrap();
        let loop_dev = match run("losetup", &["-f", "--show", img.to_str().unwrap()]) {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            Err(e) => {
                skip_or_fail("loop devices", e);
                return None;
            }
        };
        let dir = tmp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut v = Volume {
            dir,
            loop_dev,
            dm: None,
            fs,
            fuse: false,
        };
        let dev = if dm {
            let dm_name = format!("shortend-test-{name}-{}", std::process::id());
            let sectors = size_mb * 2048;
            let table = format!("0 {sectors} linear {} 0", v.loop_dev);
            if let Err(e) = run("dmsetup", &["create", &dm_name, "--table", &table]) {
                skip_or_fail("device-mapper", e);
                return None; // Drop detaches the loop device
            }
            v.dm = Some((dm_name.clone(), sectors));
            format!("/dev/mapper/{dm_name}")
        } else {
            v.loop_dev.clone()
        };
        if let Err(e) = fs.mkfs(&dev) {
            skip_or_fail(&format!("{fs:?} filesystem"), e);
            return None;
        }
        let dir = v.dir.to_str().unwrap().to_owned();
        if let Err(e) = run("mount", &["-t", fs.mount_type(), &dev, &dir]) {
            match fs.fuse_type().map(|t| run("mount", &["-t", t, &dev, &dir])) {
                Some(Ok(_)) => v.fuse = true,
                _ => {
                    skip_or_fail(&format!("{fs:?} filesystem"), e);
                    return None;
                }
            }
        }
        Some(v)
    }

    fn dev_path(&self) -> String {
        match &self.dm {
            Some((n, _)) => format!("/dev/mapper/{n}"),
            None => self.loop_dev.clone(),
        }
    }

    /// Simulated hang: I/O to the device is queued and never completes until `unfreeze`.
    fn freeze(&self) {
        let (name, _) = self.dm.as_ref().expect("dm volume");
        run("dmsetup", &["suspend", "--noflush", "--nolockfs", name]).unwrap();
    }

    fn unfreeze(&self) {
        let (name, _) = self.dm.as_ref().expect("dm volume");
        run("dmsetup", &["resume", name]).unwrap();
    }

    /// Simulated cable pull: every further I/O to the device fails with EIO.
    fn break_device(&self) {
        let (name, sectors) = self.dm.as_ref().expect("dm volume");
        run("dmsetup", &["suspend", "--noflush", "--nolockfs", name]).unwrap();
        run(
            "dmsetup",
            &["load", name, "--table", &format!("0 {sectors} error")],
        )
        .unwrap();
        run("dmsetup", &["resume", name]).unwrap();
    }

    /// Simulated reconnect: restore the device, then remount (as a desktop would on replug).
    fn reconnect(&self) {
        let (name, sectors) = self.dm.as_ref().expect("dm volume");
        let _ = run("umount", &["-l", self.dir.to_str().unwrap()]);
        run("dmsetup", &["suspend", "--noflush", name]).unwrap();
        run(
            "dmsetup",
            &[
                "load",
                name,
                "--table",
                &format!("0 {sectors} linear {} 0", self.loop_dev),
            ],
        )
        .unwrap();
        run("dmsetup", &["resume", name]).unwrap();
        let dev = self.dev_path();
        if self.fs == Fs::Ext4 {
            let _ = Command::new("e2fsck").args(["-fy", &dev]).output();
        }
        run(
            "mount",
            &["-t", self.fs.mount_type(), &dev, self.dir.to_str().unwrap()],
        )
        .unwrap();
    }

    /// Remounts read-write even if journaled (HFS+ `force`), to populate a test card.
    fn remount_force_rw(&self) {
        let dir = self.dir.to_str().unwrap();
        let _ = run("umount", &[dir]);
        run(
            "mount",
            &[
                "-t",
                self.fs.mount_type(),
                "-o",
                "force,rw",
                &self.dev_path(),
                dir,
            ],
        )
        .unwrap();
    }

    fn remount_ro(&self) {
        run("mount", &["-o", "remount,ro", self.dir.to_str().unwrap()]).unwrap();
    }

    fn remount(&self) {
        run(
            "mount",
            &[
                "-t",
                self.fs.mount_type(),
                &self.dev_path(),
                self.dir.to_str().unwrap(),
            ],
        )
        .unwrap();
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let _ = Command::new("umount")
            .args(["-l", self.dir.to_str().unwrap()])
            .output();
        if let Some((n, _)) = &self.dm {
            let _ = Command::new("dmsetup").args(["resume", n]).output();
            let _ = Command::new("dmsetup").args(["remove", "-f", n]).output();
        }
        let _ = Command::new("losetup")
            .args(["-d", &self.loop_dev])
            .output();
    }
}

// ------------------------------------------------------------------------------------------
// helpers

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

fn card(root: &Path, files: u64, size: usize) {
    for i in 0..files {
        write(
            &root.join(format!("CLIP/C{i:03}.mov")),
            &noise(size + i as usize, i),
        );
    }
    write(&root.join("Ünïcödé clip.wav"), &noise(50_000, 7));
    write(&root.join("empty.bin"), b"");
    std::fs::create_dir_all(root.join("EMPTY")).unwrap();
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

fn tree(root: &Path) -> BTreeMap<String, (String, u64, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == "_ShortEnd" || rel == "ascmhl" {
                continue;
            }
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                out.insert(rel, (xxh64(&p), m.len(), m.modified().unwrap()));
            }
        }
    }
    out
}

/// Destination tree equals the source: same content and size, mtime within `tolerance`.
fn assert_copy(src: &Path, dst: &Path, tolerance: Duration) {
    let (a, b) = (tree(src), tree(dst));
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "file lists differ"
    );
    for (rel, (h, size, mtime)) in &a {
        let (h2, size2, mtime2) = &b[rel];
        assert_eq!((h, size), (h2, size2), "{rel}");
        let diff = mtime
            .duration_since(*mtime2)
            .or_else(|e| Ok::<_, ()>(e.duration()))
            .unwrap();
        assert!(diff <= tolerance, "{rel}: mtime off by {diff:?}");
    }
}

fn spec(src: &Path, dests: &[&Path]) -> JobSpec {
    JobSpec {
        source: src.to_path_buf(),
        card_label: None,
        destinations: dests
            .iter()
            .enumerate()
            .map(|(i, d)| (format!("D{}", i + 1), d.to_path_buf()))
            .collect(),
        folder: None,
        settings: JobSettings {
            chunk_size: 64 << 10,
            parallel_files: NonZeroU8::new(1).unwrap(),
            ..JobSettings::default()
        },
        production: ProductionInfo::default(),
    }
}

/// Runs a job in-process and calls `on_file(n)` after the n-th finished file.
fn run_job(spec: JobSpec, mut on_file: impl FnMut(usize)) -> (JobReport, Vec<Event>) {
    state_dir();
    let job = shortend_core::plan(spec).expect("plan");
    let (tx, rx) = mpsc::sync_channel(64);
    let worker = std::thread::spawn(move || shortend_core::run(&job, &tx, &CancelToken::new()));
    let mut events = Vec::new();
    let mut n = 0;
    for ev in rx {
        if matches!(ev, Event::FileFinished(_)) {
            n += 1;
            on_file(n);
        }
        events.push(ev);
    }
    (worker.join().unwrap().expect("run"), events)
}

fn resume(id: &str) -> JobReport {
    state_dir();
    let (tx, rx) = mpsc::sync_channel(64);
    let drain = std::thread::spawn(move || rx.into_iter().count());
    let r = shortend_core::resume(id, &tx, &CancelToken::new());
    drop(tx);
    drain.join().unwrap();
    r.expect("resume")
}

fn dest_failure(events: &[Event], dest: u16) -> Option<Failure> {
    events.iter().find_map(|e| match e {
        Event::DestinationFailed { dest: d, failure } if d.0 == dest => Some(failure.clone()),
        _ => None,
    })
}

// ------------------------------------------------------------------------------------------
// tests

#[test]
fn offload_onto_real_filesystems() {
    privileged!();
    let _g = serial();
    // GitHub keeps at most 10 notices per step, so results are reported as one line here.
    let mut results = Vec::new();
    for fs in [Fs::Ext4, Fs::ExFat, Fs::Vfat, Fs::Ntfs, Fs::Hfs, Fs::Hfsx] {
        let tmp = tempfile::tempdir().unwrap();
        let Some(vol) = Volume::create(tmp.path(), "dst", 256, fs, false) else {
            continue;
        };
        let src = tmp.path().join("A001");
        card(&src, 12, 1_000_000);
        let (rep, _) = run_job(spec(&src, &[&vol.dir]), |_| {});
        assert_eq!(rep.state, JobState::Completed, "{fs:?}");
        assert_eq!(rep.job.destinations[0].volume.fs_type, fs.expected());
        let tolerance = rep.job.destinations[0].volume.fs_type.mtime_granularity();
        assert_copy(&src, &vol.dir.join("A001"), tolerance);
        assert!(!vol.dir.join(".shortend-staging").exists());
        results.push(format!(
            "{fs:?}{} ok (case-sensitive: {})",
            if vol.fuse { " via FUSE" } else { "" },
            rep.job.destinations[0].volume.case_sensitive
        ));
    }
    notice(&format!("filesystems: {}", results.join("; ")));
}

#[test]
fn hot_unplug_of_a_destination_isolates_it_and_resumes_after_reconnect() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "usb", 256, Fs::Ext4, true) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A002");
    card(&src, 40, 1_500_000);

    let (rep, events) = run_job(spec(&src, &[&vol.dir, &other]), |n| {
        if n == 5 {
            vol.break_device();
        }
    });
    assert_eq!(
        dest_failure(&events, 0),
        Some(Failure::DestinationOffline),
        "unplugged destination reported"
    );
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.dests[1].status == FileStatus::Verified),
        "other destination unaffected"
    );
    assert_copy(&src, &other.join("A002"), Duration::ZERO);

    vol.reconnect();
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A002"), Duration::ZERO);
    notice("hot_unplug_of_a_destination_isolates_it_and_resumes_after_reconnect: passed");
}

#[test]
fn hot_unplug_of_the_source_stops_the_job_and_resumes_after_reconnect() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(card_vol) = Volume::create(tmp.path(), "card", 256, Fs::Ext4, true) else {
        return;
    };
    let src = card_vol.dir.join("A003");
    card(&src, 40, 1_500_000);
    run("sync", &[]).unwrap();
    // Make reads hit the device rather than the page cache.
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    let dst = tmp.path().join("raid");
    std::fs::create_dir(&dst).unwrap();

    let (rep, _) = run_job(spec(&src, &[&dst]), |n| {
        if n == 5 {
            card_vol.break_device();
        }
    });
    assert_eq!(
        rep.state,
        JobState::Aborted,
        "job stops when the card is pulled"
    );
    assert!(rep.report_files.is_empty());
    for (rel, (h, size, _)) in tree(&dst.join("A003")) {
        // Whatever reached its final path is complete; nothing half-written.
        assert_eq!(
            std::fs::metadata(dst.join("A003").join(&rel))
                .unwrap()
                .len(),
            size
        );
        assert!(!h.is_empty());
    }

    card_vol.reconnect();
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &dst.join("A003"), Duration::ZERO);
    notice("hot_unplug_of_the_source_stops_the_job_and_resumes_after_reconnect: passed");
}

#[test]
fn hung_destination_is_dropped_by_the_stall_watchdog() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "hung", 256, Fs::Ext4, true) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A007");
    card(&src, 20, 1_500_000);

    let mut s = spec(&src, &[&vol.dir, &other]);
    s.settings.stall_timeout_secs = 3;
    let t0 = Instant::now();
    let (rep, events) = run_job(s, |n| {
        if n == 3 {
            vol.freeze();
        }
    });
    let took = t0.elapsed();
    assert_eq!(
        dest_failure(&events, 0),
        Some(Failure::DestinationStalled { seconds: 3 }),
        "frozen destination reported as stalled"
    );
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert!(took < Duration::from_secs(120), "job took {took:?}");
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.dests[1].status == FileStatus::Verified),
        "other destination unaffected"
    );
    assert_copy(&src, &other.join("A007"), Duration::ZERO);

    vol.unfreeze();
    std::thread::sleep(Duration::from_secs(1)); // let the abandoned writer finish or fail
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A007"), Duration::ZERO);
    notice(&format!(
        "hung_destination_is_dropped_by_the_stall_watchdog: passed (job finished in {:.1} s)",
        took.as_secs_f64()
    ));
}

/// The common real-world sequence: a drive hangs, and the DIT resumes the job. The resume's
/// re-verification of what was already copied must not hang the job.
#[test]
fn drive_hanging_during_resume_recheck_is_dropped() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "recheck", 256, Fs::Ext4, true) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A008");
    card(&src, 12, 1_500_000);
    let mut s = spec(&src, &[&vol.dir, &other]);
    s.settings.stall_timeout_secs = 3;
    let (rep, _) = run_job(s, |_| {});
    assert_eq!(rep.state, JobState::Completed);
    run("sync", &[]).unwrap();
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");

    vol.freeze();
    let t0 = Instant::now();
    state_dir();
    let (tx, rx) = mpsc::sync_channel(64);
    let drain = std::thread::spawn(move || rx.into_iter().collect::<Vec<_>>());
    let r =
        shortend_core::resume(&rep.job.id.to_string(), &tx, &CancelToken::new()).expect("resume");
    drop(tx);
    let events = drain.join().unwrap();
    let took = t0.elapsed();
    vol.unfreeze();

    assert_eq!(
        dest_failure(&events, 0),
        Some(Failure::DestinationStalled { seconds: 3 })
    );
    assert_eq!(r.state, JobState::CompletedWithFailures);
    assert!(took < Duration::from_secs(60), "resume took {took:?}");
    assert!(
        r.outcomes
            .iter()
            .all(|o| o.dests[1].status == FileStatus::AlreadyVerified)
    );

    std::thread::sleep(Duration::from_secs(1));
    let r = resume(&rep.job.id.to_string());
    assert_eq!(r.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A008"), Duration::ZERO);
    notice(&format!(
        "drive_hanging_during_resume_recheck_is_dropped: passed (resume finished in {:.1} s)",
        took.as_secs_f64()
    ));
}

#[test]
fn lazy_unmount_mid_copy_is_detected_before_commit() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "shuttle", 256, Fs::Ext4, false) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A004");
    card(&src, 30, 1_500_000);

    let (rep, events) = run_job(spec(&src, &[&vol.dir, &other]), |n| {
        if n == 3 {
            run("umount", &["-l", vol.dir.to_str().unwrap()]).unwrap();
        }
    });
    assert_eq!(dest_failure(&events, 0), Some(Failure::DestinationOffline));
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert_copy(&src, &other.join("A004"), Duration::ZERO);
    // Nothing was written into the bare mount-point directory underneath.
    assert_eq!(std::fs::read_dir(&vol.dir).unwrap().count(), 0);

    vol.remount();
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A004"), Duration::ZERO);
    notice("lazy_unmount_mid_copy_is_detected_before_commit: passed");
}

#[test]
fn disk_filling_up_mid_job_fails_that_destination_cleanly() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "small", 128, Fs::Ext4, false) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A005");
    card(&src, 30, 1_500_000); // ~45 MB: passes preflight on a ~110 MB volume
    let filler = vol.dir.join("someone-elses-file");

    let (rep, events) = run_job(spec(&src, &[&vol.dir, &other]), |n| {
        if n == 2 {
            // Another program fills the drive.
            let mut f = std::fs::File::create(&filler).unwrap();
            let block = vec![0xA5u8; 1 << 20];
            while f.write_all(&block).is_ok() {}
            let _ = f.sync_all();
        }
    });
    assert_eq!(dest_failure(&events, 0), Some(Failure::DiskFull));
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert_copy(&src, &other.join("A005"), Duration::ZERO);
    let staged = vol.dir.join(".shortend-staging");
    assert!(
        !staged.exists() || std::fs::read_dir(&staged).unwrap().count() == 0,
        "no .part left behind"
    );
    // Every file that made it to its final path is complete and correct.
    let source = tree(&src);
    for (rel, v) in tree(&vol.dir.join("A005")) {
        assert_eq!(Some(&v.0), source.get(&rel).map(|s| &s.0), "{rel}");
    }

    std::fs::remove_file(&filler).unwrap();
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A005"), Duration::ZERO);
    notice("disk_filling_up_mid_job_fails_that_destination_cleanly: passed");
}

#[test]
fn read_only_remount_mid_job_fails_that_destination() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "rofs", 256, Fs::Ext4, false) else {
        return;
    };
    let other = tmp.path().join("raid");
    std::fs::create_dir(&other).unwrap();
    let src = tmp.path().join("A006");
    card(&src, 40, 1_000_000);
    let dir = vol.dir.to_str().unwrap().to_owned();

    let (rep, events) = run_job(spec(&src, &[&vol.dir, &other]), |n| {
        if n == 3 {
            // The kernel refuses while a file is open for writing; retry into the gap between files.
            let deadline = Instant::now() + Duration::from_secs(20);
            while run("mount", &["-o", "remount,ro", &dir]).is_err() {
                assert!(Instant::now() < deadline, "could not remount read-only");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    });
    assert_eq!(dest_failure(&events, 0), Some(Failure::PermissionDenied));
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert_copy(&src, &other.join("A006"), Duration::ZERO);

    run("mount", &["-o", "remount,rw", &dir]).unwrap();
    let rep = resume(&rep.job.id.to_string());
    assert_eq!(rep.state, JobState::Completed);
    assert_copy(&src, &vol.dir.join("A006"), Duration::ZERO);
    notice("read_only_remount_mid_job_fails_that_destination: passed");
}

/// Permission edge cases, exercised through the CLI as an unprivileged user.
#[test]
fn permission_edge_cases_as_unprivileged_user() {
    privileged!();
    let _g = serial();
    if run("setpriv", &["--help"]).is_err() {
        skip_or_fail("setpriv", "not installed".into());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
    let src = tmp.path().join("CARD");
    card(&src, 3, 10_000);
    let open_dest = tmp.path().join("open");
    std::fs::create_dir(&open_dest).unwrap();
    std::fs::set_permissions(&open_dest, std::fs::Permissions::from_mode(0o777)).unwrap();
    let root_dest = tmp.path().join("root-only");
    std::fs::create_dir(&root_dest).unwrap(); // 0755, owned by root

    let as_nobody = |args: &[&Path]| {
        let mut c = Command::new("setpriv");
        c.args([
            "--reuid=65534",
            "--regid=65534",
            "--clear-groups",
            env!("CARGO_BIN_EXE_shortend"),
            "copy",
            "-s",
        ])
        .arg(args[0]);
        for d in &args[1..] {
            c.arg("-d").arg(d);
        }
        c.env("SHORTEND_STATE_DIR", &state).output().unwrap()
    };

    // A destination the user cannot write to is refused before anything is copied.
    let o = as_nobody(&[&src, &root_dest]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("not writable"));
    assert_eq!(std::fs::read_dir(&root_dest).unwrap().count(), 0);

    // An unreadable folder on the card is refused at preflight.
    let locked = src.join("LOCKED");
    std::fs::create_dir(&locked).unwrap();
    write(&locked.join("secret.mov"), b"x");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let o = as_nobody(&[&src, &open_dest]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot read"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&locked).unwrap();

    // An unreadable file fails on its own; everything else is verified (exit 1).
    let unreadable = src.join("CLIP/C001.mov");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
    let o = as_nobody(&[&src, &open_dest]);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("CLIP/C001.mov"));
    assert!(open_dest.join("CARD/CLIP/C000.mov").exists());
    assert!(!open_dest.join("CARD/CLIP/C001.mov").exists());
    notice("permission_edge_cases_as_unprivileged_user: passed");
}

#[test]
fn case_collisions_and_illegal_names_are_refused_on_exfat() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "exfat", 64, Fs::ExFat, false) else {
        return;
    };
    let src = tmp.path().join("CARD");
    write(&src.join("CLIP/A001.MOV"), b"one");
    write(&src.join("clip/a001.mov"), b"two");
    write(&src.join("notes: day 1.txt"), b"three");
    state_dir();
    match shortend_core::plan(spec(&src, &[&vol.dir])) {
        Err(shortend_core::Error::Preflight(p)) => {
            let all = p.join("\n");
            assert!(all.contains("would collide"), "{all}");
            assert!(all.contains("not allowed"), "{all}");
        }
        other => panic!("expected refusal, got {:?}", other.map(|j| j.id)),
    }
    assert_eq!(
        std::fs::read_dir(&vol.dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() != "System Volume Information")
            .count(),
        0
    );
    notice("case_collisions_and_illegal_names_are_refused_on_exfat: passed");
}

// ------------------------------------------------------------------------------------------
// Mac-formatted drives

fn plan_err(spec: JobSpec) -> Vec<String> {
    state_dir();
    match shortend_core::plan(spec) {
        Err(shortend_core::Error::Preflight(p)) => p,
        other => panic!("expected refusal, got {:?}", other.map(|j| j.id)),
    }
}

/// HFS+ as a source (journaled, mounted read-only as Linux does) and as a destination
/// (non-journaled), including resume (HFS+ stores whole-second times) and verification of
/// a decomposed-Unicode name (HFS+ returns names in NFD).
#[test]
fn hfs_plus_source_and_destination() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();

    // Journaled HFS+ card: populated with `force`, then mounted read-only.
    let Some(card_vol) = Volume::create(tmp.path(), "hfscard", 128, Fs::HfsJournaled, false) else {
        return;
    };
    card_vol.remount_force_rw();
    let src = card_vol.dir.join("A009");
    card(&src, 8, 700_000);
    write(&src.join("Caf\u{e9} clip.mov"), &noise(5_000, 42));
    run("sync", &[]).unwrap();
    card_vol.remount_ro();

    let Some(dst) = Volume::create(tmp.path(), "hfsdst", 128, Fs::Hfs, false) else {
        return;
    };
    let mut s = spec(&src, &[&dst.dir]);
    s.settings.reports.mhl = vec![shortend_core::report::mhl::MhlVersion::V2];
    let (rep, _) = run_job(s, |_| {});
    assert_eq!(rep.state, JobState::Completed, "{:?}", rep.warnings);
    assert_eq!(rep.job.source.volume.fs_type, FsType::HfsPlus);
    assert_eq!(rep.job.destinations[0].volume.fs_type, FsType::HfsPlus);
    assert!(
        !rep.job.destinations[0].volume.case_sensitive,
        "HFS+ is case-insensitive"
    );
    assert_copy(&src, &dst.dir.join("A009"), Duration::from_secs(1));
    let v = shortend_core::report::mhl::verify_folder(&dst.dir.join("A009")).unwrap();
    assert!(v[0].ok(), "ShortEnd verify on HFS+: {:?}", v[0].issues);

    // Resume re-verifies everything and copies nothing again.
    let r = resume(&rep.job.id.to_string());
    assert_eq!(r.state, JobState::Completed);
    assert!(
        r.outcomes
            .iter()
            .all(|o| o.dests[0].status == FileStatus::AlreadyVerified)
    );
    notice(
        "hfs_plus_source_and_destination: passed (journaled read-only source, non-journaled destination, resume, NFD names)",
    );
}

/// A journaled HFS+ drive is mounted read-only by Linux; as a destination ShortEnd refuses it
/// with an explanation instead of failing on the first write.
#[test]
fn journaled_hfs_plus_destination_is_refused_clearly() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(vol) = Volume::create(tmp.path(), "hfsj", 64, Fs::HfsJournaled, false) else {
        return;
    };
    let src = tmp.path().join("CARD");
    card(&src, 2, 1_000);
    let p = plan_err(spec(&src, &[&vol.dir]));
    let all = p.join("\n");
    assert!(all.contains("HFS+") && all.contains("journal"), "{all}");
    notice("journaled_hfs_plus_destination_is_refused_clearly: passed");
}

/// Names that only differ in case collide on HFS+; names that only differ in Unicode
/// normalization collide even on case-sensitive HFSX.
#[test]
fn hfs_plus_name_collisions_are_refused() {
    privileged!();
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let Some(hfs) = Volume::create(tmp.path(), "hfsci", 64, Fs::Hfs, false) else {
        return;
    };
    let Some(hfsx) = Volume::create(tmp.path(), "hfsx", 64, Fs::Hfsx, false) else {
        return;
    };
    let case = tmp.path().join("CASE");
    write(&case.join("CLIP/A001.MOV"), b"1");
    write(&case.join("CLIP/a001.mov"), b"2");
    let all = plan_err(spec(&case, &[&hfs.dir])).join("\n");
    assert!(all.contains("would collide"), "{all}");
    let mut s = spec(&case, &[&hfsx.dir]);
    s.settings.reports.mhl = Vec::new();
    state_dir();
    assert!(shortend_core::plan(s).is_ok(), "HFSX is case-sensitive");

    let norm = tmp.path().join("NORM");
    write(&norm.join("caf\u{e9}.wav"), b"composed");
    write(&norm.join("cafe\u{301}.wav"), b"decomposed");
    let all = plan_err(spec(&norm, &[&hfsx.dir])).join("\n");
    assert!(
        all.contains("Unicode normalization") && all.contains("would collide"),
        "{all}"
    );
    notice("hfs_plus_name_collisions_are_refused: passed (case on HFS+, normalization on HFSX)");
}

/// Permissions on HFS+ (it stores Unix owners and modes): an unwritable destination folder is
/// refused at preflight, an unreadable card file fails on its own.
#[test]
fn hfs_plus_permission_edge_cases() {
    privileged!();
    let _g = serial();
    if run("setpriv", &["--help"]).is_err() {
        skip_or_fail("setpriv", "not installed".into());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let Some(vol) = Volume::create(tmp.path(), "hfsperm", 64, Fs::Hfs, false) else {
        return;
    };
    let state = tmp.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
    let src = vol.dir.join("CARD");
    card(&src, 3, 10_000);
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
    let locked_dest = vol.dir.join("root-only");
    std::fs::create_dir(&locked_dest).unwrap();
    std::fs::set_permissions(&locked_dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let open_dest = tmp.path().join("open");
    std::fs::create_dir(&open_dest).unwrap();
    std::fs::set_permissions(&open_dest, std::fs::Permissions::from_mode(0o777)).unwrap();
    let as_nobody = |src: &Path, dest: &Path| {
        Command::new("setpriv")
            .args([
                "--reuid=65534",
                "--regid=65534",
                "--clear-groups",
                env!("CARGO_BIN_EXE_shortend"),
                "copy",
                "--no-pdf",
                "-s",
            ])
            .arg(src)
            .arg("-d")
            .arg(dest)
            .env("SHORTEND_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let o = as_nobody(&src, &locked_dest);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("not writable"));

    std::fs::set_permissions(
        src.join("CLIP/C001.mov"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let o = as_nobody(&src, &open_dest);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("CLIP/C001.mov"));
    assert!(open_dest.join("CARD/CLIP/C000.mov").exists());
    notice("hfs_plus_permission_edge_cases: passed");
}

/// APFS: Linux can only read it (apfs-fuse). As a source it is recognised behind FUSE; as a
/// destination ShortEnd refuses early, naming APFS and what to do instead.
#[test]
fn apfs_read_only_source_and_refused_destination() {
    privileged!();
    let _g = serial();
    let find = |var: &str, name: &str| {
        std::env::var_os(var).map(PathBuf::from).or_else(|| {
            Command::new("which")
                .arg(name)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        })
    };
    let (Some(mkapfs), Some(apfs_fuse)) = (
        find("SHORTEND_MKAPFS", "mkapfs"),
        find("SHORTEND_APFS_FUSE", "apfs-fuse"),
    ) else {
        skip_or_fail(
            "APFS",
            "mkapfs and apfs-fuse not found (set SHORTEND_MKAPFS / SHORTEND_APFS_FUSE)".into(),
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("apfs.img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(64 << 20)
        .unwrap();
    run(
        mkapfs.to_str().unwrap(),
        &["-L", "MACDRIVE", img.to_str().unwrap()],
    )
    .unwrap();
    let mnt = tmp.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();

    // Populate the image like a Mac would, through the experimental linux-apfs-rw module
    // (CI builds and loads it); then detach it and read it back the supported way.
    let populated = run(
        "mount",
        &[
            "-t",
            "apfs",
            "-o",
            "loop,readwrite",
            img.to_str().unwrap(),
            mnt.to_str().unwrap(),
        ],
    )
    .map(|_| {
        card(&mnt.join("CARD"), 6, 300_000);
        write(&mnt.join("CARD/Caf\u{e9} take 2.wav"), &noise(4_000, 7));
        run("umount", &[mnt.to_str().unwrap()]).unwrap();
    });
    if let Err(e) = &populated {
        skip_or_fail(
            "APFS source",
            format!("cannot write the test image (linux-apfs-rw module not loaded?): {e}"),
        );
    }
    run(
        apfs_fuse.to_str().unwrap(),
        &[
            "-o",
            "allow_other",
            img.to_str().unwrap(),
            mnt.to_str().unwrap(),
        ],
    )
    .unwrap();
    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).output();
        }
    }
    let _u = Unmount(mnt.clone());

    let probe = shortend_core::fs::platform::volume_probe(&mnt.join("root")).unwrap();
    assert_eq!(probe.fs_type, FsType::Apfs, "APFS recognised behind FUSE");
    assert!(probe.read_only);

    // Read-only APFS source through apfs-fuse.
    let mut source_note = "source not tested (image could not be populated)";
    if populated.is_ok() {
        let apfs_card = mnt.join("root/CARD");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let mut s = spec(&apfs_card, &[&out]);
        s.settings.reports.mhl = vec![shortend_core::report::mhl::MhlVersion::V2];
        let (rep, _) = run_job(s, |_| {});
        assert_eq!(rep.state, JobState::Completed, "{:?}", rep.warnings);
        assert_eq!(rep.job.source.volume.fs_type, FsType::Apfs);
        assert_eq!(rep.outcomes.len(), 9);
        assert_copy(&apfs_card, &out.join("CARD"), Duration::ZERO);
        let v = shortend_core::report::mhl::verify_folder(&out.join("CARD")).unwrap();
        assert!(v[0].ok(), "{:?}", v[0].issues);
        source_note = "9-file offload from read-only APFS source";
    }

    let src = tmp.path().join("CARD");
    card(&src, 2, 1_000);
    let all = plan_err(spec(&src, &[&mnt.join("root")])).join("\n");
    assert!(
        all.contains("APFS") && all.contains("read-only") && all.contains("exFAT"),
        "{all}"
    );
    notice(&format!(
        "apfs_read_only_source_and_refused_destination: passed (apfs-fuse; {source_note}; destination refused)"
    ));
}
