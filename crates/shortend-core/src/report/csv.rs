//! CSV report: one row per file × destination, one checksum column per algorithm.

use super::status_word;
use crate::model::{FileStatus, JobReport};

pub fn render(rep: &JobReport) -> Result<Vec<u8>, ::csv::Error> {
    let algos = rep.job.settings.hashes.as_slice();
    let mut w = ::csv::Writer::from_writer(Vec::new());

    let mut header = vec![
        "relative_path".to_owned(),
        "size_bytes".to_owned(),
        "modified_utc".to_owned(),
    ];
    header.extend(algos.iter().map(|a| format!("source_{a}")));
    header.extend(["destination".to_owned(), "destination_path".to_owned()]);
    header.extend(algos.iter().map(|a| format!("verified_{a}")));
    header.extend(["status".to_owned(), "detail".to_owned()]);
    w.write_record(&header)?;

    for o in &rep.outcomes {
        for d in &o.dests {
            let dest = &rep.job.destinations[d.dest.index()];
            let mut row = vec![
                o.entry.rel.to_string(),
                o.entry.size.to_string(),
                o.entry.mtime.to_string(),
            ];
            row.extend(digest_cells(o.source.as_ref(), algos.len()));
            row.push(dest.label.clone());
            row.push(o.entry.rel.to_path(&dest.target).display().to_string());
            row.extend(digest_cells(d.verified.as_ref(), algos.len()));
            row.push(status_word(&d.status).to_owned());
            row.push(match &d.status {
                FileStatus::Failed(f) => f.to_string(),
                _ => String::new(),
            });
            w.write_record(&row)?;
        }
    }
    w.into_inner().map_err(|e| e.into_error().into())
}

fn digest_cells(d: Option<&crate::hash::Digests>, n: usize) -> Vec<String> {
    match d {
        Some(ds) => ds.iter().map(crate::hash::Digest::value).collect(),
        None => vec![String::new(); n],
    }
}
