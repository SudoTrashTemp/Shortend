//! Folder-name templates, e.g. `{show}/{date:%Y%m%d}_D{day}/{camroll}_{card}_{seq:3}`, and
//! safe sequential numbering.
//!
//! Tokens: `{date[:fmt]}`, `{time[:fmt]}`, `{show}`, `{episode}`, `{day}`, `{card}`,
//! `{camroll}` (alias `{camrol}`), `{seq[:width]}`. `{{` and `}}` are literal braces; `/`
//! separates folders. Parsing rejects unknown tokens, unbalanced braces and malformed
//! arguments; expansion rejects empty values, so a template never silently yields `a//b`.
//!
//! `{seq}` is the next free number across every destination: the highest number already
//! used by a folder matching the template, plus one. It is claimed with `create_dir` on every
//! destination (which fails if the folder exists), retrying with the next number on any
//! collision, so all copies of one offload share one number and concurrent jobs never take the
//! same one. No counter is stored anywhere.

use std::fmt;
use std::path::{Path, PathBuf};

use jiff::Zoned;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::model::ProductionInfo;

const DEFAULT_DATE: &str = "%Y%m%d";
const DEFAULT_TIME: &str = "%H%M%S";
/// Characters replaced in token values: path separators, characters Windows, exFAT and FAT
/// forbid, and control characters.
const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("folder template '{template}': {message}")]
pub struct TemplateError {
    pub template: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Date(String),
    Time(String),
    Show,
    Episode,
    Day,
    Card,
    Camroll,
    Seq(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Token(Token),
}

/// A parsed folder template. Components are separated by `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    components: Vec<Vec<Part>>,
}

impl Template {
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let err = |message: String| TemplateError {
            template: text.to_owned(),
            message,
        };
        if text.trim().is_empty() {
            return Err(err("is empty".into()));
        }
        let mut components: Vec<Vec<Part>> = vec![Vec::new()];
        let mut lit = String::new();
        let mut chars = text.char_indices().peekable();
        let flush = |lit: &mut String, comps: &mut Vec<Vec<Part>>| {
            if !lit.is_empty() {
                comps
                    .last_mut()
                    .expect("one component")
                    .push(Part::Lit(std::mem::take(lit)));
            }
        };
        while let Some((pos, c)) = chars.next() {
            match c {
                '{' if chars.peek().is_some_and(|(_, n)| *n == '{') => {
                    chars.next();
                    lit.push('{');
                }
                '}' if chars.peek().is_some_and(|(_, n)| *n == '}') => {
                    chars.next();
                    lit.push('}');
                }
                '}' => {
                    return Err(err(format!(
                        "unmatched '}}' at position {} (write '}}}}' for a literal brace)",
                        pos + 1
                    )));
                }
                '{' => {
                    let mut body = String::new();
                    let mut closed = false;
                    for (_, c) in chars.by_ref() {
                        match c {
                            '}' => {
                                closed = true;
                                break;
                            }
                            '{' => break,
                            c => body.push(c),
                        }
                    }
                    if !closed {
                        return Err(err(format!(
                            "unclosed '{{' at position {} (write '{{{{' for a literal brace)",
                            pos + 1
                        )));
                    }
                    flush(&mut lit, &mut components);
                    let token = parse_token(&body).map_err(err)?;
                    components
                        .last_mut()
                        .expect("one component")
                        .push(Part::Token(token));
                }
                '/' => {
                    flush(&mut lit, &mut components);
                    components.push(Vec::new());
                }
                '\\' => {
                    return Err(err(
                        "use '/' to separate folders ('\\' is not allowed)".into()
                    ));
                }
                c => lit.push(c),
            }
        }
        flush(&mut lit, &mut components);

        if components.first().is_some_and(Vec::is_empty) {
            return Err(err(
                "starts with '/': the folder is created under each destination, so it must be relative".into(),
            ));
        }
        if components.iter().any(Vec::is_empty) {
            return Err(err(
                "contains an empty folder name ('//' or a trailing '/')".into(),
            ));
        }
        for comp in &components {
            if let [Part::Lit(s)] = comp.as_slice()
                && (s == "." || s == "..")
            {
                return Err(err(format!("'{s}' is not allowed as a folder name")));
            }
        }
        let seqs = components
            .iter()
            .flatten()
            .filter(|p| matches!(p, Part::Token(Token::Seq(_))))
            .count();
        if seqs > 1 {
            return Err(err("{seq} may appear only once".into()));
        }
        Ok(Self { components })
    }

    pub fn has_seq(&self) -> bool {
        self.components
            .iter()
            .flatten()
            .any(|p| matches!(p, Part::Token(Token::Seq(_))))
    }

    /// Expands every token except `{seq}`. Problems (empty values) are all listed.
    pub fn expand(&self, v: &Values<'_>) -> Result<Expanded, Vec<String>> {
        let mut problems = Vec::new();
        let mut warnings = Vec::new();
        let mut before = Vec::new();
        let mut slot = None;
        let mut after = Vec::new();
        for comp in &self.components {
            let mut text = String::new();
            let mut seq_at = None;
            for part in comp {
                match part {
                    Part::Lit(s) => text.push_str(s),
                    Part::Token(Token::Seq(w)) => seq_at = Some((text.len(), *w)),
                    Part::Token(t) => {
                        let (name, raw) = value(t, v);
                        let clean = sanitize(&raw);
                        if clean.is_empty() {
                            problems.push(empty_message(name));
                        } else if clean != raw.trim() {
                            warnings.push(format!(
                                "{{{name}}} '{raw}' is written as '{clean}' in the folder name"
                            ));
                        }
                        text.push_str(&clean);
                    }
                }
            }
            match seq_at {
                Some((at, width)) => {
                    slot = Some(SeqSlot {
                        prefix: text[..at].to_owned(),
                        suffix: text[at..].to_owned(),
                        width,
                    });
                }
                None if slot.is_none() => before.push(text),
                None => after.push(text),
            }
        }
        // A trailing dot or space is invalid on Windows/exFAT and stripped by some systems.
        for c in before.iter().chain(&after) {
            if c.ends_with(['.', ' ']) {
                problems.push(format!(
                    "folder name '{c}' ends with a dot or space, which several filesystems cannot store"
                ));
            }
        }
        if problems.is_empty() {
            Ok(Expanded {
                before,
                slot,
                after,
                warnings,
            })
        } else {
            Err(problems)
        }
    }
}

/// The template text in canonical form: `{camrol}` is written as `{camroll}`, default
/// arguments are omitted, literal braces are escaped.
impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, comp) in self.components.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            for part in comp {
                match part {
                    Part::Lit(s) => f.write_str(&s.replace('{', "{{").replace('}', "}}"))?,
                    Part::Token(t) => match t {
                        Token::Date(x) if x == DEFAULT_DATE => f.write_str("{date}")?,
                        Token::Date(x) => write!(f, "{{date:{x}}}")?,
                        Token::Time(x) if x == DEFAULT_TIME => f.write_str("{time}")?,
                        Token::Time(x) => write!(f, "{{time:{x}}}")?,
                        Token::Show => f.write_str("{show}")?,
                        Token::Episode => f.write_str("{episode}")?,
                        Token::Day => f.write_str("{day}")?,
                        Token::Card => f.write_str("{card}")?,
                        Token::Camroll => f.write_str("{camroll}")?,
                        Token::Seq(0) => f.write_str("{seq}")?,
                        Token::Seq(w) => write!(f, "{{seq:{w}}}")?,
                    },
                }
            }
        }
        Ok(())
    }
}

impl Serialize for Template {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Template::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn parse_token(body: &str) -> Result<Token, String> {
    let (name, arg) = match body.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a)),
        None => (body.trim(), None),
    };
    let no_arg = |t: Token| match arg {
        None => Ok(t),
        Some(_) => Err(format!("{{{name}}} takes no argument")),
    };
    match name {
        "date" | "time" => {
            let default = if name == "date" {
                DEFAULT_DATE
            } else {
                DEFAULT_TIME
            };
            let f = arg.unwrap_or(default);
            check_strftime(name, f)?;
            Ok(if name == "date" {
                Token::Date(f.to_owned())
            } else {
                Token::Time(f.to_owned())
            })
        }
        "show" => no_arg(Token::Show),
        "episode" => no_arg(Token::Episode),
        "day" => no_arg(Token::Day),
        "card" => no_arg(Token::Card),
        // `{camrol}`: ShotPut Pro presets spell it this way.
        "camroll" | "camrol" => no_arg(Token::Camroll),
        "seq" => match arg {
            None => Ok(Token::Seq(0)),
            Some(w) => match w.trim().parse::<usize>() {
                Ok(n @ 1..=9) => Ok(Token::Seq(n)),
                _ => Err(format!(
                    "{{seq:{w}}}: the width must be a number from 1 to 9"
                )),
            },
        },
        "" => Err("empty token '{}'".into()),
        other => Err(format!(
            "unknown token {{{other}}} (known: date, time, show, episode, day, card, camroll, seq)"
        )),
    }
}

fn check_strftime(name: &str, f: &str) -> Result<(), String> {
    if f.is_empty() {
        return Err(format!("{{{name}:}} needs a format such as %Y%m%d"));
    }
    if f.contains(['/', '\\']) {
        return Err(format!(
            "{{{name}:{f}}}: a date format cannot contain '/'; use separate tokens for separate folders, e.g. {{date:%Y}}/{{date:%m}}"
        ));
    }
    let sample: Zoned = "2024-01-02T03:04:05[UTC]".parse().expect("valid sample");
    jiff::fmt::strtime::format(f, &sample)
        .map(|_| ())
        .map_err(|e| format!("{{{name}:{f}}}: invalid format: {e}"))
}

/// Values the tokens expand to.
pub struct Values<'a> {
    pub now: &'a Zoned,
    pub production: &'a ProductionInfo,
    pub card: &'a str,
}

fn value(t: &Token, v: &Values<'_>) -> (&'static str, String) {
    let p = v.production;
    let opt = |s: &Option<String>| s.clone().unwrap_or_default();
    match t {
        Token::Date(f) => (
            "date",
            jiff::fmt::strtime::format(f, v.now).unwrap_or_default(),
        ),
        Token::Time(f) => (
            "time",
            jiff::fmt::strtime::format(f, v.now).unwrap_or_default(),
        ),
        Token::Show => ("show", opt(&p.show)),
        Token::Episode => ("episode", opt(&p.episode)),
        Token::Day => ("day", opt(&p.shoot_day)),
        Token::Card => ("card", v.card.to_owned()),
        Token::Camroll => ("camroll", opt(&p.camroll)),
        Token::Seq(_) => ("seq", String::new()),
    }
}

fn empty_message(name: &str) -> String {
    let flag = match name {
        "day" => "--day",
        "camroll" => "--camroll",
        "show" => "--show",
        "episode" => "--episode",
        _ => "",
    };
    if flag.is_empty() {
        format!("the folder template uses {{{name}}}, which is empty")
    } else {
        format!(
            "the folder template uses {{{name}}}, which is not set (give {flag} or set it in the preset)"
        )
    }
}

/// Makes a token value safe as part of one folder name: separators, reserved and control
/// characters become `_`; surrounding whitespace and trailing dots are removed.
pub fn sanitize(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            if FORBIDDEN.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim().trim_end_matches(['.', ' ']);
    if trimmed.chars().all(|c| c == '.') {
        return String::new(); // "." / ".." would mean the current or parent folder
    }
    trimmed.to_owned()
}

/// Position of `{seq}` in an expanded template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqSlot {
    pub prefix: String,
    pub suffix: String,
    /// Zero-padded width; 0 means no padding.
    pub width: usize,
}

impl SeqSlot {
    pub fn name(&self, n: u64) -> String {
        format!("{}{n:0w$}{}", self.prefix, self.suffix, w = self.width)
    }

    /// The number in `name` if it matches this slot (any number of digits).
    pub fn number_in(&self, name: &str) -> Option<u64> {
        let name: String = name.nfc().collect();
        let prefix: String = self.prefix.nfc().collect();
        let suffix: String = self.suffix.nfc().collect();
        let digits = name.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }
}

/// A template with every token except `{seq}` expanded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expanded {
    /// Folder components before the one holding `{seq}` (all of them if there is none).
    pub before: Vec<String>,
    pub slot: Option<SeqSlot>,
    /// Folder components after the `{seq}` one.
    pub after: Vec<String>,
    /// Token values that had to be changed to be valid folder names.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Expanded {
    /// The `/`-separated folder for sequence number `n` (ignored without `{seq}`).
    pub fn folder(&self, n: u64) -> String {
        let mut parts: Vec<String> = self.before.clone();
        if let Some(s) = &self.slot {
            parts.push(s.name(n));
            parts.extend(self.after.iter().cloned());
        }
        parts.join("/")
    }

    fn parent(&self, root: &Path) -> PathBuf {
        self.before
            .iter()
            .fold(root.to_path_buf(), |p, c| p.join(c))
    }

    /// One more than the highest number any destination already uses (1 if none).
    pub fn next_seq<'a>(&self, roots: impl IntoIterator<Item = &'a Path>) -> u64 {
        let Some(slot) = &self.slot else { return 1 };
        let mut max = 0;
        for root in roots {
            let Ok(entries) = std::fs::read_dir(self.parent(root)) else {
                continue;
            };
            for e in entries.flatten() {
                if let Some(n) = e.file_name().to_str().and_then(|n| slot.number_in(n)) {
                    max = max.max(n);
                }
            }
        }
        max.saturating_add(1)
    }

    /// Claims the first free number from `start` on every root at once: the numbered folder
    /// is created with `create_dir` on each root in a fixed order; if any already has it, the
    /// ones just created are removed and the next number is tried. Returns the number.
    pub fn reserve(&self, roots: &[&Path], start: u64) -> std::io::Result<u64> {
        let Some(slot) = &self.slot else {
            return Ok(start);
        };
        let mut order: Vec<&Path> = roots.to_vec();
        order.sort();
        order.dedup();
        let mut n = start.max(1);
        for _ in 0..10_000 {
            let name = slot.name(n);
            let mut created: Vec<PathBuf> = Vec::new();
            let mut taken = false;
            for root in &order {
                let parent = self.parent(root);
                std::fs::create_dir_all(&parent)?;
                let dir = parent.join(&name);
                match std::fs::create_dir(&dir) {
                    Ok(()) => created.push(dir),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        taken = true;
                        break;
                    }
                    Err(e) => {
                        for c in &created {
                            let _ = std::fs::remove_dir(c);
                        }
                        return Err(std::io::Error::new(
                            e.kind(),
                            format!("cannot create {}: {e}", dir.display()),
                        ));
                    }
                }
            }
            if !taken {
                return Ok(n);
            }
            for c in &created {
                let _ = std::fs::remove_dir(c);
            }
            n = (n + 1).max(self.next_seq(order.iter().copied()));
        }
        Err(std::io::Error::other(
            "could not find a free folder number after 10,000 attempts",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Zoned {
        "2024-03-07T09:05:02[UTC]".parse().unwrap()
    }

    fn prod() -> ProductionInfo {
        ProductionInfo {
            show: Some("Big Show".into()),
            episode: Some("101".into()),
            shoot_day: Some("07".into()),
            camroll: Some("A001".into()),
            ..Default::default()
        }
    }

    fn expand(t: &str, p: &ProductionInfo) -> Result<Expanded, Vec<String>> {
        let now = now();
        Template::parse(t).unwrap().expand(&Values {
            now: &now,
            production: p,
            card: "A001_C001",
        })
    }

    #[test]
    fn every_token() {
        let e = expand(
            "{show}/{episode}/{date}_{time}/D{day}_{camroll}_{camrol}_{card}_{seq:3}/RAW",
            &prod(),
        )
        .unwrap();
        assert_eq!(
            e.folder(7),
            "Big Show/101/20240307_090502/D07_A001_A001_A001_C001_007/RAW"
        );
        assert_eq!(e.before, ["Big Show", "101", "20240307_090502"]);
        assert_eq!(e.after, ["RAW"]);
        assert!(e.warnings.is_empty());
    }

    #[test]
    fn formats_and_widths() {
        let p = prod();
        let f = |t: &str| expand(t, &p).unwrap().folder(42);
        assert_eq!(f("{date:%Y-%m-%d}"), "2024-03-07");
        assert_eq!(f("{date:%y%m%d}_{time:%Hh%M}"), "240307_09h05");
        assert_eq!(f("{date:%A %d %B}"), "Thursday 07 March");
        assert_eq!(f("{seq}"), "42");
        assert_eq!(f("{seq:1}"), "42");
        assert_eq!(f("{seq:4}"), "0042");
        assert_eq!(f("R{seq:2}"), "R42");
        assert_eq!(expand("{seq:2}", &p).unwrap().folder(123), "123");
        assert_eq!(f("{{literal}}_{card}"), "{literal}_A001_C001");
    }

    #[test]
    fn parse_errors() {
        for (t, needle) in [
            ("", "is empty"),
            ("{nope}", "unknown token {nope}"),
            ("{show", "unclosed '{'"),
            ("show}", "unmatched '}'"),
            ("{sh{ow}", "unclosed '{'"),
            ("{}", "empty token"),
            ("/abs/{card}", "must be relative"),
            ("a//b", "empty folder name"),
            ("a/", "empty folder name"),
            ("a/../b", "'..' is not allowed"),
            ("a\\b", "use '/'"),
            ("{seq}/{seq}", "only once"),
            ("{seq:0}", "width"),
            ("{seq:x}", "width"),
            ("{seq:12}", "width"),
            ("{show:x}", "takes no argument"),
            ("{camrol:2}", "takes no argument"),
            ("{date:%Y/%m}", "cannot contain '/'"),
            ("{date:}", "needs a format"),
            ("{date:%K}", "invalid format"),
            ("{time:%H%}", "invalid format"),
        ] {
            let e = Template::parse(t).unwrap_err();
            assert!(e.to_string().contains(needle), "{t:?}: {e}");
        }
    }

    #[test]
    fn empty_values_are_errors_not_double_slashes() {
        let p = ProductionInfo::default();
        let problems = expand("{show}/{day}/{card}", &p).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("{show}") && problems[0].contains("--show"));
        assert!(problems[1].contains("{day}") && problems[1].contains("--day"));
        // A value that sanitizes to nothing is empty too.
        let p = ProductionInfo {
            show: Some(" ..".into()),
            ..Default::default()
        };
        assert!(expand("{show}", &p).is_err());
        // Literal text ending in a dot.
        assert!(expand("{card}/x.", &prod()).is_err());
    }

    #[test]
    fn sanitization() {
        assert_eq!(sanitize("Big/Show"), "Big_Show");
        assert_eq!(sanitize(r#"a\b:c*d?e"f<g>h|i"#), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize("tab\there\n"), "tab_here_");
        assert_eq!(sanitize("  padded  "), "padded");
        assert_eq!(sanitize("ends with dots..."), "ends with dots");
        assert_eq!(sanitize(".."), "");
        assert_eq!(sanitize("Ünïcödé 名前"), "Ünïcödé 名前");
        let p = ProductionInfo {
            show: Some("AC/DC: Live".into()),
            ..Default::default()
        };
        let e = expand("{show}", &p).unwrap();
        assert_eq!(e.folder(1), "AC_DC_ Live");
        assert_eq!(e.warnings.len(), 1);
        assert!(e.warnings[0].contains("'AC/DC: Live'"), "{:?}", e.warnings);
    }

    #[test]
    fn canonical_form() {
        let t = Template::parse("{camrol}_{date:%Y%m%d}_{time}_{seq}_{seq:3}x").unwrap_err();
        assert!(t.to_string().contains("only once"));
        let t = Template::parse("{ camrol }/{date:%Y%m%d}_{time:%H}_{{x}}_{seq:3}").unwrap();
        assert_eq!(t.to_string(), "{camroll}/{date}_{time:%H}_{{x}}_{seq:3}");
        assert_eq!(Template::parse(&t.to_string()).unwrap(), t);
    }

    #[test]
    fn seq_numbers_in_names() {
        let s = SeqSlot {
            prefix: "A001_".into(),
            suffix: "_x".into(),
            width: 3,
        };
        assert_eq!(s.name(5), "A001_005_x");
        assert_eq!(s.number_in("A001_005_x"), Some(5));
        assert_eq!(s.number_in("A001_1234_x"), Some(1234));
        assert_eq!(s.number_in("A001_05_x"), Some(5));
        assert_eq!(s.number_in("A001__x"), None);
        assert_eq!(s.number_in("A001_5a_x"), None);
        assert_eq!(s.number_in("B001_005_x"), None);
        // Decomposed names from HFS+ still match.
        let s = SeqSlot {
            prefix: "Caf\u{e9}_".into(),
            suffix: String::new(),
            width: 0,
        };
        assert_eq!(s.number_in("Cafe\u{301}_9"), Some(9));
    }

    #[test]
    fn next_and_reserve_across_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for d in ["Show/R001", "Show/R002", "Show/other", "Show/R9x"] {
            std::fs::create_dir_all(a.join(d)).unwrap();
        }
        std::fs::create_dir_all(b.join("Show/R004")).unwrap();
        std::fs::write(b.join("Show/R007"), b"a file still takes the number").unwrap();
        let e = expand("Show/R{seq:3}/RAW", &prod()).unwrap();
        let roots = [a.as_path(), b.as_path()];
        let n = e.next_seq(roots);
        assert_eq!(n, 8);
        // Another job took 8 on b only, after we computed it.
        std::fs::create_dir_all(b.join("Show/R008")).unwrap();
        let got = e.reserve(&roots, n).unwrap();
        assert_eq!(got, 9);
        assert!(a.join("Show/R009").is_dir() && b.join("Show/R009").is_dir());
        assert!(!a.join("Show/R008").exists(), "rolled back");
        // Many claims at once on overlapping destination sets, listed in different orders:
        // every number is unique and present on all of its claimant's destinations.
        let d = tmp.path().join("d");
        std::fs::create_dir(&d).unwrap();
        let sets: [[PathBuf; 2]; 3] = [
            [a.clone(), b.clone()],
            [d.clone(), a.clone()],
            [b.clone(), d.clone()],
        ];
        let got: Vec<(u64, [PathBuf; 2])> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..12)
                .map(|i| {
                    let set = sets[i % 3].clone();
                    let e = &e;
                    s.spawn(move || {
                        let roots = [set[0].as_path(), set[1].as_path()];
                        (e.reserve(&roots, e.next_seq(roots)).unwrap(), set)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut numbers: Vec<u64> = got.iter().map(|(n, _)| *n).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), 12, "{got:?}");
        for (n, set) in &got {
            for r in set {
                assert!(r.join(format!("Show/R{n:03}")).is_dir());
            }
        }
        // A missing parent folder is created.
        let c = tmp.path().join("c");
        std::fs::create_dir(&c).unwrap();
        assert_eq!(e.reserve(&[c.as_path()], 1).unwrap(), 1);
        assert!(c.join("Show/R001").is_dir());
    }
}
