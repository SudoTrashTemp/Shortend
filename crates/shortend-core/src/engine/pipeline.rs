//! The copy pipeline: one source read hashed in-stream, fanned out to every destination,
//! staged, read back from the device, and atomically committed. See PLAN.md §4.3–4.4.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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

struct Ctx<'a> {
    job: &'a Job,
    events: &'a SyncSender<Event>,
    cancel: &'a CancelToken,
    journal: &'a Journal,
    staging: Vec<PathBuf>,
    dead: Vec<Mutex<Option<Failure>>>,
    abort: AtomicBool,
    src_bytes: AtomicU64,
    dest_bytes: Vec<AtomicU64>,
    verified_bytes: Vec<AtomicU64>,
    warnings: Mutex<Vec<String>>,
    journal_failed: AtomicBool,
    #[cfg(feature = "fault-injection")]
    fault: fault::Fault,
}

impl Ctx<'_> {
    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
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
}

/// Runs (or continues) a job. `replay` is `Some` when resuming.
pub(crate) fn execute(
    job: &Job,
    journal: &Journal,
    replay: Option<&Replay>,
    events: &SyncSender<Event>,
    cancel: &CancelToken,
) -> Outcome {
    let n_dest = job.destinations.len();
    let ctx = Ctx {
        job,
        events,
        cancel,
        journal,
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
        warnings: Mutex::new(job.warnings.clone()),
        journal_failed: AtomicBool::new(false),
        #[cfg(feature = "fault-injection")]
        fault: fault::Fault::from_env(),
    };

    for d in &job.destinations {
        if let Err(f) = prepare_destination(&ctx, d) {
            ctx.kill(d.id, f);
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
        for (o, entry) in outcomes.iter().zip(&job.files) {
            let mut o = o.lock().unwrap_or_else(|p| p.into_inner());
            if reconcile(&ctx, entry, &mut o, replay) {
                already += 1;
            }
        }
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
    let done = AtomicBool::new(false);
    let workers = usize::from(job.settings.parallel_files.get()).min(work.len().max(1));
    std::thread::scope(|s| {
        s.spawn(|| progress_ticker(&ctx, &done));
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
        done.store(true, Ordering::SeqCst);
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
        })
        .collect();
    Outcome {
        state,
        outcomes,
        dest_stats,
        warnings: ctx.warnings.into_inner().unwrap_or_else(|p| p.into_inner()),
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
        match ctx.events.try_send(ev) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => return,
        }
        if finished {
            return;
        }
        std::thread::sleep(PROGRESS_EVERY);
    }
}

/// Revalidates a destination and creates its (empty) staging folder and the target tree.
fn prepare_destination(ctx: &Ctx, d: &Destination) -> Result<(), Failure> {
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
            }
            Err(e) => return Err(classify_dest(&e, d)),
        }
    }
    Ok(())
}

/// After the workers: flush directory entries and remove this job's staging folder.
fn finish_destinations(ctx: &Ctx) {
    for d in &ctx.job.destinations {
        if ctx.dead(d.id).is_some() && !same_volume_dir(&d.root, d.volume.dev) {
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

/// Resume: decides which (file, destination) pairs are already done. Returns true when the
/// file needs no work at all. Implements the skip rule in PLAN.md §4.6.
fn reconcile(ctx: &Ctx, entry: &FileEntry, o: &mut FileOutcome, replay: &Replay) -> bool {
    let hashes = &ctx.job.settings.hashes;
    let src = replay
        .src
        .get(&entry.rel)
        .filter(|d| d.same_algorithms(hashes))
        .cloned();
    o.source = src.clone();
    for d in &mut o.dests {
        let dest = ctx.dest(d.dest);
        if ctx.dead(d.dest).is_some() {
            continue;
        }
        let final_path = entry.rel.to_path(&dest.target);
        let committed = replay.commits.get(&(entry.rel.clone(), d.dest));
        match std::fs::symlink_metadata(&final_path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Ok(m) if m.is_file() && src.is_some() => {
                // A journal record is never trusted on its own: the file could have been
                // damaged after it was verified. Re-read it from the device and compare with
                // the journaled source checksums; anything that doesn't match is re-copied.
                let s = src.as_ref().expect("checked");
                match hash_path(
                    &final_path,
                    hashes,
                    ctx.cancel,
                    Some(&ctx.verified_bytes[d.dest.index()]),
                ) {
                    Ok(v) if &v == s && final_matches(&final_path, entry, dest) => {
                        if committed != Some(&v) {
                            ctx.record(&Record::Commit {
                                rel: entry.rel.clone(),
                                dest: d.dest,
                                digests: v.clone(),
                            });
                        }
                        d.status = FileStatus::AlreadyVerified;
                        d.verified = Some(v);
                    }
                    _ => remove_unproven(ctx, &final_path, dest),
                }
            }
            Ok(_) => remove_unproven(ctx, &final_path, dest),
            Err(e) => ctx.kill(d.dest, classify_dest(&e, dest)),
        }
    }
    o.dests
        .iter()
        .all(|d| d.status == FileStatus::AlreadyVerified)
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
fn process_file(ctx: &Ctx, idx: usize, o: &mut FileOutcome, pool: &Pool) {
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
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..POOL_DEPTH {
            tx.send(Vec::new()).expect("receiver alive");
        }
        Self { tx, rx, chunk }
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
    ctx: &Ctx,
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

    let src_result = std::thread::scope(|s| {
        let mut senders = Vec::new();
        let mut handles = Vec::new();
        for (d, part, file) in setups {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(QUEUE_DEPTH);
            let final_path = entry.rel.to_path(&ctx.dest(d).target);
            senders.push(tx);
            handles.push((
                d,
                s.spawn(move || writer(ctx, d, entry, part, file, final_path, rx)),
            ));
        }
        let mut result: SourceResult =
            read_and_fan_out(ctx, entry, src, &mut senders, pool).map(|digests| (digests, None));
        if ctx.job.settings.source_reread
            && let Ok((first, _)) = &result
        {
            result = reread_source(ctx, &src_path, first);
        }
        for tx in &senders {
            let msg = match &result {
                Ok((digests, _)) => Msg::End(digests.clone()),
                Err(_) => Msg::Abort,
            };
            let _ = tx.send(msg);
        }
        drop(senders);
        for (d, h) in handles {
            let r = h.join().unwrap_or_else(|_| {
                Err(Failure::Io {
                    message: "writer thread panicked".into(),
                })
            });
            results.push((d, r));
        }
        result
    });
    (Some(src_result), results)
}

/// Reads the source once, hashing in-stream and handing each chunk to every live writer.
fn read_and_fan_out(
    ctx: &Ctx,
    entry: &FileEntry,
    mut src: File,
    senders: &mut Vec<SyncSender<Msg>>,
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
        let Some(buf) = pool.get(ctx.cancel) else {
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
        senders.retain(|tx| tx.send(Msg::Data(Arc::clone(&chunk))).is_ok());
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
    match hash_path(path, &ctx.job.settings.hashes, ctx.cancel, None) {
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
    entry: &FileEntry,
    part: PathBuf,
    mut file: File,
    final_path: PathBuf,
    rx: Receiver<Msg>,
) -> Result<Digests, Failure> {
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
                let data =
                    if std::mem::take(&mut first_chunk) && ctx.fault.corrupt_dest == Some(d.0) {
                        corrupted = fault::flip_first(data);
                        &corrupted[..]
                    } else {
                        data
                    };
                if let Err(e) = file.write_all(data) {
                    drop(file);
                    return Err(fail(e, "write"));
                }
                ctx.dest_bytes[d.index()].fetch_add(data.len() as u64, Ordering::Relaxed);
                since_sync += data.len() as u64;
                if since_sync >= SYNC_INTERVAL {
                    if let Err(e) = file.sync_data() {
                        drop(file);
                        return Err(fail(e, "flush"));
                    }
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
    platform::drop_cache(&file);
    drop(file);

    let actual = match hash_path(
        &part,
        &ctx.job.settings.hashes,
        ctx.cancel,
        Some(&ctx.verified_bytes[d.index()]),
    ) {
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

    if !same_volume_dir(&dest.root, dest.volume.dev) {
        let _ = std::fs::remove_file(&part);
        return Err(Failure::DestinationOffline);
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

/// Hashes a file read from the device (page cache bypassed or dropped first).
fn hash_path(
    path: &Path,
    list: &HashList,
    cancel: &CancelToken,
    counter: Option<&AtomicU64>,
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
        if let Some(c) = counter {
            c.fetch_add(n as u64, Ordering::Relaxed);
        }
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
/// `SHORTEND_FAULT=corrupt-dest=N` flips the first byte written to destination N for every
/// file, so read-back verification must catch it.
#[cfg(feature = "fault-injection")]
mod fault {
    pub struct Fault {
        pub corrupt_dest: Option<u16>,
    }

    impl Fault {
        pub fn from_env() -> Self {
            let corrupt_dest = std::env::var("SHORTEND_FAULT")
                .ok()
                .and_then(|v| v.strip_prefix("corrupt-dest=").and_then(|n| n.parse().ok()));
            Self { corrupt_dest }
        }
    }

    pub fn flip_first(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        if let Some(b) = v.first_mut() {
            *b ^= 0xff;
        }
        v
    }
}
