//! Media-format detection: by extension, with optional magic-byte and card-folder-signature
//! refinement. Pure and reporting-only — detection never changes what gets copied (an unknown
//! file is copied like any other and reported as `Unknown`).

use crate::fs::relpath::RelPath;

/// A recognized media family. Grouping and the thumbnail/codec matrix key off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    R3d,
    ArriRaw,
    CodexHde,
    Braw,
    XOcn,
    ProRes,
    DNxHR,
    Xavc,
    Mxf,
    CinemaDng,
    Dng,
    CanonCrm,
    CanonCr3,
    Dpx,
    Cine,
    Krw,
    Jpeg,
    Heif,
    Tiff,
    Audio,
    Sidecar,
    Unknown,
}

impl MediaKind {
    /// Short label for reports.
    pub fn label(self) -> &'static str {
        use MediaKind::*;
        match self {
            R3d => "R3D",
            ArriRaw => "ARRIRAW",
            CodexHde => "Codex HDE",
            Braw => "BRAW",
            XOcn => "X-OCN",
            ProRes => "ProRes",
            DNxHR => "DNxHR",
            Xavc => "XAVC",
            Mxf => "MXF",
            CinemaDng => "CinemaDNG",
            Dng => "DNG",
            CanonCrm => "Canon RAW Light",
            CanonCr3 => "Canon CR3",
            Dpx => "DPX",
            Cine => "Phantom CINE",
            Krw => "Kinefinity RAW",
            Jpeg => "JPEG",
            Heif => "HEIF",
            Tiff => "TIFF",
            Audio => "Audio",
            Sidecar => "Sidecar",
            Unknown => "Unknown",
        }
    }

    /// A sidecar attaches to a clip's stem but never forms a clip and is never counted as a frame.
    pub fn is_sidecar(self) -> bool {
        self == MediaKind::Sidecar
    }

    /// Files of this family that share a stem are frames of one sequence (ranges in the report),
    /// rather than spanned container segments.
    pub fn is_frame_sequence(self) -> bool {
        use MediaKind::*;
        matches!(
            self,
            ArriRaw | CodexHde | CinemaDng | Dng | Dpx | Cine | Krw
        )
    }

    /// Whether stock ffmpeg + the `image` crate can produce a thumbnail (best-effort; see
    /// `docs/m5-plan.md` codec matrix). The rest get a media-type badge placeholder.
    pub fn thumbnailable(self) -> bool {
        use MediaKind::*;
        matches!(
            self,
            ProRes | DNxHR | Xavc | Mxf | Dpx | Cine | Jpeg | Heif | Tiff
        )
    }
}

/// Classifies a file from its name plus an optional header. Pure: `header` may be empty when only
/// the name/relpath is available (grouping runs on the source scan without reading bytes).
pub fn classify(rel: &RelPath, _header: &[u8]) -> MediaKind {
    use MediaKind::*;
    let name = rel.file_name();
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "r3d" => R3d,
        "ari" => ArriRaw,
        "arx" => CodexHde,
        "braw" => Braw,
        "ocn" => XOcn,
        "crm" => CanonCrm,
        "cr3" => CanonCr3,
        "cine" => Cine,
        "krw" => Krw,
        "dpx" => Dpx,
        "dng" => {
            if in_dir_cinema(rel) {
                CinemaDng
            } else {
                Dng
            }
        }
        "mov" => MediaKind::ProRes, // QuickTime; ffprobe (slice 2) refines ProRes/DNxHR/H.264
        "mxf" => classify_mxf(rel),
        "mp4" | "mts" | "m2ts" => Xavc,
        "jpg" | "jpeg" => Jpeg,
        "heif" | "heic" => Heif,
        "tif" | "tiff" => Tiff,
        "wav" | "bwf" | "aif" | "aiff" | "mp3" | "flac" => Audio,
        "xml" | "xmp" | "ale" | "cdl" | "ccc" | "cube" | "lut" | "edl" | "txt" | "sidecar" => {
            Sidecar
        }
        _ => {
            // Spanned-segment index/sidecars seen on cards (e.g. CLIP.M00/.M01).
            if ext.len() == 3
                && ext.starts_with('m')
                && ext[1..].chars().all(|c| c.is_ascii_digit())
            {
                Sidecar
            } else {
                Unknown
            }
        }
    }
}

fn in_dir_cinema(rel: &RelPath) -> bool {
    // CinemaDNG stores frames under a per-clip folder; a bare .dng beside others is a still.
    // Look only at parent directory components, never the file's own ".dng" name.
    let comps: Vec<&str> = rel.components().collect();
    comps.split_last().is_some_and(|(_, dirs)| {
        dirs.iter().any(|c| {
            let l = c.to_ascii_lowercase();
            l.ends_with(".dng") || l.contains("cinemadng") || l.ends_with("_dng")
        })
    })
}

fn classify_mxf(rel: &RelPath) -> MediaKind {
    // XDROOT/XDCAM layout → Sony; otherwise a generic MXF (ffprobe refines the codec).
    if rel
        .components()
        .any(|c| c.eq_ignore_ascii_case("XDROOT") || c.eq_ignore_ascii_case("Clip"))
    {
        MediaKind::Xavc
    } else {
        MediaKind::Mxf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(path: &str) -> MediaKind {
        classify(&RelPath::new(path).unwrap(), &[])
    }

    #[test]
    fn classifies_by_extension_and_folder() {
        assert_eq!(k("A001/A001_C001_001.R3D"), MediaKind::R3d);
        assert_eq!(k("A001C001_000001.ari"), MediaKind::ArriRaw);
        assert_eq!(k("A001C001_000001.arx"), MediaKind::CodexHde);
        assert_eq!(k("clip.braw"), MediaKind::Braw);
        assert_eq!(k("X001C001.crm"), MediaKind::CanonCrm);
        assert_eq!(k("IMG_0001.CR3"), MediaKind::CanonCr3);
        assert_eq!(k("shot.mov"), MediaKind::ProRes);
        assert_eq!(k("C0001.MP4"), MediaKind::Xavc);
        assert_eq!(k("STREAM/00001.MTS"), MediaKind::Xavc);
        assert_eq!(k("XDROOT/Clip/C0001.MXF"), MediaKind::Xavc);
        assert_eq!(k("reel/AA0001_01.mxf"), MediaKind::Mxf);
        assert_eq!(k("still.dng"), MediaKind::Dng);
        assert_eq!(k("take_dng/take_0000001.dng"), MediaKind::CinemaDng);
        assert_eq!(k("frame_0000001.dpx"), MediaKind::Dpx);
        assert_eq!(k("shot.cine"), MediaKind::Cine);
        assert_eq!(k("audio/scene1.wav"), MediaKind::Audio);
        assert_eq!(k("A001C001.xml"), MediaKind::Sidecar);
        assert_eq!(k("CLIP.M01"), MediaKind::Sidecar);
        assert_eq!(k("notes.pdf"), MediaKind::Unknown);
    }

    #[test]
    fn capabilities() {
        assert!(MediaKind::ProRes.thumbnailable());
        assert!(!MediaKind::R3d.thumbnailable());
        assert!(MediaKind::Dng.is_frame_sequence());
        assert!(MediaKind::Sidecar.is_sidecar());
    }
}
