//! ASC MHL ignore patterns. The reference implementation uses gitignore ("gitwildmatch")
//! semantics; this implements the subset that appears in practice:
//!
//! - a pattern with no `/` except a trailing one matches a name at any depth;
//! - a pattern containing `/` elsewhere is anchored at the history root (a leading `/` is
//!   dropped);
//! - a trailing `/` matches directories only;
//! - `*` and `?` never cross `/`; `**` crosses any number of levels; `[...]` and `[!...]`
//!   are character classes; `\` escapes the next character.
//!
//! Negation (`!pattern`) is not supported: such patterns are kept in the manifest but never
//! match, which can only make ShortEnd record more files, never fewer.

/// ShortEnd's report folder, ignored in every manifest it writes.
pub const SHORTEND_PATTERN: &str = "_ShortEnd/";

/// Defaults of the reference implementation for a new history.
pub fn default_patterns() -> Vec<String> {
    [".DS_Store", "ascmhl", "ascmhl/"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone)]
pub struct IgnoreSpec {
    patterns: Vec<String>,
}

impl IgnoreSpec {
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// True if `rel` (a `/`-separated path relative to the history root) or any of its parent
    /// folders is ignored. `is_dir` says whether `rel` itself is a directory.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        (0..comps.len()).any(|i| {
            let dir = i + 1 < comps.len() || is_dir;
            self.matches_one(&comps[..=i], dir)
        })
    }

    fn matches_one(&self, comps: &[&str], is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|p| pattern_matches(p, comps, is_dir))
    }
}

fn pattern_matches(pattern: &str, comps: &[&str], is_dir: bool) -> bool {
    let p = pattern.trim_end_matches(' ');
    if p.is_empty() || p.starts_with('#') || p.starts_with('!') {
        return false;
    }
    let (p, dir_only) = match p.strip_suffix('/') {
        Some(rest) => (rest, true),
        None => (p, false),
    };
    if dir_only && !is_dir {
        return false;
    }
    if p.contains('/') {
        let p = p.strip_prefix('/').unwrap_or(p);
        let pat: Vec<&str> = p.split('/').collect();
        match_segments(&pat, comps)
    } else {
        comps
            .last()
            .is_some_and(|name| glob(p.as_bytes(), name.as_bytes()))
    }
}

/// Matches pattern segments against path components; `**` matches zero or more components.
fn match_segments(pat: &[&str], comps: &[&str]) -> bool {
    match pat.split_first() {
        None => comps.is_empty(),
        Some((&"**", rest)) => (0..=comps.len()).any(|k| match_segments(rest, &comps[k..])),
        Some((seg, rest)) => comps.split_first().is_some_and(|(c, crest)| {
            glob(seg.as_bytes(), c.as_bytes()) && match_segments(rest, crest)
        }),
    }
}

/// Glob within one path component: `*`, `?`, `[...]`, `\x`.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => {
            let rest = &p[1..];
            (0..=s.len()).any(|k| glob(rest, &s[k..]))
        }
        Some(b'?') => !s.is_empty() && glob(&p[1..], &s[char_len(s)..]),
        Some(b'[') => match class(&p[1..], s) {
            Some((matched, used, n)) => matched && glob(&p[1 + used..], &s[n..]),
            None => s.first() == Some(&b'[') && glob(&p[1..], &s[1..]),
        },
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob(&p[2..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && glob(&p[1..], &s[1..]),
    }
}

fn char_len(s: &[u8]) -> usize {
    std::str::from_utf8(s)
        .ok()
        .and_then(|t| t.chars().next())
        .map_or(1, char::len_utf8)
}

/// Parses a character class after `[`; returns (matched, pattern bytes used including `]`,
/// subject bytes consumed). `None` if the class is unterminated or the subject is empty.
fn class(p: &[u8], s: &[u8]) -> Option<(bool, usize, usize)> {
    let c = s.first()?;
    let (negate, start) = match p.first() {
        Some(b'!' | b'^') => (true, 1),
        _ => (false, 0),
    };
    let mut i = start;
    let mut hit = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((hit != negate, i + 1, 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            hit |= (p[i]..=p[i + 2]).contains(c);
            i += 3;
        } else {
            hit |= p[i] == *c;
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(p: &[&str]) -> IgnoreSpec {
        IgnoreSpec::new(p.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn defaults_and_report_folder() {
        let mut p = default_patterns();
        p.push(SHORTEND_PATTERN.into());
        let s = IgnoreSpec::new(p);
        assert!(s.is_ignored(".DS_Store", false));
        assert!(s.is_ignored("CLIP/.DS_Store", false));
        assert!(s.is_ignored("ascmhl", true));
        assert!(s.is_ignored("ascmhl/0001_A_2020-01-01_000000Z.mhl", false));
        assert!(s.is_ignored("sub/ascmhl/x.mhl", false));
        assert!(s.is_ignored("_ShortEnd/report.pdf", false));
        assert!(
            !s.is_ignored("_ShortEnd", false),
            "file named like the folder"
        );
        assert!(!s.is_ignored("CLIP/A001.mov", false));
        assert!(!s.is_ignored("ascmhl.txt", false));
    }

    #[test]
    fn globs_and_anchoring() {
        let s = spec(&[
            "*.tmp",
            "/cache",
            "logs/**/*.log",
            "frame_0[0-4]?.dng",
            "[!a]bc",
        ]);
        assert!(s.is_ignored("a/b/x.tmp", false));
        assert!(!s.is_ignored("a/b/x.tmpl", false));
        assert!(s.is_ignored("cache", true));
        assert!(s.is_ignored("cache/x", false));
        assert!(!s.is_ignored("a/cache", true));
        assert!(s.is_ignored("logs/x.log", false));
        assert!(s.is_ignored("logs/a/b/x.log", false));
        assert!(!s.is_ignored("x/logs/x.log", false));
        assert!(s.is_ignored("frame_031.dng", false));
        assert!(!s.is_ignored("frame_051.dng", false));
        assert!(s.is_ignored("xbc", false));
        assert!(!s.is_ignored("abc", false));
    }

    #[test]
    fn negation_never_matches() {
        assert!(!spec(&["!keep.mov"]).is_ignored("keep.mov", false));
    }

    /// Expected values generated with Python `pathspec` ("gitwildmatch"), which the reference
    /// implementation uses.
    #[test]
    fn agrees_with_pathspec() {
        let table: &[(&str, &str, bool, bool)] = &[
            (".DS_Store", ".DS_Store", false, true),
            (".DS_Store", "CLIP/.DS_Store", false, true),
            (".DS_Store", "ascmhl", true, false),
            (".DS_Store", "ascmhl/x.mhl", false, false),
            (".DS_Store", "sub/ascmhl/x.mhl", false, false),
            (".DS_Store", "_ShortEnd/r.pdf", false, false),
            (".DS_Store", "_ShortEnd", false, false),
            (".DS_Store", "a/b/x.tmp", false, false),
            (".DS_Store", "cache", true, false),
            (".DS_Store", "cache/x", false, false),
            (".DS_Store", "a/cache", true, false),
            (".DS_Store", "logs/x.log", false, false),
            (".DS_Store", "logs/a/b/x.log", false, false),
            (".DS_Store", "x/logs/x.log", false, false),
            (".DS_Store", "frame_031.dng", false, false),
            (".DS_Store", "frame_051.dng", false, false),
            (".DS_Store", "xbc", false, false),
            (".DS_Store", "abc", false, false),
            (".DS_Store", "CLIP/A.xml", false, false),
            (".DS_Store", "CLIP/sub/A.xml", false, false),
            (".DS_Store", "x/CLIP/A.xml", false, false),
            (".DS_Store", "A001.R3D.lock", false, false),
            ("ascmhl", ".DS_Store", false, false),
            ("ascmhl", "CLIP/.DS_Store", false, false),
            ("ascmhl", "ascmhl", true, true),
            ("ascmhl", "ascmhl/x.mhl", false, true),
            ("ascmhl", "sub/ascmhl/x.mhl", false, true),
            ("ascmhl", "_ShortEnd/r.pdf", false, false),
            ("ascmhl", "_ShortEnd", false, false),
            ("ascmhl", "a/b/x.tmp", false, false),
            ("ascmhl", "cache", true, false),
            ("ascmhl", "cache/x", false, false),
            ("ascmhl", "a/cache", true, false),
            ("ascmhl", "logs/x.log", false, false),
            ("ascmhl", "logs/a/b/x.log", false, false),
            ("ascmhl", "x/logs/x.log", false, false),
            ("ascmhl", "frame_031.dng", false, false),
            ("ascmhl", "frame_051.dng", false, false),
            ("ascmhl", "xbc", false, false),
            ("ascmhl", "abc", false, false),
            ("ascmhl", "CLIP/A.xml", false, false),
            ("ascmhl", "CLIP/sub/A.xml", false, false),
            ("ascmhl", "x/CLIP/A.xml", false, false),
            ("ascmhl", "A001.R3D.lock", false, false),
            ("ascmhl/", ".DS_Store", false, false),
            ("ascmhl/", "CLIP/.DS_Store", false, false),
            ("ascmhl/", "ascmhl", true, true),
            ("ascmhl/", "ascmhl/x.mhl", false, true),
            ("ascmhl/", "sub/ascmhl/x.mhl", false, true),
            ("ascmhl/", "_ShortEnd/r.pdf", false, false),
            ("ascmhl/", "_ShortEnd", false, false),
            ("ascmhl/", "a/b/x.tmp", false, false),
            ("ascmhl/", "cache", true, false),
            ("ascmhl/", "cache/x", false, false),
            ("ascmhl/", "a/cache", true, false),
            ("ascmhl/", "logs/x.log", false, false),
            ("ascmhl/", "logs/a/b/x.log", false, false),
            ("ascmhl/", "x/logs/x.log", false, false),
            ("ascmhl/", "frame_031.dng", false, false),
            ("ascmhl/", "frame_051.dng", false, false),
            ("ascmhl/", "xbc", false, false),
            ("ascmhl/", "abc", false, false),
            ("ascmhl/", "CLIP/A.xml", false, false),
            ("ascmhl/", "CLIP/sub/A.xml", false, false),
            ("ascmhl/", "x/CLIP/A.xml", false, false),
            ("ascmhl/", "A001.R3D.lock", false, false),
            ("_ShortEnd/", ".DS_Store", false, false),
            ("_ShortEnd/", "CLIP/.DS_Store", false, false),
            ("_ShortEnd/", "ascmhl", true, false),
            ("_ShortEnd/", "ascmhl/x.mhl", false, false),
            ("_ShortEnd/", "sub/ascmhl/x.mhl", false, false),
            ("_ShortEnd/", "_ShortEnd/r.pdf", false, true),
            ("_ShortEnd/", "_ShortEnd", false, false),
            ("_ShortEnd/", "a/b/x.tmp", false, false),
            ("_ShortEnd/", "cache", true, false),
            ("_ShortEnd/", "cache/x", false, false),
            ("_ShortEnd/", "a/cache", true, false),
            ("_ShortEnd/", "logs/x.log", false, false),
            ("_ShortEnd/", "logs/a/b/x.log", false, false),
            ("_ShortEnd/", "x/logs/x.log", false, false),
            ("_ShortEnd/", "frame_031.dng", false, false),
            ("_ShortEnd/", "frame_051.dng", false, false),
            ("_ShortEnd/", "xbc", false, false),
            ("_ShortEnd/", "abc", false, false),
            ("_ShortEnd/", "CLIP/A.xml", false, false),
            ("_ShortEnd/", "CLIP/sub/A.xml", false, false),
            ("_ShortEnd/", "x/CLIP/A.xml", false, false),
            ("_ShortEnd/", "A001.R3D.lock", false, false),
            ("*.tmp", ".DS_Store", false, false),
            ("*.tmp", "CLIP/.DS_Store", false, false),
            ("*.tmp", "ascmhl", true, false),
            ("*.tmp", "ascmhl/x.mhl", false, false),
            ("*.tmp", "sub/ascmhl/x.mhl", false, false),
            ("*.tmp", "_ShortEnd/r.pdf", false, false),
            ("*.tmp", "_ShortEnd", false, false),
            ("*.tmp", "a/b/x.tmp", false, true),
            ("*.tmp", "cache", true, false),
            ("*.tmp", "cache/x", false, false),
            ("*.tmp", "a/cache", true, false),
            ("*.tmp", "logs/x.log", false, false),
            ("*.tmp", "logs/a/b/x.log", false, false),
            ("*.tmp", "x/logs/x.log", false, false),
            ("*.tmp", "frame_031.dng", false, false),
            ("*.tmp", "frame_051.dng", false, false),
            ("*.tmp", "xbc", false, false),
            ("*.tmp", "abc", false, false),
            ("*.tmp", "CLIP/A.xml", false, false),
            ("*.tmp", "CLIP/sub/A.xml", false, false),
            ("*.tmp", "x/CLIP/A.xml", false, false),
            ("*.tmp", "A001.R3D.lock", false, false),
            ("/cache", ".DS_Store", false, false),
            ("/cache", "CLIP/.DS_Store", false, false),
            ("/cache", "ascmhl", true, false),
            ("/cache", "ascmhl/x.mhl", false, false),
            ("/cache", "sub/ascmhl/x.mhl", false, false),
            ("/cache", "_ShortEnd/r.pdf", false, false),
            ("/cache", "_ShortEnd", false, false),
            ("/cache", "a/b/x.tmp", false, false),
            ("/cache", "cache", true, true),
            ("/cache", "cache/x", false, true),
            ("/cache", "a/cache", true, false),
            ("/cache", "logs/x.log", false, false),
            ("/cache", "logs/a/b/x.log", false, false),
            ("/cache", "x/logs/x.log", false, false),
            ("/cache", "frame_031.dng", false, false),
            ("/cache", "frame_051.dng", false, false),
            ("/cache", "xbc", false, false),
            ("/cache", "abc", false, false),
            ("/cache", "CLIP/A.xml", false, false),
            ("/cache", "CLIP/sub/A.xml", false, false),
            ("/cache", "x/CLIP/A.xml", false, false),
            ("/cache", "A001.R3D.lock", false, false),
            ("logs/**/*.log", ".DS_Store", false, false),
            ("logs/**/*.log", "CLIP/.DS_Store", false, false),
            ("logs/**/*.log", "ascmhl", true, false),
            ("logs/**/*.log", "ascmhl/x.mhl", false, false),
            ("logs/**/*.log", "sub/ascmhl/x.mhl", false, false),
            ("logs/**/*.log", "_ShortEnd/r.pdf", false, false),
            ("logs/**/*.log", "_ShortEnd", false, false),
            ("logs/**/*.log", "a/b/x.tmp", false, false),
            ("logs/**/*.log", "cache", true, false),
            ("logs/**/*.log", "cache/x", false, false),
            ("logs/**/*.log", "a/cache", true, false),
            ("logs/**/*.log", "logs/x.log", false, true),
            ("logs/**/*.log", "logs/a/b/x.log", false, true),
            ("logs/**/*.log", "x/logs/x.log", false, false),
            ("logs/**/*.log", "frame_031.dng", false, false),
            ("logs/**/*.log", "frame_051.dng", false, false),
            ("logs/**/*.log", "xbc", false, false),
            ("logs/**/*.log", "abc", false, false),
            ("logs/**/*.log", "CLIP/A.xml", false, false),
            ("logs/**/*.log", "CLIP/sub/A.xml", false, false),
            ("logs/**/*.log", "x/CLIP/A.xml", false, false),
            ("logs/**/*.log", "A001.R3D.lock", false, false),
            ("frame_0[0-4]?.dng", ".DS_Store", false, false),
            ("frame_0[0-4]?.dng", "CLIP/.DS_Store", false, false),
            ("frame_0[0-4]?.dng", "ascmhl", true, false),
            ("frame_0[0-4]?.dng", "ascmhl/x.mhl", false, false),
            ("frame_0[0-4]?.dng", "sub/ascmhl/x.mhl", false, false),
            ("frame_0[0-4]?.dng", "_ShortEnd/r.pdf", false, false),
            ("frame_0[0-4]?.dng", "_ShortEnd", false, false),
            ("frame_0[0-4]?.dng", "a/b/x.tmp", false, false),
            ("frame_0[0-4]?.dng", "cache", true, false),
            ("frame_0[0-4]?.dng", "cache/x", false, false),
            ("frame_0[0-4]?.dng", "a/cache", true, false),
            ("frame_0[0-4]?.dng", "logs/x.log", false, false),
            ("frame_0[0-4]?.dng", "logs/a/b/x.log", false, false),
            ("frame_0[0-4]?.dng", "x/logs/x.log", false, false),
            ("frame_0[0-4]?.dng", "frame_031.dng", false, true),
            ("frame_0[0-4]?.dng", "frame_051.dng", false, false),
            ("frame_0[0-4]?.dng", "xbc", false, false),
            ("frame_0[0-4]?.dng", "abc", false, false),
            ("frame_0[0-4]?.dng", "CLIP/A.xml", false, false),
            ("frame_0[0-4]?.dng", "CLIP/sub/A.xml", false, false),
            ("frame_0[0-4]?.dng", "x/CLIP/A.xml", false, false),
            ("frame_0[0-4]?.dng", "A001.R3D.lock", false, false),
            ("[!a]bc", ".DS_Store", false, false),
            ("[!a]bc", "CLIP/.DS_Store", false, false),
            ("[!a]bc", "ascmhl", true, false),
            ("[!a]bc", "ascmhl/x.mhl", false, false),
            ("[!a]bc", "sub/ascmhl/x.mhl", false, false),
            ("[!a]bc", "_ShortEnd/r.pdf", false, false),
            ("[!a]bc", "_ShortEnd", false, false),
            ("[!a]bc", "a/b/x.tmp", false, false),
            ("[!a]bc", "cache", true, false),
            ("[!a]bc", "cache/x", false, false),
            ("[!a]bc", "a/cache", true, false),
            ("[!a]bc", "logs/x.log", false, false),
            ("[!a]bc", "logs/a/b/x.log", false, false),
            ("[!a]bc", "x/logs/x.log", false, false),
            ("[!a]bc", "frame_031.dng", false, false),
            ("[!a]bc", "frame_051.dng", false, false),
            ("[!a]bc", "xbc", false, true),
            ("[!a]bc", "abc", false, false),
            ("[!a]bc", "CLIP/A.xml", false, false),
            ("[!a]bc", "CLIP/sub/A.xml", false, false),
            ("[!a]bc", "x/CLIP/A.xml", false, false),
            ("[!a]bc", "A001.R3D.lock", false, false),
            ("Thumbs.db", ".DS_Store", false, false),
            ("Thumbs.db", "CLIP/.DS_Store", false, false),
            ("Thumbs.db", "ascmhl", true, false),
            ("Thumbs.db", "ascmhl/x.mhl", false, false),
            ("Thumbs.db", "sub/ascmhl/x.mhl", false, false),
            ("Thumbs.db", "_ShortEnd/r.pdf", false, false),
            ("Thumbs.db", "_ShortEnd", false, false),
            ("Thumbs.db", "a/b/x.tmp", false, false),
            ("Thumbs.db", "cache", true, false),
            ("Thumbs.db", "cache/x", false, false),
            ("Thumbs.db", "a/cache", true, false),
            ("Thumbs.db", "logs/x.log", false, false),
            ("Thumbs.db", "logs/a/b/x.log", false, false),
            ("Thumbs.db", "x/logs/x.log", false, false),
            ("Thumbs.db", "frame_031.dng", false, false),
            ("Thumbs.db", "frame_051.dng", false, false),
            ("Thumbs.db", "xbc", false, false),
            ("Thumbs.db", "abc", false, false),
            ("Thumbs.db", "CLIP/A.xml", false, false),
            ("Thumbs.db", "CLIP/sub/A.xml", false, false),
            ("Thumbs.db", "x/CLIP/A.xml", false, false),
            ("Thumbs.db", "A001.R3D.lock", false, false),
            ("*.R3D.lock", ".DS_Store", false, false),
            ("*.R3D.lock", "CLIP/.DS_Store", false, false),
            ("*.R3D.lock", "ascmhl", true, false),
            ("*.R3D.lock", "ascmhl/x.mhl", false, false),
            ("*.R3D.lock", "sub/ascmhl/x.mhl", false, false),
            ("*.R3D.lock", "_ShortEnd/r.pdf", false, false),
            ("*.R3D.lock", "_ShortEnd", false, false),
            ("*.R3D.lock", "a/b/x.tmp", false, false),
            ("*.R3D.lock", "cache", true, false),
            ("*.R3D.lock", "cache/x", false, false),
            ("*.R3D.lock", "a/cache", true, false),
            ("*.R3D.lock", "logs/x.log", false, false),
            ("*.R3D.lock", "logs/a/b/x.log", false, false),
            ("*.R3D.lock", "x/logs/x.log", false, false),
            ("*.R3D.lock", "frame_031.dng", false, false),
            ("*.R3D.lock", "frame_051.dng", false, false),
            ("*.R3D.lock", "xbc", false, false),
            ("*.R3D.lock", "abc", false, false),
            ("*.R3D.lock", "CLIP/A.xml", false, false),
            ("*.R3D.lock", "CLIP/sub/A.xml", false, false),
            ("*.R3D.lock", "x/CLIP/A.xml", false, false),
            ("*.R3D.lock", "A001.R3D.lock", false, true),
            ("CLIP/*.xml", ".DS_Store", false, false),
            ("CLIP/*.xml", "CLIP/.DS_Store", false, false),
            ("CLIP/*.xml", "ascmhl", true, false),
            ("CLIP/*.xml", "ascmhl/x.mhl", false, false),
            ("CLIP/*.xml", "sub/ascmhl/x.mhl", false, false),
            ("CLIP/*.xml", "_ShortEnd/r.pdf", false, false),
            ("CLIP/*.xml", "_ShortEnd", false, false),
            ("CLIP/*.xml", "a/b/x.tmp", false, false),
            ("CLIP/*.xml", "cache", true, false),
            ("CLIP/*.xml", "cache/x", false, false),
            ("CLIP/*.xml", "a/cache", true, false),
            ("CLIP/*.xml", "logs/x.log", false, false),
            ("CLIP/*.xml", "logs/a/b/x.log", false, false),
            ("CLIP/*.xml", "x/logs/x.log", false, false),
            ("CLIP/*.xml", "frame_031.dng", false, false),
            ("CLIP/*.xml", "frame_051.dng", false, false),
            ("CLIP/*.xml", "xbc", false, false),
            ("CLIP/*.xml", "abc", false, false),
            ("CLIP/*.xml", "CLIP/A.xml", false, true),
            ("CLIP/*.xml", "CLIP/sub/A.xml", false, false),
            ("CLIP/*.xml", "x/CLIP/A.xml", false, false),
            ("CLIP/*.xml", "A001.R3D.lock", false, false),
        ];
        for (pat, path, is_dir, want) in table {
            assert_eq!(
                spec(&[pat]).is_ignored(path, *is_dir),
                *want,
                "{pat} vs {path}"
            );
        }
    }
}
