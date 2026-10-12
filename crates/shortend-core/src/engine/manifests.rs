//! Writes the MHL manifest into every usable destination after the copy, and checks the card
//! against its own ASC MHL history when it carried one.

use std::collections::HashSet;

use jiff::Timestamp;

use super::journal::{Journal, Record, Replay};
use crate::fs::same_volume_dir;
use crate::hash::Digests;
use crate::model::{DestId, Failure, FileStatus, JobReport, JobState};
use crate::report::mhl::{self, MhlVersion, v2, v11};

/// Writes manifests for a finished run and folds any card-vs-manifest problems into `rep`.
pub(crate) fn write(rep: &mut JobReport, journal: &Journal, replay: Option<&Replay>) {
    let versions = rep.job.settings.reports.mhl.clone();
    if versions.is_empty() {
        return;
    }
    let now = Timestamp::now();
    let mut warnings = Vec::new();
    let mut issues = Vec::new();
    let mut omitted: Vec<String> = Vec::new();
    let mut written = Vec::new();

    for (i, d) in rep.job.destinations.iter().enumerate() {
        let id = DestId(i as u16);
        if matches!(
            rep.dest_stats[i].failure,
            Some(Failure::DestinationStalled { .. })
        ) || !same_volume_dir(&d.root, d.volume.dev)
            || !d.target.is_dir()
        {
            continue;
        }
        let ok: Vec<(usize, &Digests)> = rep
            .outcomes
            .iter()
            .enumerate()
            .filter(|(_, o)| o.dests[i].status.is_ok())
            .filter_map(|(k, o)| {
                o.dests[i]
                    .verified
                    .as_ref()
                    .or(o.source.as_ref())
                    .map(|dg| (k, dg))
            })
            .collect();
        if ok.is_empty() {
            continue;
        }
        let fresh = rep
            .outcomes
            .iter()
            .any(|o| o.dests[i].status == FileStatus::Verified);
        if replay.is_some_and(|r| r.mhl.contains(&id)) && !fresh {
            continue; // this destination already has a manifest covering everything it holds
        }

        // v1.1 first, so the ASC MHL generation can ignore the v1.1 file next to the media
        // (otherwise every ASC MHL verify would report it as a new, unlisted file).
        let mut order = versions.clone();
        order.sort_by_key(|v| *v == MhlVersion::V2);
        let mut extra_ignore = Vec::new();
        for &version in &order {
            let result = match version {
                MhlVersion::V2 => write_v2(rep, i, &ok, now, &extra_ignore, &mut warnings),
                MhlVersion::V1_1 => write_v11(rep, i, &ok, now).map(|w| {
                    if !w.skipped_empty.is_empty() {
                        let n = w.skipped_empty.len();
                        let shown: Vec<&str> = w.skipped_empty.iter().take(10).map(String::as_str).collect();
                        issues.push(format!(
                            "{}: the MHL v1.1 manifest does not list {n} zero-byte file(s) (its schema cannot); they are verified and listed in the ASC MHL v2 and the CSV/TXT/PDF reports: {}{}",
                            d.label,
                            shown.join(", "),
                            if n > shown.len() { ", …" } else { "" }
                        ));
                        for f in w.skipped_empty {
                            if !omitted.contains(&f) {
                                omitted.push(f);
                            }
                        }
                    }
                    if let Some(name) = w.path.file_name() {
                        extra_ignore.push(name.to_string_lossy().into_owned());
                    }
                    w.path
                }),
            };
            match result {
                Ok(path) => {
                    let file = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if journal.append(&Record::Mhl { dest: id, file }).is_err() {
                        warnings.push("could not record the manifest in the job journal".into());
                    }
                    written.push(path);
                }
                Err(e) => issues.push(format!("{}: could not write {version}: {e}", d.label)),
            }
        }
    }
    rep.manifests.extend(written);
    rep.warnings.extend(warnings);
    rep.manifest_issues.extend(issues);
    omitted.sort();
    rep.manifest_omissions = omitted;
    // After writing: a file that contradicts the card's history is still on the destination
    // and is recorded there with action `failed`.
    if versions.contains(&MhlVersion::V2) {
        check_card(rep);
    }
    if !rep.manifest_issues.is_empty() && rep.state == JobState::Completed {
        rep.state = JobState::CompletedWithFailures;
    }
}

/// Compares the card with its own ASC MHL history (if it has one): files whose checksums
/// contradict the history fail on every destination, and files the history lists that are
/// missing from the card are reported. Runs on every completed run, including resumes.
fn check_card(rep: &mut JobReport) {
    let history = match v2::History::load(&rep.job.source.root) {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            rep.manifest_issues.push(format!(
                "the card's ASC MHL history could not be checked: {e}"
            ));
            return;
        }
    };
    let mut issues = Vec::new();
    for o in &mut rep.outcomes {
        let (Some(rec), Some(actual)) = (history.record(o.entry.rel.as_str()), o.source.clone())
        else {
            continue;
        };
        if rec.original.is_none() {
            continue;
        }
        let bad = actual.iter().find_map(|d| {
            rec.first
                .iter()
                .find(|(a, v)| *a == d.algo() && *v != d.value())
                .map(|(a, v)| format!("{a}:{v}"))
        });
        let Some(recorded) = bad else { continue };
        issues.push(format!(
            "{}: card does not match its own ASC MHL (recorded {recorded}, card has {})",
            o.entry.rel, actual
        ));
        for d in &mut o.dests {
            if d.status.is_ok() {
                d.status = FileStatus::Failed(Failure::SourceManifestMismatch {
                    recorded: recorded.clone(),
                    actual: actual.clone(),
                });
            }
        }
    }
    let on_card: HashSet<&str> = rep.job.files.iter().map(|f| f.rel.as_str()).collect();
    let spec = v2::History::ignore_spec(Some(&history));
    let mut missing: Vec<&str> = history
        .file_paths()
        .filter(|p| !on_card.contains(p) && !spec.is_ignored(p, false))
        .collect();
    missing.sort_unstable();
    for m in missing {
        issues.push(format!(
            "{m}: listed in the card's ASC MHL but missing from the card"
        ));
    }
    rep.manifest_issues.extend(issues);
}

fn write_v2(
    rep: &JobReport,
    i: usize,
    ok: &[(usize, &Digests)],
    now: Timestamp,
    extra_ignore: &[String],
    warnings: &mut Vec<String>,
) -> Result<std::path::PathBuf, mhl::MhlError> {
    let d = &rep.job.destinations[i];
    let history = v2::History::load(&d.target)?;
    let files: Vec<v2::GenFile<'_>> = ok
        .iter()
        .map(|(k, dg)| {
            let e = &rep.outcomes[*k].entry;
            v2::GenFile {
                rel: e.rel.as_str(),
                size: e.size,
                mtime: e.mtime,
                digests: dg,
            }
        })
        .collect();
    let dirs: Vec<&str> = rep.job.dirs.iter().map(|r| r.as_str()).collect();
    let p = &rep.job.production;
    let comment: Vec<String> = p
        .fields()
        .into_iter()
        .filter(|(k, _)| *k != "Operator")
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let creator = v2::Creator {
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        hostname: mhl::hostname(),
        author: p.operator.clone(),
        location: None,
        comment: (!comment.is_empty()).then(|| comment.join("; ")),
    };
    let generation = v2::write_generation(
        &d.target,
        history.as_ref(),
        &files,
        &dirs,
        &creator,
        &v2::Process {
            kind: "transfer",
            extra_ignore,
        },
        now,
    )?;

    for u in &generation.unverifiable {
        warnings.push(format!(
            "{u}: the card's ASC MHL uses checksum algorithms this job did not compute; left out of the new generation"
        ));
    }
    Ok(generation.path)
}

fn write_v11(
    rep: &JobReport,
    i: usize,
    ok: &[(usize, &Digests)],
    now: Timestamp,
) -> Result<v11::Written, mhl::MhlError> {
    let d = &rep.job.destinations[i];
    let entries: Vec<v11::Entry<'_>> = ok
        .iter()
        .map(|(k, dg)| {
            let e = &rep.outcomes[*k].entry;
            v11::Entry {
                rel: e.rel.as_str(),
                size: e.size,
                mtime: e.mtime,
                digests: dg,
                hashed_at: rep.finished,
            }
        })
        .collect();
    let host = mhl::hostname();
    let user = mhl::username();
    let tool = format!("ShortEnd {}", env!("CARGO_PKG_VERSION"));
    let creator = v11::Creator {
        name: rep.job.production.operator.as_deref(),
        username: &user,
        hostname: &host,
        tool: &tool,
        started: rep.started,
        finished: rep.finished,
    };
    v11::write(&d.target, &entries, &creator, now)
}
