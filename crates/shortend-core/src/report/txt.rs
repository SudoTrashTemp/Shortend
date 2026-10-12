//! Plain-text report: summary first, then failures, then the full file list.

use std::fmt::Write as _;

use super::{human_bytes, human_duration, human_rate, local_time};
use crate::model::{FileStatus, JobReport, JobState};

pub fn render(rep: &JobReport) -> String {
    let job = &rep.job;
    let mut s = String::new();
    let failed = rep.failed_files();
    let n_files = job.files.len();
    let n_dest = job.destinations.len();

    let _ = writeln!(s, "ShortEnd offload report");
    let _ = writeln!(s, "=======================");
    let verdict = match rep.state {
        JobState::Completed => {
            format!("PASS - all {n_files} files verified on all {n_dest} destinations")
        }
        _ => format!(
            "FAIL - {failed} of {n_files} files not verified on every destination ({})",
            rep.state
        ),
    };
    let _ = writeln!(s, "Result:       {verdict}");
    let _ = writeln!(s, "Job:          {}", job.id);
    let _ = writeln!(s, "Started:      {}", local_time(rep.started));
    let _ = writeln!(s, "Finished:     {}", local_time(rep.finished));
    let _ = writeln!(s, "Duration:     {}", human_duration(rep.elapsed()));
    let _ = writeln!(
        s,
        "Source:       {} (card {}, {})",
        job.source.root.display(),
        job.source.label,
        job.source.volume.fs_type
    );
    let _ = writeln!(s, "Folder:       {}", job.folder);
    let _ = writeln!(s, "Checksums:    {}", job.settings.hashes);
    let _ = writeln!(
        s,
        "Verification: destination read-back{}",
        if job.settings.source_reread {
            " + second source read"
        } else {
            ""
        }
    );
    let _ = writeln!(
        s,
        "Files:        {n_files} ({})",
        human_bytes(job.total_bytes())
    );
    for (k, v) in job.production.fields() {
        let _ = writeln!(s, "{:<13} {v}", format!("{k}:"));
    }

    let _ = writeln!(s, "\nDestinations");
    for (d, st) in job.destinations.iter().zip(&rep.dest_stats) {
        let ok = rep
            .outcomes
            .iter()
            .filter(|o| o.dests[d.id.index()].status.is_ok())
            .count();
        let _ = writeln!(
            s,
            "  [{}] {}  {}  ({})",
            d.id.0 + 1,
            d.label,
            d.target.display(),
            d.volume.fs_type
        );
        let _ = writeln!(
            s,
            "      {ok}/{n_files} files verified, {} written this run at {}",
            human_bytes(st.bytes_written),
            human_rate(st.bytes_written, rep.elapsed())
        );
        if let Some(f) = &st.failure {
            let _ = writeln!(s, "      DESTINATION FAILED: {f}");
        }
        if matches!(
            d.volume.fs_type,
            crate::fs::FsType::Smb | crate::fs::FsType::Nfs
        ) {
            let _ = writeln!(
                s,
                "      note: network filesystem; read-back may be served from the server's cache"
            );
        }
    }

    if !rep.warnings.is_empty() {
        let _ = writeln!(s, "\nWarnings");
        for w in &rep.warnings {
            let _ = writeln!(s, "  - {w}");
        }
    }

    if failed > 0 {
        let _ = writeln!(s, "\nFailures");
        for o in rep.outcomes.iter().filter(|o| !o.all_ok()) {
            for d in o.dests.iter().filter(|d| !d.status.is_ok()) {
                let label = &job.destinations[d.dest.index()].label;
                let why = match &d.status {
                    FileStatus::Failed(f) => f.to_string(),
                    _ => "not copied".to_owned(),
                };
                let _ = writeln!(s, "  {}  [{label}]  {why}", o.entry.rel);
            }
        }
    }

    let _ = writeln!(
        s,
        "\nFiles ({} checksum, size, path)",
        job.settings.hashes.primary()
    );
    for o in &rep.outcomes {
        let sum = o
            .source
            .as_ref()
            .map(|d| d.primary().value())
            .unwrap_or_else(|| "-".to_owned());
        let mark = if o.all_ok() { "" } else { "  [FAILED]" };
        let _ = writeln!(s, "  {sum}  {:>15}  {}{mark}", o.entry.size, o.entry.rel);
    }
    s
}
