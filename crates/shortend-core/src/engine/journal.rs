//! Append-only JSON Lines journal of a job's progress, used by `resume`.
//!
//! Records are appended with one `write` each and `fdatasync`'d at most once per second and
//! at the end of the job. Losing the tail after a crash is safe: resume re-proves anything a
//! missing record would have claimed (see PLAN.md §4.6).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::fs::RelPath;
use crate::hash::Digests;
use crate::model::{DestId, Failure, JobState};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Start {
        at: Timestamp,
        resume: bool,
    },
    /// Source checksums of a file, recorded once it has been read completely.
    Src {
        rel: RelPath,
        digests: Digests,
    },
    /// The file is verified and in place on `dest`.
    Commit {
        rel: RelPath,
        dest: DestId,
        digests: Digests,
    },
    DestFailed {
        dest: DestId,
        failure: Failure,
    },
    End {
        at: Timestamp,
        state: JobState,
    },
}

const SYNC_EVERY: Duration = Duration::from_secs(1);

pub struct Journal {
    inner: Mutex<(File, Instant)>,
}

impl Journal {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            inner: Mutex::new((file, Instant::now())),
        })
    }

    pub fn append(&self, rec: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(rec).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (file, last_sync) = &mut *guard;
        file.write_all(&line)?;
        if last_sync.elapsed() >= SYNC_EVERY {
            file.sync_data()?;
            *last_sync = Instant::now();
        }
        Ok(())
    }

    pub fn sync(&self) -> std::io::Result<()> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.0.sync_data()
    }
}

/// What a previous run established.
#[derive(Debug, Default)]
pub struct Replay {
    pub src: HashMap<RelPath, Digests>,
    pub commits: HashMap<(RelPath, DestId), Digests>,
    pub last_state: Option<JobState>,
}

/// Reads a journal, skipping lines that don't parse (a torn final write after a crash).
pub fn replay(path: &Path) -> std::io::Result<Replay> {
    let mut out = Replay::default();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for line in BufReader::new(file).split(b'\n') {
        let Ok(rec) = serde_json::from_slice::<Record>(&line?) else {
            continue;
        };
        match rec {
            Record::Start { .. } => out.last_state = None,
            Record::Src { rel, digests } => {
                out.src.insert(rel, digests);
            }
            Record::Commit { rel, dest, digests } => {
                out.commits.insert((rel, dest), digests);
            }
            Record::DestFailed { .. } => {}
            Record::End { state, .. } => out.last_state = Some(state),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{HashAlgorithm, HashList, MultiHasher};

    #[test]
    fn torn_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let j = Journal::open(&path).unwrap();
        let rel = RelPath::new("a/b.mov").unwrap();
        let mut h = MultiHasher::new(&HashList::single(HashAlgorithm::Xxh64));
        h.update(b"x");
        let d = h.finish();
        j.append(&Record::Start {
            at: Timestamp::now(),
            resume: false,
        })
        .unwrap();
        j.append(&Record::Src {
            rel: rel.clone(),
            digests: d.clone(),
        })
        .unwrap();
        j.append(&Record::Commit {
            rel: rel.clone(),
            dest: DestId(0),
            digests: d.clone(),
        })
        .unwrap();
        j.sync().unwrap();
        drop(j);
        // Simulate a crash in the middle of a write.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"t":"commit","rel":"a/b.mov","dest":1,"dig"#)
            .unwrap();

        let r = replay(&path).unwrap();
        assert_eq!(r.src.get(&rel), Some(&d));
        assert_eq!(r.commits.get(&(rel.clone(), DestId(0))), Some(&d));
        assert!(!r.commits.contains_key(&(rel, DestId(1))));
        assert_eq!(r.last_state, None);
    }
}
