//! Core data model shared by the engine, the reports and the front ends.

use std::fmt;
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::fs::{RelPath, VolumeInfo};
use crate::hash::{Digests, HashList};
use crate::report::mhl::MhlVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(uuid::Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_hyphenated().fmt(f)
    }
}

impl FromStr for JobId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self)
    }
}

/// Index into `Job::destinations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DestId(pub u16);

impl DestId {
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub rel: RelPath,
    pub size: u64,
    /// Preserved on every destination copy.
    pub mtime: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Canonical path; the root itself is never a symlink.
    pub root: PathBuf,
    /// Card name.
    pub label: String,
    pub volume: VolumeInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub id: DestId,
    pub label: String,
    /// Canonical, pre-existing directory chosen by the user; never a symlink.
    pub root: PathBuf,
    /// `root` joined with the job's folder; created by the job.
    pub target: PathBuf,
    /// Captured at preflight and compared before every file is written.
    pub volume: VolumeInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportOptions {
    pub csv: bool,
    pub txt: bool,
    /// Manifests written into every destination's target folder (any of ASC MHL v2 and MHL
    /// v1.1; empty writes none). A single value is accepted when reading.
    #[serde(default = "default_mhl", deserialize_with = "one_or_many")]
    pub mhl: Vec<MhlVersion>,
    /// PDF report. `None` writes none.
    #[serde(default = "default_pdf")]
    pub pdf: Option<PdfOptions>,
}

fn default_mhl() -> Vec<MhlVersion> {
    vec![MhlVersion::V2]
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<MhlVersion>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(MhlVersion),
        Many(Vec<MhlVersion>),
    }
    let mut v = match OneOrMany::deserialize(d)? {
        OneOrMany::One(x) => vec![x],
        OneOrMany::Many(v) => v,
    };
    let mut seen = Vec::new();
    v.retain(|x| {
        let new = !seen.contains(x);
        seen.push(*x);
        new
    });
    Ok(v)
}

fn default_pdf() -> Option<PdfOptions> {
    Some(PdfOptions::default())
}

impl Default for ReportOptions {
    fn default() -> Self {
        Self {
            csv: true,
            txt: true,
            mhl: default_mhl(),
            pdf: default_pdf(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfOptions {
    /// PNG or JPEG shown at the top of the first page.
    pub logo: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSettings {
    /// Default `[xxh64]`. More than one algorithm is computed in the same read pass.
    pub hashes: HashList,
    /// Re-read the source after copying (cache bypassed) and compare checksums.
    pub source_reread: bool,
    /// Files copied concurrently.
    pub parallel_files: NonZeroU8,
    /// Read/write chunk size in bytes.
    pub chunk_size: u32,
    /// Extra attempts for a file whose verification failed.
    pub retries: u8,
    /// A destination with work in progress but no bytes written or verified for this many
    /// seconds is marked stalled and dropped from the job. 0 disables the watchdog.
    #[serde(default = "default_stall_timeout")]
    pub stall_timeout_secs: u32,
    pub reports: ReportOptions,
}

fn default_stall_timeout() -> u32 {
    60
}

impl Default for JobSettings {
    fn default() -> Self {
        Self {
            hashes: HashList::default(),
            source_reread: false,
            parallel_files: NonZeroU8::new(2).expect("nonzero"),
            chunk_size: 8 << 20,
            retries: 1,
            stall_timeout_secs: default_stall_timeout(),
            reports: ReportOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionInfo {
    pub show: Option<String>,
    pub episode: Option<String>,
    #[serde(rename = "day", alias = "shoot_day")]
    pub shoot_day: Option<String>,
    pub camroll: Option<String>,
    pub operator: Option<String>,
    pub company: Option<String>,
    pub notes: Option<String>,
}

impl ProductionInfo {
    /// Non-empty fields as (label, value) pairs, in display order.
    pub fn fields(&self) -> Vec<(&'static str, &str)> {
        [
            ("Show", &self.show),
            ("Episode", &self.episode),
            ("Shoot day", &self.shoot_day),
            ("Camera roll", &self.camroll),
            ("Operator", &self.operator),
            ("Company", &self.company),
            ("Notes", &self.notes),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.as_deref().filter(|s| !s.is_empty()).map(|s| (k, s)))
        .collect()
    }
}

/// What a front end hands to [`crate::plan`].
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub source: PathBuf,
    /// Card name; defaults to the source folder's name.
    pub card_label: Option<String>,
    /// (label, root) pairs.
    pub destinations: Vec<(String, PathBuf)>,
    /// Folder template (see [`crate::naming`]) created under each destination root; defaults
    /// to the card name.
    pub folder: Option<String>,
    pub settings: JobSettings,
    pub production: ProductionInfo,
}

/// A fully resolved job. The header is stored as `job.toml` and the scan snapshot as
/// `snapshot.json` in the job's state directory, which is all `resume` needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub created: Timestamp,
    /// The folder created under every destination root. With `{seq}`, the number is
    /// provisional until [`crate::run`] claims it on every destination.
    pub folder: RelPath,
    /// The expanded template, kept while `{seq}` is still to be claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming: Option<crate::naming::Expanded>,
    /// The sequence number in `folder`, if the template has `{seq}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub source: Source,
    pub destinations: Vec<Destination>,
    pub settings: JobSettings,
    pub production: ProductionInfo,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub files: Vec<FileEntry>,
    #[serde(skip)]
    pub dirs: Vec<RelPath>,
}

impl Job {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    HashMismatch {
        expected: Digests,
        actual: Digests,
    },
    /// Size or modification time differs from the scan snapshot.
    SourceChanged,
    SourceReread {
        first: Digests,
        second: Digests,
    },
    /// The file differs from what the card's own ASC MHL history recorded for it. The copy is
    /// faithful to the card, but the card no longer matches its manifest.
    SourceManifestMismatch {
        recorded: String,
        actual: Digests,
    },
    /// The source volume disappeared or failed; the job stops and can be resumed.
    SourceUnavailable,
    /// The destination volume disappeared, was replaced, or failed with an I/O error.
    DestinationOffline,
    /// The destination made no progress for the stall timeout (a hung drive or mount).
    DestinationStalled {
        seconds: u32,
    },
    DiskFull,
    PermissionDenied,
    Io {
        message: String,
    },
    Cancelled,
}

impl Failure {
    /// Failures that end all further work on a destination.
    pub fn is_fatal_for_destination(&self) -> bool {
        matches!(
            self,
            Self::DestinationOffline
                | Self::DestinationStalled { .. }
                | Self::DiskFull
                | Self::PermissionDenied
        )
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HashMismatch { expected, actual } => {
                write!(
                    f,
                    "checksum mismatch: source {expected}, destination {actual}"
                )
            }
            Self::SourceChanged => f.write_str("source file changed during the job"),
            Self::SourceReread { first, second } => {
                write!(f, "source re-read differs: first {first}, second {second}")
            }
            Self::SourceManifestMismatch { recorded, actual } => write!(
                f,
                "card does not match its own ASC MHL (recorded {recorded}, card has {actual})"
            ),
            Self::SourceUnavailable => f.write_str("source unavailable (removed or failing)"),
            Self::DestinationStalled { seconds } => {
                write!(f, "destination stalled (no progress for {seconds} s)")
            }
            Self::DestinationOffline => {
                f.write_str("destination offline (removed, replaced or failing)")
            }
            Self::DiskFull => f.write_str("destination full"),
            Self::PermissionDenied => f.write_str("permission denied / read-only destination"),
            Self::Io { message } => write!(f, "I/O error: {message}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileStatus {
    /// Not attempted (job cancelled or aborted first).
    Pending,
    Verified,
    /// Copied in an earlier run of this job and re-verified against the source checksums
    /// when resuming.
    AlreadyVerified,
    Failed(Failure),
}

impl FileStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Verified | Self::AlreadyVerified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestOutcome {
    pub dest: DestId,
    pub status: FileStatus,
    pub verified: Option<Digests>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOutcome {
    pub entry: FileEntry,
    pub source: Option<Digests>,
    pub reread: Option<Digests>,
    pub dests: Vec<DestOutcome>,
}

impl FileOutcome {
    pub fn all_ok(&self) -> bool {
        self.dests.iter().all(|d| d.status.is_ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Every file verified on every destination.
    Completed,
    /// Finished, but at least one file failed on at least one destination.
    CompletedWithFailures,
    Cancelled,
    /// Stopped early because the source became unavailable. Resumable.
    Aborted,
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Completed => "completed",
            Self::CompletedWithFailures => "completed with failures",
            Self::Cancelled => "cancelled",
            Self::Aborted => "aborted (source unavailable)",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DestStats {
    pub bytes_written: u64,
    pub bytes_verified: u64,
    /// Why the destination was dropped from the job, if it was.
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone)]
pub struct JobReport {
    pub job: Job,
    pub state: JobState,
    pub started: Timestamp,
    pub finished: Timestamp,
    /// One entry per file of `job.files`, same order.
    pub outcomes: Vec<FileOutcome>,
    /// One entry per destination, same order as `job.destinations`.
    pub dest_stats: Vec<DestStats>,
    pub warnings: Vec<String>,
    /// Problems between the card and its own ASC MHL history (e.g. files the history lists
    /// that are missing from the card). Any entry fails the job.
    pub manifest_issues: Vec<String>,
    /// Files verified on the destinations but left out of an MHL v1.1 manifest because that
    /// format cannot list zero-byte files. Non-empty also fails the job (see `manifest_issues`).
    pub manifest_omissions: Vec<String>,
    /// MHL files written (one per destination and manifest version).
    pub manifests: Vec<PathBuf>,
    pub report_files: Vec<PathBuf>,
}

impl JobReport {
    pub fn failed_files(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.all_ok()).count()
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.finished.duration_since(self.started).unsigned_abs()
    }
}

/// Progress and lifecycle notifications. Progress is sampled (a few times per second) and
/// carries cumulative counters, so a front end that misses some loses nothing.
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        job: JobId,
        files: usize,
        bytes: u64,
        already_verified: usize,
    },
    Progress {
        source_bytes: u64,
        dest_bytes: Vec<u64>,
        total_bytes: u64,
    },
    FileFinished(Box<FileOutcome>),
    DestinationFailed {
        dest: DestId,
        failure: Failure,
    },
    Warning(String),
    ReportWritten(PathBuf),
}

/// Shared cancellation flag, checked between chunks.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
