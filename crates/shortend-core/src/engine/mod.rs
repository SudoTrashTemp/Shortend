//! Job lifecycle: plan → run (or resume) → reports.

pub mod journal;
mod manifests;
mod pipeline;
pub mod preflight;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use jiff::Timestamp;

use crate::error::{Error, Result};
use crate::model::{CancelToken, Event, Job, JobId, JobReport, JobSpec, JobState};
use crate::report;
use journal::{Journal, Record};

/// Validates a job and resolves it fully. Writes nothing except a short-lived writability
/// probe inside each destination's staging folder.
pub fn plan(spec: JobSpec) -> Result<Job> {
    preflight::plan(spec)
}

/// Runs a planned job to completion, cancellation or source loss. The job's state is
/// persisted first, so it can be resumed after a crash at any point.
pub fn run(job: &Job, events: &SyncSender<Event>, cancel: &CancelToken) -> Result<JobReport> {
    let dir = state::jobs_root().join(job.id.to_string());
    state::save(&dir, job)?;
    let journal = open_journal(&dir)?;
    append(
        &journal,
        &dir,
        &Record::Start {
            at: Timestamp::now(),
            resume: false,
        },
    )?;
    finish(job, &dir, &journal, None, events, cancel)
}

/// Continues an interrupted job by full ID or unique prefix. Files already verified on a
/// destination are skipped; everything else is copied from the start.
pub fn resume(id: &str, events: &SyncSender<Event>, cancel: &CancelToken) -> Result<JobReport> {
    let dir = state::find(id)?;
    let mut job = state::load(&dir)?;
    preflight::revalidate(&mut job)?;
    state::save(&dir, &job)?; // device numbers may have changed after a reconnect
    let replay =
        journal::replay(&dir.join(state::JOURNAL_FILE)).map_err(|e| Error::io_at(&dir, e))?;
    let journal = open_journal(&dir)?;
    append(
        &journal,
        &dir,
        &Record::Start {
            at: Timestamp::now(),
            resume: true,
        },
    )?;
    finish(&job, &dir, &journal, Some(&replay), events, cancel)
}

/// Loads a stored job (header and scan) by full ID or unique prefix.
pub fn load_job(id: &str) -> Result<Job> {
    state::load(&state::find(id)?)
}

#[derive(Debug, Clone)]
pub struct JobListing {
    pub id: JobId,
    pub created: Timestamp,
    pub source: PathBuf,
    pub folder: String,
    pub destinations: usize,
    /// `None` while unfinished (running, or interrupted and resumable).
    pub state: Option<JobState>,
}

/// All jobs in the state directory, oldest first.
pub fn list_jobs() -> Result<Vec<JobListing>> {
    let root = state::jobs_root();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::io_at(&root, e)),
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(job) = state::load_header(&dir) else {
            continue;
        };
        let state = journal::replay(&dir.join(state::JOURNAL_FILE))
            .ok()
            .and_then(|r| r.last_state);
        out.push(JobListing {
            id: job.id,
            created: job.created,
            source: job.source.root,
            folder: job.folder.to_string(),
            destinations: job.destinations.len(),
            state,
        });
    }
    out.sort_by_key(|j| j.id);
    Ok(out)
}

fn open_journal(dir: &std::path::Path) -> Result<Arc<Journal>> {
    let path = dir.join(state::JOURNAL_FILE);
    Journal::open(&path)
        .map(Arc::new)
        .map_err(|e| Error::io_at(&path, e))
}

fn append(journal: &Journal, dir: &std::path::Path, rec: &Record) -> Result<()> {
    journal.append(rec).map_err(|e| Error::io_at(dir, e))
}

fn finish(
    job: &Job,
    dir: &std::path::Path,
    journal: &Arc<Journal>,
    replay: Option<&journal::Replay>,
    events: &SyncSender<Event>,
    cancel: &CancelToken,
) -> Result<JobReport> {
    let started = Timestamp::now();
    let out = pipeline::execute(job, journal, replay, events, cancel);
    let mut rep = JobReport {
        job: job.clone(),
        state: out.state,
        started,
        finished: Timestamp::now(),
        outcomes: out.outcomes,
        dest_stats: out.dest_stats,
        warnings: out.warnings,
        manifest_issues: Vec::new(),
        manifests: Vec::new(),
        report_files: Vec::new(),
    };
    // Reports describe a finished job; a cancelled or aborted one gets them when resumed.
    if matches!(
        rep.state,
        JobState::Completed | JobState::CompletedWithFailures
    ) {
        let before = rep.warnings.len();
        manifests::write(&mut rep, journal, replay);
        for w in &rep.warnings[before..] {
            let _ = events.send(Event::Warning(w.clone()));
        }
        for m in &rep.manifests {
            let _ = events.send(Event::ReportWritten(m.clone()));
        }
        let (files, warnings) = report::write_all(&rep, dir);
        for w in warnings {
            let _ = events.send(Event::Warning(w.clone()));
            rep.warnings.push(w);
        }
        for f in &files {
            let _ = events.send(Event::ReportWritten(f.clone()));
        }
        rep.report_files = files;
    }
    append(
        journal,
        dir,
        &Record::End {
            at: rep.finished,
            state: rep.state,
        },
    )?;
    journal.sync().map_err(|e| Error::io_at(dir, e))?;
    Ok(rep)
}
