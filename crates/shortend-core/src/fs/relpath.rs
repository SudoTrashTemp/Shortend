use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A validated path relative to a job root: UTF-8, `/`-separated, with no empty, `.` or `..`
/// components. Every path the engine writes is built from one of these, so nothing can escape
/// a destination's target folder.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid relative path '{path}': {reason}")]
pub struct RelPathError {
    pub path: String,
    pub reason: &'static str,
}

impl RelPath {
    pub fn new(s: &str) -> Result<Self, RelPathError> {
        let err = |reason| RelPathError {
            path: s.to_owned(),
            reason,
        };
        if s.is_empty() {
            return Err(err("empty"));
        }
        if s.contains('\0') {
            return Err(err("contains a NUL byte"));
        }
        for c in s.split('/') {
            match c {
                "" => return Err(err("empty component or leading/trailing '/'")),
                "." | ".." => return Err(err("'.' and '..' are not allowed")),
                _ => {}
            }
        }
        if cfg!(windows) && (s.contains('\\') || s.contains(':')) {
            return Err(err("'\\' and ':' are not allowed"));
        }
        Ok(Self(s.to_owned()))
    }

    /// Converts a path produced by walking a directory (relative to its root).
    pub fn from_path(rel: &Path) -> Result<Self, RelPathError> {
        let display = rel.to_string_lossy().into_owned();
        let mut parts = Vec::new();
        for comp in rel.components() {
            match comp {
                Component::Normal(os) => match os.to_str() {
                    Some(s) => parts.push(s),
                    None => {
                        return Err(RelPathError {
                            path: display,
                            reason: "name is not valid UTF-8",
                        });
                    }
                },
                _ => {
                    return Err(RelPathError {
                        path: display,
                        reason: "not a plain relative path",
                    });
                }
            }
        }
        Self::new(&parts.join("/"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn to_path(&self, base: &Path) -> PathBuf {
        let mut p = base.to_path_buf();
        for c in self.components() {
            p.push(c);
        }
        p
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(&s)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> Self {
        p.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert!(RelPath::new("A001/CLIP/A001C001.mxf").is_ok());
        assert!(RelPath::new(".DS_Store").is_ok());
        for bad in ["", "/abs", "a//b", "a/", "../x", "a/./b", "a\0b"] {
            assert!(RelPath::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn from_walked_path() {
        let p = Path::new("DCIM").join("100").join("x.jpg");
        assert_eq!(RelPath::from_path(&p).unwrap().as_str(), "DCIM/100/x.jpg");
        assert!(RelPath::from_path(Path::new("../x")).is_err());
    }

    #[test]
    fn to_path_and_name() {
        let r = RelPath::new("a/b/c.mov").unwrap();
        assert_eq!(r.file_name(), "c.mov");
        assert_eq!(
            r.to_path(Path::new("root")),
            Path::new("root").join("a").join("b").join("c.mov")
        );
    }
}
