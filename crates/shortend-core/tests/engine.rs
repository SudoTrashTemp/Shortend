//! Engine integration tests through the public library API (no CLI, no privileges).

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use common::*;
use shortend_core::{CancelToken, Error, Event, FileStatus, HashAlgorithm, HashList, JobState};

const SKIP: &[&str] = &["_ShortEnd"];

fn atimes(root: &Path) -> BTreeMap<String, SystemTime> {
    walk(root)
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            let t = std::fs::metadata(&p).unwrap().accessed().unwrap();
            (
                p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                t,
            )
        })
        .collect()
}

fn assert_no_staging(dest_root: &Path) {
    assert!(
        !dest_root.join(".shortend-staging").exists(),
        "staging left in {}",
        dest_root.display()
    );
}

#[test]
fn offload_to_three_destinations_verifies_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("A001");
    make_card(&src);
    let dests: Vec<_> = (1..=3)
        .map(|i| tmp.path().join(format!("dest{i}")))
        .collect();
    for d in &dests {
        std::fs::create_dir(d).unwrap();
    }
    let before = tree_state(&src, &[]);
    let before_dirs = dirs(&src, &[]);
    let before_atimes = atimes(&src);

    let refs: Vec<&Path> = dests.iter().map(|d| d.as_path()).collect();
    let (rep, events) = run(spec(&src, &refs));

    assert_eq!(
        rep.state,
        JobState::Completed,
        "{:#?}",
        rep.outcomes
            .iter()
            .filter(|o| !o.all_ok())
            .collect::<Vec<_>>()
    );
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.dests.iter().all(|d| d.status == FileStatus::Verified))
    );
    for d in &dests {
        let target = d.join("A001");
        assert_eq!(
            tree_state(&target, SKIP),
            before,
            "content/mtime mismatch in {}",
            d.display()
        );
        assert_eq!(dirs(&target, SKIP), before_dirs);
        assert_no_staging(d);
        let reports: Vec<_> = std::fs::read_dir(target.join("_ShortEnd"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(reports.len(), 2, "csv + txt");
        let csv = reports
            .iter()
            .find(|p| p.extension().unwrap() == "csv")
            .unwrap();
        let rows = std::fs::read_to_string(csv).unwrap().lines().count();
        assert_eq!(rows, 1 + before.len() * dests.len());
        let txt = reports
            .iter()
            .find(|p| p.extension().unwrap() == "txt")
            .unwrap();
        assert!(
            std::fs::read_to_string(txt)
                .unwrap()
                .contains("Result:       PASS")
        );
    }

    // The source is never modified, and on Linux not even its access times change.
    assert_eq!(tree_state(&src, &[]), before);
    assert_eq!(dirs(&src, &[]), before_dirs);
    if cfg!(target_os = "linux") {
        assert_eq!(atimes(&src), before_atimes);
    }

    let finished = events
        .iter()
        .filter(|e| matches!(e, Event::FileFinished(_)))
        .count();
    assert_eq!(finished, before.len());
    assert!(events.iter().any(|e| matches!(e, Event::Progress { .. })));
}

#[test]
fn tiny_chunks_and_high_parallelism_stay_correct() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let mut s = spec(&src, &[&d]);
    s.settings.chunk_size = 4096;
    s.settings.parallel_files = NonZeroU8::new(6).unwrap();
    let (rep, _) = run(s);
    assert_eq!(rep.state, JobState::Completed);
    assert_eq!(tree_state(&d.join("CARD"), SKIP), tree_state(&src, &[]));
}

#[test]
fn all_checksum_algorithms_in_one_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let mut s = spec(&src, &[&d]);
    s.settings.hashes = HashList::new(HashAlgorithm::ALL.to_vec()).unwrap();
    let (rep, _) = run(s);
    assert_eq!(rep.state, JobState::Completed);
    for o in &rep.outcomes {
        let src_d = o.source.as_ref().unwrap();
        assert!(src_d.same_algorithms(&rep.job.settings.hashes));
        assert_eq!(o.dests[0].verified.as_ref(), Some(src_d));
        let file = o.entry.rel.to_path(&src);
        assert_eq!(src_d.primary().value(), xxh64_file(&file));
    }
    let csv = std::fs::read_dir(d.join("CARD/_ShortEnd"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().unwrap() == "csv")
        .unwrap();
    let header = std::fs::read_to_string(csv.path())
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    for a in HashAlgorithm::ALL {
        assert!(
            header.contains(&format!("source_{a}")) && header.contains(&format!("verified_{a}")),
            "{header}"
        );
    }
}

#[test]
fn source_reread_records_the_second_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let mut s = spec(&src, &[&d]);
    s.settings.source_reread = true;
    let (rep, _) = run(s);
    assert_eq!(rep.state, JobState::Completed);
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.reread.is_some() && o.reread == o.source)
    );
}

fn refused(spec: shortend_core::JobSpec, needle: &str) {
    init_state_dir();
    match shortend_core::plan(spec) {
        Err(Error::Preflight(problems)) => {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "expected '{needle}' in {problems:#?}"
            );
        }
        other => panic!("expected refusal containing '{needle}', got {other:?}"),
    }
}

#[test]
fn preflight_refuses_unsafe_or_impossible_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();

    refused(spec(&tmp.path().join("missing"), &[&d]), "missing");
    refused(spec(&src, &[]), "no destinations");
    refused(spec(&src, &[&src.join("CLIP")]), "overlaps the source");
    refused(spec(&src, &[&d, &d]), "overlaps");

    let empty = tmp.path().join("EMPTY_CARD");
    std::fs::create_dir(&empty).unwrap();
    refused(spec(&empty, &[&d]), "contains no files");

    // Target folder already populated (an earlier offload of the same card).
    std::fs::create_dir_all(d.join("CARD")).unwrap();
    std::fs::write(d.join("CARD/old.mov"), b"x").unwrap();
    refused(spec(&src, &[&d]), "not empty");
    std::fs::remove_dir_all(d.join("CARD")).unwrap();

    #[cfg(unix)]
    {
        let link_dest = tmp.path().join("link_dest");
        std::os::unix::fs::symlink(&d, &link_dest).unwrap();
        refused(spec(&src, &[&link_dest]), "symbolic link");

        std::os::unix::fs::symlink("/etc/passwd", src.join("CLIP/evil")).unwrap();
        refused(spec(&src, &[&d]), "symbolic links are not copied");
        std::fs::remove_file(src.join("CLIP/evil")).unwrap();

        let fifo = src.join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        refused(spec(&src, &[&d]), "special file");
        std::fs::remove_file(&fifo).unwrap();
    }

    // Refusals leave nothing behind on the destination.
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
}

#[test]
fn cancel_then_resume_skips_already_verified_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    for i in 0..40 {
        write_file(&src.join(format!("C{i:03}.mov")), &noise(1_500_000, i));
    }
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let mut s = spec(&src, &[&d]);
    s.settings.chunk_size = 16 << 10;
    s.settings.parallel_files = NonZeroU8::new(1).unwrap();
    init_state_dir();
    let job = shortend_core::plan(s).unwrap();

    let cancel = CancelToken::new();
    let (tx, rx) = mpsc::sync_channel(64);
    let (job2, c2) = (job.clone(), cancel.clone());
    let worker = std::thread::spawn(move || shortend_core::run(&job2, &tx, &c2));
    let mut done = 0;
    for ev in rx {
        if matches!(ev, Event::FileFinished(_)) {
            done += 1;
            if done == 5 {
                cancel.cancel();
            }
        }
    }
    let rep = worker.join().unwrap().unwrap();
    assert_eq!(rep.state, JobState::Cancelled);
    assert_no_staging(&d);
    assert!(
        rep.report_files.is_empty(),
        "no reports for a cancelled job"
    );

    // Everything that reached its final path is complete and correct.
    let target = d.join("CARD");
    let partial = tree_state(&target, SKIP);
    let source = tree_state(&src, &[]);
    assert!(
        partial.len() >= 5 && partial.len() < 40,
        "{} files present",
        partial.len()
    );
    for (rel, st) in &partial {
        assert_eq!(Some(st), source.get(rel), "{rel}");
    }
    #[cfg(unix)]
    let inodes: BTreeMap<_, _> = partial
        .keys()
        .map(|r| (r.clone(), inode(&target.join(r))))
        .collect();

    let (rep, events) = resume(&job.id.to_string());
    let rep = rep.unwrap();
    assert_eq!(rep.state, JobState::Completed);
    let skipped = rep
        .outcomes
        .iter()
        .filter(|o| o.dests[0].status == FileStatus::AlreadyVerified)
        .count();
    assert_eq!(skipped, partial.len());
    assert!(events.iter().any(|e| matches!(e, Event::Started { already_verified, .. } if *already_verified == partial.len())));
    assert_eq!(tree_state(&target, SKIP), source);
    #[cfg(unix)]
    for (rel, ino) in inodes {
        assert_eq!(inode(&target.join(&rel)), ino, "{rel} was re-copied");
    }
    assert_no_staging(&d);
}

#[test]
fn resume_reproves_files_whose_journal_records_were_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let (d1, d2) = (tmp.path().join("d1"), tmp.path().join("d2"));
    std::fs::create_dir(&d1).unwrap();
    std::fs::create_dir(&d2).unwrap();
    let (rep, _) = run(spec(&src, &[&d1, &d2]));
    assert_eq!(rep.state, JobState::Completed);
    let id = rep.job.id.to_string();

    // Crash simulation: only the source records survived; commits and end are gone.
    let journal = Path::new(&std::env::var_os("SHORTEND_STATE_DIR").unwrap())
        .join("jobs")
        .join(&id)
        .join("journal.jsonl");
    let kept: String = std::fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .filter(|l| l.contains(r#""t":"start""#) || l.contains(r#""t":"src""#))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&journal, kept).unwrap();

    // One file tampered with on d1 (same size), one missing on d2.
    let tampered = d1.join("CARD/CLIP/A001C002_240101_R1AB.mxf");
    let len = std::fs::metadata(&tampered).unwrap().len() as usize;
    let mtime = std::fs::metadata(&tampered).unwrap().modified().unwrap();
    std::fs::write(&tampered, noise(len, 999)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&tampered)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::fs::remove_file(d2.join("CARD/empty.bin")).unwrap();
    #[cfg(unix)]
    let keep_ino = inode(&d1.join("CARD/CLIP/A001C001_240101_R1AB.mxf"));

    let (rep, _) = resume(&id);
    let rep = rep.unwrap();
    assert_eq!(rep.state, JobState::Completed);
    let source = tree_state(&src, &[]);
    assert_eq!(tree_state(&d1.join("CARD"), SKIP), source);
    assert_eq!(tree_state(&d2.join("CARD"), SKIP), source);
    let status = |dest: usize, rel: &str| {
        rep.outcomes
            .iter()
            .find(|o| o.entry.rel.as_str() == rel)
            .unwrap()
            .dests[dest]
            .status
            .clone()
    };
    assert_eq!(
        status(0, "CLIP/A001C002_240101_R1AB.mxf"),
        FileStatus::Verified,
        "tampered file recopied"
    );
    assert_eq!(
        status(1, "empty.bin"),
        FileStatus::Verified,
        "missing file recopied"
    );
    assert_eq!(
        status(0, "CLIP/A001C001_240101_R1AB.mxf"),
        FileStatus::AlreadyVerified
    );
    #[cfg(unix)]
    assert_eq!(
        inode(&d1.join("CARD/CLIP/A001C001_240101_R1AB.mxf")),
        keep_ino
    );
}

#[test]
fn resume_refuses_a_different_or_modified_card() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let (rep, _) = run(spec(&src, &[&d]));
    std::fs::write(src.join("new_clip.mov"), b"appeared later").unwrap();
    match resume(&rep.job.id.to_string()).0 {
        Err(Error::Preflight(p)) => {
            assert!(p.iter().any(|s| s.contains("no longer matches")), "{p:?}")
        }
        other => panic!("expected refusal, got {:?}", other.map(|r| r.state)),
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "Windows cannot rename a folder that contains open files"
)]
fn destination_disappearing_mid_job_is_isolated_and_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    for i in 0..30 {
        write_file(&src.join(format!("C{i:03}.mov")), &noise(1_000_000, i));
    }
    let (d1, d2) = (tmp.path().join("d1"), tmp.path().join("d2"));
    std::fs::create_dir(&d1).unwrap();
    std::fs::create_dir(&d2).unwrap();
    let mut s = spec(&src, &[&d1, &d2]);
    s.settings.chunk_size = 16 << 10;
    s.settings.parallel_files = NonZeroU8::new(1).unwrap();
    init_state_dir();
    let job = shortend_core::plan(s).unwrap();

    let (tx, rx) = mpsc::sync_channel(64);
    let job2 = job.clone();
    let worker = std::thread::spawn(move || shortend_core::run(&job2, &tx, &CancelToken::new()));
    let gone = tmp.path().join("d2_unplugged");
    let mut finished = 0;
    let mut dest_failed = false;
    for ev in rx {
        match ev {
            Event::FileFinished(_) => {
                finished += 1;
                if finished == 3 {
                    // "Unplug": the mount point's directory is no longer the volume we planned on.
                    std::fs::rename(&d2, &gone).unwrap();
                }
            }
            Event::DestinationFailed { dest, .. } => {
                assert_eq!(dest.0, 1);
                dest_failed = true;
            }
            _ => {}
        }
    }
    let rep = worker.join().unwrap().unwrap();
    assert!(dest_failed);
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.dests[0].status == FileStatus::Verified),
        "d1 unaffected"
    );
    assert!(rep.outcomes.iter().any(
        |o| o.dests[1].status == FileStatus::Failed(shortend_core::Failure::DestinationOffline)
    ));
    assert_eq!(tree_state(&d1.join("CARD"), SKIP), tree_state(&src, &[]));

    // "Replug" and resume: d2 is completed, d1 untouched.
    std::fs::rename(&gone, &d2).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    let (rep, _) = resume(&job.id.to_string());
    let rep = rep.unwrap();
    assert_eq!(rep.state, JobState::Completed);
    assert!(
        rep.outcomes
            .iter()
            .all(|o| o.dests[0].status == FileStatus::AlreadyVerified)
    );
    assert_eq!(tree_state(&d2.join("CARD"), SKIP), tree_state(&src, &[]));
    assert_no_staging(&d2);
}

/// A destination file damaged *after* it was verified — same size, same mtime, journal still
/// says "committed" — must be caught on resume and re-copied, never skipped.
#[test]
fn resume_catches_corruption_after_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let d = tmp.path().join("out");
    std::fs::create_dir(&d).unwrap();
    let (rep, _) = run(spec(&src, &[&d]));
    assert_eq!(rep.state, JobState::Completed);

    // Silent bit rot: flip one byte in the middle, keep size and mtime.
    let victim = d.join("CARD/CLIP/A001C001_240101_R1AB.mxf");
    let meta = std::fs::metadata(&victim).unwrap();
    let mut bytes = std::fs::read(&victim).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(&victim, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&victim)
        .unwrap()
        .set_modified(meta.modified().unwrap())
        .unwrap();
    assert_eq!(std::fs::metadata(&victim).unwrap().len(), meta.len());

    let (rep, _) = resume(&rep.job.id.to_string());
    let rep = rep.unwrap();
    assert_eq!(rep.state, JobState::Completed);
    let status = rep
        .outcomes
        .iter()
        .find(|o| o.entry.rel.as_str() == "CLIP/A001C001_240101_R1AB.mxf")
        .unwrap()
        .dests[0]
        .status
        .clone();
    assert_eq!(
        status,
        FileStatus::Verified,
        "corrupted file must be re-copied, not skipped"
    );
    assert_eq!(tree_state(&d.join("CARD"), SKIP), tree_state(&src, &[]));
}
