# ShortEnd — Design Plan

Status: **APPROVED** (2026-10-10, with the decisions in §11) · M1–M4 implemented (see §9 checklists)

ShortEnd is a Linux-first, verified media offloader for DIT work: one source (a camera card or
folder) copied to N destinations, every copy verified by read-back against a single in-stream
source hash, with CSV/TXT/PDF reports and ASC MHL manifests. The core engine is a Rust library
driven by a CLI; the GUI is a thin egui shell over the same library.

---

## 1. Guiding decisions

| Decision | Choice | Why |
|---|---|---|
| Concurrency | Plain `std::thread` + bounded `std::sync::mpsc::sync_channel` | All work is blocking file I/O; an async runtime adds weight and buys nothing here. |
| Async runtime | None | See above. |
| Platform syscalls | `rustix` (Linux/macOS), `windows-sys` (Windows) | Safe, thin wrappers for `renameat2`, `fadvise`, `statvfs`, `O_NOATIME` etc. without hand-written `libc` unsafe. |
| Checksums | `xxhash-rust`, `md-5`, `sha1`, `sha2` | `xxhash-rust` as requested; RustCrypto crates are the standard pure-Rust digests. C4 = SHA-512 + base58 (≈40 lines of our own code, no extra crate). |
| XML (MHL) | `quick-xml` | Fast streaming writer/reader; manifests for 100k-file cards never need a DOM. |
| PDF | `printpdf` | Pure Rust, embeds fonts and images, no system libraries — keeps the static/AppImage builds simple. We do our own (simple) table layout. |
| Dates/times | `jiff` | Correct local time zones on Linux in multithreaded programs (the `time` crate refuses local offsets there); good formatting for `{date}` tokens. |
| Presets / job specs | `serde` + `toml` | Human-readable, diff-able, shareable, as requested. |
| Journal | `serde_json` (JSON Lines) | Append-only, one fsync'd record per line, trivially recoverable after a crash. |
| CSV | `csv` | Correct quoting of arbitrary filenames is easy to get wrong by hand. |
| Directory walk | `walkdir` | Small, well-tested, gives us `file_type()` without following symlinks. |
| IDs | `uuid` (v7) | Time-ordered, so job listings sort naturally. |
| Errors | `thiserror` in core, `anyhow` in CLI/GUI | As requested. |
| CLI | `clap` (derive) | As requested. |
| GUI | `eframe`/`egui` with the `glow` backend, `rfd` for folder pickers | Immediate mode fits a progress-heavy dashboard; iced's Elm architecture adds ceremony without benefit here, so egui is accepted. `glow` (OpenGL) gives a smaller binary and fewer runtime deps than `wgpu`, which matters for the AppImage. `rfd` uses the xdg-desktop-portal on Linux. |
| Images | `image` | Logo decoding (M2) and thumbnail downscale (M5). |
| Video metadata/thumbnails | `ffprobe`/`ffmpeg` CLI via `std::process::Command` | As requested; absent ffmpeg degrades gracefully. |
| MHL schema validation | `xmllint --schema` + reference `ascmhl` CLI, **in tests/CI only** | There is no mature pure-Rust XSD validator. Runtime correctness comes from a typed writer; CI proves it against the official XSDs and the reference tool. |

Deliberately **not** included: async runtimes, plugin systems, a database, logging frameworks
(the engine's typed event stream plus the TXT report cover it), progress-bar crates, sidecar
`.xxh64` files, auto-mount detection, vendor RAW SDKs.

---

## 2. Workspace layout

```
shortend/
├── Cargo.toml                    # workspace, shared lints (clippy::pedantic subset), edition 2024
├── PLAN.md
├── xsd/                          # vendored official schemas (MIT, with license file)
│   ├── ascmhl.xsd                # ASC MHL v2 manifest
│   ├── ascmhl-directory.xsd      # ASC MHL v2 chain file
│   └── mediahashlist-v1.1.xsd    # MHL v1.1
├── crates/
│   ├── shortend-core/            # library: the entire engine; no GUI types, no printing to stdout
│   │   ├── src/
│   │   │   ├── lib.rs            # public API: plan(), run(), resume(), verify(), presets
│   │   │   ├── model.rs          # Job, Source, Destination, FileEntry, outcomes (§3)
│   │   │   ├── hash/             # mod.rs (Hasher enum, Digest), c4.rs
│   │   │   ├── fs/
│   │   │   │   ├── relpath.rs    # validated relative paths
│   │   │   │   ├── scan.rs       # source walk, symlink/special-file refusal
│   │   │   │   ├── names.rs      # cross-FS filename legality, case-collision checks
│   │   │   │   └── platform/     # linux.rs, macos.rs, windows.rs (+ unix.rs shared)
│   │   │   ├── engine/
│   │   │   │   ├── preflight.rs  # validation, free space, volume identity
│   │   │   │   ├── pipeline.rs   # reader → fan-out → writers → verifiers
│   │   │   │   ├── stage.rs      # staging dir, atomic commit
│   │   │   │   ├── journal.rs    # crash-safe resume log
│   │   │   │   └── cancel.rs     # CancelToken
│   │   │   ├── report/           # csv.rs, txt.rs, pdf.rs, mhl/{v2.rs, v11.rs, read.rs}
│   │   │   ├── preset/           # mod.rs (TOML), template.rs (token naming)
│   │   │   └── media/            # M5: detect.rs, ffmpeg.rs, clips.rs
│   │   ├── assets/               # embedded font for PDFs (OFL-licensed)
│   │   └── tests/                # integration tests (see §8)
│   ├── shortend-cli/             # `shortend` binary: argument parsing + event rendering only
│   └── shortend-gui/             # `shortend-gui` binary: egui over shortend-core
├── packaging/                    # appimage/, macos/, windows/
└── .github/workflows/            # ci.yml, release.yml
```

Three crates instead of two: the CLI must exist without the GUI's dependency tree (the static
musl CLI build cannot link a GUI toolkit), and keeping the engine free of `clap` keeps the
library clean. The GUI depends only on `shortend-core`.

Platform code is a small set of free functions behind `cfg` in `fs/platform/`, not a trait:
there is exactly one implementation per target, so a trait would be a speculative abstraction.

```text
platform::fs_info(path)          -> VolumeInfo        // dev, fs type, uuid, block size, case sensitivity
platform::free_space(path)       -> u64
platform::open_source(path)      -> File              // read-only, O_NOATIME when permitted, O_NOFOLLOW
platform::create_staged(path)    -> File              // O_CREAT|O_EXCL|O_NOFOLLOW
platform::sync_file(&File)                            // fsync / F_FULLFSYNC / FlushFileBuffers
platform::sync_dir(path)                              // no-op on Windows
platform::drop_cache(&File)                           // fadvise DONTNEED / F_NOCACHE / n.a.
platform::open_uncached_read(path) -> UncachedReader  // read-back for verification
platform::rename_noreplace(from, to) -> Result<()>    // renameat2 / renamex_np / MoveFileExW
```

---

## 3. Core data model

Types below are the intended public shapes (field-level detail may shift during
implementation; anything structural will be raised before changing).

```rust
// ---------- identity ----------
pub struct JobId(uuid::Uuid);          // v7
pub struct DestId(pub u16);            // index into Job::destinations

// ---------- checksums ----------
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm { Xxh64, Xxh3, Xxh128, Md5, Sha1, Sha256, C4 }

impl HashAlgorithm {
    /// Element name in ASC MHL v2, or None if the vendored XSD does not define it.
    pub fn mhl_v2_name(self) -> Option<&'static str>;
    /// Element name in MHL v1.1 ("xxhash64be", "md5", "sha1"); None for XXH3/XXH128/SHA-256/C4.
    pub fn mhl_v11_name(self) -> Option<&'static str>;
}

pub struct Digest {
    pub algo: HashAlgorithm,
    bytes: Box<[u8]>,                  // 8..64 bytes depending on algo
}
impl Digest {
    pub fn to_hex(&self) -> String;    // lowercase
    pub fn to_mhl(&self) -> String;    // hex, or "c4…" base58 ID for C4
    pub fn parse(algo: HashAlgorithm, s: &str) -> Result<Self, HashParseError>;
}

/// Enum, not trait object: closed set, no allocation per chunk, inlined update loop.
pub enum Hasher {
    Xxh64(xxhash_rust::xxh64::Xxh64),
    Xxh3(Box<xxhash_rust::xxh3::Xxh3>),     // 64-bit digest
    Xxh128(Box<xxhash_rust::xxh3::Xxh3>),   // 128-bit digest
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    C4(sha2::Sha512),
}
impl Hasher {
    pub fn new(algo: HashAlgorithm) -> Self;
    pub fn update(&mut self, buf: &[u8]);
    pub fn finish(self) -> Digest;
}

/// Computes every algorithm of a HashList in the same read pass (one buffer, N updates).
/// With the default single algorithm this is a one-element loop.
pub struct MultiHasher(Vec<Hasher>);
impl MultiHasher {
    pub fn new(list: &HashList) -> Self;
    pub fn update(&mut self, buf: &[u8]);
    pub fn finish(self) -> Digests;
}

// ---------- volumes & paths ----------
pub struct VolumeInfo {
    pub dev: u64,                      // st_dev / volume serial
    pub fs_uuid: Option<String>,
    pub fs_type: FsType,
    pub case_sensitive: bool,          // probed, not assumed from fs_type
    pub block_size: u64,               // for free-space slack (exFAT clusters)
    pub read_only: bool,
}
pub enum FsType { Ext4, Xfs, Btrfs, ExFat, Vfat, Ntfs, Apfs, HfsPlus, Smb, Nfs, Other(String) }

/// UTF-8, '/'-separated, relative, no empty/"."/".." components.
/// Non-UTF-8 source names are a preflight error (MHL is XML text; fail closed).
pub struct RelPath(String);

pub struct FileEntry {
    pub rel: RelPath,
    pub size: u64,
    pub mtime: SystemTime,             // preserved on destination copies
}

// ---------- job ----------
pub struct Source {
    pub root: PathBuf,                 // canonical; must not be (or traverse) a symlink
    pub label: String,                 // card name → {card}
    pub volume: VolumeInfo,
}

pub struct Destination {
    pub id: DestId,
    pub label: String,                 // "RAID", "Shuttle A", …
    pub root: PathBuf,                 // must already exist; never a symlink
    pub target: PathBuf,               // root.join(expanded naming template)
    pub volume: VolumeInfo,            // captured at preflight, revalidated before each write batch
}

/// Non-empty, de-duplicated, ordered set of algorithms; the first is the *primary*
/// (shown first in reports, used for MHL). Default: [Xxh64]. Exposing more than one is an
/// explicit option: `settings.hashes` in a preset, `--hash xxh64,md5` on the CLI (M3).
pub struct HashList(Vec<HashAlgorithm>);   // not `HashSet`, to avoid clashing with std

/// One digest per algorithm in the job's HashList, same order.
pub struct Digests(Vec<Digest>);

pub struct JobSettings {
    pub hashes: HashList,              // default [Xxh64]
    pub source_reread: bool,           // optional second source pass (default false)
    pub parallel_files: NonZeroU8,     // files in flight (default 2; tuned in M6)
    pub chunk_size: u32,               // default 8 MiB
    pub reports: ReportOptions,
}

pub struct ProductionInfo {
    pub show: Option<String>,
    pub episode: Option<String>,
    pub shoot_day: Option<String>,
    pub camroll: Option<String>,
    pub operator: Option<String>,
    pub company: Option<String>,
    pub notes: Option<String>,
}

pub struct Job {                       // fully resolved, serialized to job.toml for resume
    pub id: JobId,
    pub created: jiff::Timestamp,
    pub source: Source,
    pub destinations: Vec<Destination>,
    pub settings: JobSettings,
    pub production: ProductionInfo,
    pub files: Vec<FileEntry>,         // preflight scan snapshot
}

/// What the CLI/GUI hands to the engine; `plan()` turns it into a `Job`.
pub struct JobSpec {
    pub source: PathBuf,
    pub card_label: Option<String>,
    pub destinations: Vec<(String /*label*/, PathBuf /*root*/)>,
    pub naming: Template,
    pub settings: JobSettings,
    pub production: ProductionInfo,
}

// ---------- outcomes ----------
pub enum FileStatus {
    Pending,
    Verified,
    Failed(Failure),
    AlreadyVerified,                   // resume: committed in an earlier run, re-checked by size+mtime
}

pub enum Failure {
    HashMismatch { expected: Digests, actual: Digests },
    SourceChanged,                     // size/mtime differed from the scan snapshot
    SourceReread { first: Digests, second: Digests },
    SourceUnavailable,                 // card pulled / EIO on read
    DestinationOffline,                // ENODEV/ENOENT on root, volume identity changed
    DiskFull,
    PermissionDenied,
    Io(String),
    Cancelled,
}

pub struct DestOutcome {
    pub dest: DestId,
    pub status: FileStatus,
    pub verified: Option<Digests>,
}

pub struct FileOutcome {
    pub entry: FileEntry,
    pub source: Option<Digests>,
    pub reread: Option<Digests>,
    pub dests: Vec<DestOutcome>,
}

// ---------- engine I/O ----------
pub struct CancelToken(Arc<AtomicBool>);

pub enum Event {                       // sent over a bounded channel; GUI/CLI render these
    Preflight { files: u64, bytes: u64 },
    FileStarted { rel: RelPath },
    Progress { dest: Option<DestId>, bytes_done: u64 }, // None = source read
    FileFinished { outcome: FileOutcome },
    DestinationFailed { dest: DestId, reason: Failure },
    ReportWritten { path: PathBuf },
    Finished { summary: JobSummary },
}

pub fn plan(spec: JobSpec) -> Result<Job, PreflightError>;
pub fn run(job: &Job, events: SyncSender<Event>, cancel: CancelToken) -> Result<JobReport, EngineError>;
pub fn resume(id: &JobId, events: SyncSender<Event>, cancel: CancelToken) -> Result<JobReport, EngineError>;
pub fn verify(dir: &Path) -> Result<VerifyReport, EngineError>;   // M2

// ---------- reports ----------
pub struct ReportOptions {
    pub csv: bool,                     // default true
    pub txt: bool,                     // default true
    pub mhl: Option<MhlVersion>,       // default Some(V2)
    pub pdf: Option<PdfOptions>,       // default Some(..)
}
pub enum MhlVersion { V2, V1_1 }
pub struct PdfOptions { pub logo: Option<PathBuf>, pub thumbnails: bool }

pub struct DestStats { pub dest: DestId, pub bytes: u64, pub write_time: Duration, pub verify_time: Duration }

pub struct JobReport {
    pub job: Job,
    pub started: jiff::Timestamp,
    pub finished: jiff::Timestamp,
    pub outcomes: Vec<FileOutcome>,
    pub dest_stats: Vec<DestStats>,
    pub warnings: Vec<String>,
    pub clips: Vec<ClipInfo>,          // M5; empty before
}

pub struct ClipInfo {                  // M5
    pub files: Vec<RelPath>,           // one clip may span files (R3D segments, DNG sequences)
    pub format: MediaFormat,
    pub codec: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub fps: Option<String>,           // rational as text, e.g. "24000/1001"
    pub tc_start: Option<String>,
    pub duration: Option<Duration>,
    pub thumbnail: Option<PathBuf>,    // JPEG in the job state dir
}

// ---------- presets ----------
pub struct Preset {
    pub name: String,
    pub format_version: u32,           // 1; bumps only on breaking changes
    pub naming: Template,
    pub destinations: Vec<PresetDestination>,
    pub settings: JobSettings,
    pub production: ProductionInfo,
}
pub struct PresetDestination { pub label: String, pub root: PathBuf }

pub struct Template { raw: String, parts: Vec<Part> }   // parsed once, validated on load
enum Part { Literal(String), Token(Token) }
enum Token { Date { fmt: Option<String> }, Time, Show, Episode, Day, Card, Camroll, Seq { width: u8 } }
// `{camrol}` parses to Token::Camroll (silent alias, ShotPut Pro preset compatibility).
```

Memory: file *data* never accumulates (bounded buffer pool, §4). Per-file *metadata* is held
in RAM — about 200 B per file, i.e. ~20 MB for a 100k-frame CinemaDNG card — which is acceptable
and keeps reporting simple.

---

## 4. Engine design

### 4.1 Job lifecycle

```
JobSpec ──plan()──▶ Job ──run()──▶ [preflight recheck] ──▶ pipeline ──▶ commit ──▶ reports ──▶ JobReport
                     │                                      │
                     └── job.toml + journal.jsonl ◀─────────┘   (state dir; drives resume())
```

State dir: `$XDG_STATE_HOME/shortend/jobs/<job-id>/` (default `~/.local/state/…`) on Linux,
`~/Library/Application Support/ShortEnd/jobs/` on macOS, `%LOCALAPPDATA%\ShortEnd\jobs\` on
Windows (resolved from env vars — three cases don't justify a crate).

### 4.2 Preflight (fail closed — refuse to start rather than discover problems mid-copy)

1. Source root exists, is a directory, is not a symlink; no path component is a symlink.
2. Walk source without following links. **Refuse** if any symlink, FIFO, socket or device node
   is found, or any name is not valid UTF-8; list all offenders. Hidden/system files
   (`.DS_Store`, `._*`) are copied like everything else — the card is copied as-is.
3. Each destination root exists, is a directory, is not a symlink, is writable (probe file in
   the staging dir), and is not inside the source or another destination (and vice versa).
4. Two destinations on the same volume → warning (allowed, but it halves throughput and is
   rarely intentional).
5. Expand the naming template; sanitize each token *value* for the strictest common filesystem
   (exFAT/NTFS illegal chars, trailing dots/spaces, reserved names like `CON`); a target folder
   that already contains files → refuse unless this is a resume.
6. Filename compatibility per destination: illegal names for that FS, over-long names
   (255 bytes on ext4, 255 UTF-16 units on exFAT/NTFS), and **case collisions** (`A.MOV` and
   `a.mov`) when the destination probes as case-insensitive → refuse.
7. Free space: `Σ size rounded up to block_size` + 1% margin ≤ free space on each destination.
8. Hash/manifest compatibility: e.g. MHL v1.1 + XXH3 is unrepresentable → refuse with a clear
   message (see §5.3).

### 4.3 Copy pipeline

```
              ┌────────── bounded queue ──▶ writer[dest 0] ──▶ stage file ──▶ verifier[dest 0]
 reader ──▶ hash once ──▶ Arc<Chunk> ─┼────────── bounded queue ──▶ writer[dest 1] ──▶ stage file ──▶ verifier[dest 1]
 (source, O_NOATIME)          └────────── bounded queue ──▶ writer[dest N] ──▶ …
```

- **One source read, hashed in-stream.** The reader reads `chunk_size` buffers from a fixed
  pool, feeds the hasher, and fans the same `Arc` buffer out to every destination writer. A
  buffer returns to the pool when the last writer drops it, so peak memory is
  `parallel_files × pool_depth × chunk_size` (default 2 × 4 × 8 MiB = 64 MiB), independent of
  file size. The slowest destination sets the pace — inherent to single-read fan-out.
- **Source integrity.** Source size and mtime are re-checked after the read; a change fails the
  file (`SourceChanged`). Optional `source_reread` re-reads the source with the cache dropped and
  compares hashes, catching flaky card readers.
- **Writers** write to a staged file (§4.4), then `fsync` it. Writes are buffered (not
  `O_DIRECT`, which breaks on exFAT and needs alignment everywhere), but on Linux we call
  `fadvise(DONTNEED)` behind the write head so a 300 GB clip doesn't flood the page cache and
  hide a dead drive until the final `fsync`.
- **Verification is a real read-back from the destination**, never a hash of the bytes we wrote
  (that proves nothing about the disk). After `fsync` the cache is dropped and the staged file
  is re-read and hashed: Linux `fadvise(DONTNEED)`; macOS `F_FULLFSYNC` + `F_NOCACHE`; Windows
  `FlushFileBuffers` + `FILE_FLAG_NO_BUFFERING` with sector-aligned buffers. On SMB/NFS the
  server may still answer from its cache; the report states this per destination rather than
  over-claiming. Verification of file *k* runs while file *k+1* is being written (pipelined).
  Source hashing stays in-stream; the only extra pass over the *source* is the opt-in reread.
- **Parallelism:** `parallel_files` files in flight (helps card dumps of thousands of small
  frames, where per-file `open`/`fsync`/`rename` dominates); each file fans out to all
  destinations concurrently. Defaults are tuned with benchmarks in M6.

### 4.4 Staging and atomic commit

- Staged files live **inside the destination volume** (rename must not cross filesystems):
  `<dest_root>/.shortend-staging/<job-id>/<rel>.part`, created with `O_EXCL|O_NOFOLLOW`.
- After verification passes: set mtime, then `rename_noreplace(part, target/rel)`, then
  `sync_dir` on the target directory (batched per directory for small-file cards).
- `rename_noreplace`: Linux `renameat2(RENAME_NOREPLACE)`; macOS `renamex_np(RENAME_EXCL)`;
  Windows `MoveFileExW` without `REPLACE_EXISTING`. Some Linux filesystems (exFAT, FUSE
  ntfs-3g, some network FSs) return `EINVAL` for `RENAME_NOREPLACE`; fallback is
  `symlink_metadata(target)` must be `NotFound` → plain `rename`. The remaining TOCTOU window
  is confined to a target folder this job created and is documented.
- **Before each write batch** the destination is revalidated: root still exists, is not a
  symlink, and `VolumeInfo.dev`/`fs_uuid` match preflight. Any mismatch → that destination is
  marked `DestinationOffline` and stops; other destinations continue.
- A failed verification leaves no file at the final path; the `.part` is deleted and the file
  is retried once (configurable count) before being reported as failed. The staging dir is
  removed when the job completes successfully.

### 4.5 Source safety

The source is opened read-only (`O_NOATIME` on Linux when we own the files, silently falling
back otherwise; `O_NOFOLLOW` everywhere). The engine has no code path that writes, renames or
deletes under the source root; a test asserts the source tree's bytes, mtimes and atimes are
unchanged after every engine test that uses a source fixture.

### 4.6 Journal and resume

`journal.jsonl` is append-only; each record is written and `fsync`'d before the action it
records is considered done:

```jsonl
{"t":"start","job":"0190…","at":"2026-10-09T22:22:00Z"}
{"t":"src","rel":"A001C001.mxf","hash":"ef46db3751d8e999"}
{"t":"commit","rel":"A001C001.mxf","dest":0,"hash":"ef46db3751d8e999"}
{"t":"dest_failed","dest":1,"reason":"DestinationOffline"}
{"t":"end","state":"completed"}
```

Resume (`shortend resume <job-id>`, also after `kill -9` or power loss):

1. Reload `job.toml`; re-run preflight checks against the *same* volumes (fs UUID + device
   identity). For the source, also confirm the file list, sizes and mtimes match the snapshot —
   a different card with the same name is refused.
2. A truncated final journal line is ignored.
3. **Skip rule.** A file is not re-copied to a destination (`AlreadyVerified`) only when it is
   *re-verified now*: the final file exists, is re-read from the device and hashed, its
   checksums equal the journaled source checksums, its size and mtime match (mtime within the
   destination filesystem's granularity, 2 s for FAT), and the source still matches the scan
   snapshot. A journal `commit` record alone is never trusted — the file may have been damaged
   after it was verified. Anything that fails re-verification, or has no journaled source
   checksum, is removed (it can only be ours: the target folder belongs to this job) and
   re-copied. Stale `.part` files are deleted. Cost: resume reads back every file already on
   the destination once, which is the price of never skipping an unproven copy.
4. Resume granularity is **per file**, not mid-file: mid-file resume would require persisting
   hasher state (not supported by the hash crates) and re-proving the partial bytes, so a
   restarted file is re-read from byte 0 (approved).
5. The journal is written line-by-line and `fdatasync`'d periodically and at job end rather
   than per record: the skip rule above re-proves anything a lost record would have claimed,
   so per-record fsync would cost throughput on small-file cards without adding safety.

### 4.7 Errors, unplug, cancellation

- I/O errors are classified from `io::ErrorKind` + raw OS errno: `ENOSPC`→`DiskFull`,
  `EACCES/EPERM/EROFS`→`PermissionDenied`, `EIO/ENODEV/ENXIO/ENOENT-on-root`→`DestinationOffline`
  or `SourceUnavailable`. A destination failure isolates that destination; a source failure
  stops the job (nothing more can be read) and leaves it resumable.
- A read or write that blocks on a dead device is unavoidable in the kernel; threads are never
  killed. The engine reports the destination as stalled after a timeout with no progress and
  lets the user cancel; the UI never freezes because the engine runs off the UI thread.
- **Cancellation:** `CancelToken` is checked between chunks. In-flight `.part` files are
  removed, committed files stay, the journal records `cancelled`, and the job is resumable.
  `run()` returns within one chunk-time (plus any blocked syscall) of cancel.

---

## 5. Reports

### 5.1 Placement

Reports are written into each destination's target folder under `_ShortEnd/` (and optionally
to a user reports folder). `_ShortEnd/` is recorded as an ignore pattern in the MHL so reports
don't hash themselves. Nothing is ever written to the source.

### 5.2 CSV and TXT (M1)

- CSV: one row per file × destination — `relative_path, size_bytes, modified, algorithm,
  source_hash, destination, verified_hash, status, error`.
- TXT: human-readable summary (job, production fields, source, destinations, totals, per-
  destination speed, PASS/FAIL), followed by the failure list and then the full file list.

### 5.3 ASC MHL (M2)

**v2 (default)** — written per destination at the offload target folder:
`<target>/ascmhl/NNNN_<folder>_<YYYY-MM-DD_HHMMSS>Z.mhl` plus `ascmhl_chain.xml`, which records
each generation's C4 hash (so C4 is implemented regardless of the user's chosen algorithm).
Each generation contains creator/process info, the ignore list, every file with size and
last-modification date, and a hash element marked `original` (first sighting) or `verified`
(when the source already carried an ASC MHL history, which is copied along with the card and
extended with a new generation). Directory content/structure hashes follow the reference
implementation's algorithm. Exact file naming and hash semantics are matched to the vendored
spec and reference tool output at M2 — not reconstructed from memory.

Hash support: ASC MHL v2 supports MD5, SHA-1, C4, XXH64, XXH3 and XXH128. SHA-256 appears in
newer reference-library documentation but not in older schema releases; M2 vendors the
current official XSD and enables SHA-256 in MHL v2 only if that XSD defines it — otherwise
SHA-256 + MHL is refused at preflight (SHA-256 still works for CSV/TXT/PDF).

**v1.1 (interop)** — single `<folder>_<date>.mhl` per target (`<hashlist version="1.1">`).
v1.1 can only carry MD5, SHA-1 and xxHash64 (emitted as `xxhash64be`; both `xxhash64` and
`xxhash64be` are read, with byte-order handling checked against MhlFileVerify/simple-mhl
fixtures). XXH3/XXH128/SHA-256/C4 + v1.1 is refused at preflight.

**Reading:** v2 histories and v1.1 files are parsed (streaming) for `shortend verify <dir>`
and to check a source card against an existing manifest during offload. `verify` on a v2
folder appends a new generation with `verified`/`failed` entries, as the reference tool does.

**Validation (tests/CI):** every generated manifest is validated with
`xmllint --noout --schema xsd/<schema>.xsd`, and v2 output is additionally checked by the
reference `ascmhl verify` (pip-installed in CI). Golden fixtures produced by the reference
tool are committed and read back in unit tests.

### 5.4 PDF (M2, thumbnails in M5)

Page 1: logo (PNG/JPEG via `image`), production fields, operator, job ID, source, destinations
with per-destination throughput and verify time, algorithm, file/byte totals, start/end/
duration, large PASS/FAIL. Then failures (if any), then a file manifest table (path, size,
hash, per-destination status). M5 adds a clip section: thumbnail, codec, resolution, fps,
start TC, duration. An OFL font is embedded so Unicode filenames render identically
everywhere. Frame sequences (DNG, ARRIRAW) are listed as ranges in the clip section in M5.

---

## 6. Presets and naming (M3)

Presets are TOML files in the presets folder (`$XDG_CONFIG_HOME/shortend/presets/`,
`~/Library/Application Support/ShortEnd/presets/`, `%APPDATA%\ShortEnd\presets\`;
`SHORTEND_CONFIG_DIR` overrides), loadable from any path, with no machine-specific state
inside (so they can be shared between DITs). Every key except `format_version` is optional;
unknown keys and unknown `format_version`s are errors. `examples/bigshow.toml` is the
reference:

```toml
format_version = 1
name = "Big Show S01"

[naming]
folder = "{show}/{date:%Y%m%d}_D{day}/{camroll}_{seq:3}"

[settings]
hashes = ["xxh64", "md5"]      # or one: hashes = "xxh64"; computed in one read pass
source_reread = false

[reports]
csv = true
txt = true
mhl = ["v2", "v1.1"]           # "v2", "v1.1", both, or "none"
pdf = { logo = "bigshow.png" } # true / false / options; relative to the preset file

[[destinations]]
label = "RAID"
root = "/mnt/raid/BIGSHOW"

[production]
show = "Big Show"
episode = "101"
company = "Example Productions"
operator = "Jane Doe"
```

Tokens: `{date[:fmt]}` (default `%Y%m%d`), `{time[:fmt]}` (default `%H%M%S`), `{show}`,
`{episode}`, `{day}`, `{card}`, `{camroll}`, `{seq[:width]}` (width 1–9); `{{`/`}}` are literal
braces. `{camrol}` is accepted as a silent alias of `{camroll}` so ShotPut Pro-style presets
load unchanged; saved presets always write `{camroll}`. Unknown tokens, unbalanced braces,
bad widths/formats, `{seq}` more than once and a date format containing `/` are load-time
errors; a token whose value is empty is a preflight error (never silently produces `//`).
Token values are sanitized (path separators, `:*?"<>|` and control characters become `_`;
surrounding spaces and trailing dots are dropped) and the plan warns when a value changed.

**Safe sequential numbering:** `{seq}` is computed across *all* destinations as
`max(existing matching numbers) + 1` at plan time (provisional), then claimed when the job
starts: the numbered folder is created with `create_dir` (fails if it exists) on every
destination in a fixed (sorted) order; on any collision the folders just created are removed
and the next number is tried, so all copies of one offload share one number and two concurrent
jobs can't take the same one. No counter file is stored, so a preset copied to another
machine can't drift. Resume uses the stored folder and never claims a new number.

---

## 7. Format awareness and thumbnails (M5)

- **Detection** by extension + magic bytes + card folder signatures: ARRIRAW (`.ari`,
  ARRIRAW MXF), Codex HDE (`.arx`), R3D (`.R3D` + `.RDC` folders, spanned segments), Sony
  X-OCN/XAVC (`XDROOT`, `.mxf`), BRAW, ProRes/DNxHR (`.mov`/`.mxf` codec via ffprobe),
  CinemaDNG sequences, Canon `.crm`/`.cr3`, JPEG/HEIF/TIFF, WAV/BWF audio, sidecars
  (`.xml`, `.xmp`, `.ale`, `.cdl`, LUTs). Unknown files pass through untouched with no
  metadata — detection never affects what gets copied.
- **Clip grouping** (spanned R3D, frame sequences) so reports list clips, not 40,000 frames.
- **Metadata:** `ffprobe -show_format -show_streams -of json` → codec, resolution, fps,
  timecode tag, duration.
- **Thumbnails:** `ffmpeg -skip_frame nokey -ss <~1/3 duration> -i <clip> -frames:v 1
  -f image2pipe -c:v png -` (I-frame only), downscaled and JPEG-encoded with `image`.
  Generated from the **verified destination copy**, after the copy finishes, so the card can
  be ejected immediately and the source is never re-read for reporting.
- **Degradation:** ffmpeg/ffprobe missing, failing, or timing out (per-call timeout; child is
  killed) → no thumbnail/metadata for that clip, a single warning in the report, never a
  crash. ffmpeg cannot decode ARRIRAW, R3D, BRAW, X-OCN or HDE; those clips get a format
  label and filename-derived info instead of a thumbnail. Vendor SDK/CLI integration is out
  of scope.

---

## 8. Testing strategy

All tests run with `cargo test` from M1 onward unless marked privileged.

| Area | Tests |
|---|---|
| Checksums | Known vectors for every algorithm (e.g. XXH64("") = `ef46db3751d8e999`, XXH3-64("") = `2d06800538d394c2`, MD5("") = `d41d8cd98f00b204e9800998ecf8427e`, SHA-256("abc") = `ba7816bf…15ad`, C4 vectors from the C4 spec), plus streaming-equals-one-shot over random chunk boundaries, plus cross-checks against `xxhsum`/`sha256sum` output for generated files. |
| Copy/verify | Small/empty/large (sparse-created multi-GB) files, deep trees, 10k tiny files, Unicode names, N=1..4 destinations, mtime preservation, injected corruption on one destination detected as `HashMismatch` while others pass. |
| Source safety | Source tree checksum + mtimes + atimes identical before/after every fixture run; symlink/FIFO in source → preflight refusal; symlinked destination root → refusal. |
| Resume | CLI subprocess killed with SIGKILL at random points (looped, seeded) → `resume` completes; no partial file at any final path; journal consistent; every committed file verifies. Truncated journal line tolerated. |
| Cancellation | Cancel mid-file → no `.part` left, resumable, returns promptly. |
| Naming | Template parse errors, every token, date formats, sanitization of illegal chars/reserved names, `{seq}` across destinations with pre-existing folders and concurrent creation. |
| Presets | TOML round-trip, unknown keys rejected, `format_version` checks. |
| MHL | `xmllint --schema` validation of every v2/v1.1 output; `ascmhl verify` on v2 output; reading reference-generated golden fixtures; refusal of unrepresentable algorithm/version combos. |
| Cross-FS | Case-collision refusal on case-insensitive targets; long-name refusal; exFAT `RENAME_NOREPLACE` fallback path. |

**Linux privileged integration tests** (`tests/linux_devices.rs`, run when
`SHORTEND_PRIVILEGED_TESTS=1`; a dedicated CI job with passwordless sudo):

- Loop-device filesystems: ext4, exFAT (`exfatprogs`), NTFS (`ntfs3`), vfat — full offload +
  verify on each.
- **Hot-unplug simulation:** destination on a device-mapper target; mid-copy the table is
  swapped to `dm-error` (`dmsetup load … error && dmsetup resume`) → that destination fails as
  `DestinationOffline`, the other destination completes and verifies, the job is resumable.
  Same for the source (`SourceUnavailable`, job stops, resumable after the device is restored
  with the same UUID). A second variant uses `umount -l` mid-copy.
- **Disk full:** small loop filesystem that passes preflight but is filled by an outside writer
  mid-job → `DiskFull`, no partial file at a final path.
- **Permissions:** read-only remount mid-job, `chmod 000` target subdir, root-owned dest root.
- Where the runner kernel provides `scsi_debug`, a variant deletes the SCSI device via sysfs
  (`echo 1 > /sys/block/sdX/device/delete`), which is the closest software analogue to pulling
  a USB/eSATA cable; otherwise it is run manually before releases (§9, M6 checklist), together
  with real USB, eSATA and NVMe pulls.

---

## 9. Milestones

Every milestone ends with: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, and `cargo build --workspace --release` all passing on
Linux, plus the milestone's own done criteria. CI (Linux/macOS/Windows build + test) is set
up in M1 — M6 extends it rather than introducing it, since the requirement is tests on every
commit.

### M1 — CLI engine: copy + verify + CSV/TXT

Scope: workspace skeleton; `hash` (all 7 algorithms incl. C4); `RelPath`, scan, preflight
(§4.2 items 1–7); pipeline with fan-out, read-back verification, staging and atomic commit;
journal + `resume`; cancellation (Ctrl-C); Linux/macOS/Windows platform functions; CSV/TXT
reports; CLI:

```
shortend copy  --source <dir> --dest [LABEL=]<dir>… [--folder <name>] [--hash xxh64]
               [--reread-source] [--parallel N] [--no-csv] [--no-txt]
shortend resume <job-id>
shortend jobs                       # list jobs and their state
```

Exit codes: 0 all verified · 1 some files failed · 2 preflight refused / usage · 3 cancelled ·
4 internal error.

Done when:
- [x] All checksum vector tests pass for all 7 algorithms (vectors cross-checked against Python
      `hashlib`, the `xxhash` package and the ASC MHL reference library's C4).
- [x] Offload of a fixture tree to 3 destinations verifies, preserves mtimes, leaves no
      staging dir, and leaves the source byte-for-byte and atime-unchanged.
- [x] Injected destination corruption is detected and reported, exit code 1.
- [x] Randomized SIGKILL + `resume` test passes 25 seeded iterations.
- [x] Linux privileged tests pass: ext4 + exFAT offload, dm-error unplug of one destination
      (other completes), disk-full, read-only remount. *CI (`linux-devices`, as root, with
      `SHORTEND_REQUIRE_ALL_FS=1` so nothing can skip): ext4, kernel FAT, exFAT and NTFS via
      FUSE; dm-error unplug of a destination and of the source, both resumed after reconnect;
      lazy unmount; disk full; read-only remount; unprivileged-user permissions; exFAT
      case-collision refusal.*
- [x] Peak RSS for a single-file offload to 2 destinations stays under 100 MB. The test file
      size defaults to 4 GB (≥ 40× the bound, so any whole-file buffering would show) and is
      set by `SHORTEND_RSS_TEST_GB`; run with 20 on a machine with ≥ 60 GB free.
      *Measured: 39 MB peak at 4 GB and at 10 GB.*
- [x] CI green on Linux, macOS, Windows (privileged job on Linux). *github.com/SudoTrashTemp/Shortend,
      two consecutive green runs on commit a61f57e (Ubuntu/macOS/Windows tests incl. 25× SIGKILL+resume,
      linux-devices, memory-bound 39 MB at 4 GB). Test results are posted as check annotations.*

**M1 implementation notes** (deviations and decisions made while building):
- Job state is `job.toml` (human-readable header) plus `snapshot.json` (the file list), so
  `shortend jobs` stays fast for 100k-file cards.
- `Event::Progress` carries cumulative counters and is sampled 4×/s with `try_send`, so a slow
  front end never stalls the engine; `run()` returns the `JobReport` instead of a final event.
- `ReportOptions` has only `csv`/`txt` until M2 adds `mhl`/`pdf`; `JobSpec.folder` is a plain
  `/`-separated name until M3 adds templates.
- Within a file all destinations write and verify concurrently; read-back of file *k* overlaps
  file *k+1* through `parallel_files` (default 2) rather than inside one worker. Revisit with
  M6 benchmarks.
- An EIO/ENODEV/ENXIO on the source stops the job (`Aborted`, resumable) rather than failing
  file by file.
- On resume, volumes are matched by filesystem UUID where known (a reconnected drive often
  gets a new device number) and the recorded device number is refreshed. macOS matches by
  volume UUID too since M4.
- Resume re-verifies every file already at its final path (added after M1 review: a file
  corrupted after verification with size and mtime intact must be caught —
  `resume_catches_corruption_after_verification`).
- Linux `fuseblk` mounts (ntfs-3g, exfat-fuse) get Windows naming rules.
- Not in M1: stall detection for a destination whose writes hang in the kernel — moved to M2
  (minimal watchdog) after review; the full progress view stays in M4.
- `shortend-core` has a test-only `fault-injection` feature (`SHORTEND_FAULT=corrupt-dest=N`);
  it is enabled only for tests and is absent from release builds.

### M2 — MHL + PDF reports

Scope: ASC MHL v2 writer (generations, chain file, directory hashes, ignore list), v1.1 writer,
streaming readers for both; carry-forward of an existing source history; `shortend verify
<dir>`; PDF report without thumbnails (logo, production fields, speeds, manifest table);
vendored XSDs; preflight checks for algorithm/manifest compatibility.

Also in M2 (from M1 review): a minimal **stall watchdog**. `JobSettings.stall_timeout`
(default 60 s, configurable, CLI `--stall-timeout`): a destination with zero bytes written or
verified for that long while it has work is marked `Failure::DestinationStalled`, its files are
reported failed, and the job finishes with the other destinations. The hung writer thread is
detached rather than joined (a thread blocked in the kernel cannot be killed); its `.part` file
is cleaned up on resume. The full progress view stays in M4.

Before any M2 feature work: push to GitHub, get CI green on Linux/macOS/Windows including the
device-mapper unplug tests, and fix what it finds.

Done when:
- [x] CI green on all three platforms and the privileged Linux job (carried from M1).
- [x] Stall watchdog: a test with a destination whose writes block (FUSE or dm `delay`/
      suspended target) marks it `DestinationStalled` within the timeout, the other
      destination completes, and the job reports it; exit code 1. *Two tests: a frozen
      (suspended) device-mapper target in `linux-devices` (job finishes ~1 s after the 3 s
      timeout; resume completes the destination after unfreezing), and an injected hung
      writer through the CLI on all three platforms (exit 1, no report written to the hung
      destination, resume completes it).*
- [x] Every MHL produced by the test suite passes `xmllint --schema` against the vendored XSD.
      *All manifests written by `tests/mhl.rs` (v2 generations, chain files, v1.1) are
      validated; CI fails if `xmllint` is missing on Linux/macOS.*
- [x] `ascmhl verify` accepts ShortEnd v2 output; ShortEnd `verify` accepts reference-tool
      golden fixtures (v2 and v1.1) and detects a tampered file. *The reference CLI's verify is
      `ascmhl-debug verify` (ascmhl 1.2); `simple-mhl verify` (mhl-suite) accepts our v1.1.
      Fixtures: reference histories in all six algorithms plus a two-generation history,
      a `simple-mhl`-sealed v1.1 and a legacy little-endian `xxhash64` v1.1.*
- [x] Second offload of an already-MHL'd folder produces generation 0002 with correct chain.
      *For every algorithm; directory and root hashes equal the reference's generation 0001.*
- [x] PDF for a 10k-file job renders, opens in at least two viewers (poppler `pdftotext`
      check in CI), and includes logo and Unicode filenames correctly. *Checked with
      poppler (`pdftotext`, `pdfinfo`, `pdfimages`) and `qpdf --check`. Latin, Greek and
      Cyrillic names render; scripts the embedded font lacks (e.g. CJK) appear as
      `[U+XXXX]` — see notes.*

**M2 implementation notes** (deviations and decisions):
- `shortend verify` is read-only, like the reference `ascmhl-debug verify` (§5.3 said it
  would append a generation; the reference tool's verify does not, and a read-only check is
  safer to run on a delivery drive). It verifies an ASC MHL history, or every `.mhl` v1.1
  file in the folder.
- SHA-256 is not in the official ASC MHL v2 XSD (commit 0fb61f1), so a job whose only
  checksum is SHA-256 is refused with a manifest enabled; extra algorithms a manifest format
  cannot carry are kept in CSV/TXT/PDF and reported as a warning.
- ASC MHL generations use process `transfer`; author = operator; the other production
  fields go into `<comment>`. File order, directory content/structure hashes and the default
  ignore list match the reference implementation byte-for-value; `_ShortEnd/` is added to
  the ignore list.
- A card that carries an ASC MHL history: its chain is verified at preflight (a damaged
  history refuses the job unless `--mhl none`), every algorithm it uses is added to the job
  (computed in the same read pass), the copy extends it with a new generation, and the card is
  checked against its own history on every completed run (including resumes): files that
  contradict it fail on every destination (`SourceManifestMismatch`) and files it lists that
  the card lacks are reported; either fails the job. Nested histories are copied but not
  extended (warning).
- Resume: the card's `ascmhl_chain.xml` on a destination legitimately differs from the card
  once ShortEnd has extended it, so it is not re-copied when the journal records that a
  manifest was written there. A new generation is written only if the run copied something.
- Several manifests per job: `reports.mhl` is a list (`--mhl v2,v1.1`); v2 is the default.
  ASC MHL v2 lists zero-byte files. MHL v1.1 cannot (its schema requires `size ≥ 1`), so
  the omission is loud: a manifest issue in the summary, job state `CompletedWithFailures`
  (exit 1), and the omitted files named in the CSV (detail column), TXT and PDF reports. A
  job with v1.1 as its *only* manifest and zero-byte files on the card is refused at
  preflight rather than writing an incomplete manifest. When both are written, v1.1 is
  written first and its file name is added to the v2 generation's ignore list.
- PDF fonts: DejaVu Sans/Bold/Mono (Bitstream Vera license, permissive; not OFL as planned)
  embedded and subset with `allsorts` (printpdf only subsets with its HTML layout engine,
  which would pull in a large dependency tree). A 10k-file report is ~340 KB (about 120
  pages, rendered in ~1 s in release builds). CJK and other
  scripts DejaVu lacks are shown as `[U+XXXX]` rather than altered; exact names stay in
  CSV/TXT/MHL. Candidate for M6: an optional system fallback font.
- Stall watchdog coverage: the watchdog runs from job start until cleanup — destination
  setup (folder creation, staging), resume's re-verification pass, the copy, and manifest/
  report writing. Per-destination setup and re-check run on detached threads, so one hung
  drive is dropped (`DestinationStalled`) while the others continue. Tested with injected
  hangs in setup and re-check (CLI, all platforms) and a frozen device-mapper target during
  resume re-check (`linux-devices`).
- Mac-formatted drives (addendum): HFS+ (plain, journaled, HFSX) and APFS are covered by
  loop-mounted images in `linux-devices` (`mkfs.hfsplus`; `mkapfs` + `apfs-fuse`). FUSE mounts
  are identified by their backing device (udev `ID_FS_TYPE` or on-disk signature), so
  APFS-through-FUSE is recognised. Read-only APFS and journaled-HFS+ destinations are refused
  at preflight with the reason and what to do instead. Name collision checks and manifest
  name matching are normalization-aware (HFS+ returns NFD names). Support matrix in README.
- Windows: canonical paths are reported without the `\\?\` prefix (it broke external tools
  and was unreadable in reports).

### M3 — Presets and naming templates

Scope: template parser/expander, sanitization, `{seq}` allocation; preset TOML load/save/
validate; CLI `--preset <file|name>` with flag overrides; `shortend preset new|show|list`.

Also in M3 (from the M1 plan decision): several checksum algorithms per preset
(`settings.hashes`, CLI `--hash xxh64,md5`), computed in the single read pass the M1 hashing
layer was built for, and written to every manifest that can carry them.

Done when:
- [x] Template tests cover every token, formats, sanitization and error cases.
      *`naming.rs` unit tests; `tests/naming.rs` runs templates through the engine (empty
      values refused with nothing written, sanitized values warned).*
- [x] `{seq}` test with pre-existing folders on mismatched destinations and two concurrent
      jobs yields unique, identical-per-job numbers. *Two jobs planned together (both
      provisional 005) run concurrently and get 005 and 006 on both destinations; plus a
      12-thread claim stress test over overlapping destination sets; resume keeps the folder.*
- [x] Preset round-trip and rejection tests pass; an example preset ships in `examples/`.
      *Round trip (load → save → load) incl. `{camrol}` → `{camroll}`; 19 rejection cases;
      CLI `preset new|show|list`, `--from`, `--force`, invalid presets listed and refused;
      a preset with three checksums and both manifests drives a copy end to end.*

**M3 implementation notes:**
- Flags override the preset field by field; `--dest` replaces the preset's destinations as a
  whole. `shortend preset new NAME [flags]` saves the flags (paths made absolute);
  `--from OTHER` starts from another preset.
- A preset's destination roots must be absolute (`~/` is expanded). A relative logo path is
  relative to the preset file.
- `[production] day` is the shoot day (`shoot_day` is still read, for job files written by M1/M2).

### M4 — GUI (egui)

Scope: `shortend-gui` with four views — **Offload** (source picker, destinations, preset
picker, production fields, preflight results shown inline before Start), **Progress**
(per-destination bars and speeds, current file, failures, Cancel), **History** (jobs from the
state dir, open reports, Resume), **Presets** (edit/save). The engine runs on a worker thread;
the GUI only consumes `Event`s and calls the public API — no GUI types or crates in
`shortend-core`.

**Blocker carried from M1 (fixed):** on macOS, volumes were identified by device number only,
so a drive that was unplugged and reconnected under a new device number was refused on resume.
macOS now reads the volume UUID (`diskutil info -plist`, which needs no FFI; APFS, HFS+ and a
serial-derived UUID for exFAT/FAT) and matches on it as Linux does.

Done when:
- [x] macOS volume UUID implemented and covered by a test (blocker above).
      *`tests/macos_volumes.rs`: `hdiutil` images formatted exFAT, HFS+ and APFS; a re-attached
      image under a new device number resumes, a different image with the same name at the
      same mount point is refused.*
- [x] `shortend-core` has no dependency on any GUI crate (checked in CI with `cargo tree`).
- [x] A full offload, cancel, and resume can be done from the GUI on Linux (X11 and Wayland),
      macOS and Windows (manual checklist recorded in `docs/qa.md`). *Automated on all three
      OSes by `shortend-gui/tests/gui.rs` (egui_kittest drives the real widgets: Check →
      Start → Cancel → History → Resume → PASS, files identical); the real window is
      smoke-tested in CI on X11 (Xvfb) and Wayland (headless weston). The manual checklist
      (native dialogs, drag and drop, real drive pulls, HiDPI) is run per release.*
- [x] UI stays responsive (≥30 fps) during a 10k-file job and while a destination is stalled.
      *`tests/responsive.rs`: p95 frame < 33 ms and no frame > 200 ms through a 10,000-file
      job with one destination hung until the watchdog drops it (measured 3.5 ms p95).*
- [x] Idle GUI with no job: <1% CPU, no continuous repaint. *The test checks the UI settles in
      ≤3 frames with no repaint pending; the release build used 0 CPU ticks in 10 s.*

**M4 implementation notes:**
- `shortend-gui` (eframe 0.33, glow; rfd for native dialogs) is a library plus a thin binary,
  so the tests drive the same `ShortEndApp`. The Offload form maps to and from a `Preset`, so
  the GUI reuses M3's validation and the CLI's override rules.
- The plan shown by Check is invalidated whenever the form differs from the form that was
  checked; Start always runs the job that was shown.
- New engine event `FileStarted` (sent with `try_send`, may be dropped) for the "current
  files" list; it never slows the copy.
- Closing the window during a copy asks first; "Cancel job and quit" leaves it resumable.
- Found by the kill/resume test on Windows and fixed: a run killed while writing an ASC MHL
  generation could leave a generation the chain doesn't list, which the resumed job then
  refused. Manifest writes are journaled first; resume keeps a generation whose chain entry
  landed and removes an orphan otherwise (`tests/mhl.rs`, four crash points).
- Known gaps: without an XDG desktop portal, Browse… does nothing (type the path); egui's
  fonts lack CJK glyphs (names are exact in reports and manifests).

### M5 — Format awareness + thumbnails

Scope: detection, clip grouping, ffprobe metadata, ffmpeg I-frame thumbnails from destination
copies, PDF clip section, graceful degradation.

Done when:
- [ ] Detection tests over small synthetic/real fixture headers for each listed format family;
      unknown files pass through unchanged.
- [ ] With ffmpeg present: thumbnails + metadata for ProRes, DNxHR, XAVC, H.264 test clips
      (generated with ffmpeg in the test itself).
- [ ] With ffmpeg absent (PATH stripped) or hanging (fake binary that sleeps): job completes,
      PDF has placeholders and a warning, no crash, timeout respected.
- [ ] Frame sequences collapse to ranges in the PDF clip section.

### M6 — Polish, packaging, CI

Scope: benchmarks and default tuning (`parallel_files`, `chunk_size`, pool depth) on NVMe,
USB HDD, and NAS; release workflow; packaging:

- **Linux:** static **CLI** tarball (`x86_64` and `aarch64-unknown-linux-musl`) and an
  **AppImage** containing CLI + GUI built on an older glibc base (Ubuntu 22.04) for broad
  compatibility. A fully static GUI is not possible: egui/winit `dlopen` libGL/Wayland/X11,
  which musl static binaries cannot do. ffmpeg is not bundled (size/licensing); it is used if
  found on `PATH` or configured.
- **macOS:** universal `.dmg` (app bundle + CLI). Signing/notarization needs an Apple
  Developer account — wired up but conditional on secrets.
- **Windows:** `.zip` and an `.msi` via `cargo-wix`.

Done when:
- [ ] Release workflow produces all artifacts from a tag; AppImage launches on a clean
      Ubuntu 22.04, Debian 12 and Fedora current VM; static CLI runs on Alpine.
- [ ] Benchmarks recorded in `docs/bench.md`; ShortEnd within 10% of `cp` + `xxhsum` wall time
      for single destination on NVMe, and source read once for N destinations.
- [ ] Manual pre-release hardware checklist completed on Linux: physical USB, eSATA and NVMe
      enclosure pulls mid-copy (source and destination), all resumable.
- [ ] README with CLI reference and preset docs.

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| MHL v2 subtleties (directory hashes, generation naming) differ from what we infer | Golden fixtures from the reference tool + `ascmhl verify` in CI decide correctness, not our reading of the spec. |
| Read-back verification hits cache on network filesystems | Stated per destination in reports; no false claims. |
| CI runners lack `scsi_debug` / limit device-mapper | `dm-error` is the primary unplug test (available on GitHub Ubuntu runners with sudo); physical pulls are a release-gate checklist. |
| exFAT/NTFS drivers on Linux vary (kernel `ntfs3` vs FUSE `ntfs-3g`) | Tested explicitly on loop devices; `rename_noreplace` fallback covers missing `RENAME_NOREPLACE`. |
| Very large cards (≥100k files) inflate PDF size | Clip-level grouping in M5; full per-file detail remains in CSV/MHL. |

---

## 11. Decisions (resolved 2026-10-10)

1. **Resume granularity:** per file; interrupted files restart from byte 0. Files already
   fully copied and verified are skipped, never re-copied (§4.6 skip rule).
2. **Checksums:** one algorithm per job by default. The hashing layer (`HashList`,
   `MultiHasher`, `Digests`) computes several algorithms in a single read pass and the data
   model, journal and reports carry all of them; multiple algorithms become an explicit
   per-preset option in M3. No multi-hash UI before then.
3. **Token:** `{camroll}`, with `{camrol}` as a silent alias.
