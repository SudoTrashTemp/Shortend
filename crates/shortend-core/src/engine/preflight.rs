//! Preflight: validate everything that can be validated before a byte is copied, and refuse
//! the job (listing every problem) rather than discover them mid-copy.

use std::path::Path;

use jiff::Timestamp;

use crate::error::{Error, Result};
use crate::fs::names::{case_collisions, component_problem};
use crate::fs::scan::{Scan, scan};
use crate::fs::{FsType, RelPath, VolumeInfo, canonical_dir, platform, same_volume_dir};
use crate::model::{DestId, Destination, Job, JobId, JobSpec, Source};

/// Created at each destination root to hold in-flight `.part` files on the same volume.
pub const STAGING_DIR: &str = ".shortend-staging";
/// Created inside each destination's target folder to hold reports.
pub const REPORT_DIR: &str = "_ShortEnd";

/// Free-space slack on top of block-rounded file sizes: 1 % plus room for reports.
const SPACE_MARGIN_FIXED: u64 = 16 << 20;
/// FAT32 cannot hold files of 4 GiB or more.
const FAT_MAX_FILE: u64 = (4 << 30) - 1;
/// Cap on listed naming problems so a broken card doesn't print 40,000 lines.
const MAX_LISTED: usize = 20;

pub fn plan(mut spec: JobSpec) -> Result<Job> {
    let refuse = |msg: String| Error::Preflight(vec![msg]);
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let id = JobId::new();

    // --- source --------------------------------------------------------------------------
    let src_root = canonical_dir(&spec.source)
        .map_err(|e| refuse(format!("source {}: {e}", spec.source.display())))?;
    let label = spec
        .card_label
        .clone()
        .or_else(|| {
            src_root
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "card".to_owned());
    let folder_text = spec.folder.clone().unwrap_or_else(|| label.clone());
    let folder = RelPath::new(&folder_text).map_err(|e| refuse(format!("folder name: {e}")))?;
    let src_probe = platform::volume_probe(&src_root)
        .map_err(|e| refuse(format!("source {}: {e}", src_root.display())))?;

    let found = scan(&src_root);
    problems.extend(found.problems.iter().cloned());
    if found.files.is_empty() && found.problems.is_empty() {
        problems.push(format!("source {} contains no files", src_root.display()));
    }
    if found
        .files
        .iter()
        .map(|f| &f.rel)
        .chain(&found.dirs)
        .any(|r| r.as_str() == REPORT_DIR)
    {
        problems.push(format!(
            "source contains a top-level '{REPORT_DIR}' entry, which ShortEnd reserves for reports"
        ));
    }

    // --- manifests ---------------------------------------------------------------------------
    check_manifest_options(&src_root, &found, &mut spec, &mut problems, &mut warnings);

    // --- destinations ----------------------------------------------------------------------
    if spec.destinations.is_empty() {
        problems.push("no destinations given".to_owned());
    }
    let mut dests: Vec<Destination> = Vec::new();
    for (i, (dlabel, root)) in spec.destinations.iter().enumerate() {
        let name = if dlabel.is_empty() {
            format!("destination {}", i + 1)
        } else {
            dlabel.clone()
        };
        let root = match canonical_dir(root) {
            Ok(r) => r,
            Err(e) => {
                problems.push(format!("{name} {}: {e}", root.display()));
                continue;
            }
        };
        let target = folder.to_path(&root);
        if root.starts_with(&src_root) || overlaps(&target, &src_root) {
            problems.push(format!(
                "{name} {} overlaps the source {}",
                target.display(),
                src_root.display()
            ));
            continue;
        }
        if let Some(other) = dests.iter().find(|d| overlaps(&d.target, &target)) {
            problems.push(format!(
                "{name} {} overlaps {} {}",
                target.display(),
                other.label,
                other.target.display()
            ));
            continue;
        }
        match check_destination(&name, &root, &target, &id, &found) {
            Ok((volume, mut p)) => {
                if let Some(other) = dests.iter().find(|d| d.volume.dev == volume.dev) {
                    warnings.push(format!(
                        "{name} is on the same volume as {}; both copies share one drive's bandwidth and fail together",
                        other.label
                    ));
                }
                problems.append(&mut p);
                dests.push(Destination {
                    id: DestId(i as u16),
                    label: name,
                    root,
                    target,
                    volume,
                });
            }
            Err(p) => problems.extend(p),
        }
    }

    if !problems.is_empty() {
        return Err(Error::Preflight(problems));
    }
    Ok(Job {
        id,
        created: Timestamp::now(),
        folder,
        source: Source {
            root: src_root,
            label,
            volume: VolumeInfo {
                dev: src_probe.dev,
                fs_uuid: src_probe.fs_uuid,
                fs_type: src_probe.fs_type,
                case_sensitive: true, // not probed: nothing is ever written to the source
                block_size: src_probe.block_size,
                read_only: src_probe.read_only,
            },
        },
        destinations: dests,
        settings: spec.settings,
        production: spec.production,
        warnings,
        files: found.files,
        dirs: found.dirs,
    })
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Checks one destination. Returns its volume info plus any problems, or only problems if it
/// is unusable.
fn check_destination(
    name: &str,
    root: &Path,
    target: &Path,
    id: &JobId,
    found: &Scan,
) -> std::result::Result<(VolumeInfo, Vec<String>), Vec<String>> {
    let mut problems = Vec::new();
    let probe = platform::volume_probe(root)
        .map_err(|e| vec![format!("{name} {}: {e}", root.display())])?;
    if probe.read_only {
        return Err(vec![format!(
            "{name} {} is on a read-only volume",
            root.display()
        )]);
    }

    // Writability and case sensitivity, probed inside our own staging folder.
    let case_sensitive = probe_in_staging(root, id)
        .map_err(|e| vec![format!("{name} {} is not writable: {e}", root.display())])?;

    match std::fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            problems.push(format!(
                "{name}: {} exists and is not a folder",
                target.display()
            ));
        }
        Ok(_) => {
            let non_empty = std::fs::read_dir(target)
                .map(|mut d| d.next().is_some())
                .unwrap_or(true);
            if non_empty {
                problems.push(format!(
                    "{name}: target folder {} already exists and is not empty",
                    target.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => problems.push(format!("{name}: {}: {e}", target.display())),
    }

    // Names that cannot exist on this filesystem.
    let mut naming = Vec::new();
    let folder_rel = target
        .strip_prefix(root)
        .ok()
        .and_then(|p| RelPath::from_path(p).ok());
    for rel in folder_rel
        .iter()
        .chain(&found.dirs)
        .chain(found.files.iter().map(|f| &f.rel))
    {
        for comp in rel.components() {
            if let Some(why) = component_problem(comp, &probe.fs_type) {
                naming.push(format!("{name}: '{rel}': {why}"));
                break;
            }
        }
    }
    if probe.fs_type == FsType::Vfat {
        for f in found.files.iter().filter(|f| f.size > FAT_MAX_FILE) {
            naming.push(format!(
                "{name}: '{}' is 4 GiB or larger, which FAT cannot store",
                f.rel
            ));
        }
    }
    if !case_sensitive {
        let all = found.dirs.iter().chain(found.files.iter().map(|f| &f.rel));
        for (a, b) in case_collisions(all) {
            naming.push(format!(
                "{name} is case-insensitive: '{a}' and '{b}' would collide"
            ));
        }
    }
    let extra = naming.len().saturating_sub(MAX_LISTED);
    problems.extend(naming.into_iter().take(MAX_LISTED));
    if extra > 0 {
        problems.push(format!("{name}: … and {extra} more naming problems"));
    }

    // Space, with every file and folder rounded up to the allocation unit.
    let bs = probe.block_size.max(1);
    let needed: u64 = found
        .files
        .iter()
        .map(|f| f.size.div_ceil(bs) * bs)
        .sum::<u64>()
        + (found.dirs.len() as u64 + 1) * bs;
    let needed = needed + needed / 100 + SPACE_MARGIN_FIXED;
    match platform::free_space(root) {
        Ok(free) if free < needed => problems.push(format!(
            "{name}: not enough space ({} needed, {} free)",
            crate::report::human_bytes(needed),
            crate::report::human_bytes(free)
        )),
        Ok(_) => {}
        Err(e) => problems.push(format!("{name}: cannot read free space: {e}")),
    }

    let volume = VolumeInfo {
        dev: probe.dev,
        fs_uuid: probe.fs_uuid,
        fs_type: probe.fs_type,
        case_sensitive,
        block_size: probe.block_size,
        read_only: false,
    };
    Ok((volume, problems))
}

/// Creates `<root>/.shortend-staging/<job>/`, probes it, and removes it again. Planning a job
/// leaves nothing behind.
fn probe_in_staging(root: &Path, id: &JobId) -> std::io::Result<bool> {
    let staging = root.join(STAGING_DIR);
    let dir = staging.join(id.to_string());
    std::fs::create_dir_all(&dir)?;
    let result = platform::probe_case_sensitive(&dir);
    let _ = std::fs::remove_dir(&dir);
    let _ = std::fs::remove_dir(&staging); // fails harmlessly if other jobs are using it
    result
}

/// Re-checks a stored job before resuming: same source card (same files, sizes and times),
/// same destination volumes. Fails closed with every mismatch listed.
///
/// Volumes are matched by filesystem UUID where one is known, because a drive that was
/// unplugged and reconnected usually comes back under a new device number; the job's
/// recorded device numbers are then updated for the run's per-file checks.
pub fn revalidate(job: &mut Job) -> Result<()> {
    let mut problems = Vec::new();
    let src_ok = reidentify(&job.source.root, &mut job.source.volume);
    if !src_ok {
        problems.push(format!(
            "source {} is not the original card or is not mounted",
            job.source.root.display()
        ));
    } else {
        let now = scan(&job.source.root);
        problems.extend(now.problems);
        if now.files != job.files || now.dirs != job.dirs {
            problems.push(format!(
                "source {} no longer matches the original scan (files added, removed or modified)",
                job.source.root.display()
            ));
        }
    }
    for d in &mut job.destinations {
        if !reidentify(&d.root, &mut d.volume) {
            problems.push(format!(
                "{} {} is offline or a different volume",
                d.label,
                d.root.display()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Preflight(problems))
    }
}

/// True if `path` is a real directory on the recorded volume; refreshes `vol.dev`.
fn reidentify(path: &Path, vol: &mut VolumeInfo) -> bool {
    let is_dir =
        std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
    if !is_dir {
        return false;
    }
    match &vol.fs_uuid {
        None => same_volume_dir(path, vol.dev),
        Some(want) => match platform::volume_probe(path) {
            Ok(p) if p.fs_uuid.as_ref() == Some(want) => {
                vol.dev = p.dev;
                true
            }
            _ => false,
        },
    }
}

/// Checksum/manifest compatibility, and an existing ASC MHL history on the card: its chain must
/// verify, and every algorithm it uses is added to the job so the new generation can verify
/// against it (computed in the same read pass).
fn check_manifest_options(
    src_root: &Path,
    found: &Scan,
    spec: &mut JobSpec,
    problems: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    use crate::hash::{HashAlgorithm, HashList};
    use crate::report::mhl::{MhlVersion, v2};

    let versions = spec.settings.reports.mhl.clone();
    if versions.is_empty() {
        return;
    }
    let hashes = spec.settings.hashes.as_slice().to_vec();
    for &version in &versions {
        let (written, dropped): (Vec<HashAlgorithm>, Vec<HashAlgorithm>) =
            hashes.iter().partition(|a| version.supports(**a));
        if written.is_empty() {
            let names: Vec<String> = hashes.iter().map(ToString::to_string).collect();
            problems.push(format!(
                "{version} cannot record {} (it supports {}); choose another checksum or turn that manifest off",
                names.join("+"),
                match version {
                    MhlVersion::V2 => "c4, md5, sha1, xxh128, xxh3, xxh64",
                    MhlVersion::V1_1 => "md5, sha1, xxh64",
                }
            ));
            continue;
        }
        for a in dropped {
            warnings.push(format!(
                "{a} checksums appear in the reports but cannot be written to {version}"
            ));
        }
    }

    // MHL v1.1 cannot list zero-byte files (its schema requires size >= 1). As the only
    // manifest that would be an incomplete record of the card, so the job is refused.
    if versions.contains(&MhlVersion::V1_1) {
        let empty: Vec<&str> = found
            .files
            .iter()
            .filter(|f| f.size == 0)
            .map(|f| f.rel.as_str())
            .collect();
        if !empty.is_empty() && !versions.contains(&MhlVersion::V2) {
            let shown: Vec<&str> = empty.iter().take(10).copied().collect();
            problems.push(format!(
                "MHL v1.1 cannot list zero-byte files and the card has {} ({}{}); add ASC MHL v2 (--mhl v2,v1.1) so every file is in a manifest, or use --mhl v2",
                empty.len(),
                shown.join(", "),
                if empty.len() > shown.len() { ", …" } else { "" }
            ));
        }
    }

    if !versions.contains(&MhlVersion::V2) {
        return;
    }
    let nested = found
        .dirs
        .iter()
        .filter(|d| d.file_name() == v2::FOLDER && d.as_str() != v2::FOLDER)
        .count();
    if nested > 0 {
        warnings.push(format!(
            "{nested} nested ASC MHL histories on the card are copied as files but not extended"
        ));
    }
    match v2::History::load(src_root) {
        Ok(None) => {}
        Ok(Some(h)) => {
            let mut list = hashes.clone();
            let mut added = Vec::new();
            for a in h.algorithms() {
                if !list.contains(&a) {
                    list.push(a);
                    added.push(a.to_string());
                }
            }
            if !added.is_empty() {
                warnings.push(format!(
                    "the card's ASC MHL history uses {}; also computing it so the new generation can verify against the history",
                    added.join(", ")
                ));
                spec.settings.hashes = HashList::new(list).expect("non-empty, unique");
            }
        }
        Err(e) => problems.push(format!(
            "the card's ASC MHL history cannot be trusted: {e}. Offload without a manifest (--mhl none) or repair the history first"
        )),
    }
}
