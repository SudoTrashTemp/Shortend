//! Offload reports. M1: CSV and TXT. Written into `<target>/_ShortEnd/` on every reachable
//! destination and into the job's state directory.

mod csv;
pub mod mhl;
mod pdf;
mod txt;

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::engine::preflight::REPORT_DIR;
use crate::fs::same_volume_dir;
use crate::model::{Failure, FileStatus, JobReport};

/// Writes every enabled report. Returns the files written and warnings for places that failed;
/// a report that can't be written never fails the job.
pub fn write_all(rep: &JobReport, state_dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let opts = &rep.job.settings.reports;
    let mut rendered: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut warnings = Vec::new();
    if opts.csv {
        match csv::render(rep) {
            Ok(b) => rendered.push(("csv", b)),
            Err(e) => warnings.push(format!("could not build the CSV report: {e}")),
        }
    }
    if opts.txt {
        rendered.push(("txt", txt::render(rep).into_bytes()));
    }
    if opts.pdf.is_some() {
        match pdf::render(rep) {
            Ok((b, w)) => {
                rendered.push(("pdf", b));
                warnings.extend(w);
            }
            Err(e) => warnings.push(format!("could not build the PDF report: {e}")),
        }
    }

    let base = base_name(rep);
    let mut places: Vec<PathBuf> = rep
        .job
        .destinations
        .iter()
        .zip(&rep.dest_stats)
        // Never touch a stalled destination again: any call could block forever.
        .filter(|(_, st)| !matches!(st.failure, Some(Failure::DestinationStalled { .. })))
        .map(|(d, _)| d)
        .filter(|d| same_volume_dir(&d.root, d.volume.dev) && d.target.is_dir())
        .map(|d| d.target.join(REPORT_DIR))
        .collect();
    places.push(state_dir.to_path_buf());

    let mut written = Vec::new();
    for dir in places {
        for (ext, bytes) in &rendered {
            let path = dir.join(format!("{base}.{ext}"));
            match write_file(&path, bytes) {
                Ok(()) => written.push(path),
                Err(e) => warnings.push(format!("could not write report {}: {e}", path.display())),
            }
        }
    }
    (written, warnings)
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    f.sync_all()
}

fn base_name(rep: &JobReport) -> String {
    let card = rep.job.folder.file_name();
    let stamp = rep
        .job
        .created
        .to_zoned(TimeZone::system())
        .strftime("%Y%m%d-%H%M%S");
    let short = &rep.job.id.to_string()[..8];
    format!("ShortEnd_{card}_{stamp}_{short}")
}

pub(crate) fn local_time(ts: Timestamp) -> String {
    ts.to_zoned(TimeZone::system())
        .strftime("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// Decimal units, as drive vendors and most DIT tools use.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{:.1} s", d.as_secs_f64()),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m {:02}s", s / 3600, (s / 60) % 60, s % 60),
    }
}

pub fn human_rate(bytes: u64, d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs <= 0.0 {
        return "-".to_owned();
    }
    format!("{}/s", human_bytes((bytes as f64 / secs) as u64))
}

pub(crate) fn status_word(s: &FileStatus) -> &'static str {
    match s {
        FileStatus::Verified => "verified",
        FileStatus::AlreadyVerified => "reverified_on_resume",
        FileStatus::Failed(_) => "failed",
        FileStatus::Pending => "not_copied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_500_000_000), "1.50 GB");
        assert_eq!(human_duration(Duration::from_secs(83)), "1m 23s");
        assert_eq!(human_duration(Duration::from_secs(3723)), "1h 02m 03s");
        assert_eq!(
            human_rate(200_000_000, Duration::from_secs(2)),
            "100.00 MB/s"
        );
    }
}
