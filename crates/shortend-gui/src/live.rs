//! Work off the UI thread.
//!
//! - [`Task`]: a one-shot background computation (planning a job, listing history).
//! - [`Live`]: the state of a running job. The engine sends [`Event`]s to a forwarder thread
//!   that folds them into a shared `Live`, so the engine never waits on the UI's frame rate
//!   (and a hidden or busy window can't slow a copy down). The UI reads a snapshot each frame.

use std::sync::mpsc;

use eframe::egui;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use shortend_core::{CancelToken, Event, FileStatus, Job, JobReport, RelPath};

/// A value being computed on another thread.
pub struct Task<T>(Arc<Mutex<Option<T>>>);

impl<T: Send + 'static> Task<T> {
    pub fn spawn(ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let v = f();
            *lock(&out) = Some(v);
            ctx.request_repaint();
        });
        Self(slot)
    }

    /// The result, once (later calls return `None`).
    pub fn take(&self) -> Option<T> {
        lock(&self.0).take()
    }
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bytes-per-second over a sliding window of about two seconds.
#[derive(Debug, Clone, Default)]
pub struct Rate {
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Rate {
    const WINDOW: Duration = Duration::from_secs(2);

    pub fn add(&mut self, at: Instant, bytes: u64) {
        self.samples.push_back((at, bytes));
        while self.samples.len() > 2 && at.duration_since(self.samples[0].0) > Self::WINDOW {
            self.samples.pop_front();
        }
    }

    pub fn per_sec(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(a), Some(b)) if b.0 > a.0 => {
                b.1.saturating_sub(a.1) as f64 / b.0.duration_since(a.0).as_secs_f64()
            }
            _ => 0.0,
        }
    }
}

/// Most failures and warnings kept for display; the reports have all of them.
const MAX_LISTED: usize = 50_000;

#[derive(Debug)]
pub struct Live {
    pub job: Job,
    pub resumed: bool,
    pub started: Instant,
    pub finished_at: Option<Instant>,
    pub source_bytes: u64,
    pub dest_bytes: Vec<u64>,
    pub total_bytes: u64,
    pub files_done: usize,
    pub already_verified: usize,
    /// Files being copied right now.
    pub current: Vec<RelPath>,
    pub failures: Vec<String>,
    pub dest_failed: Vec<Option<String>>,
    pub warnings: Vec<String>,
    pub source_rate: Rate,
    pub dest_rates: Vec<Rate>,
    pub cancelling: bool,
    /// Set when the engine returns.
    pub result: Option<Result<JobReport, String>>,
}

impl Live {
    fn new(job: Job, resumed: bool) -> Self {
        let n = job.destinations.len();
        Self {
            total_bytes: job.total_bytes(),
            job,
            resumed,
            started: Instant::now(),
            finished_at: None,
            source_bytes: 0,
            dest_bytes: vec![0; n],
            files_done: 0,
            already_verified: 0,
            current: Vec::new(),
            failures: Vec::new(),
            dest_failed: vec![None; n],
            warnings: Vec::new(),
            source_rate: Rate::default(),
            dest_rates: vec![Rate::default(); n],
            cancelling: false,
            result: None,
        }
    }

    pub fn running(&self) -> bool {
        self.result.is_none()
    }

    pub fn apply(&mut self, ev: Event) {
        let now = Instant::now();
        match ev {
            Event::Started {
                bytes,
                already_verified,
                ..
            } => {
                self.total_bytes = bytes;
                self.already_verified = already_verified;
                self.files_done = already_verified;
            }
            Event::Progress {
                source_bytes,
                dest_bytes,
                total_bytes,
            } => {
                self.source_bytes = source_bytes;
                self.total_bytes = total_bytes;
                self.source_rate.add(now, source_bytes);
                for (i, b) in dest_bytes.iter().enumerate() {
                    if let Some(r) = self.dest_rates.get_mut(i) {
                        r.add(now, *b);
                    }
                }
                self.dest_bytes = dest_bytes;
            }
            Event::FileStarted(rel) => {
                if !self.current.contains(&rel) {
                    self.current.push(rel);
                }
            }
            Event::FileFinished(o) => {
                self.current.retain(|r| *r != o.entry.rel);
                self.files_done += 1;
                for d in &o.dests {
                    if let FileStatus::Failed(f) = &d.status
                        && self.failures.len() < MAX_LISTED
                    {
                        let label = &self.job.destinations[d.dest.index()].label;
                        self.failures
                            .push(format!("{}  [{label}]  {f}", o.entry.rel));
                    }
                }
            }
            Event::DestinationFailed { dest, failure } => {
                if let Some(slot) = self.dest_failed.get_mut(dest.index()) {
                    *slot = Some(failure.to_string());
                }
            }
            Event::Warning(w) => {
                if self.warnings.len() < MAX_LISTED {
                    self.warnings.push(w);
                }
            }
            Event::ReportWritten(_) => {}
        }
    }
}

/// A job running on worker threads.
pub struct Running {
    pub live: Arc<Mutex<Live>>,
    pub cancel: CancelToken,
}

impl Running {
    /// Runs `job` (a planned job, or a stored one with `resume = true`).
    pub fn start(ctx: &egui::Context, job: Job, resume: bool) -> Self {
        let live = Arc::new(Mutex::new(Live::new(job.clone(), resume)));
        let cancel = CancelToken::new();
        let (tx, rx) = mpsc::sync_channel::<Event>(1024);

        let forward = live.clone();
        let fctx = ctx.clone();
        let forwarder = std::thread::spawn(move || {
            for ev in rx {
                let important =
                    matches!(ev, Event::DestinationFailed { .. } | Event::Started { .. });
                lock(&forward).apply(ev);
                if important {
                    fctx.request_repaint();
                }
            }
        });

        let engine_live = live.clone();
        let engine_cancel = cancel.clone();
        let ectx = ctx.clone();
        std::thread::spawn(move || {
            let result = if resume {
                shortend_core::resume(&job.id.to_string(), &tx, &engine_cancel)
            } else {
                shortend_core::run(&job, &tx, &engine_cancel)
            };
            drop(tx);
            let _ = forwarder.join(); // every event is applied before the result shows
            let mut l = lock(&engine_live);
            if let Ok(rep) = &result {
                l.job = rep.job.clone(); // the folder with its claimed `{seq}` number
            }
            l.current.clear();
            l.finished_at = Some(Instant::now());
            l.result = Some(result.map_err(|e| e.to_string()));
            drop(l);
            ectx.request_repaint();
        });
        Self { live, cancel }
    }

    pub fn cancel(&self) {
        lock(&self.live).cancelling = true;
        self.cancel.cancel();
    }

    pub fn is_running(&self) -> bool {
        lock(&self.live).running()
    }
}
