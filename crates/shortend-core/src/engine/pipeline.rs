//! The copy pipeline: one source read hashed in-stream, fanned out to every destination,
//! staged, read back from the device, and atomically committed. See PLAN.md §4.3–4.4.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;

use super::journal::{Journal, Record, Replay};
use super::preflight::STAGING_DIR;
use crate::fs::{platform, same_volume_dir};
use crate::hash::{Digests, HashList, MultiHasher};
use crate::model::{
    CancelToken, DestId, DestOutcome, DestStats, Destination, Event, Failure, FileEntry,
    FileOutcome, FileStatus, Job, JobState,
};

/// Chunks queued per destination writer. With the reader's own buffer this bounds the
/// buffers alive per file in flight to `POOL_DEPTH`.
const QUEUE_DEPTH: usize = 2;
const POOL_DEPTH: usize = QUEUE_DEPTH + 2;
/// Flush and evict written pages this often so a dead drive surfaces early and a huge clip
/// doesn't fill the page cache.
const SYNC_INTERVAL: u64 = 256 << 20;
const VERIFY_BUF_MAX: usize = 2 << 20;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

pub(crate) struct Outcome {
    pub state: JobState,
    pub outcomes: Vec<FileOutcome>,
    pub dest_stats: Vec<DestStats>,
    pub warnings: Vec<String>,
}

/// Per-destination activity, read by the stall watchdog.
struct Activity {
    /// Writers currently working on this destination.
    busy: AtomicUsize,
    /// Milliseconds since `Ctx::started` of the last byte written or verified.
    last_progress_ms: AtomicU64,
}

/// Shared state of one run. Owned (behind an `Arc`) so writer threads can outlive a call:
/// a writer blocked inside the kernel on a hung drive cannot be joined or killed, so it is
/// left behind once the watchdog has given up on its destination.
struct Ctx {
    job: Arc<Job>,
    /// Taken (set to `None`) when the run ends, so an abandoned writer thread that still holds
    /// the context does not keep the caller's event channel open.
    events: Mutex<Option<SyncSender<Event>>>,
    cancel: CancelToken,
    journal: Arc<Journal>,
    staging: Vec<PathBuf>,
    dead: Vec<Mutex<Option<Failure>>>,
    abort: AtomicBool,
    src_bytes: AtomicU64,
    dest_bytes: Vec<AtomicU64>,
    verified_bytes: Vec<AtomicU64>,
    activity: Vec<Activity>,
    started: Instant,
    warnings: Mutex<Vec<String>>,
    journal_failed: AtomicBool,
    #[cfg(feature = "fault-injection")]
    fault: fault::Fault,
}

impl Ctx {
    fn emit(&self, ev: Event) {
        let tx = self
            .events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.send(ev);
        }
    }

    fn try_emit(&self, ev: Event) -> bool {
        let tx = self
            .events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        match tx.map(|tx| tx.try_send(ev)) {
            Some(Ok(()) | Err(TrySendError::Full(_))) => true,
            Some(Err(TrySendError::Disconnected(_))) | None => false,
        }
    }

    fn warn(&self, msg: String) {
        self.emit(Event::Warning(msg.clone()));
        self.warnings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(msg);
    }

    fn record(&self, rec: &Record) {
        if let Err(e) = self.journal.append(rec)
            && !self.journal_failed.swap(true, Ordering::SeqCst)
        {
            self.warn(format!(
                "cannot write the job journal ({e}); resume will re-verify more files"
            ));
        }
    }

    fn dead(&self, d: DestId) -> Option<Failure> {
        self.dead[d.index()]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn stalled(&self, d: DestId) -> bool {
        matches!(self.dead(d), Some(Failure::DestinationStalled { .. }))
    }

    /// Marks a destination unusable for the rest of the job (first failure wins).
    fn kill(&self, d: DestId, failure: Failure) {
        let mut slot = self.dead[d.index()]
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(failure.clone());
            drop(slot);
            self.record(&Record::DestFailed {
                dest: d,
                failure: failure.clone(),
            });
            self.emit(Event::DestinationFailed { dest: d, failure });
        }
    }

    fn dest(&self, d: DestId) -> &Destination {
        &self.job.destinations[d.index()]
    }

    fn stopping(&self) -> bool {
        self.cancel.is_cancelled() || self.abort.load(Ordering::SeqCst)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Records progress on a destination for the watchdog.
    fn tick(&self, d: DestId) {
        self.activity[d.index()]
            .last_progress_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    /// Marks a writer as working on `d` until the guard is dropped.
    fn busy(self: &Arc<Self>, d: DestId) -> BusyGuard {
        self.tick(d);
        self.activity[d.index()].busy.fetch_add(1, Ordering::SeqCst);
        BusyGuard {
            ctx: Arc::clone(self),
            d,
        }
    }

    /// Stall watchdog: a destination with a writer in progress but no bytes written or
    /// verified for `stall_timeout_secs` is given up on.
    fn check_stalls(&self) {
        let secs = self.job.settings.stall_timeout_secs;
        if secs == 0 {
            return;
        }
        let limit = u64::from(secs) * 1000;
        let now = self.now_ms();
        for (i, a) in self.activity.iter().enumerate() {
            let d = DestId(i as u16);
            if a.busy.load(Ordering::SeqCst) > 0
                && now.saturating_sub(a.last_progress_ms.load(Ordering::Relaxed)) >= limit
                && self.dead(d).is_none()
            {
                self.kill(d, Failure::DestinationStalled { seconds: secs });
            }
        }
    }
}

struct BusyGuard {
    ctx: Arc<Ctx>,
    d: DestId,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.ctx.activity[self.d.index()]
            .busy
            .fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs (or continues) a job. `replay` is `Some` when resuming.
pub(crate) fn execute(
    job: &Job,
    journal: &Arc<Journal>,
    replay: Option<&Replay>,
    events: &SyncSender<Event>,
    cancel: &CancelToken,
) -> Outcome {
    let n_dest = job.destinations.len();
    let ctx = Arc::new(Ctx {
        job: Arc::new(job.clone()),
        events: Mutex::new(Some(events.clone())),
        cancel: cancel.clone(),
        journal: Arc::clone(journal),
        staging: job
            .destinations
            .iter()
            .map(|d| d.root.join(STAGING_DIR).join(job.id.to_string()))
            .collect(),
        dead: (0..n_dest).map(|_| Mutex::new(None)).collect(),
        abort: AtomicBool::new(false),
        src_bytes: AtomicU64::new(0),
        dest_bytes: (0..n_dest).map(|_| AtomicU64::new(0)).collect(),
        verified_bytes: (0..n_dest).map(|_| AtomicU64::new(0)).collect(),
        activity: (0..n_dest)
            .map(|_| Activity {
                busy: AtomicUsize::new(0),
                last_progress_ms: AtomicU64::new(0),
            })
            .collect(),
        started: Instant::now(),
        warnings: Mutex::new(job.warnings.clone()),
        journal_failed: AtomicBool::new(false),
        #[cfg(feature = "fault-injection")]
        fault: fault::Fault::from_env(),
    });

    // The watchdog runs from the start: setup and resume's re-verification touch the
    // destinations too, and a hung drive there must not hang the whole job.
    let done = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (ctx, done) = (Arc::clone(&ctx), Arc::clone(&done));
        std::thread::spawn(move || progress_ticker(&ctx, &done))
    };

    let ids: Vec<DestId> = job.destinations.iter().map(|d| d.id).collect();
    for (d, r) in guarded(&ctx, &ids, |ctx, d| prepare_destination(ctx, ctx.dest(d))) {
        if let Some(Err(f)) = r {
            ctx.kill(d, f);
        }
    }

    let outcomes: Vec<Mutex<FileOutcome>> = job
        .files
        .iter()
        .map(|entry| {
            Mutex::new(FileOutcome {
                entry: entry.clone(),
                source: None,
                reread: None,
                dests: job
                    .destinations
                    .iter()
                    .map(|d| DestOutcome {
                        dest: d.id,
                        status: FileStatus::Pending,
                        verified: None,
                    })
                    .collect(),
            })
        })
        .collect();

    let mut already = 0;
    if let Some(replay) = replay {
        let hashes = &job.settings.hashes;
        for o in &outcomes {
            let mut o = o.lock().unwrap_or_else(|p| p.into_inner());
            o.source = replay
                .src
                .get(&o.entry.rel)
                .filter(|d| d.same_algorithms(hashes))
                .cloned();
        }
        let sources: Arc<Vec<Option<Digests>>> = Arc::new(
            outcomes
                .iter()
                .map(|o| o.lock().unwrap_or_else(|p| p.into_inner()).source.clone())
                .collect(),
        );
        let replay = Arc::new(replay.clone());
        let live: Vec<DestId> = ids
            .iter()
            .copied()
            .filter(|d| ctx.dead(*d).is_none())
            .collect();
        let results = guarded(&ctx, &live, move |ctx, d| {
            reconcile_dest(ctx, d, &sources, &replay)
        });
        for (d, r) in results {
            for (idx, digests) in r.unwrap_or_default() {
                let mut o = outcomes[idx].lock().unwrap_or_else(|p| p.into_inner());
                let slot = &mut o.dests[d.index()];
                slot.status = FileStatus::AlreadyVerified;
                slot.verified = Some(digests);
            }
        }
        already = outcomes
            .iter()
            .filter(|o| {
                let o = o.lock().unwrap_or_else(|p| p.into_inner());
                o.dests
                    .iter()
                    .all(|d| d.status == FileStatus::AlreadyVerified)
            })
            .count();
    }

    let work: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            let o = o.lock().unwrap_or_else(|p| p.into_inner());
            o.dests
                .iter()
                .any(|d| d.status == FileStatus::Pending && ctx.dead(d.dest).is_none())
        })
        .map(|(i, _)| i)
        .collect();

    ctx.emit(Event::Started {
        job: job.id,
        files: job.files.len(),
        bytes: job.total_bytes(),
        already_verified: already,
    });

    let next = AtomicUsize::new(0);
    let workers = usize::from(job.settings.parallel_files.get()).min(work.len().max(1));
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let pool = Pool::new(job.settings.chunk_size as usize);
                    loop {
                        if ctx.stopping() {
                            break;
                        }
                        let k = next.fetch_add(1, Ordering::SeqCst);
                        let Some(&idx) = work.get(k) else { break };
                        let mut outcome = outcomes[idx]
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        process_file(&ctx, idx, &mut outcome, &pool);
                        let finished = !outcome
                            .dests
                            .iter()
                            .any(|d| d.status == FileStatus::Pending);
                        *outcomes[idx].lock().unwrap_or_else(|p| p.into_inner()) = outcome.clone();
                        if finished {
                            ctx.emit(Event::FileFinished(Box::new(outcome)));
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
    });

    let mut outcomes: Vec<FileOutcome> = outcomes
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|p| p.into_inner()))
        .collect();

    // A destination that died marks everything it never got as failed with its reason.
    for o in &mut outcomes {
        for d in &mut o.dests {
            if d.status == FileStatus::Pending
                && let Some(f) = ctx.dead(d.dest)
            {
                d.status = FileStatus::Failed(f);
            }
        }
    }

    finish_destinations(&ctx);
    done.store(true, Ordering::SeqCst);
    let _ = ticker.join();
    let _ = journal.sync();

    let state = if cancel.is_cancelled() {
        JobState::Cancelled
    } else if ctx.abort.load(Ordering::SeqCst) {
        JobState::Aborted
    } else if outcomes.iter().all(FileOutcome::all_ok) {
        JobState::Completed
    } else {
        JobState::CompletedWithFailures
    };

    let dest_stats = (0..n_dest)
        .map(|i| DestStats {
            bytes_written: ctx.dest_bytes[i].load(Ordering::SeqCst),
            bytes_verified: ctx.verified_bytes[i].load(Ordering::SeqCst),
            failure: ctx.dead(DestId(i as u16)),
        })
        .collect();
    let warnings = ctx
        .warnings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    *ctx.events.lock().unwrap_or_else(|p| p.into_inner()) = None;
    Outcome {
        state,
        outcomes,
        dest_stats,
        warnings,
    }
}

fn progress_ticker(ctx: &Ctx, done: &AtomicBool) {
    let total = ctx.job.total_bytes();
    loop {
        let finished = done.load(Ordering::SeqCst);
        let ev = Event::Progress {
            source_bytes: ctx.src_bytes.load(Ordering::SeqCst),
            dest_bytes: ctx
                .dest_bytes
                .iter()
                .map(|b| b.load(Ordering::SeqCst))
                .collect(),
            total_bytes: total,
        };
        if !ctx.try_emit(ev) {
            return;
        }
        ctx.check_stalls();
        if finished {
            return;
        }
        std::thread::sleep(PROGRESS_EVERY);
    }
}

/// Revalidates a destination and creates its (empty) staging folder and the target tree.
fn prepare_destination(ctx: &Ctx, d: &Destination) -> Result<(), Failure> {
    #[cfg(feature = "fault-injection")]
    if ctx.fault.hang_setup_dest == Some(d.id.0) {
        fault::hang();
    }
    if !same_volume_dir(&d.root, d.volume.dev) {
        return Err(Failure::DestinationOffline);
    }
    let staging = &ctx.staging[d.id.index()];
    // Leftover `.part` files from an interrupted run of this same job are ours to discard.
    match std::fs::remove_dir_all(staging) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(classify_dest(&e, d)),
    }
    std::fs::create_dir_all(staging).map_err(|e| classify_dest(&e, d))?;
    for dir in
        std::iter::once(d.target.clone()).chain(ctx.job.dirs.iter().map(|r| r.to_path(&d.target)))
    {
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Failure::Io {
                    message: format!("{} exists and is not a folder", dir.display()),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&dir).map_err(|e| classify_dest(&e, d))?;
                ctx.tick(d.id);
            }
            Err(e) => return Err(classify_dest(&e, d)),
        }
    }
    Ok(())
}

/// After the workers: flush directory entries and remove this job's staging folder.
fn finish_destinations(ctx: &Ctx) {
    for d in &ctx.job.destinations {
        // A stalled destination is not touched again: any call could block forever.
        if ctx.stalled(d.id)
            || (ctx.dead(d.id).is_some() && !same_volume_dir(&d.root, d.volume.dev))
        {
            continue;
        }
        for dir in std::iter::once(d.target.clone())
            .chain(ctx.job.dirs.iter().map(|r| r.to_path(&d.target)))
        {
            let _ = platform::sync_dir(&dir);
        }
        let staging = &ctx.staging[d.id.index()];
        let _ = std::fs::remove_dir_all(staging);
        let _ = std::fs::remove_dir(d.root.join(STAGING_DIR));
        let _ = platform::sync_dir(&d.root);
    }
}

/// Runs `f` for each destination on its own detached thread under the stall watchdog and
/// returns its result, or `None` for a destination the watchdog gave up on (its thread is
/// abandoned, blocked in the kernel).
fn guarded<T: Send + 'static>(
    ctx: &Arc<Ctx>,
    dests: &[DestId],
    f: impl Fn(&Arc<Ctx>, DestId) -> T + Send + Sync + 'static,
) -> Vec<(DestId, Option<T>)> {
    let f = Arc::new(f);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut pending = Vec::new();
    let mut out: Vec<(DestId, Option<T>)> = Vec::new();
    for &d in dests {
        let (ctx2, f, tx) = (Arc::clone(ctx), Arc::clone(&f), tx.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("shortend-dest-{}", d.0))
            .spawn(move || {
                let guard = ctx2.busy(d);
                let r = f(&ctx2, d);
                drop(guard);
                let _ = tx.send((d, r));
            });
        match spawned {
            Ok(_) => pending.push(d),
            Err(_) => out.push((d, None)),
        }
    }
    drop(tx);
    while !pending.is_empty() {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((d, r)) => {
                pending.retain(|x| *x != d);
                out.push((d, Some(r)));
            }
            Err(RecvTimeoutError::Timeout) => {
                for d in pending.clone() {
                    if ctx.stalled(d) {
                        pending.retain(|x| *x != d);
                        out.push((d, None));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                out.extend(pending.drain(..).map(|d| (d, None)));
            }
        }
    }
    out
}

/// Resume, for one destination: which files are already there and verified. Every file at
/// its final path is re-read from the device and compared with the journaled source checksums
/// (a journal record alone is never trusted); anything unproven is removed so it is copied
/// again. Implements the skip rule in PLAN.md §4.6.
fn reconcile_dest(
    ctx: &Arc<Ctx>,
    d: DestId,
    sources: &[Option<Digests>],
    replay: &Replay,
) -> Vec<(usize, Digests)> {
    let hashes = &ctx.job.settings.hashes;
    let dest = ctx.dest(d);
    let mut done = Vec::new();
    for (idx, (entry, src)) in ctx.job.files.iter().zip(sources).enumerate() {
        if ctx.dead(d).is_some() || ctx.cancel.is_cancelled() {
            break;
        }
        #[cfg(feature = "fault-injection")]
        if idx == 0 && ctx.fault.hang_reverify_dest == Some(d.0) {
            fault::hang();
        }
        let final_path = entry.rel.to_path(&dest.target);
        match std::fs::symlink_metadata(&final_path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Ok(m) if m.is_file() && src.is_some() => {
                let s = src.as_ref().expect("checked");
                match hash_path(&final_path, hashes, &ctx.cancel, |n| {
                    ctx.verified_bytes[d.index()].fetch_add(n, Ordering::Relaxed);
                    ctx.tick(d);
                }) {
                    // The card's ASC MHL chain file, which this job's manifest extended.
                    Ok(_) if replay.mhl.contains(&d) && is_chain_file(entry) => {
                        done.push((idx, s.clone()));
                    }
                    Ok(v) if &v == s && final_matches(&final_path, entry, dest) => {
                        if replay.commits.get(&(entry.rel.clone(), d)) != Some(&v) {
                            ctx.record(&Record::Commit {
                                rel: entry.rel.clone(),
                                dest: d,
                                digests: v.clone(),
                            });
                        }
                        done.push((idx, v));
                    }
                    _ => remove_unproven(ctx, &final_path, dest),
                }
            }
            Ok(_) => remove_unproven(ctx, &final_path, dest),
            Err(e) => ctx.kill(d, classify_dest(&e, dest)),
        }
        ctx.tick(d);
    }
    done
}

fn is_chain_file(entry: &FileEntry) -> bool {
    entry.rel.as_str()
        == format!(
            "{}/{}",
            crate::report::mhl::v2::FOLDER,
            crate::report::mhl::v2::CHAIN_FILE
        )
}

/// Removes a file at a final path that this job cannot prove it verified. It can only be ours:
/// preflight required the target folder to be new or empty.
fn remove_unproven(ctx: &Ctx, path: &Path, dest: &Destination) {
    if let Err(e) = std::fs::remove_file(path) {
        ctx.kill(dest.id, classify_dest(&e, dest));
    }
}

fn final_matches(path: &Path, entry: &FileEntry, dest: &Destination) -> bool {
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !m.is_file() || m.len() != entry.size {
        return false;
    }
    let Ok(mtime) = m.modified().map(Timestamp::try_from) else {
        return false;
    };
    let Ok(mtime) = mtime else { return false };
    let diff = mtime.duration_since(entry.mtime).unsigned_abs();
    diff <= dest.volume.fs_type.mtime_granularity()
}

/// Copies one file to every destination that still needs it, retrying verification
/// failures up to `settings.retries` times.
fn process_file(ctx: &Arc<Ctx>, idx: usize, o: &mut FileOutcome, pool: &Pool) {
    let retries = ctx.job.settings.retries;
    let mut attempt: u8 = 0;
    loop {
        let mut pending = Vec::new();
        for slot in o
            .dests
            .iter_mut()
            .filter(|x| x.status == FileStatus::Pending)
        {
            match ctx.dead(slot.dest) {
                Some(f) => slot.status = FileStatus::Failed(f),
                None => pending.push(slot.dest),
            }
        }
        if pending.is_empty() || ctx.stopping() {
            return;
        }
        let can_retry = attempt < retries;
        let (src, results) = copy_once(ctx, idx, &pending, attempt, pool);

        let src_failure = match src {
            Some(Ok((digests, reread))) => {
                if let Some(prev) = &o.source
                    && prev != &digests
                {
                    // A retry read different bytes than the first attempt: the source is unstable.
                    let f = Failure::SourceReread {
                        first: prev.clone(),
                        second: digests,
                    };
                    for &d in &pending {
                        set_status(o, d, FileStatus::Failed(f.clone()), None);
                    }
                    return;
                }
                if o.source.is_none() {
                    ctx.record(&Record::Src {
                        rel: o.entry.rel.clone(),
                        digests: digests.clone(),
                    });
                }
                o.source = Some(digests);
                o.reread = reread;
                None
            }
            Some(Err(f)) => Some(f),
            None => None,
        };
        if matches!(src_failure, Some(Failure::SourceUnavailable)) {
            ctx.abort.store(true, Ordering::SeqCst);
        }

        for (d, r) in results {
            match r {
                Ok(v) => {
                    ctx.record(&Record::Commit {
                        rel: o.entry.rel.clone(),
                        dest: d,
                        digests: v.clone(),
                    });
                    set_status(o, d, FileStatus::Verified, Some(v));
                }
                // The writer stopped because the reader did; handled with the source failure below.
                Err(Failure::Cancelled) => {}
                Err(f) if f.is_fatal_for_destination() => {
                    ctx.kill(d, f.clone());
                    set_status(o, d, FileStatus::Failed(f), None);
                }
                Err(_) if can_retry => {} // stays Pending; retried below
                Err(f) => set_status(o, d, FileStatus::Failed(f), None),
            }
        }
        if let Some(f) = &src_failure {
            let retry = can_retry && matches!(f, Failure::SourceReread { .. } | Failure::Io { .. });
            if !retry && *f != Failure::Cancelled {
                for &d in &pending {
                    if o.dests
                        .iter()
                        .any(|x| x.dest == d && x.status == FileStatus::Pending)
                    {
                        set_status(o, d, FileStatus::Failed(f.clone()), None);
                    }
                }
            }
            if !retry {
                return;
            }
        }
        if ctx.stopping() {
            return;
        }
        attempt += 1;
    }
}

fn set_status(o: &mut FileOutcome, d: DestId, status: FileStatus, verified: Option<Digests>) {
    if let Some(slot) = o.dests.iter_mut().find(|x| x.dest == d) {
        slot.status = status;
        slot.verified = verified;
    }
}

enum Msg {
    Data(Arc<Chunk>),
    End(Digests),
    Abort,
}

/// A filled buffer shared by all writers; returns itself to the pool when the last one drops it.
struct Chunk {
    buf: Vec<u8>,
    len: usize,
    home: Sender<Vec<u8>>,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let _ = self.home.send(std::mem::take(&mut self.buf));
    }
}

/// Fixed set of `POOL_DEPTH` buffers per worker: memory stays bounded whatever the file size.
struct Pool {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    chunk: usize,
}

impl Pool {
    fn new(chunk: usize) -> Self {
        let pool = Self {
            tx: std::sync::mpsc::channel().0,
            rx: std::sync::mpsc::channel().1,
            chunk,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let pool = Self { tx, rx, ..pool };
        pool.replenish(POOL_DEPTH);
        pool
    }

    /// Adds `n` buffers. Used when a stalled writer is abandoned while holding some: those
    /// stay with the hung thread, and the bound grows by at most `QUEUE_DEPTH + 1` per stall.
    fn replenish(&self, n: usize) {
        for _ in 0..n {
            let _ = self.tx.send(Vec::new());
        }
    }

    fn get(&self, cancel: &CancelToken) -> Option<Vec<u8>> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(mut v) => {
                    if v.len() != self.chunk {
                        v.resize(self.chunk, 0);
                    }
                    return Some(v);
                }
                Err(RecvTimeoutError::Timeout) if !cancel.is_cancelled() => {}
                Err(_) => return None,
            }
        }
    }
}

type SourceResult = Result<(Digests, Option<Digests>), Failure>;
type DestResults = Vec<(DestId, Result<Digests, Failure>)>;

/// One attempt at copying file `idx` to `dests`. Returns the source result (`None` if the
/// source was never read because no destination could start) and one result per destination.
fn copy_once(
    ctx: &Arc<Ctx>,
    idx: usize,
    dests: &[DestId],
    attempt: u8,
    pool: &Pool,
) -> (Option<SourceResult>, DestResults) {
    let entry = &ctx.job.files[idx];
    let src_path = entry.rel.to_path(&ctx.job.source.root);
    let mut results = Vec::new();

    if !same_volume_dir(&ctx.job.source.root, ctx.job.source.volume.dev) {
        return (Some(Err(Failure::SourceUnavailable)), results);
    }
    let src = match platform::open_source(&src_path) {
        Ok(f) => f,
        Err(e) => return (Some(Err(classify_source(&e, ctx))), results),
    };

    let mut setups = Vec::new();
    for &d in dests {
        let dest = ctx.dest(d);
        if !same_volume_dir(&dest.root, dest.volume.dev) {
            results.push((d, Err(Failure::DestinationOffline)));
            continue;
        }
        let part = ctx.staging[d.index()].join(format!("{idx}-{attempt}.part"));
        let _ = std::fs::remove_file(&part);
        match platform::create_staged(&part) {
            Ok(file) => setups.push((d, part, file)),
            Err(e) => results.push((d, Err(classify_dest(&e, dest)))),
        }
    }
    if setups.is_empty() {
        return (None, results);
    }

    // Writers are detached threads reporting through a channel: a writer stuck in the kernel
    // on a hung drive is abandoned once the watchdog marks its destination stalled.
    let (res_tx, res_rx) = std::sync::mpsc::channel::<(DestId, Result<Digests, Failure>)>();
    let mut senders = Vec::new();
    let mut outstanding = Vec::new();
    for (d, part, file) in setups {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(QUEUE_DEPTH);
        let final_path = entry.rel.to_path(&ctx.dest(d).target);
        let (ctx2, res_tx) = (Arc::clone(ctx), res_tx.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("shortend-writer-{}", d.0))
            .spawn(move || {
                let guard = ctx2.busy(d);
                let r = writer(&ctx2, d, idx, part, file, final_path, rx);
                drop(guard);
                let _ = res_tx.send((d, r));
            });
        match spawned {
            Ok(_) => {
                senders.push((d, tx));
                outstanding.push(d);
            }
            Err(e) => results.push((
                d,
                Err(Failure::Io {
                    message: format!("cannot start writer thread: {e}"),
                }),
            )),
        }
    }
    drop(res_tx);

    let mut result: SourceResult =
        read_and_fan_out(ctx, entry, src, &mut senders, pool).map(|digests| (digests, None));
    if ctx.job.settings.source_reread
        && let Ok((first, _)) = &result
    {
        result = reread_source(ctx, &src_path, first);
    }
    let last = match &result {
        Ok((digests, _)) => Some(digests.clone()),
        Err(_) => None,
    };
    for (d, tx) in &senders {
        let msg = match &last {
            Some(digests) => Msg::End(digests.clone()),
            None => Msg::Abort,
        };
        send_unless_dead(ctx, *d, tx, msg);
    }
    drop(senders);

    while !outstanding.is_empty() {
        match res_rx.recv_timeout(Duration::from_millis(100)) {
            Ok((d, r)) => {
                outstanding.retain(|x| *x != d);
                results.push((d, r));
            }
            Err(RecvTimeoutError::Timeout) => {
                for d in outstanding.clone() {
                    if let Some(f @ Failure::DestinationStalled { .. }) = ctx.dead(d) {
                        outstanding.retain(|x| *x != d);
                        results.push((d, Err(f)));
                        pool.replenish(QUEUE_DEPTH + 1);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                for d in outstanding.drain(..) {
                    results.push((
                        d,
                        Err(Failure::Io {
                            message: "writer thread ended without a result".into(),
                        }),
                    ));
                }
            }
        }
    }
    (Some(result), results)
}

/// Sends to a writer without ever blocking on a destination the watchdog has given up on.
/// Returns false if the writer is gone or its destination is dead.
fn send_unless_dead(ctx: &Ctx, d: DestId, tx: &SyncSender<Msg>, msg: Msg) -> bool {
    let mut msg = msg;
    let mut backoff = Duration::from_micros(50);
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(m)) => {
                if ctx.stalled(d) {
                    return false;
                }
                msg = m;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(2));
            }
        }
    }
}

/// Reads the source once, hashing in-stream and handing each chunk to every live writer.
fn read_and_fan_out(
    ctx: &Ctx,
    entry: &FileEntry,
    mut src: File,
    senders: &mut Vec<(DestId, SyncSender<Msg>)>,
    pool: &Pool,
) -> Result<Digests, Failure> {
    let mut hasher = MultiHasher::new(&ctx.job.settings.hashes);
    let mut total: u64 = 0;
    loop {
        if ctx.cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if senders.is_empty() {
            // Every writer failed; their own errors describe what happened.
            return Err(Failure::Cancelled);
        }
        let Some(buf) = pool.get(&ctx.cancel) else {
            return Err(Failure::Cancelled);
        };
        // Wrapped immediately so the buffer goes back to the pool on every path.
        let mut chunk = Chunk {
            buf,
            len: 0,
            home: pool.tx.clone(),
        };
        while chunk.len < chunk.buf.len() {
            match src.read(&mut chunk.buf[chunk.len..]) {
                Ok(0) => break,
                Ok(n) => chunk.len += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(classify_source(&e, ctx)),
            }
        }
        if chunk.len == 0 {
            break;
        }
        hasher.update(&chunk.buf[..chunk.len]);
        total += chunk.len as u64;
        ctx.src_bytes.fetch_add(chunk.len as u64, Ordering::Relaxed);
        let chunk = Arc::new(chunk);
        senders.retain(|(d, tx)| send_unless_dead(ctx, *d, tx, Msg::Data(Arc::clone(&chunk))));
    }
    let meta = src.metadata().map_err(|e| classify_source(&e, ctx))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| Timestamp::try_from(t).ok());
    if total != entry.size || meta.len() != entry.size || mtime != Some(entry.mtime) {
        return Err(Failure::SourceChanged);
    }
    Ok(hasher.finish())
}

/// Optional second pass: reads the source again from the device and compares checksums.
fn reread_source(ctx: &Ctx, path: &Path, first: &Digests) -> SourceResult {
    match hash_path(path, &ctx.job.settings.hashes, &ctx.cancel, |_| {}) {
        Ok(second) if &second == first => Ok((first.clone(), Some(second))),
        Ok(second) => Err(Failure::SourceReread {
            first: first.clone(),
            second,
        }),
        Err(HashErr::Cancelled) => Err(Failure::Cancelled),
        Err(HashErr::Io(e)) => Err(classify_source(&e, ctx)),
    }
}

/// Writes one destination's staged copy, then verifies it by reading it back from the device,
/// then renames it into place without overwriting anything.
fn writer(
    ctx: &Ctx,
    d: DestId,
    idx: usize,
    part: PathBuf,
    mut file: File,
    final_path: PathBuf,
    rx: Receiver<Msg>,
) -> Result<Digests, Failure> {
    let entry = &ctx.job.files[idx];
    let dest = ctx.dest(d);
    let fail = |e: io::Error, what: &str| {
        let f = match classify_dest(&e, dest) {
            Failure::Io { message } => Failure::Io {
                message: format!("{what}: {message}"),
            },
            other => other,
        };
        let _ = std::fs::remove_file(&part);
        f
    };
    let mut since_sync = 0u64;
    #[cfg(feature = "fault-injection")]
    let mut first_chunk = true;
    let expected = loop {
        match rx.recv() {
            Ok(Msg::Data(chunk)) => {
                let data = &chunk.buf[..chunk.len];
                #[cfg(feature = "fault-injection")]
                let corrupted;
                #[cfg(feature = "fault-injection")]
                let data = {
                    let first = std::mem::take(&mut first_chunk);
                    if first && ctx.fault.hang_dest == Some(d.0) {
                        fault::hang();
                    }
                    if first && ctx.fault.corrupt_dest == Some(d.0) {
                        corrupted = fault::flip_first(data);
                        &corrupted[..]
                    } else {
                        data
                    }
                };
                if let Err(e) = file.write_all(data) {
                    drop(file);
                    return Err(fail(e, "write"));
                }
                ctx.dest_bytes[d.index()].fetch_add(data.len() as u64, Ordering::Relaxed);
                ctx.tick(d);
                since_sync += data.len() as u64;
                if since_sync >= SYNC_INTERVAL {
                    if let Err(e) = file.sync_data() {
                        drop(file);
                        return Err(fail(e, "flush"));
                    }
                    ctx.tick(d);
                    platform::drop_cache(&file);
                    since_sync = 0;
                }
            }
            Ok(Msg::End(digests)) => break digests,
            Ok(Msg::Abort) | Err(_) => {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(Failure::Cancelled);
            }
        }
    };
    drop(rx);

    if let Err(e) = file.set_modified(SystemTime::from(entry.mtime)) {
        drop(file);
        return Err(fail(e, "set modification time"));
    }
    if let Err(e) = platform::sync_file(&file) {
        drop(file);
        return Err(fail(e, "flush"));
    }
    ctx.tick(d);
    platform::drop_cache(&file);
    drop(file);

    let actual = match hash_path(&part, &ctx.job.settings.hashes, &ctx.cancel, |n| {
        ctx.verified_bytes[d.index()].fetch_add(n, Ordering::Relaxed);
        ctx.tick(d);
    }) {
        Ok(v) => v,
        Err(HashErr::Cancelled) => {
            let _ = std::fs::remove_file(&part);
            return Err(Failure::Cancelled);
        }
        Err(HashErr::Io(e)) => return Err(fail(e, "verify read")),
    };
    if actual != expected {
        let _ = std::fs::remove_file(&part);
        return Err(Failure::HashMismatch { expected, actual });
    }

    if ctx.dead(d).is_some() || !same_volume_dir(&dest.root, dest.volume.dev) {
        let _ = std::fs::remove_file(&part);
        return Err(ctx.dead(d).unwrap_or(Failure::DestinationOffline));
    }
    match platform::rename_noreplace(&part, &final_path) {
        Ok(()) => Ok(actual),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&part);
            Err(Failure::Io {
                message: format!("{} already exists; not overwritten", final_path.display()),
            })
        }
        Err(e) => Err(fail(e, "rename into place")),
    }
}

enum HashErr {
    Io(io::Error),
    Cancelled,
}

/// Hashes a file read from the device (page cache bypassed or dropped first). `on_read` is
/// called with the size of every block read.
fn hash_path(
    path: &Path,
    list: &HashList,
    cancel: &CancelToken,
    on_read: impl Fn(u64),
) -> Result<Digests, HashErr> {
    let size = std::fs::symlink_metadata(path).map_err(HashErr::Io)?.len();
    let mut r = platform::open_uncached(path).map_err(HashErr::Io)?;
    let cap = usize::try_from(size.saturating_add(1))
        .unwrap_or(VERIFY_BUF_MAX)
        .clamp(64 << 10, VERIFY_BUF_MAX);
    let mut buf = vec![0u8; cap];
    let mut h = MultiHasher::new(list);
    loop {
        if cancel.is_cancelled() {
            return Err(HashErr::Cancelled);
        }
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(HashErr::Io(e)),
        };
        h.update(&buf[..n]);
        on_read(n as u64);
    }
    Ok(h.finish())
}

/// Maps an OS error to the failures the engine acts on, independent of side.
fn io_failure(e: &io::Error) -> Option<Failure> {
    use io::ErrorKind as K;
    match e.kind() {
        K::StorageFull | K::QuotaExceeded => return Some(Failure::DiskFull),
        K::PermissionDenied | K::ReadOnlyFilesystem => return Some(Failure::PermissionDenied),
        K::StaleNetworkFileHandle | K::NotConnected => return Some(Failure::DestinationOffline),
        _ => {}
    }
    // EIO, ENXIO, ENODEV: the device is failing or gone.
    #[cfg(unix)]
    let gone = matches!(e.raw_os_error(), Some(5 | 6 | 19));
    // ERROR_NOT_READY, ERROR_DEV_NOT_EXIST, ERROR_DEVICE_HARDWARE_ERROR, ERROR_DEVICE_NOT_CONNECTED
    #[cfg(windows)]
    let gone = matches!(e.raw_os_error(), Some(21 | 55 | 483 | 1167));
    gone.then_some(Failure::DestinationOffline)
}

pub(crate) fn classify_dest(e: &io::Error, dest: &Destination) -> Failure {
    if !same_volume_dir(&dest.root, dest.volume.dev) {
        return Failure::DestinationOffline;
    }
    io_failure(e).unwrap_or_else(|| Failure::Io {
        message: e.to_string(),
    })
}

fn classify_source(e: &io::Error, ctx: &Ctx) -> Failure {
    let src = &ctx.job.source;
    if !same_volume_dir(&src.root, src.volume.dev) {
        return Failure::SourceUnavailable;
    }
    match io_failure(e) {
        Some(Failure::DestinationOffline) => Failure::SourceUnavailable,
        _ if e.kind() == io::ErrorKind::NotFound => Failure::SourceChanged,
        _ => Failure::Io {
            message: format!("reading source: {e}"),
        },
    }
}

/// Test-only fault injection, compiled in with the `fault-injection` feature.
/// `SHORTEND_FAULT` holds comma-separated faults:
/// - `corrupt-dest=N`: flip the first byte written to destination N for every file, so
///   read-back verification must catch it;
/// - `hang-dest=N`: the writer for destination N blocks forever on its first chunk, like a
///   drive that stopped answering, so the stall watchdog must catch it;
/// - `hang-setup-dest=N` / `hang-reverify-dest=N`: the same while destination N is being
///   prepared at job start, or re-verified on resume.
#[cfg(feature = "fault-injection")]
mod fault {
    pub struct Fault {
        pub corrupt_dest: Option<u16>,
        pub hang_dest: Option<u16>,
        pub hang_setup_dest: Option<u16>,
        pub hang_reverify_dest: Option<u16>,
    }

    impl Fault {
        pub fn from_env() -> Self {
            let spec = std::env::var("SHORTEND_FAULT").unwrap_or_default();
            let get = |key: &str| {
                spec.split(',')
                    .find_map(|f| f.trim().strip_prefix(key).and_then(|n| n.parse().ok()))
            };
            Self {
                corrupt_dest: get("corrupt-dest="),
                hang_dest: get("hang-dest="),
                hang_setup_dest: get("hang-setup-dest="),
                hang_reverify_dest: get("hang-reverify-dest="),
            }
        }
    }

    pub fn flip_first(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        if let Some(b) = v.first_mut() {
            *b ^= 0xff;
        }
        v
    }

    pub fn hang() -> ! {
        loop {
            std::thread::park();
        }
    }
}
