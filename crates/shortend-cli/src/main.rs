//! `shortend` command line: argument parsing and event rendering only. All work happens in
//! `shortend-core`.

use std::io::{IsTerminal, Write};
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use shortend_core::report::mhl::{self, MhlVersion};
use shortend_core::report::{human_bytes, human_duration, human_rate};
use shortend_core::{
    CancelToken, Error, Event, FileStatus, HashAlgorithm, HashList, Job, JobReport, JobSettings,
    JobSpec, JobState, PdfOptions, ProductionInfo, ReportOptions,
};

const EXIT_FAILED: u8 = 1;
const EXIT_REFUSED: u8 = 2;
const EXIT_CANCELLED: u8 = 3;
const EXIT_INTERNAL: u8 = 4;

#[derive(Parser)]
#[command(
    name = "shortend",
    version,
    about = "Verified media offloads: one source, many destinations, every copy read back and checked."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy a card or folder to one or more destinations and verify every copy.
    Copy(Box<CopyArgs>),
    /// Continue an interrupted job. Files already verified are skipped.
    Resume {
        /// Job ID, or a unique prefix of it (see `shortend jobs`).
        job: String,
    },
    /// List jobs and their state.
    Jobs,
    /// Check a folder against its manifest: an ASC MHL history (`ascmhl/`) or MHL v1.1 files.
    /// Read-only.
    Verify {
        /// The offload folder (the one containing `ascmhl/` or the `.mhl` file).
        dir: PathBuf,
    },
}

#[derive(Args)]
struct CopyArgs {
    /// Card or folder to offload. It is only ever read.
    #[arg(long, short)]
    source: PathBuf,
    /// Destination root, optionally labelled: `--dest RAID=/mnt/raid`. Repeat for each copy.
    #[arg(
        long = "dest",
        short = 'd',
        required = true,
        value_name = "[LABEL=]DIR"
    )]
    dests: Vec<String>,
    /// Folder created under every destination root. Defaults to the source folder's name.
    #[arg(long)]
    folder: Option<String>,
    /// Checksum algorithm: xxh64, xxh3, xxh128, md5, sha1, sha256, c4.
    #[arg(long, default_value = "xxh64")]
    hash: HashAlgorithm,
    /// After copying, read the source again (bypassing the cache) and compare checksums.
    #[arg(long)]
    reread_source: bool,
    /// Files copied at the same time.
    #[arg(long, default_value = "2")]
    parallel: NonZeroU8,
    /// Don't write the CSV report.
    #[arg(long)]
    no_csv: bool,
    /// Don't write the TXT report.
    #[arg(long)]
    no_txt: bool,
    /// Manifest written into every destination: v2 (ASC MHL), v1.1, or none.
    #[arg(long, default_value = "v2", value_parser = ["v2", "v1.1", "none"])]
    mhl: String,
    /// Don't write the PDF report.
    #[arg(long)]
    no_pdf: bool,
    /// Logo (PNG or JPEG) for the PDF report.
    #[arg(long, value_name = "FILE")]
    logo: Option<PathBuf>,
    #[command(flatten)]
    production: ProductionArgs,
    /// Give up on a destination after this many seconds without progress (0 = never).
    #[arg(long, default_value = "60", value_name = "SECS")]
    stall_timeout: u32,
    /// Read/write chunk size in MiB (tuning).
    #[arg(long, default_value = "8", hide = true, value_parser = clap::value_parser!(u32).range(1..=256))]
    chunk_mib: u32,
}

/// Production details shown in the reports and written into the manifest.
#[derive(Args)]
struct ProductionArgs {
    #[arg(long, help_heading = "Production")]
    show: Option<String>,
    #[arg(long, help_heading = "Production")]
    episode: Option<String>,
    /// Shoot day.
    #[arg(long, help_heading = "Production")]
    day: Option<String>,
    /// Camera roll.
    #[arg(long, help_heading = "Production")]
    camroll: Option<String>,
    /// DIT / operator name.
    #[arg(long, help_heading = "Production")]
    operator: Option<String>,
    #[arg(long, help_heading = "Production")]
    company: Option<String>,
    #[arg(long, help_heading = "Production")]
    notes: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Copy(args) => copy(*args),
        Cmd::Resume { job } => resume(&job),
        Cmd::Jobs => jobs(),
        Cmd::Verify { dir } => verify(&dir),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(match e {
                Error::Preflight(_) | Error::JobNotFound(_) | Error::AmbiguousJob(_) => {
                    EXIT_REFUSED
                }
                _ => EXIT_INTERNAL,
            })
        }
    }
}

fn parse_dest(i: usize, s: &str) -> (String, PathBuf) {
    if let Some((label, path)) = s.split_once('=')
        && !label.is_empty()
        && !label.contains(['/', '\\'])
    {
        return (label.to_owned(), PathBuf::from(path));
    }
    let path = PathBuf::from(s);
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("Destination {}", i + 1));
    (label, path)
}

fn copy(a: CopyArgs) -> shortend_core::Result<ExitCode> {
    let spec = JobSpec {
        source: a.source,
        card_label: None,
        destinations: a
            .dests
            .iter()
            .enumerate()
            .map(|(i, s)| parse_dest(i, s))
            .collect(),
        folder: a.folder,
        settings: JobSettings {
            hashes: HashList::single(a.hash),
            source_reread: a.reread_source,
            parallel_files: a.parallel,
            chunk_size: a.chunk_mib << 20,
            stall_timeout_secs: a.stall_timeout,
            reports: ReportOptions {
                csv: !a.no_csv,
                txt: !a.no_txt,
                mhl: match a.mhl.as_str() {
                    "v1.1" => Some(MhlVersion::V1_1),
                    "none" => None,
                    _ => Some(MhlVersion::V2),
                },
                pdf: (!a.no_pdf).then_some(PdfOptions { logo: a.logo }),
            },
            ..JobSettings::default()
        },
        production: ProductionInfo {
            show: a.production.show,
            episode: a.production.episode,
            shoot_day: a.production.day,
            camroll: a.production.camroll,
            operator: a.production.operator,
            company: a.production.company,
            notes: a.production.notes,
        },
    };
    let job = shortend_core::plan(spec)?;
    print_plan(&job);
    let job_for_run = job.clone();
    execute(&job, move |tx, cancel| {
        shortend_core::run(&job_for_run, &tx, &cancel)
    })
}

fn resume(id: &str) -> shortend_core::Result<ExitCode> {
    let job = shortend_core::load_job(id)?;
    println!("Resuming job {}", job.id);
    print_plan(&job);
    let id = job.id.to_string();
    execute(&job, move |tx, cancel| {
        shortend_core::resume(&id, &tx, &cancel)
    })
}

fn print_plan(job: &Job) {
    println!("Job {}", job.id);
    println!(
        "  source  {}  ({} files, {})",
        job.source.root.display(),
        job.files.len(),
        human_bytes(job.total_bytes())
    );
    for d in &job.destinations {
        println!(
            "  dest    {}  {}  ({})",
            d.label,
            d.target.display(),
            d.volume.fs_type
        );
    }
    println!(
        "  verify  {} (destination read-back{})",
        job.settings.hashes,
        if job.settings.source_reread {
            " + source re-read"
        } else {
            ""
        }
    );
    for w in &job.warnings {
        println!("  warning: {w}");
    }
    let _ = std::io::stdout().flush();
}

/// Runs the engine on a worker thread and renders its events on this one.
fn execute(
    job: &Job,
    work: impl FnOnce(mpsc::SyncSender<Event>, CancelToken) -> shortend_core::Result<JobReport>
    + Send
    + 'static,
) -> shortend_core::Result<ExitCode> {
    let cancel = CancelToken::new();
    install_ctrlc(cancel.clone());
    let (tx, rx) = mpsc::sync_channel(256);
    let worker_cancel = cancel.clone();
    let handle = std::thread::spawn(move || work(tx, worker_cancel));

    let mut progress = Progress::new(job);
    for ev in rx {
        match ev {
            Event::Started {
                already_verified, ..
            } if already_verified > 0 => {
                progress.line(&format!(
                    "{already_verified} files from an earlier run re-verified against the source checksums; not re-copied"
                ));
            }
            Event::Started { .. } => {}
            Event::Progress {
                source_bytes,
                dest_bytes,
                total_bytes,
            } => {
                progress.update(source_bytes, &dest_bytes, total_bytes);
            }
            Event::FileFinished(o) => {
                for d in o.dests.iter().filter(|d| !d.status.is_ok()) {
                    if let FileStatus::Failed(f) = &d.status {
                        let label = &job.destinations[d.dest.index()].label;
                        progress.line(&format!("FAILED  {}  [{label}]  {f}", o.entry.rel));
                    }
                }
            }
            Event::DestinationFailed { dest, failure } => {
                let label = &job.destinations[dest.index()].label;
                progress.line(&format!(
                    "DESTINATION FAILED  {label}: {failure} — continuing with the others"
                ));
            }
            Event::Warning(w) => progress.line(&format!("warning: {w}")),
            Event::ReportWritten(_) => {}
        }
    }
    progress.clear();
    let report = handle.join().map_err(|_| Error::Io {
        context: "engine".into(),
        source: std::io::Error::other("engine thread panicked"),
    })??;
    Ok(print_summary(&report))
}

fn install_ctrlc(cancel: CancelToken) {
    let _ = ctrlc::set_handler(move || {
        if cancel.is_cancelled() {
            eprintln!("\nstopping immediately");
            std::process::exit(130);
        }
        eprintln!(
            "\ncancelling… (in-flight files are discarded; resume later with `shortend resume`; press Ctrl-C again to quit now)"
        );
        cancel.cancel();
    });
}

fn print_summary(r: &JobReport) -> ExitCode {
    let elapsed = r.elapsed();
    let n = r.job.files.len();
    println!();
    match r.state {
        JobState::Completed => println!(
            "PASS  {n} files verified on {} destinations in {}",
            r.job.destinations.len(),
            human_duration(elapsed)
        ),
        JobState::CompletedWithFailures => println!(
            "FAIL  {} of {n} files not verified on every destination",
            r.failed_files()
        ),
        JobState::Cancelled => println!("CANCELLED  resume with: shortend resume {}", r.job.id),
        JobState::Aborted => println!(
            "STOPPED  the source became unavailable; reconnect it and run: shortend resume {}",
            r.job.id
        ),
    }
    for (d, st) in r.job.destinations.iter().zip(&r.dest_stats) {
        let ok = r
            .outcomes
            .iter()
            .filter(|o| o.dests[d.id.index()].status.is_ok())
            .count();
        println!(
            "  {:<12} {ok}/{n} verified  {} written  {}",
            d.label,
            human_bytes(st.bytes_written),
            human_rate(st.bytes_written, elapsed)
        );
    }
    for issue in &r.manifest_issues {
        println!("  MANIFEST  {issue}");
    }
    for m in &r.manifests {
        println!("  manifest {}", m.display());
    }
    for f in &r.report_files {
        println!("  report  {}", f.display());
    }
    ExitCode::from(match r.state {
        JobState::Completed => 0,
        JobState::CompletedWithFailures | JobState::Aborted => EXIT_FAILED,
        JobState::Cancelled => EXIT_CANCELLED,
    })
}

fn verify(dir: &std::path::Path) -> shortend_core::Result<ExitCode> {
    let reports = match mhl::verify_folder(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::from(EXIT_REFUSED));
        }
    };
    let mut ok = true;
    for r in &reports {
        let kind = match r.kind {
            mhl::ManifestKind::AscMhlV2 => "ASC MHL",
            mhl::ManifestKind::MhlV11 => "MHL v1.1",
        };
        println!("{kind}  {}", r.manifest.display());
        for issue in &r.issues {
            println!("  {issue}");
        }
        println!("  {} verified, {} problem(s)", r.verified, r.issues.len());
        ok &= r.ok();
    }
    println!();
    println!("{}", if ok { "PASS" } else { "FAIL" });
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_FAILED)
    })
}

fn jobs() -> shortend_core::Result<ExitCode> {
    let list = shortend_core::list_jobs()?;
    if list.is_empty() {
        println!("no jobs");
    }
    for j in list {
        let state = j
            .state
            .map_or_else(|| "interrupted (resumable)".to_owned(), |s| s.to_string());
        println!(
            "{}  {}  {}  -> {} destination(s), folder {}  [{state}]",
            j.id,
            j.created
                .to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M"),
            j.source.display(),
            j.destinations,
            j.folder
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Single-line progress on a terminal; a line every few seconds otherwise (logs, CI).
struct Progress {
    tty: bool,
    labels: Vec<String>,
    start: Instant,
    last: Instant,
    drawn: bool,
}

impl Progress {
    fn new(job: &Job) -> Self {
        let now = Instant::now();
        Self {
            tty: std::io::stderr().is_terminal(),
            labels: job.destinations.iter().map(|d| d.label.clone()).collect(),
            start: now,
            last: now,
            drawn: false,
        }
    }

    fn update(&mut self, src: u64, dests: &[u64], total: u64) {
        let every = if self.tty {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        if self.last.elapsed() < every {
            return;
        }
        self.last = Instant::now();
        let pct = if total == 0 {
            100.0
        } else {
            src as f64 * 100.0 / total as f64
        };
        let mut s = format!(
            "{pct:5.1}%  read {} / {}",
            human_bytes(src),
            human_bytes(total)
        );
        for (l, b) in self.labels.iter().zip(dests) {
            s.push_str(&format!("  |  {l} {}", human_bytes(*b)));
        }
        s.push_str(&format!("  |  {}", human_rate(src, self.start.elapsed())));
        if self.tty {
            eprint!("\r\x1b[2K{s}");
            self.drawn = true;
        } else {
            eprintln!("{s}");
        }
    }

    fn line(&mut self, s: &str) {
        self.clear();
        eprintln!("{s}");
    }

    fn clear(&mut self) {
        if self.drawn {
            eprint!("\r\x1b[2K");
            self.drawn = false;
        }
    }
}
