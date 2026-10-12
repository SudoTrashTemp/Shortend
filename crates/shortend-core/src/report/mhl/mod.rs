//! Media hash lists: ASC MHL v2 (default) and MHL v1.1 (interop).

pub mod ignore;
pub mod v11;
pub mod v2;
mod xml;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hash::HashAlgorithm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MhlVersion {
    #[serde(rename = "v2")]
    V2,
    #[serde(rename = "v1.1")]
    V1_1,
}

impl MhlVersion {
    pub fn supports(self, a: HashAlgorithm) -> bool {
        match self {
            Self::V2 => v2::supports(a),
            Self::V1_1 => v11::supports(a),
        }
    }
}

impl fmt::Display for MhlVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::V2 => "ASC MHL v2",
            Self::V1_1 => "MHL v1.1",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MhlError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unsupported(String),
}

impl MhlError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            context: path.display().to_string(),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    AscMhlV2,
    MhlV11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyIssue {
    Mismatch {
        rel: String,
        recorded: String,
        actual: String,
    },
    Missing {
        rel: String,
    },
    NotInManifest {
        rel: String,
    },
    Unreadable {
        rel: String,
        reason: String,
    },
}

impl fmt::Display for VerifyIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch {
                rel,
                recorded,
                actual,
            } => write!(f, "MISMATCH  {rel}  (manifest {recorded}, file {actual})"),
            Self::Missing { rel } => write!(f, "MISSING   {rel}"),
            Self::NotInManifest { rel } => write!(f, "NEW       {rel}  (not in the manifest)"),
            Self::Unreadable { rel, reason } => write!(f, "ERROR     {rel}  ({reason})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub kind: ManifestKind,
    pub manifest: PathBuf,
    pub verified: usize,
    pub issues: Vec<VerifyIssue>,
}

impl VerifyReport {
    pub(crate) fn new(kind: ManifestKind, manifest: PathBuf) -> Self {
        Self {
            kind,
            manifest,
            verified: 0,
            issues: Vec::new(),
        }
    }

    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Verifies a folder against its manifest: an ASC MHL history in `<dir>/ascmhl`, or else every
/// MHL v1.1 file directly in `<dir>`. Read-only.
pub fn verify_folder(dir: &Path) -> Result<Vec<VerifyReport>, MhlError> {
    if let Some(h) = v2::History::load(dir)? {
        return Ok(vec![v2::verify(dir, &h)?]);
    }
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| MhlError::io(dir, e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mhl") && p.is_file())
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        return Err(MhlError::Invalid(format!(
            "{} has no ASC MHL history (ascmhl/) and no .mhl file",
            dir.display()
        )));
    }
    manifests.iter().map(|m| v11::verify(m)).collect()
}

/// This machine's name, for `<hostname>`.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let u = rustix::system::uname();
        let n = u.nodename().to_string_lossy().into_owned();
        if !n.is_empty() {
            return n;
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

pub fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
