//! Manifest tests: ShortEnd's ASC MHL v2 and MHL v1.1 output, checked against the official XSDs
//! (`xmllint`), against manifests produced by the reference tools (committed fixtures, see
//! `tests/fixtures/make_fixtures.sh`), and against the reference tools themselves when they are
//! installed (`ascmhl`, `ascmhl-debug`, `simple-mhl`).
//!
//! External tools are optional locally; with `SHORTEND_REQUIRE_TOOLS=1` (set in CI) a missing
//! tool fails the test instead of skipping that check.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use shortend_core::report::mhl::{self, MhlVersion, VerifyIssue};
use shortend_core::{Error, FileStatus, HashAlgorithm, HashList, JobState};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn xsd(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../xsd")
        .join(name)
}

fn require_tools() -> bool {
    std::env::var("SHORTEND_REQUIRE_TOOLS").as_deref() == Ok("1")
}

/// Runs an external checker. `None` if the tool isn't installed (and isn't required).
fn tool(cmd: &str, args: &[&std::ffi::OsStr]) -> Option<std::process::Output> {
    match Command::new(cmd).args(args).output() {
        Ok(o) => Some(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !require_tools() => {
            eprintln!("skipping {cmd} check: not installed");
            None
        }
        Err(e) => panic!("{cmd}: {e}"),
    }
}

fn assert_schema_valid(file: &Path, schema: &str) {
    if let Some(o) = tool(
        "xmllint",
        &[
            "--nonet".as_ref(),
            "--noout".as_ref(),
            "--schema".as_ref(),
            xsd(schema).as_os_str(),
            file.as_os_str(),
        ],
    ) {
        assert!(
            o.status.success(),
            "{} fails {schema}:\n{}",
            file.display(),
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

fn assert_reference_verifies(folder: &Path) {
    if let Some(o) = tool("ascmhl-debug", &["verify".as_ref(), folder.as_os_str()]) {
        assert!(
            o.status.success(),
            "reference ascmhl-debug verify rejects {}:\n{}{}",
            folder.display(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), &dst).unwrap();
        }
    }
}

/// Every `.mhl` generation and the chain in `<folder>/ascmhl` pass the official XSDs.
fn assert_history_valid(folder: &Path) {
    let dir = folder.join("ascmhl");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "mhl") {
            assert_schema_valid(&e.path(), "ASCMHL.xsd");
        }
    }
    assert_schema_valid(&dir.join("ascmhl_chain.xml"), "ASCMHLDirectory.xsd");
}

fn generations(folder: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(folder.join("ascmhl"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mhl"))
        .collect();
    v.sort();
    v
}

/// Directory and root hash values of one generation file: path → (content, structure) per
/// algorithm element text. Parsed loosely (the files were validated against the XSD already).
fn dir_hashes(gen_file: &Path) -> BTreeMap<String, Vec<String>> {
    let text = std::fs::read_to_string(gen_file).unwrap();
    let mut out = BTreeMap::new();
    let mut rest = text.as_str();
    if let Some(i) = rest.find("<roothash>") {
        let end = rest[i..].find("</roothash>").unwrap() + i;
        out.insert(".".to_owned(), values(&rest[i..end]));
    }
    while let Some(i) = rest.find("<directoryhash>") {
        let end = rest[i..].find("</directoryhash>").unwrap() + i;
        let block = &rest[i..end];
        let p0 = block.find("<path").unwrap();
        let p1 = block[p0..].find('>').unwrap() + p0 + 1;
        let p2 = block.find("</path>").unwrap();
        out.insert(block[p1..p2].to_owned(), values(&block[p2..]));
        rest = &rest[end..];
    }
    out
}

fn values(block: &str) -> Vec<String> {
    let mut v = Vec::new();
    for algo in ["c4", "md5", "sha1", "xxh128", "xxh3", "xxh64"] {
        let open = format!("<{algo}");
        let mut s = block;
        while let Some(i) = s.find(&open) {
            let gt = s[i..].find('>').unwrap() + i + 1;
            let lt = s[gt..].find('<').unwrap() + gt;
            v.push(format!("{algo}:{}", &s[gt..lt]));
            s = &s[lt..];
        }
    }
    v
}

fn offload(
    card: &Path,
    dest: &Path,
    hashes: Vec<HashAlgorithm>,
    mhl: Vec<MhlVersion>,
) -> shortend_core::JobReport {
    let mut s = spec(card, &[dest]);
    s.settings.hashes = HashList::new(hashes).unwrap();
    s.settings.reports.mhl = mhl;
    s.production.operator = Some("Test DIT".into());
    s.production.show = Some("Fixture & <Show>".into());
    run(s).0
}

#[test]
fn fresh_offload_writes_valid_ascmhl_that_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    make_card(&card);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let rep = offload(
        &card,
        &dest,
        vec![HashAlgorithm::Xxh64, HashAlgorithm::Md5, HashAlgorithm::C4],
        vec![MhlVersion::V2],
    );
    assert_eq!(rep.state, JobState::Completed);
    let target = dest.join("A001");
    assert_eq!(rep.manifests.len(), 1);
    assert_eq!(generations(&target).len(), 1);
    assert!(
        generations(&target)[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("0001_A001_")
    );
    assert_history_valid(&target);
    assert_reference_verifies(&target);

    let reports = mhl::verify_folder(&target).unwrap();
    assert!(reports[0].ok(), "{:?}", reports[0].issues);
    // .DS_Store and the report folder are ignored, everything else is listed.
    let listed = make_card_file_count(&card) - 1;
    assert_eq!(reports[0].verified, listed);

    // Damage, delete and add files: all three are reported.
    let victim = target.join("CLIP/A001C002_240101_R1AB.mxf");
    let mut b = std::fs::read(&victim).unwrap();
    b[7] ^= 1;
    std::fs::write(&victim, b).unwrap();
    std::fs::remove_file(target.join("empty.bin")).unwrap();
    std::fs::write(target.join("CLIP/extra.mov"), b"new").unwrap();
    let r = &mhl::verify_folder(&target).unwrap()[0];
    assert!(r.issues.iter().any(
        |i| matches!(i, VerifyIssue::Mismatch { rel, .. } if rel == "CLIP/A001C002_240101_R1AB.mxf")
    ));
    assert!(
        r.issues
            .iter()
            .any(|i| matches!(i, VerifyIssue::Missing { rel } if rel == "empty.bin"))
    );
    assert!(
        r.issues
            .iter()
            .any(|i| matches!(i, VerifyIssue::NotInManifest { rel } if rel == "CLIP/extra.mov"))
    );
}

fn make_card_file_count(card: &Path) -> usize {
    walk(card).iter().filter(|p| p.is_file()).count()
}

/// For every algorithm, ShortEnd's directory and root hashes over a card equal those the
/// reference implementation recorded, and its new generation verifies against the history.
#[test]
fn second_generation_on_reference_histories() {
    for algo in ["xxh64", "xxh3", "xxh128", "md5", "sha1", "c4"] {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("CARD");
        copy_tree(&fixtures().join("history").join(algo).join("CARD"), &card);
        let a: HashAlgorithm = algo.parse().unwrap();
        assert!(
            mhl::verify_folder(&card).unwrap()[0].ok(),
            "{algo}: fixture verifies"
        );

        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let rep = offload(&card, &dest, vec![a], vec![MhlVersion::V2]);
        assert_eq!(
            rep.state,
            JobState::Completed,
            "{algo}: {:?}",
            rep.manifest_issues
        );
        assert!(rep.manifest_issues.is_empty());
        let target = dest.join("CARD");
        let gens = generations(&target);
        assert_eq!(gens.len(), 2, "{algo}");
        assert!(
            gens[1]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0002_CARD_")
        );
        assert_history_valid(&target);
        assert_reference_verifies(&target);

        let text = std::fs::read_to_string(&gens[1]).unwrap();
        assert!(
            !text.contains("action=\"original\""),
            "{algo}: nothing is new"
        );
        assert!(!text.contains("action=\"failed\""), "{algo}");
        assert_eq!(text.matches("action=\"verified\"").count(), 6, "{algo}");
        assert_eq!(
            dir_hashes(&gens[0]),
            dir_hashes(&gens[1]),
            "{algo}: directory hashes"
        );
        assert!(mhl::verify_folder(&target).unwrap()[0].ok());
    }
}

#[test]
fn history_algorithms_are_added_to_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    copy_tree(&fixtures().join("history/two_gen/CARD"), &card);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    // The job asks for c4 only; the history uses xxh64 and md5.
    let rep = offload(&card, &dest, vec![HashAlgorithm::C4], vec![MhlVersion::V2]);
    assert_eq!(rep.state, JobState::Completed);
    assert_eq!(
        rep.job.settings.hashes.as_slice(),
        [HashAlgorithm::C4, HashAlgorithm::Md5, HashAlgorithm::Xxh64]
    );
    assert!(
        rep.job
            .warnings
            .iter()
            .any(|w| w.contains("also computing"))
    );
    let target = dest.join("CARD");
    let gens = generations(&target);
    assert_eq!(gens.len(), 3);
    let text = std::fs::read_to_string(&gens[2]).unwrap();
    assert_eq!(text.matches("<xxh64 action=\"verified\"").count(), 6);
    assert_eq!(text.matches("<md5 action=\"verified\"").count(), 6);
    assert_eq!(
        text.matches("<c4 action=\"verified\"").count(),
        6,
        "new algorithm, verified by the others"
    );
    assert_history_valid(&target);
    assert_reference_verifies(&target);
}

#[test]
fn card_that_contradicts_its_history_fails_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    copy_tree(&fixtures().join("history/xxh64/CARD"), &card);
    std::fs::write(card.join("README.txt"), b"top level!").unwrap(); // same size, new bytes
    std::fs::remove_file(card.join("SOUND/T001.wav")).unwrap();
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let rep = offload(
        &card,
        &dest,
        vec![HashAlgorithm::Xxh64],
        vec![MhlVersion::V2],
    );
    assert_eq!(rep.state, JobState::CompletedWithFailures);
    assert!(
        rep.manifest_issues
            .iter()
            .any(|m| m.starts_with("README.txt: card does not match")),
        "{:?}",
        rep.manifest_issues
    );
    assert!(
        rep.manifest_issues
            .iter()
            .any(|m| m.starts_with("SOUND/T001.wav: listed in the card's ASC MHL but missing"))
    );
    let readme = rep
        .outcomes
        .iter()
        .find(|o| o.entry.rel.as_str() == "README.txt")
        .unwrap();
    assert!(matches!(
        readme.dests[0].status,
        FileStatus::Failed(shortend_core::Failure::SourceManifestMismatch { .. })
    ));
    let gens = generations(&dest.join("CARD"));
    let text = std::fs::read_to_string(&gens[1]).unwrap();
    assert!(text.contains("<xxh64 action=\"failed\">"));
    assert_history_valid(&dest.join("CARD"));

    // Resuming must not turn the job green: the card still contradicts its manifest.
    let (r, _) = resume(&rep.job.id.to_string());
    let r = r.unwrap();
    assert_eq!(r.state, JobState::CompletedWithFailures);
    assert_eq!(r.manifest_issues.len(), 2, "{:?}", r.manifest_issues);
}

#[test]
fn damaged_card_history_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    copy_tree(&fixtures().join("history/xxh64/CARD"), &card);
    let first = &generations(&card)[0];
    let text = std::fs::read_to_string(first)
        .unwrap()
        .replace("Reference", "Forged!!!");
    std::fs::write(first, text).unwrap();
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    init_state_dir();
    let mut s = spec(&card, &[&dest]);
    s.settings.reports.mhl = vec![MhlVersion::V2];
    match shortend_core::plan(s.clone()) {
        Err(Error::Preflight(p)) => assert!(
            p.iter()
                .any(|x| x.contains("modified after it was written")),
            "{p:?}"
        ),
        other => panic!("expected refusal, got {:?}", other.map(|j| j.id)),
    }
    // Without a manifest the card can still be offloaded (its history is copied as files).
    s.settings.reports.mhl = Vec::new();
    assert!(shortend_core::plan(s).is_ok());
}

#[test]
fn unrepresentable_algorithms_are_refused_or_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    make_card(&card);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    init_state_dir();
    let plan = |hashes: Vec<HashAlgorithm>, mhl: Vec<MhlVersion>| {
        let mut s = spec(&card, &[&dest]);
        s.settings.hashes = HashList::new(hashes).unwrap();
        s.settings.reports.mhl = mhl;
        shortend_core::plan(s)
    };
    assert!(matches!(
        plan(vec![HashAlgorithm::Sha256], vec![MhlVersion::V2]),
        Err(Error::Preflight(_))
    ));
    assert!(matches!(
        plan(vec![HashAlgorithm::Xxh128], vec![MhlVersion::V1_1]),
        Err(Error::Preflight(_))
    ));
    assert!(plan(vec![HashAlgorithm::Sha256], Vec::new()).is_ok());
    let job = plan(
        vec![HashAlgorithm::Xxh64, HashAlgorithm::Sha256],
        vec![MhlVersion::V2],
    )
    .unwrap();
    assert!(
        job.warnings
            .iter()
            .any(|w| w.contains("sha256 checksums appear in the reports but cannot be written"))
    );
}

#[test]
fn mhl_v11_output_validates_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    make_card(&card);
    std::fs::remove_file(card.join("empty.bin")).unwrap();
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let rep = offload(
        &card,
        &dest,
        vec![
            HashAlgorithm::Xxh64,
            HashAlgorithm::Md5,
            HashAlgorithm::Sha1,
        ],
        vec![MhlVersion::V1_1],
    );
    assert_eq!(rep.state, JobState::Completed);
    assert_eq!(rep.manifests.len(), 1);
    assert!(rep.manifest_omissions.is_empty());
    let m = &rep.manifests[0];
    assert_schema_valid(m, "MediaHashList_v1_1.xsd");
    let r = &mhl::verify_folder(&dest.join("A001")).unwrap()[0];
    assert!(r.ok(), "{:?}", r.issues);
    assert_eq!(r.verified, make_card_file_count(&card));
    if let Some(o) = tool("simple-mhl", &["verify".as_ref(), m.as_os_str()]) {
        assert!(
            o.status.success(),
            "simple-mhl rejects our v1.1:\n{}",
            String::from_utf8_lossy(&o.stdout)
        );
    }
}

/// MHL v1.1 cannot list zero-byte files. As the only manifest that would leave files out of
/// the record, so the job is refused; next to ASC MHL v2 the omission is loud: the job fails
/// (exit 1), and the files are named in the summary, CSV, TXT and PDF.
#[test]
fn zero_byte_files_and_mhl_v11() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    make_card(&card); // contains empty.bin
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    init_state_dir();

    let mut s = spec(&card, &[&dest]);
    s.settings.reports.mhl = vec![MhlVersion::V1_1];
    match shortend_core::plan(s) {
        Err(Error::Preflight(p)) => assert!(
            p.iter()
                .any(|x| x.contains("cannot list zero-byte files") && x.contains("empty.bin")),
            "{p:?}"
        ),
        other => panic!("expected refusal, got {:?}", other.map(|j| j.id)),
    }
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        0,
        "nothing written"
    );

    let rep = offload(
        &card,
        &dest,
        vec![HashAlgorithm::Xxh64],
        vec![MhlVersion::V2, MhlVersion::V1_1],
    );
    assert_eq!(
        rep.state,
        JobState::CompletedWithFailures,
        "an incomplete manifest fails the job"
    );
    assert_eq!(rep.manifest_omissions, ["empty.bin"]);
    assert!(
        rep.manifest_issues
            .iter()
            .any(|m| m.contains("does not list 1 zero-byte file") && m.contains("empty.bin"))
    );
    assert!(
        rep.outcomes.iter().all(|o| o.all_ok()),
        "every file is still verified"
    );
    assert_eq!(rep.manifests.len(), 2);
    let target = dest.join("A001");

    // ASC MHL v2 lists the zero-byte file; the v1.1 file next to the media is ignored by it.
    let first = std::fs::read_to_string(&generations(&target)[0]).unwrap();
    assert!(
        first.contains(r#"<path size="0""#) && first.contains(">empty.bin</path>"),
        "{first}"
    );
    assert_history_valid(&target);
    assert_reference_verifies(&target);
    let v11 = rep
        .manifests
        .iter()
        .find(|m| m.extension().unwrap() == "mhl" && m.parent() == Some(&*target))
        .unwrap();
    assert_schema_valid(v11, "MediaHashList_v1_1.xsd");
    assert!(!std::fs::read_to_string(v11).unwrap().contains("empty.bin"));

    // Named in every report.
    let reports = target.join("_ShortEnd");
    let read = |ext: &str| {
        let p = std::fs::read_dir(&reports)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().unwrap() == ext)
            .unwrap();
        std::fs::read(p).unwrap()
    };
    let csv = String::from_utf8(read("csv")).unwrap();
    assert!(
        csv.lines()
            .any(|l| l.starts_with("empty.bin,")
                && l.contains("not listed in the MHL v1.1 manifest")),
        "{csv}"
    );
    let txt = String::from_utf8(read("txt")).unwrap();
    assert!(
        txt.contains("Not listed in the MHL v1.1 manifest") && txt.contains("Result:       FAIL"),
        "{txt}"
    );
    let pdf = read("pdf");
    let pdf_path = tmp.path().join("r.pdf");
    std::fs::write(&pdf_path, pdf).unwrap();
    if let Some(o) = tool(
        "pdftotext",
        &[
            "-enc".as_ref(),
            "UTF-8".as_ref(),
            pdf_path.as_os_str(),
            "-".as_ref(),
        ],
    ) {
        let text = String::from_utf8_lossy(&o.stdout);
        assert!(
            text.contains("Not listed in the MHL v1.1 manifest"),
            "{text}"
        );
    }
}

#[test]
fn reads_reference_v11_manifests() {
    for which in ["sealed", "legacy"] {
        let r = mhl::verify_folder(&fixtures().join("mhl11").join(which).join("CARD")).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].ok(), "{which}: {:?}", r[0].issues);
        assert!(r[0].verified >= 2, "{which}");
    }
    // Tampering is detected.
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    copy_tree(&fixtures().join("mhl11/legacy/CARD"), &card);
    std::fs::write(card.join("README.txt"), b"top levXl\n").unwrap();
    let r = &mhl::verify_folder(&card).unwrap()[0];
    assert!(
        matches!(&r.issues[..], [VerifyIssue::Mismatch { rel, .. }] if rel == "README.txt"),
        "{:?}",
        r.issues
    );
}

/// Our directory hashes match the reference on a tree with an empty folder (which git cannot
/// store in the fixtures), when the reference tool is installed.
#[test]
fn empty_folders_hash_like_the_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    make_card(&card); // contains EMPTY_DIR/nested
    let reference = tmp.path().join("REF");
    copy_tree(&card, &reference);
    std::fs::create_dir_all(reference.join("EMPTY_DIR/nested")).unwrap();
    let Some(o) = tool(
        "ascmhl",
        &[
            "create".as_ref(),
            "-h".as_ref(),
            "xxh64".as_ref(),
            reference.as_os_str(),
        ],
    ) else {
        return;
    };
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let rep = offload(
        &card,
        &dest,
        vec![HashAlgorithm::Xxh64],
        vec![MhlVersion::V2],
    );
    assert_eq!(rep.state, JobState::Completed);
    let ours = dir_hashes(&generations(&dest.join("CARD"))[0]);
    let theirs = dir_hashes(&generations(&reference)[0]);
    assert_eq!(ours, theirs);
}

#[test]
fn resume_keeps_the_extended_chain_and_writes_one_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("CARD");
    copy_tree(&fixtures().join("history/xxh64/CARD"), &card);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let rep = offload(
        &card,
        &dest,
        vec![HashAlgorithm::Xxh64],
        vec![MhlVersion::V2],
    );
    assert_eq!(rep.state, JobState::Completed);
    let target = dest.join("CARD");
    assert_eq!(generations(&target).len(), 2);

    // Resume the finished job: the chain file differs from the card's (we extended it) and
    // must be kept; nothing new was copied, so no third generation.
    let (r, _) = resume(&rep.job.id.to_string());
    let r = r.unwrap();
    assert_eq!(r.state, JobState::Completed);
    assert_eq!(generations(&target).len(), 2);
    assert_history_valid(&target);
    assert!(mhl::verify_folder(&target).unwrap()[0].ok());

    // A destination file lost after the job: resume restores it and records a generation.
    std::fs::remove_file(target.join("SOUND/T001.wav")).unwrap();
    let (r, _) = resume(&rep.job.id.to_string());
    assert_eq!(r.unwrap().state, JobState::Completed);
    assert_eq!(generations(&target).len(), 3);
    assert_history_valid(&target);
    assert_reference_verifies(&target);
}
