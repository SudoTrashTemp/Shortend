//! Folder templates through the engine: `{seq}` numbering across destinations that disagree
//! about which numbers exist, concurrent jobs, resume, and refusals.

mod common;

use std::path::Path;

use common::*;
use shortend_core::{CancelToken, Error, FileStatus, JobState, ProductionInfo};

fn production() -> ProductionInfo {
    ProductionInfo {
        show: Some("Big Show".into()),
        camroll: Some("A001".into()),
        ..Default::default()
    }
}

fn mkdirs(root: &Path, names: &[&str]) {
    for n in names {
        std::fs::create_dir_all(root.join(n)).unwrap();
    }
}

/// Destination A has rolls 001-002, B has 004 (and a stray folder that doesn't match). Two
/// jobs planned at the same moment both see 005 as next; run concurrently, one gets 005 and
/// the other 006, each identical on both destinations. A later job gets 007; resume keeps its
/// folder.
#[test]
fn seq_is_unique_across_mismatched_destinations_and_concurrent_jobs() {
    init_state_dir();
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("A");
    let b = tmp.path().join("B");
    mkdirs(
        &a,
        &["Big Show/A001_001", "Big Show/A001_002", "Big Show/notes"],
    );
    mkdirs(&b, &["Big Show/A001_004", "Big Show/B001_009"]);

    let plan = |card: &str| {
        let src = tmp.path().join(card);
        make_card(&src);
        let mut s = spec(&src, &[&a, &b]);
        s.folder = Some("{show}/{camroll}_{seq:3}".into());
        s.production = production();
        shortend_core::plan(s).expect("plan")
    };
    let j1 = plan("CARD1");
    let j2 = plan("CARD2");
    assert_eq!((j1.seq, j2.seq), (Some(5), Some(5)), "both provisional");

    let run = |job: shortend_core::Job| {
        std::thread::spawn(move || {
            with_events(|tx| shortend_core::run(&job, tx, &CancelToken::new()))
                .0
                .expect("run")
        })
    };
    let (h1, h2) = (run(j1), run(j2));
    let (r1, r2) = (h1.join().unwrap(), h2.join().unwrap());
    let mut got = [r1.job.seq.unwrap(), r2.job.seq.unwrap()];
    got.sort_unstable();
    assert_eq!(got, [5, 6]);
    for r in [&r1, &r2] {
        assert_eq!(r.state, JobState::Completed, "{:?}", r.warnings);
        let folder = format!("Big Show/A001_{:03}", r.job.seq.unwrap());
        assert_eq!(r.job.folder.as_str(), folder);
        for (d, root) in r.job.destinations.iter().zip([&a, &b]) {
            assert_eq!(
                d.target,
                shortend_core::fs::canonical_dir(root)
                    .unwrap()
                    .join(&folder)
            );
            assert!(d.target.join("CLIP/A001C001_240101_R1AB.mxf").is_file());
        }
    }
    let loser = if r1.job.seq == Some(6) { &r1 } else { &r2 };
    assert!(
        loser
            .warnings
            .iter()
            .any(|w| w.contains("number 5 was taken")),
        "{:?}",
        loser.warnings
    );

    let src3 = tmp.path().join("CARD3");
    make_card(&src3);
    let mut s = spec(&src3, &[&a, &b]);
    s.folder = Some("{show}/{camroll}_{seq:3}".into());
    s.production = production();
    let (r3, _) = common::run(s);
    assert_eq!(r3.job.seq, Some(7));

    // Resume uses the stored folder; it never claims a new number.
    let (again, _) = resume(&r1.job.id.to_string());
    let again = again.unwrap();
    assert_eq!(again.job.folder, r1.job.folder);
    assert!(again.outcomes.iter().all(|o| {
        o.dests
            .iter()
            .all(|d| d.status == FileStatus::AlreadyVerified)
    }));
    assert!(!a.join("Big Show/A001_008").exists());
}

/// `{seq}` in a middle folder, with fixed folders after it, and a destination that doesn't
/// have the parent folder yet.
#[test]
fn seq_in_a_middle_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("A");
    let b = tmp.path().join("B");
    mkdirs(&a, &["D03_1/x"]);
    std::fs::create_dir(&b).unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let mut s = spec(&src, &[&a, &b]);
    s.folder = Some("D{day}_{seq}/{card}/RAW".into());
    s.production.shoot_day = Some("03".into());
    let (r, _) = run(s);
    assert_eq!(r.state, JobState::Completed);
    assert_eq!(r.job.folder.as_str(), "D03_2/CARD/RAW");
    assert!(
        b.join("D03_2/CARD/RAW/CLIP/A001C001_240101_R1AB.mxf")
            .is_file()
    );
}

/// Template problems refuse the job before anything is written.
#[test]
fn template_problems_refuse_the_job() {
    init_state_dir();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    for (template, needle) in [
        ("{show}/{day}_{seq}", "{show}"),
        ("{show}/{day}_{seq}", "{day}"),
        ("{card}/{nope}", "unknown token {nope}"),
        ("{card}/{seq:0}", "width"),
        ("{card}//x", "empty folder name"),
    ] {
        let mut s = spec(&src, &[&dest]);
        s.folder = Some(template.into());
        let Err(Error::Preflight(p)) = shortend_core::plan(s) else {
            panic!("{template} was accepted");
        };
        assert!(p.join("\n").contains(needle), "{template}: {p:?}");
    }
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        0,
        "nothing written"
    );
}

/// A value with characters no folder name can hold is changed, and the plan says so.
#[test]
fn unsafe_values_are_sanitized_with_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("CARD");
    make_card(&src);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let mut s = spec(&src, &[&dest]);
    s.folder = Some("{show}/{episode}".into());
    s.production.show = Some("AC/DC: Live?".into());
    s.production.episode = Some("../101".into());
    let (r, _) = run(s);
    assert_eq!(r.state, JobState::Completed);
    assert_eq!(r.job.folder.as_str(), "AC_DC_ Live_/.._101");
    assert!(dest.join("AC_DC_ Live_/.._101").is_dir());
    assert_eq!(
        r.job
            .warnings
            .iter()
            .filter(|w| w.contains("in the folder name"))
            .count(),
        2
    );
}
