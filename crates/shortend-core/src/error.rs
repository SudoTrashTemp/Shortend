use std::io;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Preflight refused the job. Each entry is one human-readable problem.
    #[error("job refused:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
    Preflight(Vec<String>),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("no job matches '{0}'")]
    JobNotFound(String),

    #[error("'{0}' matches more than one job; give more of the ID")]
    AmbiguousJob(String),

    #[error("job state at {path} is unreadable: {message}")]
    State { path: String, message: String },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn io_at(path: &Path, source: io::Error) -> Self {
        Self::io(path.display().to_string(), source)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
