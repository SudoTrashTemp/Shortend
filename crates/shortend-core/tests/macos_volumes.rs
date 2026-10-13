//! macOS: destinations are recognised by volume UUID, so a drive that is ejected and
//! re-attached under a new device number is accepted on resume, and a different drive at the
//! same mount point is refused. Uses real disk images (`hdiutil`), formatted exFAT, HFS+ and
//! APFS.
#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use shortend_core::{Error, FileStatus, JobState};

fn hdiutil(args: &[&str]) -> Result<String, String> {
    let o = Command::new("hdiutil")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!(
            "hdiutil {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        ))
    }
}

fn create(img: &Path, fs: &str, name: &str) {
    hdiutil(&[
        "create",
        "-quiet",
        "-size",
        "64m",
        "-fs",
        fs,
        "-volname",
        name,
        "-o",
        img.to_str().unwrap(),
    ])
    .unwrap();
}

fn attach(img: &Path, mnt: &Path) {
    hdiutil(&[
        "attach",
        "-quiet",
        "-nobrowse",
        "-mountpoint",
        mnt.to_str().unwrap(),
        img.to_str().unwrap(),
    ])
    .unwrap();
}

/// Attached without mounting, so it takes the next free disk number.
fn attach_unmounted(img: &Path) -> String {
    let out = hdiutil(&["attach", "-nomount", img.to_str().unwrap()]).unwrap();
    out.split_whitespace().next().unwrap().to_owned() // /dev/diskN
}

/// macOS services (Spotlight, fseventsd) briefly hold a freshly used image, so "Resource busy"
/// is retried for a while.
fn detach(what: &str) {
    let mut last = String::new();
    for _ in 0..20 {
        match hdiutil(&["detach", "-force", what]) {
            Ok(_) => return,
            Err(e) => last = e,
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("{last}");
}

#[test]
fn reattached_drive_is_matched_by_uuid_and_a_different_drive_is_refused() {
    init_state_dir();
    for fs in ["ExFAT", "HFS+", "APFS"] {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("dest.dmg");
        let other = tmp.path().join("other.dmg");
        let filler = tmp.path().join("filler.dmg");
        create(&img, fs, "SEDEST");
        // exFAT/FAT volume IDs come from the format time (to the second); wait so the second
        // drive really is different, as two real cards would be.
        std::thread::sleep(std::time::Duration::from_millis(2100));
        create(&other, fs, "SEDEST");
        create(&filler, fs, "FILLER");
        let mnt = tmp.path().join("mnt");
        attach(&img, &mnt);

        let src = tmp.path().join("CARD");
        make_card(&src);
        let (rep, _) = run(spec(&src, &[&mnt]));
        assert_eq!(rep.state, JobState::Completed, "{fs}: {:?}", rep.warnings);
        let vol = &rep.job.destinations[0].volume;
        assert!(vol.fs_uuid.is_some(), "{fs}: no volume UUID: {vol:?}");
        let id = rep.job.id.to_string();

        // Eject, let another image take the device node, re-attach at the same place.
        detach(mnt.to_str().unwrap());
        let filler_dev = attach_unmounted(&filler);
        attach(&img, &mnt);
        let (again, _) = resume(&id);
        let again = again.unwrap_or_else(|e| panic!("{fs}: resume after re-attach: {e}"));
        assert_eq!(again.state, JobState::Completed);
        assert!(
            again
                .outcomes
                .iter()
                .all(|o| o.dests[0].status == FileStatus::AlreadyVerified)
        );
        let moved = again.job.destinations[0].volume.dev != vol.dev;
        eprintln!("{fs}: device number changed on re-attach: {moved}");

        // A different drive with the same name at the same mount point.
        detach(mnt.to_str().unwrap());
        attach(&other, &mnt);
        let other_uuid = shortend_core::fs::platform::volume_probe(&mnt)
            .unwrap()
            .fs_uuid;
        eprintln!("{fs}: UUIDs {:?} / {other_uuid:?}", vol.fs_uuid);
        assert_ne!(other_uuid, vol.fs_uuid, "{fs}: test images must differ");
        match resume(&id).0 {
            Err(Error::Preflight(p)) => {
                assert!(p.join("\n").contains("different volume"), "{fs}: {p:?}")
            }
            other => panic!(
                "{fs}: a different drive was accepted: {:?}",
                other.map(|r| r.state)
            ),
        }
        detach(mnt.to_str().unwrap());
        detach(&filler_dev);
    }
}
