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
    let Some(version) = rep.job.settings.reports.mhl else {
        return;
    };
    let now = Timestamp::now();
    let mut card_mismatches: Vec<v2::Mismatch> = Vec::new();
    let mut card_checked = false;
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
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

        let result = match version {
            MhlVersion::V2 => write_v2(rep, i, &ok, now, !card_checked, &mut issues, &mut warnings)
                .map(|(path, mism)| {
                    if !card_checked {
                        card_checked = true;
                        card_mismatches = mism;
                    }
                    path
                }),
            MhlVersion::V1_1 => write_v11(rep, i, &ok, now, &mut warnings),
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
            Err(e) => warnings.push(format!("{}: could not write {version}: {e}", d.label)),
        }
    }
    rep.manifests.extend(written);
    rep.warnings.extend(warnings);
    rep.manifest_issues.extend(issues);

    for m in card_mismatches {
        rep.manifest_issues.push(format!(
            "{}: card does not match its own ASC MHL ({} recorded {}, card has {})",
            m.rel,
            m.actual.algo(),
            m.recorded,
            m.actual.value()
        ));
        if let Some(o) = rep
            .outcomes
            .iter_mut()
            .find(|o| o.entry.rel.as_str() == m.rel)
        {
            let actual = o.source.clone();
            for d in &mut o.dests {
                if d.status.is_ok() {
                    d.status = FileStatus::Failed(Failure::SourceManifestMismatch {
                        recorded: format!("{}:{}", m.actual.algo(), m.recorded),
                        actual: actual.clone().expect("verified files have checksums"),
                    });
                }
            }
        }
    }
    if !rep.manifest_issues.is_empty() && rep.state == JobState::Completed {
        rep.state = JobState::CompletedWithFailures;
    }
}

#[allow(clippy::too_many_arguments)]
fn write_v2(
    rep: &JobReport,
    i: usize,
    ok: &[(usize, &Digests)],
    now: Timestamp,
    check_card: bool,
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(std::path::PathBuf, Vec<v2::Mismatch>), mhl::MhlError> {
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
        "transfer",
        now,
    )?;

    // Files the card's history lists that the card no longer has.
    if let Some(h) = &history
        && check_card
    {
        let on_card: HashSet<&str> = rep.job.files.iter().map(|f| f.rel.as_str()).collect();
        let spec = v2::History::ignore_spec(Some(h));
        let mut missing: Vec<String> = h
            .file_paths()
            .filter(|p| !on_card.contains(p) && !spec.is_ignored(p, false))
            .map(str::to_owned)
            .collect();
        missing.sort();
        for m in missing {
            issues.push(format!(
                "{m}: listed in the card's ASC MHL but missing from the card"
            ));
        }
    }
    for u in &generation.unverifiable {
        warnings.push(format!(
            "{u}: the card's ASC MHL uses checksum algorithms this job did not compute; left out of the new generation"
        ));
    }
    Ok((generation.path, generation.mismatches))
}

fn write_v11(
    rep: &JobReport,
    i: usize,
    ok: &[(usize, &Digests)],
    now: Timestamp,
    warnings: &mut Vec<String>,
) -> Result<std::path::PathBuf, mhl::MhlError> {
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
    let written = v11::write(&d.target, &entries, &creator, now)?;
    if !written.skipped_empty.is_empty() {
        let n = written.skipped_empty.len();
        warnings.push(format!(
            "{}: {n} zero-byte file(s) not listed in the MHL v1.1 (its schema requires size ≥ 1); they are in the CSV/TXT reports",
            d.label
        ));
    }
    Ok(written.path)
}
