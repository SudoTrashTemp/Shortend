//! Format awareness (M5): detection and clip grouping (pure, reporting-only), plus — in later
//! slices — ffprobe metadata and ffmpeg thumbnails behind a typed command boundary. See
//! `docs/m5-plan.md`. Grouping runs on the source scan; metadata/thumbnails on the verified
//! destination copy; only the PDF rolls up to clips.

pub mod clip;
pub mod detect;

pub use clip::{Clip, ClipSet, FrameRange, group};
pub use detect::{MediaKind, classify};
