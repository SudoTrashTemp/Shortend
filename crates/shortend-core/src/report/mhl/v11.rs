//! MHL v1.1 (mediahashlist.org), kept for interoperability with older tools.
//!
//! v1.1 can carry MD5, SHA-1 and xxHash64. xxHash64 is written as `xxhash64be` (big-endian hex,
//! the same text as `xxh64sum`); on reading, the deprecated `xxhash64` element is accepted in
//! either byte order because tools disagreed on it. The schema requires `size` ≥ 1, so
//! zero-byte files cannot be listed; they are reported as a warning instead.

use std::fmt::Write as _;
use std::io::BufReader;
use std::path::Path;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::xml::{self, Node, esc};
use super::{ManifestKind, MhlError, VerifyIssue, VerifyReport};
use crate::hash::{Digests, HashAlgorithm};

pub fn supports(a: HashAlgorithm) -> bool {
    matches!(
        a,
        HashAlgorithm::Md5 | HashAlgorithm::Sha1 | HashAlgorithm::Xxh64
    )
}

fn element(a: HashAlgorithm) -> Option<&'static str> {
    match a {
        HashAlgorithm::Md5 => Some("md5"),
        HashAlgorithm::Sha1 => Some("sha1"),
        HashAlgorithm::Xxh64 => Some("xxhash64be"),
        _ => None,
    }
}

pub struct Entry<'a> {
    pub rel: &'a str,
    pub size: u64,
    pub mtime: Timestamp,
    pub digests: &'a Digests,
    pub hashed_at: Timestamp,
}

pub struct Creator<'a> {
    pub name: Option<&'a str>,
    pub username: &'a str,
    pub hostname: &'a str,
    pub tool: &'a str,
    pub started: Timestamp,
    pub finished: Timestamp,
}

pub struct Written {
    pub path: std::path::PathBuf,
    /// Zero-byte files left out (the schema cannot represent them).
    pub skipped_empty: Vec<String>,
}

fn utc(t: Timestamp) -> String {
    t.to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Writes `<root>/<folder>_<YYYY-MM-DD_HHMMSS>.mhl`. Paths are relative to `root`.
pub fn write(
    root: &Path,
    entries: &[Entry<'_>],
    creator: &Creator<'_>,
    now: Timestamp,
) -> Result<Written, MhlError> {
    let mut out =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<hashlist version=\"1.1\">\n");
    let _ = writeln!(out, "  <creatorinfo>");
    if let Some(n) = creator.name.filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "    <name>{}</name>", esc(n));
    }
    let _ = writeln!(out, "    <username>{}</username>", esc(creator.username));
    let _ = writeln!(out, "    <hostname>{}</hostname>", esc(creator.hostname));
    let _ = writeln!(out, "    <tool>{}</tool>", esc(creator.tool));
    let _ = writeln!(out, "    <startdate>{}</startdate>", utc(creator.started));
    let _ = writeln!(
        out,
        "    <finishdate>{}</finishdate>",
        utc(creator.finished)
    );
    let _ = writeln!(out, "  </creatorinfo>");
    let mut skipped_empty = Vec::new();
    let mut listed = 0;
    for e in entries {
        if e.size == 0 {
            skipped_empty.push(e.rel.to_owned());
            continue;
        }
        let hashes: Vec<(&str, String)> = e
            .digests
            .iter()
            .filter_map(|d| element(d.algo()).map(|el| (el, d.value())))
            .collect();
        if hashes.is_empty() {
            return Err(MhlError::Unsupported(
                "none of the job's checksum algorithms can be written to MHL v1.1 (md5, sha1, xxh64)"
                    .into(),
            ));
        }
        listed += 1;
        let _ = writeln!(out, "  <hash>");
        let _ = writeln!(out, "    <file>{}</file>", esc(e.rel));
        let _ = writeln!(out, "    <size>{}</size>", e.size);
        let _ = writeln!(
            out,
            "    <lastmodificationdate>{}</lastmodificationdate>",
            utc(e.mtime)
        );
        for (el, v) in hashes {
            let _ = writeln!(out, "    <{el}>{v}</{el}>");
        }
        let _ = writeln!(out, "    <hashdate>{}</hashdate>", utc(e.hashed_at));
        let _ = writeln!(out, "  </hash>");
    }
    if listed == 0 {
        return Err(MhlError::Unsupported(
            "MHL v1.1 needs at least one non-empty file".into(),
        ));
    }
    out.push_str("</hashlist>\n");

    let folder = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let path = root.join(format!(
        "{folder}_{}.mhl",
        now.to_zoned(TimeZone::UTC).strftime("%Y-%m-%d_%H%M%S")
    ));
    super::v2::write_new(&path, out.as_bytes())?;
    Ok(Written {
        path,
        skipped_empty,
    })
}

#[derive(Debug, Clone)]
pub struct Listed {
    pub file: String,
    pub size: Option<u64>,
    /// (element name, value) for every hash element present.
    pub hashes: Vec<(String, String)>,
}

pub fn parse(path: &Path) -> Result<Vec<Listed>, MhlError> {
    let f = std::fs::File::open(path).map_err(|e| MhlError::io(path, e))?;
    let mut out = Vec::new();
    let mut cur: Option<Listed> = None;
    let mut is_v11 = false;
    xml::walk(BufReader::new(f), |n| {
        match n {
            Node::Open { path: p, attrs } => {
                if p.len() == 1 {
                    is_v11 = p[0] == "hashlist" && xml::attr(attrs, "version") == Some("1.1");
                }
                if p.len() == 2 && p[1] == "hash" {
                    cur = Some(Listed {
                        file: String::new(),
                        size: None,
                        hashes: Vec::new(),
                    });
                }
            }
            Node::Close { path: p, text, .. } => {
                if let Some(c) = cur.as_mut() {
                    match (p.len(), p.last().map(String::as_str)) {
                        (3, Some("file")) => c.file = text.to_owned(),
                        (3, Some("size")) => c.size = text.parse().ok(),
                        (
                            3,
                            Some(
                                h
                                @ ("md5" | "sha1" | "xxhash64be" | "xxhash64" | "xxhash" | "null"),
                            ),
                        ) => {
                            c.hashes.push((h.to_owned(), text.to_ascii_lowercase()));
                        }
                        (2, Some("hash")) => out.push(cur.take().expect("current")),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })
    .map_err(|e| MhlError::Invalid(format!("{}: {e}", path.display())))?;
    if !is_v11 {
        return Err(MhlError::Invalid(format!(
            "{} is not an MHL v1.1 file",
            path.display()
        )));
    }
    Ok(out)
}

/// Verifies the files listed in a v1.1 manifest (paths relative to the manifest's folder).
pub fn verify(path: &Path) -> Result<VerifyReport, MhlError> {
    let base = path.parent().unwrap_or(Path::new("."));
    let mut report = VerifyReport::new(ManifestKind::MhlV11, path.to_path_buf());
    for l in parse(path)? {
        let rel = l.file.replace('\\', "/");
        if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
            report.issues.push(VerifyIssue::Unreadable {
                rel,
                reason: "unsafe path in manifest".into(),
            });
            continue;
        }
        let file = base.join(&rel);
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                report.issues.push(VerifyIssue::Unreadable {
                    rel,
                    reason: "not a regular file".into(),
                });
                continue;
            }
            Err(_) => {
                report.issues.push(VerifyIssue::Missing { rel });
                continue;
            }
        };
        if let Some(size) = l.size
            && size != meta.len()
        {
            report.issues.push(VerifyIssue::Mismatch {
                rel,
                recorded: format!("size {size}"),
                actual: format!("size {}", meta.len()),
            });
            continue;
        }
        let Some((el, recorded)) = l
            .hashes
            .iter()
            .find(|(el, _)| el != "null" && el != "xxhash")
        else {
            if l.hashes.iter().any(|(el, _)| el == "null") {
                report.verified += 1; // size-only entry, size matched
            } else {
                report.issues.push(VerifyIssue::Unreadable {
                    rel,
                    reason: "no supported checksum (32-bit xxhash is not supported)".into(),
                });
            }
            continue;
        };
        let algo = match el.as_str() {
            "md5" => HashAlgorithm::Md5,
            "sha1" => HashAlgorithm::Sha1,
            _ => HashAlgorithm::Xxh64,
        };
        match super::v2::hash_file(&file, algo) {
            Ok(actual) => {
                let v = actual.value();
                let reversed: String = (0..v.len())
                    .step_by(2)
                    .rev()
                    .map(|i| &v[i..i + 2])
                    .collect();
                let ok = &v == recorded || (el == "xxhash64" && &reversed == recorded);
                if ok {
                    report.verified += 1;
                } else {
                    report.issues.push(VerifyIssue::Mismatch {
                        rel,
                        recorded: format!("{el}:{recorded}"),
                        actual: actual.to_string(),
                    });
                }
            }
            Err(e) => report.issues.push(VerifyIssue::Unreadable {
                rel,
                reason: e.to_string(),
            }),
        }
    }
    Ok(report)
}
