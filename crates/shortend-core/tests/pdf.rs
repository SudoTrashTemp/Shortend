//! PDF report tests: a 10,000-file report renders and is readable by two independent PDF
//! implementations (poppler's `pdftotext`/`pdfinfo` and `qpdf`), with the logo, production
//! fields and Unicode names. With `SHORTEND_REQUIRE_TOOLS=1` (CI) missing tools fail the test.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use shortend_core::{FileEntry, FileOutcome, JobState, PdfOptions, RelPath};

fn tool(cmd: &str, args: &[&std::ffi::OsStr]) -> Option<std::process::Output> {
    match Command::new(cmd).args(args).output() {
        Ok(o) => Some(o),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && std::env::var("SHORTEND_REQUIRE_TOOLS").as_deref() != Ok("1") =>
        {
            eprintln!("skipping {cmd} check: not installed");
            None
        }
        Err(e) => panic!("{cmd}: {e}"),
    }
}

fn pdf_in(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pdf"))
        .expect("a PDF report")
}

fn check_readable(pdf: &Path) -> Option<String> {
    let bytes = std::fs::read(pdf).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "not a PDF");
    if let Some(o) = tool("qpdf", &["--check".as_ref(), pdf.as_os_str()]) {
        assert!(
            o.status.success(),
            "qpdf --check:\n{}",
            String::from_utf8_lossy(&o.stdout)
        );
    }
    if let Some(o) = tool("pdfinfo", &[pdf.as_os_str()]) {
        assert!(
            o.status.success(),
            "pdfinfo:\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    tool(
        "pdftotext",
        &[
            "-enc".as_ref(),
            "UTF-8".as_ref(),
            "-layout".as_ref(),
            pdf.as_os_str(),
            "-".as_ref(),
        ],
    )
    .map(|o| {
        assert!(
            o.status.success(),
            "pdftotext:\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    })
}

#[test]
fn report_with_logo_and_production_fields() {
    for logo in ["logo.png", "logo.jpg"] {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("A001");
        make_card(&card);
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let mut s = spec(&card, &[&dest]);
        s.settings.reports.pdf = Some(PdfOptions {
            logo: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures")
                    .join(logo),
            ),
        });
        s.production.show = Some("Big Show".into());
        s.production.operator = Some("Jane Doe".into());
        s.production.camroll = Some("A001".into());
        let (rep, _) = run(s);
        assert_eq!(rep.state, JobState::Completed);
        assert!(
            !rep.warnings.iter().any(|w| w.contains("logo")),
            "{:?}",
            rep.warnings
        );
        let pdf = pdf_in(&dest.join("A001/_ShortEnd"));
        assert!(
            std::fs::metadata(&pdf).unwrap().len() < 300_000,
            "fonts are subset"
        );
        if let Some(text) = check_readable(&pdf) {
            for needle in [
                "Offload Report",
                "PASS",
                "Big Show",
                "Jane Doe",
                "A001C001_240101_R1AB.mxf",
                "Ünïcödé — [U+540D][U+524D].wav",
            ] {
                assert!(
                    text.contains(needle),
                    "{logo}: missing {needle:?} in\n{text}"
                );
            }
        }
        if let Some(o) = tool("pdfimages", &["-list".as_ref(), pdf.as_os_str()]) {
            let list = String::from_utf8_lossy(&o.stdout);
            assert!(
                list.lines().count() >= 3,
                "{logo}: logo image embedded:\n{list}"
            );
        }
    }
}

#[test]
fn unreadable_logo_is_a_warning_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    make_card(&card);
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let mut s = spec(&card, &[&dest]);
    s.settings.reports.pdf = Some(PdfOptions {
        logo: Some(tmp.path().join("missing.png")),
    });
    let (rep, _) = run(s);
    assert_eq!(rep.state, JobState::Completed);
    assert!(rep.warnings.iter().any(|w| w.contains("PDF logo")));
    check_readable(&pdf_in(&dest.join("A001/_ShortEnd")));
}

/// A 10,000-file job: the report renders, every file is listed (first and last), and it stays
/// readable. Built from a real job whose file list is extended, so no 10,000-file copy is
/// needed.
#[test]
fn ten_thousand_file_report() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("DNG_CARD");
    write_file(&card.join("frame.dng"), b"x");
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let mut s = spec(&card, &[&dest]);
    s.settings.reports.csv = false;
    s.settings.reports.txt = false;
    s.settings.reports.mhl = Vec::new();
    let (mut rep, _) = run(s);
    let template = rep.outcomes[0].clone();
    rep.outcomes = (0..10_000)
        .map(|i| {
            let mut o: FileOutcome = template.clone();
            o.entry = FileEntry {
                rel: RelPath::new(&format!(
                    "A001_C{:03}/A001C{:03}_frame_{i:07}.dng",
                    i / 1000,
                    i / 1000
                ))
                .unwrap(),
                size: 12_345_678 + i as u64,
                mtime: template.entry.mtime,
            };
            o
        })
        .collect();
    rep.job.files = rep.outcomes.iter().map(|o| o.entry.clone()).collect();
    let reports = tmp.path().join("reports");
    let started = std::time::Instant::now();
    let (files, warnings) = shortend_core::report::write_all(&rep, &reports);
    assert!(warnings.is_empty(), "{warnings:?}");
    eprintln!("10k-file PDF rendered in {:.1?}", started.elapsed());
    let pdf = files
        .iter()
        .find(|p| p.extension().is_some_and(|x| x == "pdf"))
        .unwrap();
    let size = std::fs::metadata(pdf).unwrap().len();
    eprintln!("10k-file PDF is {size} bytes");
    assert!(size < 8_000_000, "PDF is {size} bytes");
    if let Some(text) = check_readable(pdf) {
        assert!(text.contains("A001C000_frame_0000000.dng"));
        assert!(text.contains("A001C009_frame_0009999.dng"));
        assert!(text.contains("Files (10000)"));
        let pages = text.matches('\u{c}').count();
        assert!(pages > 100, "{pages} pages");
    }
}
