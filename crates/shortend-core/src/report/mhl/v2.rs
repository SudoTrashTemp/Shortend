//! ASC MHL v2: history reading (with chain verification) and generation writing.
//!
//! Matches the reference implementation (github.com/ascmitc/mhl, `ascmhl` 1.2): generation file
//! naming, post-order lexicographic traversal, directory content/structure hashes, the
//! `original`/`verified`/`failed` actions, the C4-referenced chain file, and the default
//! ignore list. Output is validated against the official XSD and the reference `ascmhl verify`
//! in tests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::ignore::{IgnoreSpec, SHORTEND_PATTERN, default_patterns};
use super::xml::{self, Node, attr, esc};
use super::{MhlError, VerifyIssue, VerifyReport};
use crate::fs::platform;
use crate::hash::{Digest, Digests, HashAlgorithm, Hasher};

pub const FOLDER: &str = "ascmhl";
pub const CHAIN_FILE: &str = "ascmhl_chain.xml";
const NS: &str = "urn:ASC:MHL:v2.0";
const NS_DIR: &str = "urn:ASC:MHL:DIRECTORY:v2.0";

/// Algorithms the official ASC MHL v2 XSD defines, in the order it requires them.
const SCHEMA_ORDER: [HashAlgorithm; 6] = [
    HashAlgorithm::C4,
    HashAlgorithm::Md5,
    HashAlgorithm::Sha1,
    HashAlgorithm::Xxh128,
    HashAlgorithm::Xxh3,
    HashAlgorithm::Xxh64,
];

pub fn supports(a: HashAlgorithm) -> bool {
    SCHEMA_ORDER.contains(&a)
}

/// A `<hash>`/`<directoryhash>` being parsed: path, is-directory, (algorithm, value, action).
type PendingHash = (String, bool, Vec<(HashAlgorithm, String, Option<String>)>);
/// A chain `<hashlist>` being parsed: sequence number, file, hash.
type PendingChain = (u32, String, Option<(HashAlgorithm, String)>);

/// What the history records for one path, across all generations (oldest first).
#[derive(Debug, Default, Clone)]
pub struct PathRecord {
    pub is_dir: bool,
    /// The first hash marked `original`.
    pub original: Option<(HashAlgorithm, String)>,
    /// The first hash recorded in each algorithm.
    pub first: Vec<(HashAlgorithm, String)>,
}

impl PathRecord {
    fn first_for(&self, a: HashAlgorithm) -> Option<&str> {
        self.first
            .iter()
            .find(|(x, _)| *x == a)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ChainEntry {
    pub sequence: u32,
    pub file: String,
    pub algo: HashAlgorithm,
    pub value: String,
}

/// A loaded, chain-verified ASC MHL history.
#[derive(Debug)]
pub struct History {
    pub root: PathBuf,
    pub generations: Vec<(u32, String)>,
    pub chain: Vec<ChainEntry>,
    pub records: HashMap<String, PathRecord>,
    /// Ignore patterns of the latest generation.
    pub ignore: Option<Vec<String>>,
}

impl History {
    /// Loads `<root>/ascmhl`. `Ok(None)` if there is no history. Fails if the chain file is
    /// missing or any generation is missing or modified (the chain records each one's C4).
    pub fn load(root: &Path) -> Result<Option<Self>, MhlError> {
        let dir = root.join(FOLDER);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(MhlError::Invalid(format!(
                    "{} is not a folder",
                    dir.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MhlError::io(&dir, e)),
        }
        let chain_path = dir.join(CHAIN_FILE);
        if !chain_path.is_file() {
            return Err(MhlError::Invalid(format!(
                "{} has no {CHAIN_FILE}; the history cannot be trusted",
                dir.display()
            )));
        }
        let chain = parse_chain(&chain_path)?;
        for c in &chain {
            let p = dir.join(&c.file);
            if !p.is_file() {
                return Err(MhlError::Invalid(format!(
                    "generation {} listed in the chain is missing",
                    c.file
                )));
            }
            let actual = hash_file(&p, c.algo)?;
            if actual.value() != c.value {
                return Err(MhlError::Invalid(format!(
                    "generation {} was modified after it was written (chain {} mismatch)",
                    c.file, c.algo
                )));
            }
        }

        let mut gens = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(|e| MhlError::io(&dir, e))? {
            let e = e.map_err(|e| MhlError::io(&dir, e))?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("._") || !name.ends_with(".mhl") || !e.path().is_file() {
                continue;
            }
            if let Some(n) = generation_number(&name) {
                gens.push((n, name));
            }
        }
        gens.sort();

        let mut h = History {
            root: root.to_path_buf(),
            generations: gens.clone(),
            chain,
            records: HashMap::new(),
            ignore: None,
        };
        for (_, name) in &gens {
            h.absorb(&dir.join(name))?;
        }
        Ok(Some(h))
    }

    fn absorb(&mut self, path: &Path) -> Result<(), MhlError> {
        let f = std::fs::File::open(path).map_err(|e| MhlError::io(path, e))?;
        let mut cur: Option<PendingHash> = None;
        let mut ignore: Option<Vec<String>> = None;
        let records = &mut self.records;
        xml::walk(BufReader::new(f), |n| {
            match n {
                Node::Open { path: p, .. } => {
                    let name = p.last().map(String::as_str);
                    let parent = p.len().checked_sub(2).map(|i| p[i].as_str());
                    match (parent, name) {
                        (Some("hashes"), Some("hash")) => {
                            cur = Some((String::new(), false, Vec::new()))
                        }
                        (Some("hashes"), Some("directoryhash")) => {
                            cur = Some((String::new(), true, Vec::new()));
                        }
                        (Some("processinfo"), Some("ignore")) => ignore = Some(Vec::new()),
                        _ => {}
                    }
                }
                Node::Close {
                    path: p,
                    attrs,
                    text,
                } => {
                    let name = p.last().map_or("", String::as_str);
                    let parent = p.len().checked_sub(2).map_or("", |i| p[i].as_str());
                    if parent == "ignore"
                        && name == "pattern"
                        && let Some(v) = ignore.as_mut()
                    {
                        v.push(text.to_owned());
                    }
                    if let Some((path, is_dir, entries)) = cur.as_mut() {
                        if name == "path" && (parent == "hash" || parent == "directoryhash") {
                            *path = text.to_owned();
                        } else if let Ok(a) = name.parse::<HashAlgorithm>()
                            && supports(a)
                            && (parent == "hash" || parent == "content")
                        {
                            entries.push((
                                a,
                                text.to_owned(),
                                attr(attrs, "action").map(str::to_owned),
                            ));
                        } else if (name == "hash" && !*is_dir)
                            || (name == "directoryhash" && *is_dir)
                        {
                            let (path, is_dir, entries) = cur.take().expect("current");
                            let rec = records.entry(path).or_default();
                            rec.is_dir = is_dir;
                            for (a, v, action) in entries {
                                if rec.original.is_none() && action.as_deref() == Some("original") {
                                    rec.original = Some((a, v.clone()));
                                }
                                if rec.first_for(a).is_none() {
                                    rec.first.push((a, v));
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        })
        .map_err(|e| MhlError::Invalid(format!("{}: {e}", path.display())))?;
        if ignore.is_some() {
            self.ignore = ignore;
        }
        Ok(())
    }

    pub fn latest_number(&self) -> u32 {
        self.generations.last().map_or(0, |(n, _)| *n)
    }

    /// Ignore patterns in effect: the latest generation's, or the reference defaults.
    pub fn ignore_spec(history: Option<&Self>) -> IgnoreSpec {
        IgnoreSpec::new(
            history
                .and_then(|h| h.ignore.clone())
                .unwrap_or_else(default_patterns),
        )
    }

    /// Paths of files (not folders) that have an original hash.
    pub fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.records
            .iter()
            .filter(|(_, r)| !r.is_dir && r.original.is_some())
            .map(|(p, _)| p.as_str())
    }

    /// Algorithms recorded for files, in schema order.
    pub fn algorithms(&self) -> Vec<HashAlgorithm> {
        let used: HashSet<HashAlgorithm> = self
            .records
            .values()
            .filter(|r| !r.is_dir)
            .flat_map(|r| r.first.iter().map(|(a, _)| *a))
            .collect();
        SCHEMA_ORDER
            .into_iter()
            .filter(|a| used.contains(a))
            .collect()
    }

    pub fn record(&self, path: &str) -> Option<&PathRecord> {
        self.records.get(path)
    }
}

/// `0001_Name_2020-01-01_120000Z.mhl` → 1.
pub fn generation_number(file_name: &str) -> Option<u32> {
    let stem = file_name.strip_suffix(".mhl")?;
    let digits: String = stem.chars().take_while(char::is_ascii_digit).collect();
    let rest = &stem[digits.len()..];
    if digits.len() < 4 || !(rest.is_empty() || rest.starts_with('_') && rest.len() > 1) {
        return None;
    }
    digits.parse().ok()
}

fn parse_chain(path: &Path) -> Result<Vec<ChainEntry>, MhlError> {
    let f = std::fs::File::open(path).map_err(|e| MhlError::io(path, e))?;
    let mut out = Vec::new();
    let mut cur: Option<PendingChain> = None;
    xml::walk(BufReader::new(f), |n| {
        match n {
            Node::Open { path: p, attrs } if p.last().is_some_and(|x| x == "hashlist") => {
                let seq = attr(attrs, "sequencenr")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                cur = Some((seq, String::new(), None));
            }
            Node::Close { path: p, text, .. } => {
                let name = p.last().map_or("", String::as_str);
                if let Some((seq, file, hash)) = cur.as_mut() {
                    if name == "path" {
                        *file = text.to_owned();
                    } else if let Ok(a) = name.parse::<HashAlgorithm>() {
                        *hash = Some((a, text.to_owned()));
                    } else if name == "hashlist" {
                        let (Some((algo, value)), false) = (hash.take(), file.is_empty()) else {
                            return Err(xml::XmlError(format!("chain entry {seq} is incomplete")));
                        };
                        out.push(ChainEntry {
                            sequence: *seq,
                            file: std::mem::take(file),
                            algo,
                            value,
                        });
                        cur = None;
                    }
                }
            }
            Node::Open { .. } => {}
        }
        Ok(())
    })
    .map_err(|e| MhlError::Invalid(format!("{}: {e}", path.display())))?;
    for c in &out {
        if c.file.contains('/') || c.file.contains('\\') || c.file.starts_with('.') {
            return Err(MhlError::Invalid(format!(
                "chain entry with unsafe path '{}'",
                c.file
            )));
        }
    }
    Ok(out)
}

/// Hashes a whole file with one algorithm.
pub fn hash_file(path: &Path, algo: HashAlgorithm) -> Result<Digest, MhlError> {
    let mut f = platform::open_source(path).map_err(|e| MhlError::io(path, e))?;
    let mut h = Hasher::new(algo);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| MhlError::io(path, e))?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..n]);
    }
}

// ------------------------------------------------------------------------------------------
// writing

/// One file to record in a new generation.
pub struct GenFile<'a> {
    pub rel: &'a str,
    pub size: u64,
    pub mtime: Timestamp,
    pub digests: &'a Digests,
}

/// Identity written into `<creatorinfo>`.
pub struct Creator {
    pub tool_version: String,
    pub hostname: String,
    pub author: Option<String>,
    pub location: Option<String>,
    pub comment: Option<String>,
}

/// `<processinfo>` settings of a new generation.
pub struct Process<'a> {
    /// `in-place`, `transfer` or `flatten`.
    pub kind: &'a str,
    /// Ignore patterns added to the history's (e.g. a sibling MHL v1.1 file written alongside).
    pub extra_ignore: &'a [String],
}

/// A file whose hash no longer matches what the history recorded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub rel: String,
    pub recorded: String,
    pub actual: Digest,
}

#[derive(Debug)]
pub struct Generation {
    pub path: PathBuf,
    pub number: u32,
    /// Files that contradict the existing history (written with action `failed`).
    pub mismatches: Vec<Mismatch>,
    /// Files whose recorded algorithms were not computed; written as new originals' peers
    /// cannot be verified, so they were left out of the generation.
    pub unverifiable: Vec<String>,
}

/// Writes a new generation for the folder `root` and extends the chain. `history` is the
/// already loaded history of `root` (if any). `dirs` lists every folder (relative, `/`-separated)
/// that exists under `root`, including empty ones.
pub fn write_generation(
    root: &Path,
    history: Option<&History>,
    files: &[GenFile<'_>],
    dirs: &[&str],
    creator: &Creator,
    process: &Process<'_>,
    now: Timestamp,
) -> Result<Generation, MhlError> {
    let mut patterns = History::ignore_spec(history).patterns().to_vec();
    for p in
        std::iter::once(SHORTEND_PATTERN).chain(process.extra_ignore.iter().map(String::as_str))
    {
        if !patterns.iter().any(|x| x == p) {
            patterns.push(p.to_owned());
        }
    }
    let spec = IgnoreSpec::new(patterns);

    // Tree of recorded entries.
    let mut children: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    children.entry(String::new()).or_default();
    let add_dir = |rel: &str, children: &mut BTreeMap<String, Vec<(String, bool)>>| {
        let mut parent = String::new();
        for comp in rel.split('/') {
            let path = if parent.is_empty() {
                comp.to_owned()
            } else {
                format!("{parent}/{comp}")
            };
            if !children.contains_key(&path) {
                children.insert(path.clone(), Vec::new());
                children
                    .entry(parent.clone())
                    .or_default()
                    .push((comp.to_owned(), true));
            }
            parent = path;
        }
    };
    for d in dirs {
        if !spec.is_ignored(d, true) {
            add_dir(d, &mut children);
        }
    }
    let mut by_path: HashMap<&str, &GenFile<'_>> = HashMap::new();
    for f in files {
        if spec.is_ignored(f.rel, false) {
            continue;
        }
        let (parent, name) = match f.rel.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", f.rel),
        };
        if !parent.is_empty() {
            add_dir(parent, &mut children);
        }
        children
            .entry(parent.to_owned())
            .or_default()
            .push((name.to_owned(), false));
        by_path.insert(f.rel, f);
    }
    for v in children.values_mut() {
        v.sort();
        v.dedup();
    }

    let algos: Vec<HashAlgorithm> = SCHEMA_ORDER
        .into_iter()
        .filter(|a| {
            files
                .first()
                .is_some_and(|f| f.digests.iter().any(|d| d.algo() == *a))
        })
        .collect();
    if algos.is_empty() && !files.is_empty() {
        return Err(MhlError::Unsupported(
            "none of the job's checksum algorithms can be written to ASC MHL v2".into(),
        ));
    }

    let number = history.map_or(0, History::latest_number) + 1;
    let folder_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let file_name = format!(
        "{number:04}_{folder_name}_{}.mhl",
        now.to_zoned(TimeZone::UTC).strftime("%Y-%m-%d_%H%M%SZ")
    );

    let mut out = String::new();
    let _ = writeln!(
        out,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<hashlist version=\"2.0\" xmlns=\"{NS}\">"
    );
    // creatorinfo
    let _ = writeln!(out, "  <creatorinfo>");
    let _ = writeln!(out, "    <creationdate>{}</creationdate>", iso_local(now));
    let _ = writeln!(out, "    <hostname>{}</hostname>", esc(&creator.hostname));
    let _ = writeln!(
        out,
        "    <tool version=\"{}\">ShortEnd</tool>",
        esc(&creator.tool_version)
    );
    if let Some(a) = creator.author.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "    <author>{}</author>", esc(a));
    }
    if let Some(l) = creator.location.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "    <location>{}</location>", esc(l));
    }
    if let Some(c) = creator.comment.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "    <comment>{}</comment>", esc(c));
    }
    let _ = writeln!(out, "  </creatorinfo>");

    // Walk post-order: entries first, root hash last.
    let mut body = String::new();
    let mut mismatches = Vec::new();
    let mut unverifiable = Vec::new();
    let mut root_hashes: Option<Vec<(HashAlgorithm, String, String)>> = None;
    let mut dir_hashes: HashMap<String, Vec<(HashAlgorithm, String, String)>> = HashMap::new();
    for dir in post_order(&children) {
        let mut contexts: Vec<DirContext> = algos.iter().map(|&a| DirContext::new(a)).collect();
        for (name, is_dir) in &children[&dir] {
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            if *is_dir {
                let h = dir_hashes.remove(&rel).unwrap_or_default();
                for (ctx, (_, content, structure)) in contexts.iter_mut().zip(&h) {
                    ctx.add_dir(name, content, structure)?;
                }
                continue;
            }
            let f = by_path[rel.as_str()];
            let mut entries = Vec::new();
            match history
                .and_then(|h| h.record(&rel))
                .filter(|r| r.original.is_some())
            {
                None => {
                    for &a in &algos {
                        entries.push((a, digest_for(f.digests, a)?, "original"));
                    }
                }
                Some(rec) => {
                    let mut any_existing = false;
                    let mut failed = false;
                    for &a in &algos {
                        if let Some(recorded) = rec.first_for(a) {
                            any_existing = true;
                            let actual = f.digests.iter().find(|d| d.algo() == a).expect("algo");
                            if actual.value() == recorded {
                                entries.push((a, actual.value(), "verified"));
                            } else {
                                failed = true;
                                entries.push((a, actual.value(), "failed"));
                                mismatches.push(Mismatch {
                                    rel: rel.clone(),
                                    recorded: recorded.to_owned(),
                                    actual: actual.clone(),
                                });
                            }
                        }
                    }
                    if !any_existing {
                        unverifiable.push(rel.clone());
                        continue;
                    }
                    if !failed {
                        for &a in &algos {
                            if rec.first_for(a).is_none() {
                                entries.push((a, digest_for(f.digests, a)?, "verified"));
                            }
                        }
                        entries.sort_by_key(|(a, ..)| SCHEMA_ORDER.iter().position(|x| x == a));
                    }
                }
            }
            for ctx in &mut contexts {
                let v = digest_for(f.digests, ctx.algo)?;
                ctx.add_file(name, &v)?;
            }
            let _ = writeln!(body, "    <hash>");
            let _ = writeln!(
                body,
                "      <path size=\"{}\" lastmodificationdate=\"{}\">{}</path>",
                f.size,
                iso_local(f.mtime),
                esc(&rel)
            );
            for (a, v, action) in entries {
                let _ = writeln!(body, "      <{a} action=\"{action}\">{v}</{a}>");
            }
            let _ = writeln!(body, "    </hash>");
        }
        let finished: Vec<(HashAlgorithm, String, String)> = contexts
            .into_iter()
            .map(DirContext::finish)
            .collect::<Result<_, _>>()?;
        if dir.is_empty() {
            root_hashes = Some(finished);
        } else {
            let mtime = std::fs::metadata(root.join(&dir))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| Timestamp::try_from(t).ok());
            let _ = writeln!(body, "    <directoryhash>");
            match mtime {
                Some(t) => {
                    let _ = writeln!(
                        body,
                        "      <path lastmodificationdate=\"{}\">{}</path>",
                        iso_local(t),
                        esc(&dir)
                    );
                }
                None => {
                    let _ = writeln!(body, "      <path>{}</path>", esc(&dir));
                }
            }
            write_dir_hashes(&mut body, &finished, "      ");
            let _ = writeln!(body, "    </directoryhash>");
            dir_hashes.insert(dir, finished);
        }
    }

    // processinfo
    let _ = writeln!(out, "  <processinfo>");
    let _ = writeln!(out, "    <process>{}</process>", process.kind);
    if let Some(r) = root_hashes.as_ref().filter(|r| !r.is_empty()) {
        let _ = writeln!(out, "    <roothash>");
        write_dir_hashes(&mut out, r, "      ");
        let _ = writeln!(out, "    </roothash>");
    }
    let _ = writeln!(out, "    <ignore>");
    for p in spec.patterns() {
        let _ = writeln!(out, "      <pattern>{}</pattern>", esc(p));
    }
    let _ = writeln!(out, "    </ignore>");
    let _ = writeln!(out, "  </processinfo>");
    if !body.is_empty() {
        let _ = writeln!(out, "  <hashes>");
        out.push_str(&body);
        let _ = writeln!(out, "  </hashes>");
    }
    let _ = writeln!(out, "</hashlist>");

    let dir = root.join(FOLDER);
    std::fs::create_dir_all(&dir).map_err(|e| MhlError::io(&dir, e))?;
    let path = dir.join(&file_name);
    write_new(&path, out.as_bytes())?;

    // Chain: every previous generation plus this one, each referenced by the C4 of its file.
    let mut chain = history.map(|h| h.chain.clone()).unwrap_or_default();
    chain.push(ChainEntry {
        sequence: number,
        file: file_name,
        algo: HashAlgorithm::C4,
        value: hash_file(&path, HashAlgorithm::C4)?.value(),
    });
    let mut c = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ascmhldirectory xmlns=\"{NS_DIR}\">\n"
    );
    for e in &chain {
        let _ = writeln!(
            c,
            "  <hashlist sequencenr=\"{}\">\n    <path>{}</path>\n    <{a}>{}</{a}>\n  </hashlist>",
            e.sequence,
            esc(&e.file),
            esc(&e.value),
            a = e.algo
        );
    }
    c.push_str("</ascmhldirectory>\n");
    replace_file(&dir.join(CHAIN_FILE), c.as_bytes())?;
    let _ = platform::sync_dir(&dir);

    Ok(Generation {
        path,
        number,
        mismatches,
        unverifiable,
    })
}

fn digest_for(d: &Digests, a: HashAlgorithm) -> Result<String, MhlError> {
    d.iter()
        .find(|x| x.algo() == a)
        .map(Digest::value)
        .ok_or_else(|| MhlError::Unsupported(format!("missing {a} checksum")))
}

fn write_dir_hashes(out: &mut String, hashes: &[(HashAlgorithm, String, String)], indent: &str) {
    let _ = writeln!(out, "{indent}<content>");
    for (a, content, _) in hashes {
        let _ = writeln!(out, "{indent}  <{a}>{content}</{a}>");
    }
    let _ = writeln!(out, "{indent}</content>");
    let _ = writeln!(out, "{indent}<structure>");
    for (a, _, structure) in hashes {
        let _ = writeln!(out, "{indent}  <{a}>{structure}</{a}>");
    }
    let _ = writeln!(out, "{indent}</structure>");
}

/// Folders deepest-first, siblings in lexicographic order, the root (`""`) last.
fn post_order(children: &BTreeMap<String, Vec<(String, bool)>>) -> Vec<String> {
    fn visit(dir: &str, children: &BTreeMap<String, Vec<(String, bool)>>, out: &mut Vec<String>) {
        for (name, is_dir) in &children[dir] {
            if *is_dir {
                let rel = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                visit(&rel, children, out);
            }
        }
        out.push(dir.to_owned());
    }
    let mut out = Vec::new();
    visit("", children, &mut out);
    out
}

/// Directory hash computation as in the reference `DirectoryHashContext`.
struct DirContext {
    algo: HashAlgorithm,
    content: Vec<String>,
    structure: Vec<String>,
}

impl DirContext {
    fn new(algo: HashAlgorithm) -> Self {
        Self {
            algo,
            content: Vec::new(),
            structure: Vec::new(),
        }
    }

    fn named(&self, name: &str, hash: &str) -> Result<String, MhlError> {
        let bytes = Digest::parse(self.algo, hash).map_err(|e| MhlError::Invalid(e.to_string()))?;
        let mut h = Hasher::new(self.algo);
        h.update(name.as_bytes());
        h.update(bytes.as_bytes());
        Ok(h.finish().value())
    }

    fn add_file(&mut self, name: &str, content: &str) -> Result<(), MhlError> {
        self.structure.push(self.named(name, content)?);
        self.content.push(content.to_owned());
        Ok(())
    }

    fn add_dir(&mut self, name: &str, content: &str, structure: &str) -> Result<(), MhlError> {
        self.structure.push(self.named(name, structure)?);
        self.content.push(content.to_owned());
        Ok(())
    }

    fn finish(self) -> Result<(HashAlgorithm, String, String), MhlError> {
        let content = hash_of_list(self.algo, self.content)?;
        let structure = hash_of_list(self.algo, self.structure)?;
        Ok((self.algo, content, structure))
    }
}

/// Hash of the lexicographically sorted digest strings, each fed as raw bytes.
fn hash_of_list(algo: HashAlgorithm, mut list: Vec<String>) -> Result<String, MhlError> {
    list.sort();
    let mut h = Hasher::new(algo);
    for s in &list {
        let d = Digest::parse(algo, s).map_err(|e| MhlError::Invalid(e.to_string()))?;
        h.update(d.as_bytes());
    }
    Ok(h.finish().value())
}

/// `2026-10-11T22:15:03-07:00`: local time with offset, second precision (as the reference).
pub fn iso_local(t: Timestamp) -> String {
    t.to_zoned(TimeZone::system())
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// Creates `path` (must not exist) durably.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), MhlError> {
    let tmp = path.with_extension("mhl.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = platform::create_staged(&tmp).map_err(|e| MhlError::io(&tmp, e))?;
    f.write_all(bytes).map_err(|e| MhlError::io(&tmp, e))?;
    platform::sync_file(&f).map_err(|e| MhlError::io(&tmp, e))?;
    drop(f);
    platform::rename_noreplace(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        MhlError::io(path, e)
    })
}

/// Atomically replaces `path`.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), MhlError> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = platform::create_staged(&tmp).map_err(|e| MhlError::io(&tmp, e))?;
    f.write_all(bytes).map_err(|e| MhlError::io(&tmp, e))?;
    platform::sync_file(&f).map_err(|e| MhlError::io(&tmp, e))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| MhlError::io(path, e))
}

// ------------------------------------------------------------------------------------------
// verification

/// Checks every file under `root` against the history, as `ascmhl verify` does: each file is
/// hashed with the algorithm of its original record; files missing from disk and files the
/// history doesn't know are reported. Read-only.
pub fn verify(root: &Path, history: &History) -> Result<VerifyReport, MhlError> {
    let spec = History::ignore_spec(Some(history));
    let mut report = VerifyReport::new(super::ManifestKind::AscMhlV2, root.join(FOLDER));
    let mut seen = HashSet::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let abs = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        for e in std::fs::read_dir(&abs).map_err(|e| MhlError::io(&abs, e))? {
            let e = e.map_err(|e| MhlError::io(&abs, e))?;
            let ft = e.file_type().map_err(|err| MhlError::io(&e.path(), err))?;
            names.push((e.file_name().to_string_lossy().into_owned(), ft));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ft) in names {
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if spec.is_ignored(&rel, ft.is_dir()) {
                continue;
            }
            if ft.is_dir() {
                stack.push(rel);
                continue;
            }
            if ft.is_symlink() || !ft.is_file() {
                report.issues.push(VerifyIssue::Unreadable {
                    rel,
                    reason: "not a regular file".into(),
                });
                continue;
            }
            seen.insert(rel.clone());
            match history.record(&rel).and_then(|r| r.original.clone()) {
                None => report.issues.push(VerifyIssue::NotInManifest { rel }),
                Some((algo, recorded)) => match hash_file(&root.join(&rel), algo) {
                    Ok(actual) if actual.value() == recorded => report.verified += 1,
                    Ok(actual) => report.issues.push(VerifyIssue::Mismatch {
                        rel,
                        recorded: format!("{algo}:{recorded}"),
                        actual: actual.to_string(),
                    }),
                    Err(e) => report.issues.push(VerifyIssue::Unreadable {
                        rel,
                        reason: e.to_string(),
                    }),
                },
            }
        }
    }
    let mut missing: Vec<&str> = history
        .file_paths()
        .filter(|p| !seen.contains(*p) && !spec.is_ignored(p, false))
        .collect();
    missing.sort_unstable();
    for rel in missing {
        report.issues.push(VerifyIssue::Missing {
            rel: rel.to_owned(),
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_names() {
        assert_eq!(
            generation_number("0001_A001_2020-01-15_130000Z.mhl"),
            Some(1)
        );
        assert_eq!(generation_number("0012.mhl"), Some(12));
        assert_eq!(generation_number("12345_x.mhl"), Some(12345));
        assert_eq!(generation_number("001_x.mhl"), None);
        assert_eq!(generation_number("packinglist_x.mhl"), None);
        assert_eq!(generation_number("0001_.mhl"), None);
    }

    #[test]
    fn empty_dir_hash_is_hash_of_nothing() {
        let (_, c, s) = DirContext::new(HashAlgorithm::Xxh64).finish().unwrap();
        assert_eq!(c, "ef46db3751d8e999");
        assert_eq!(s, "ef46db3751d8e999");
    }
}
