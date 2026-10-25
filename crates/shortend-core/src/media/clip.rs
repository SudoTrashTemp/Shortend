//! Clip grouping. Pure and reporting-only: it runs on the **source scan** (relpaths, no byte
//! reads), so spanned segments and frame sequences collapse to clips in the report while the copy
//! engine still handles every file individually. Grouping is heuristic — it gets the common camera
//! layouts right and falls back to one-clip-per-file for anything unrecognized, never a refusal.

use super::detect::{MediaKind, classify};
use crate::fs::names::fold;
use crate::fs::relpath::RelPath;
use crate::model::FileEntry;

/// An inclusive run of frame numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub first: u64,
    pub last: u64,
}

/// A group of files the report treats as one clip.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Display stem (original casing of the first member, trailing separators trimmed).
    pub stem: String,
    pub kind: MediaKind,
    /// Media files in this clip (frames or spanned segments), in scan order. Never empty.
    pub members: Vec<RelPath>,
    /// Sidecars attached to this clip's stem (not counted as frames).
    pub sidecars: Vec<RelPath>,
    /// Frame-number ranges, gaps shown as separate ranges. Empty for single files / spanned parts.
    pub ranges: Vec<FrameRange>,
}

impl Clip {
    /// A frame sequence (ranges meaningful) rather than a single file or spanned container.
    pub fn is_sequence(&self) -> bool {
        !self.ranges.is_empty()
    }
    pub fn member_count(&self) -> usize {
        self.members.len()
    }
}

/// The grouping of a scan's files.
#[derive(Debug, Clone, Default)]
pub struct ClipSet {
    pub clips: Vec<Clip>,
    /// Sidecars that matched no clip stem; still copied, just not attached.
    pub unattached_sidecars: Vec<RelPath>,
}

/// Groups files (from `scan.files`) into clips. Order of first appearance is preserved.
pub fn group(files: &[FileEntry]) -> ClipSet {
    // Build media clips keyed by (folded grouping-stem, kind); collect sidecars separately.
    let mut keys: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut clips: Vec<ClipBuild> = Vec::new();
    let mut sidecars: Vec<RelPath> = Vec::new();

    for f in files {
        let rel = &f.rel;
        let kind = classify(rel, &[]);
        if kind.is_sidecar() {
            sidecars.push(rel.clone());
            continue;
        }
        let g = grouping(rel, kind);
        let key = format!("{:?}|{}", kind, fold(&g.key));
        let idx = *by_key.entry(key.clone()).or_insert_with(|| {
            keys.push(key);
            clips.push(ClipBuild {
                stem: g.display.clone(),
                kind,
                members: Vec::new(),
                numbers: Vec::new(),
            });
            clips.len() - 1
        });
        clips[idx].members.push(rel.clone());
        if let Some(n) = g.number {
            clips[idx].numbers.push(n);
        }
    }

    let mut out: Vec<Clip> = clips
        .into_iter()
        .map(|b| {
            // Frame ranges only when this kind is a frame sequence and we parsed numbers.
            let ranges = if b.kind.is_frame_sequence()
                && b.members.len() == b.numbers.len()
                && !b.numbers.is_empty()
            {
                ranges_from(&b.numbers)
            } else {
                Vec::new()
            };
            Clip {
                stem: b.stem,
                kind: b.kind,
                members: b.members,
                sidecars: Vec::new(),
                ranges,
            }
        })
        .collect();

    // Attach sidecars to the clip whose folded stem is the longest prefix of the sidecar's stem.
    let mut unattached = Vec::new();
    for sc in sidecars {
        let scfold = fold(stem_of(&sc));
        let mut best: Option<(usize, usize)> = None; // (clip index, matched length)
        for (i, c) in out.iter().enumerate() {
            let cf = fold(&c.stem);
            if !cf.is_empty()
                && scfold.starts_with(&cf)
                && best.is_none_or(|(_, len)| cf.len() > len)
            {
                best = Some((i, cf.len()));
            }
        }
        match best {
            Some((i, _)) => out[i].sidecars.push(sc),
            None => unattached.push(sc),
        }
    }

    ClipSet {
        clips: out,
        unattached_sidecars: unattached,
    }
}

struct ClipBuild {
    stem: String,
    kind: MediaKind,
    members: Vec<RelPath>,
    numbers: Vec<u64>,
}

struct Grouping {
    /// Grouping key stem (folded by the caller).
    key: String,
    /// Stem shown in the report.
    display: String,
    /// Frame/segment number, when this file is part of a sequence or spanned clip.
    number: Option<u64>,
}

/// Decides how a single file participates in grouping, from its name and kind.
fn grouping(rel: &RelPath, kind: MediaKind) -> Grouping {
    let stem = stem_of(rel);
    // Families that are always one-clip-per-file (stills, audio, single containers, unknown).
    let always_single = matches!(
        kind,
        MediaKind::Audio
            | MediaKind::Jpeg
            | MediaKind::Heif
            | MediaKind::Tiff
            | MediaKind::CanonCr3
            | MediaKind::Braw
            | MediaKind::ProRes
            | MediaKind::XOcn
            | MediaKind::Unknown
    );
    if always_single {
        return Grouping {
            key: stem.to_string(),
            display: stem.to_string(),
            number: None,
        };
    }
    let (prefix, digits, sep_before) = split_trailing_number(stem);
    if kind.is_frame_sequence() {
        // A long, zero-padded trailing number is a frame index; short numbers are not sequences.
        if digits.len() >= 4 {
            let key = trim_sep(prefix);
            return Grouping {
                key: key.to_string(),
                display: key.to_string(),
                number: digits.parse().ok(),
            };
        }
        return Grouping {
            key: stem.to_string(),
            display: stem.to_string(),
            number: None,
        };
    }
    // Spanned containers (R3D, MXF, XAVC, Canon CRM): a separator + short number is a part suffix.
    if sep_before && !digits.is_empty() && digits.len() <= 3 {
        let key = trim_sep(prefix);
        return Grouping {
            key: key.to_string(),
            display: key.to_string(),
            number: digits.parse().ok(),
        };
    }
    Grouping {
        key: stem.to_string(),
        display: stem.to_string(),
        number: None,
    }
}

/// Filename without its final extension.
fn stem_of(rel: &RelPath) -> &str {
    let name = rel.file_name();
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Splits off the longest trailing run of ASCII digits: ("A001_C001_", "0000123", sep_before=true).
fn split_trailing_number(stem: &str) -> (&str, &str, bool) {
    let bytes = stem.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    let prefix = &stem[..i];
    let digits = &stem[i..];
    let sep_before = i > 0 && matches!(bytes[i - 1], b'_' | b'-' | b'.');
    (prefix, digits, sep_before)
}

fn trim_sep(s: &str) -> &str {
    s.trim_end_matches(['_', '-', '.'])
}

/// Merges sorted-unique numbers into inclusive ranges; gaps split into separate ranges.
fn ranges_from(numbers: &[u64]) -> Vec<FrameRange> {
    let mut nums: Vec<u64> = numbers.to_vec();
    nums.sort_unstable();
    nums.dedup();
    let mut out = Vec::new();
    let mut iter = nums.into_iter();
    let Some(first) = iter.next() else { return out };
    let (mut lo, mut hi) = (first, first);
    for n in iter {
        if n == hi + 1 {
            hi = n;
        } else {
            out.push(FrameRange {
                first: lo,
                last: hi,
            });
            lo = n;
            hi = n;
        }
    }
    out.push(FrameRange {
        first: lo,
        last: hi,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe(path: &str) -> FileEntry {
        FileEntry {
            rel: RelPath::new(path).unwrap(),
            size: 1,
            mtime: jiff::Timestamp::UNIX_EPOCH,
        }
    }
    fn files(paths: &[&str]) -> Vec<FileEntry> {
        paths.iter().map(|p| fe(p)).collect()
    }

    #[test]
    fn r3d_spanned_groups() {
        let s = group(&files(&[
            "A001/A001_C001_001.R3D",
            "A001/A001_C001_002.R3D",
            "A001/A001_C001_003.R3D",
        ]));
        assert_eq!(s.clips.len(), 1);
        assert_eq!(s.clips[0].kind, MediaKind::R3d);
        assert_eq!(s.clips[0].member_count(), 3);
        assert!(
            !s.clips[0].is_sequence(),
            "spanned parts are not a frame sequence"
        );
    }

    #[test]
    fn dng_sequence_ranges_and_gaps() {
        let mut p: Vec<String> = (0..3).map(|n| format!("take/take_{n:07}.dng")).collect();
        p.push("take/take_0000010.dng".into());
        p.push("take/take_0000011.dng".into());
        let refs: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
        let s = group(&files(&refs));
        assert_eq!(s.clips.len(), 1);
        assert!(s.clips[0].is_sequence());
        assert_eq!(
            s.clips[0].ranges,
            vec![
                FrameRange { first: 0, last: 2 },
                FrameRange {
                    first: 10,
                    last: 11
                }
            ]
        );
        assert_eq!(s.clips[0].member_count(), 5);
    }

    #[test]
    fn stills_audio_and_single_containers_are_one_per_file() {
        assert_eq!(
            group(&files(&["DCIM/IMG_0001.JPG", "DCIM/IMG_0002.JPG"]))
                .clips
                .len(),
            2
        );
        assert_eq!(
            group(&files(&["SND/A001.wav", "SND/A002.wav"])).clips.len(),
            2
        );
        assert_eq!(
            group(&files(&["A001C001.mov", "A001C002.mov"])).clips.len(),
            2
        );
        assert_eq!(group(&files(&["x.braw", "y.braw"])).clips.len(), 2);
    }

    #[test]
    fn sidecars_attach_never_form_clips() {
        let s = group(&files(&[
            "A001C001.mov",
            "A001C001.xml",
            "A001C001.ale",
            "loose.cdl",
        ]));
        assert_eq!(s.clips.len(), 1, "only the .mov is a clip");
        assert_eq!(s.clips[0].sidecars.len(), 2);
        assert_eq!(s.unattached_sidecars.len(), 1);
    }

    #[test]
    fn grouping_is_case_insensitive() {
        let s = group(&files(&["A001_C001_001.R3D", "a001_c001_002.r3d"]));
        assert_eq!(s.clips.len(), 1, "case must not split a clip");
        assert_eq!(s.clips[0].member_count(), 2);
    }

    #[test]
    fn unknown_passes_through_one_per_file() {
        let s = group(&files(&["weird.bin", "also.bin"]));
        assert_eq!(s.clips.len(), 2);
        assert!(s.clips.iter().all(|c| c.kind == MediaKind::Unknown));
    }
}
