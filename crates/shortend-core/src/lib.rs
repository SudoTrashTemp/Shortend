//! ShortEnd engine: verified media offloads from one source to many destinations.
//!
//! The whole engine is usable without any front end:
//!
//! 1. [`plan`] validates a [`JobSpec`] and returns a resolved [`Job`] (or every problem found).
//! 2. [`run`] copies, verifies and commits, sending [`Event`]s, and returns a [`JobReport`].
//! 3. [`resume`] continues an interrupted job, skipping files already verified.

pub mod engine;
mod error;
pub mod fs;
pub mod hash;
pub mod media;
pub mod model;
pub mod naming;
pub mod preset;
pub mod report;

pub use engine::{JobListing, list_jobs, load_job, plan, resume, run};
pub use error::{Error, Result};
pub use fs::{FsType, RelPath, VolumeInfo};
pub use hash::{Digest, Digests, HashAlgorithm, HashList};
pub use model::*;
