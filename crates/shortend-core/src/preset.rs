//! Presets: shareable, human-readable TOML files holding a job's naming template, settings,
//! reports, destinations and production details. Every field is optional; a preset only
//! states what it sets, and command-line flags override it field by field ([`Preset::overlay`]).
//!
//! ```toml
//! format_version = 1
//! name = "Big Show S01"
//!
//! [naming]
//! folder = "{show}/{date:%Y%m%d}_D{day}/{camroll}_{seq:3}"
//!
//! [settings]
//! hashes = ["xxh64", "md5"]
//!
//! [reports]
//! mhl = ["v2", "v1.1"]
//! pdf = { logo = "bigshow.png" }   # relative to the preset file
//!
//! [[destinations]]
//! label = "RAID"
//! root = "/mnt/raid/BIGSHOW"
//!
//! [production]
//! show = "Big Show"
//! ```
//!
//! Unknown keys are errors (a typo must not silently fall back to a default), and so is a
//! `format_version` this build does not know.

use std::fmt;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::hash::{HashAlgorithm, HashList};
use crate::model::{JobSettings, JobSpec, PdfOptions, ProductionInfo, ReportOptions};
use crate::naming::Template;
use crate::report::mhl::MhlVersion;

/// The preset format this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{message}", .path.as_ref().map(|p| format!("preset {}: ", p.display())).unwrap_or_default())]
pub struct PresetError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl PresetError {
    fn new(path: Option<&Path>, message: impl Into<String>) -> Self {
        Self {
            path: path.map(Path::to_path_buf),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Required; see [`FORMAT_VERSION`].
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Naming::is_empty")]
    pub naming: Naming,
    #[serde(default, skip_serializing_if = "Settings::is_empty")]
    pub settings: Settings,
    #[serde(default, skip_serializing_if = "Reports::is_empty")]
    pub reports: Reports,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<PresetDestination>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub production: ProductionInfo,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Naming {
    /// Folder template created under every destination (see [`crate::naming`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<Template>,
}

impl Naming {
    fn is_empty(&self) -> bool {
        self.folder.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// One algorithm (`"xxh64"`) or several computed in the same read pass
    /// (`["xxh64", "md5"]`); the first is the primary.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_hashes",
        serialize_with = "ser_hashes"
    )]
    pub hashes: Option<HashList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reread: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_files: Option<NonZeroU8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_timeout_secs: Option<u32>,
}

impl Settings {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reports {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csv: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt: Option<bool>,
    /// `"v2"`, `"v1.1"`, `["v2", "v1.1"]`, or `"none"` / `[]`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_mhl",
        serialize_with = "ser_mhl"
    )]
    pub mhl: Option<Vec<MhlVersion>>,
    /// `false`, `true`, or `{ logo = "file.png" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfSetting>,
}

impl Reports {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The `pdf` key: off, on, or on with options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfSetting {
    Off,
    On(PdfOptions),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetDestination {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub root: PathBuf,
}

impl Preset {
    pub fn new() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            ..Self::default()
        }
    }

    /// Parses and validates a preset. Relative paths (the PDF logo) are resolved against
    /// `base_dir`, normally the preset file's folder; `~/` is expanded.
    pub fn from_toml(text: &str, base_dir: Option<&Path>) -> Result<Self, PresetError> {
        Self::parse(text, base_dir, None)
    }

    pub fn load(path: &Path) -> Result<Self, PresetError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PresetError::new(Some(path), format!("cannot read: {e}")))?;
        Self::parse(&text, path.parent(), Some(path))
    }

    fn parse(text: &str, base: Option<&Path>, path: Option<&Path>) -> Result<Self, PresetError> {
        // The version is checked before anything else, so a newer file gets a clear message
        // rather than an error about a key this build does not know.
        #[derive(Deserialize)]
        struct Version {
            format_version: Option<toml::Value>,
        }
        let v: Version = toml::from_str(text)
            .map_err(|e| PresetError::new(path, e.to_string().trim_end().to_owned()))?;
        match v.format_version {
            None => {
                return Err(PresetError::new(
                    path,
                    format!("missing `format_version` (this build writes {FORMAT_VERSION})"),
                ));
            }
            Some(toml::Value::Integer(n)) if n == i64::from(FORMAT_VERSION) => {}
            Some(toml::Value::Integer(n)) if n > i64::from(FORMAT_VERSION) => {
                return Err(PresetError::new(
                    path,
                    format!(
                        "format_version {n} was written by a newer ShortEnd (this one reads {FORMAT_VERSION}); update ShortEnd"
                    ),
                ));
            }
            Some(other) => {
                return Err(PresetError::new(
                    path,
                    format!("unsupported format_version {other} (expected {FORMAT_VERSION})"),
                ));
            }
        }
        let mut p: Preset = toml::from_str(text)
            .map_err(|e| PresetError::new(path, e.to_string().trim_end().to_owned()))?;
        for d in &mut p.destinations {
            d.root = expand_home(&d.root);
            if !d.root.has_root() {
                return Err(PresetError::new(
                    path,
                    format!(
                        "destination root '{}' must be an absolute path",
                        d.root.display()
                    ),
                ));
            }
        }
        if let Some(PdfSetting::On(PdfOptions { logo: Some(logo) })) = &mut p.reports.pdf {
            let expanded = expand_home(logo);
            *logo = match base {
                Some(b) if expanded.is_relative() => b.join(expanded),
                _ => expanded,
            };
        }
        Ok(p)
    }

    /// Canonical TOML: only the keys that are set, templates in canonical form (`{camroll}`).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("presets always serialize")
    }

    /// Writes the preset; refuses to replace an existing file unless `overwrite`.
    pub fn save(&self, path: &Path, overwrite: bool) -> Result<(), PresetError> {
        let err = |e: std::io::Error| PresetError::new(Some(path), e.to_string());
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(err)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true);
        if overwrite {
            opts.create(true).truncate(true);
        } else {
            opts.create_new(true);
        }
        let mut f = opts.open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                PresetError::new(Some(path), "already exists (use --force to replace it)")
            } else {
                err(e)
            }
        })?;
        use std::io::Write;
        f.write_all(self.to_toml().as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(err)
    }

    /// `other`'s settings win wherever it sets them; destinations are replaced as a whole
    /// when `other` lists any.
    pub fn overlay(&self, other: &Preset) -> Preset {
        let s = &self.settings;
        let o = &other.settings;
        let r = &self.reports;
        let q = &other.reports;
        let pick = |a: &Option<String>, b: &Option<String>| b.clone().or_else(|| a.clone());
        let (p, x) = (&self.production, &other.production);
        Preset {
            format_version: FORMAT_VERSION,
            name: other.name.clone().or_else(|| self.name.clone()),
            naming: Naming {
                folder: other
                    .naming
                    .folder
                    .clone()
                    .or_else(|| self.naming.folder.clone()),
            },
            settings: Settings {
                hashes: o.hashes.clone().or_else(|| s.hashes.clone()),
                source_reread: o.source_reread.or(s.source_reread),
                parallel_files: o.parallel_files.or(s.parallel_files),
                retries: o.retries.or(s.retries),
                stall_timeout_secs: o.stall_timeout_secs.or(s.stall_timeout_secs),
            },
            reports: Reports {
                csv: q.csv.or(r.csv),
                txt: q.txt.or(r.txt),
                mhl: q.mhl.clone().or_else(|| r.mhl.clone()),
                pdf: match (&r.pdf, &q.pdf) {
                    // A logo given alone keeps the PDF on.
                    (_, Some(x)) => Some(x.clone()),
                    (a, None) => a.clone(),
                },
            },
            destinations: if other.destinations.is_empty() {
                self.destinations.clone()
            } else {
                other.destinations.clone()
            },
            production: ProductionInfo {
                show: pick(&p.show, &x.show),
                episode: pick(&p.episode, &x.episode),
                shoot_day: pick(&p.shoot_day, &x.shoot_day),
                camroll: pick(&p.camroll, &x.camroll),
                operator: pick(&p.operator, &x.operator),
                company: pick(&p.company, &x.company),
                notes: pick(&p.notes, &x.notes),
            },
        }
    }

    /// A job for `source` with everything the preset leaves unset at its default.
    pub fn job_spec(&self, source: PathBuf) -> JobSpec {
        let d = JobSettings::default();
        let s = &self.settings;
        let r = &self.reports;
        JobSpec {
            source,
            card_label: None,
            destinations: self
                .destinations
                .iter()
                .enumerate()
                .map(|(i, dst)| {
                    let label = dst.label.clone().unwrap_or_else(|| {
                        dst.root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("Destination {}", i + 1))
                    });
                    (label, dst.root.clone())
                })
                .collect(),
            folder: self.naming.folder.as_ref().map(ToString::to_string),
            settings: JobSettings {
                hashes: s.hashes.clone().unwrap_or(d.hashes),
                source_reread: s.source_reread.unwrap_or(d.source_reread),
                parallel_files: s.parallel_files.unwrap_or(d.parallel_files),
                retries: s.retries.unwrap_or(d.retries),
                stall_timeout_secs: s.stall_timeout_secs.unwrap_or(d.stall_timeout_secs),
                chunk_size: d.chunk_size,
                reports: ReportOptions {
                    csv: r.csv.unwrap_or(d.reports.csv),
                    txt: r.txt.unwrap_or(d.reports.txt),
                    mhl: r.mhl.clone().unwrap_or(d.reports.mhl),
                    pdf: match &r.pdf {
                        None => d.reports.pdf,
                        Some(PdfSetting::Off) => None,
                        Some(PdfSetting::On(o)) => Some(o.clone()),
                    },
                },
            },
            production: self.production.clone(),
        }
    }
}

/// Where named presets live. `SHORTEND_CONFIG_DIR` overrides the per-OS default
/// (`$XDG_CONFIG_HOME/shortend`, `~/Library/Application Support/ShortEnd`, `%APPDATA%\ShortEnd`).
pub fn presets_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SHORTEND_CONFIG_DIR") {
        return PathBuf::from(p).join("presets");
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let base = if cfg!(target_os = "macos") {
        home().join("Library/Application Support/ShortEnd")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("ShortEnd")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".config"))
            .join("shortend")
    };
    base.join("presets")
}

/// A preset argument: a path to a file (anything with a separator or a `.toml` ending), or
/// the name of a preset in [`presets_dir`].
pub fn locate(name_or_path: &str) -> PathBuf {
    let p = Path::new(name_or_path);
    if name_or_path.contains(['/', '\\']) || name_or_path.ends_with(".toml") {
        p.to_path_buf()
    } else {
        presets_dir().join(format!("{name_or_path}.toml"))
    }
}

/// Named presets in [`presets_dir`], sorted by name, each loaded (or the reason it won't load).
pub fn list() -> Vec<(String, PathBuf, Result<Preset, PresetError>)> {
    let mut out: Vec<_> = std::fs::read_dir(presets_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| {
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let loaded = Preset::load(&p);
            (name, p, loaded)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn expand_home(p: &Path) -> PathBuf {
    let Ok(rest) = p.strip_prefix("~") else {
        return p.to_path_buf();
    };
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match home {
        Some(h) => PathBuf::from(h).join(rest),
        None => p.to_path_buf(),
    }
}

// --- serde helpers --------------------------------------------------------------------------

/// A string or a list of strings.
fn str_or_list<'de, D: Deserializer<'de>>(
    d: D,
    what: &'static str,
) -> Result<Vec<String>, D::Error> {
    struct V(&'static str);
    impl<'de> Visitor<'de> for V {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{} as a string or a list of strings", self.0)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_owned()])
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }
    d.deserialize_any(V(what))
}

fn de_hashes<'de, D: Deserializer<'de>>(d: D) -> Result<Option<HashList>, D::Error> {
    let names = str_or_list(d, "checksum algorithms")?;
    let algos = names
        .iter()
        .map(|n| n.parse::<HashAlgorithm>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(de::Error::custom)?;
    HashList::new(algos).map(Some).map_err(de::Error::custom)
}

fn ser_hashes<S: Serializer>(h: &Option<HashList>, s: S) -> Result<S::Ok, S::Error> {
    let names: Vec<&str> = h
        .as_ref()
        .map(|h| h.as_slice().iter().map(|a| a.name()).collect())
        .unwrap_or_default();
    match names.as_slice() {
        [one] => s.serialize_str(one),
        many => many.serialize(s),
    }
}

fn de_mhl<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<MhlVersion>>, D::Error> {
    let names = str_or_list(d, "manifest versions")?;
    if names == ["none"] {
        return Ok(Some(Vec::new()));
    }
    let mut out = Vec::new();
    for n in names {
        let v = match n.as_str() {
            "v2" => MhlVersion::V2,
            "v1.1" => MhlVersion::V1_1,
            other => {
                return Err(de::Error::custom(format!(
                    "unknown manifest '{other}' (expected \"v2\", \"v1.1\" or \"none\")"
                )));
            }
        };
        if out.contains(&v) {
            return Err(de::Error::custom(format!("manifest {v} listed twice")));
        }
        out.push(v);
    }
    Ok(Some(out))
}

fn ser_mhl<S: Serializer>(m: &Option<Vec<MhlVersion>>, s: S) -> Result<S::Ok, S::Error> {
    match m.as_deref().unwrap_or_default() {
        [] => s.serialize_str("none"),
        [one] => one.serialize(s),
        many => many.serialize(s),
    }
}

impl Serialize for PdfSetting {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => s.serialize_bool(false),
            Self::On(PdfOptions { logo: None }) => s.serialize_bool(true),
            Self::On(o) => o.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for PdfSetting {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = PdfSetting;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("true, false, or a table such as { logo = \"logo.png\" }")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(if v {
                    PdfSetting::On(PdfOptions::default())
                } else {
                    PdfSetting::Off
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                PdfOptions::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(PdfSetting::On)
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
format_version = 1
name = "Big Show S01"

[naming]
folder = "{show}/{date:%Y%m%d}_D{day}/{camrol}_{card}_{seq:3}"

[settings]
hashes = ["xxh64", "md5"]
source_reread = true
parallel_files = 3
retries = 2
stall_timeout_secs = 30

[reports]
csv = true
txt = false
mhl = ["v2", "v1.1"]
pdf = { logo = "logos/bigshow.png" }

[[destinations]]
label = "RAID"
root = "/mnt/raid/BIGSHOW"

[[destinations]]
root = "/media/dit/SHUTTLE_A"

[production]
show = "Big Show"
episode = "101"
day = "07"
company = "Example Productions"
operator = "Jane Doe"
"#;

    #[test]
    fn full_preset_loads_and_round_trips() {
        let p = Preset::from_toml(FULL, Some(Path::new("/presets"))).unwrap();
        assert_eq!(p.name.as_deref(), Some("Big Show S01"));
        assert_eq!(
            p.settings.hashes.as_ref().unwrap().as_slice(),
            [HashAlgorithm::Xxh64, HashAlgorithm::Md5]
        );
        assert_eq!(
            p.reports.pdf,
            Some(PdfSetting::On(PdfOptions {
                logo: Some(PathBuf::from("/presets/logos/bigshow.png"))
            }))
        );
        assert_eq!(p.production.shoot_day.as_deref(), Some("07"));
        let text = p.to_toml();
        assert!(
            text.contains("{camroll}_{card}"),
            "canonical token:\n{text}"
        );
        assert!(!text.contains("camrol}"), "{text}");
        assert!(text.contains("day = \"07\""), "{text}");
        let again = Preset::from_toml(&text, None).unwrap();
        assert_eq!(again, p);

        let spec = p.job_spec(PathBuf::from("/card"));
        assert_eq!(
            spec.destinations[0],
            ("RAID".into(), "/mnt/raid/BIGSHOW".into())
        );
        assert_eq!(spec.destinations[1].0, "SHUTTLE_A");
        assert_eq!(
            spec.folder.as_deref(),
            Some("{show}/{date}_D{day}/{camroll}_{card}_{seq:3}")
        );
        assert_eq!(spec.settings.parallel_files.get(), 3);
        assert_eq!(spec.settings.stall_timeout_secs, 30);
        assert!(spec.settings.source_reread && spec.settings.reports.csv);
        assert!(!spec.settings.reports.txt);
        assert_eq!(
            spec.settings.reports.mhl,
            [MhlVersion::V2, MhlVersion::V1_1]
        );
    }

    #[test]
    fn minimal_preset_uses_defaults() {
        let p = Preset::from_toml("format_version = 1", None).unwrap();
        assert_eq!(p, Preset::new());
        assert_eq!(p.to_toml().trim(), "format_version = 1");
        let spec = p.job_spec(PathBuf::from("/card"));
        assert_eq!(spec.settings, JobSettings::default());
        assert!(spec.folder.is_none() && spec.destinations.is_empty());
    }

    #[test]
    fn single_values_and_switches() {
        let p = Preset::from_toml(
            "format_version = 1\n[settings]\nhashes = \"sha1\"\n[reports]\nmhl = \"none\"\npdf = false\n",
            None,
        )
        .unwrap();
        assert_eq!(
            p.settings.hashes,
            Some(HashList::single(HashAlgorithm::Sha1))
        );
        let spec = p.job_spec("/c".into());
        assert!(spec.settings.reports.mhl.is_empty());
        assert!(spec.settings.reports.pdf.is_none());
        let text = p.to_toml();
        assert!(
            text.contains("hashes = \"sha1\"")
                && text.contains("mhl = \"none\"")
                && text.contains("pdf = false"),
            "{text}"
        );
        assert_eq!(Preset::from_toml(&text, None).unwrap(), p);
        let on = Preset::from_toml(
            "format_version = 1\n[reports]\npdf = true\nmhl = \"v1.1\"",
            None,
        )
        .unwrap();
        assert_eq!(
            on.job_spec("/c".into()).settings.reports.pdf,
            Some(PdfOptions::default())
        );
        assert_eq!(on.reports.mhl, Some(vec![MhlVersion::V1_1]));
    }

    #[test]
    fn rejections() {
        for (text, needle) in [
            ("name = \"x\"", "missing `format_version`"),
            ("format_version = 2", "newer ShortEnd"),
            ("format_version = 0", "unsupported format_version"),
            ("format_version = \"1\"", "unsupported format_version"),
            (
                "format_version = 1\ncolour = \"red\"",
                "unknown field `colour`",
            ),
            (
                "format_version = 1\n[settings]\nhash = \"xxh64\"",
                "unknown field `hash`",
            ),
            (
                "format_version = 1\n[settings]\nhashes = \"crc32\"",
                "unknown checksum algorithm 'crc32'",
            ),
            (
                "format_version = 1\n[settings]\nhashes = []",
                "at least one",
            ),
            (
                "format_version = 1\n[settings]\nhashes = [\"md5\", \"md5\"]",
                "listed twice",
            ),
            (
                "format_version = 1\n[settings]\nparallel_files = 0",
                "nonzero",
            ),
            (
                "format_version = 1\n[reports]\nmhl = \"v3\"",
                "unknown manifest 'v3'",
            ),
            (
                "format_version = 1\n[reports]\npdf = { logo = \"a.png\", thumbnails = true }",
                "unknown field `thumbnails`",
            ),
            (
                "format_version = 1\n[reports]\npdf = \"yes\"",
                "true, false, or a table",
            ),
            (
                "format_version = 1\n[naming]\nfolder = \"{nope}\"",
                "unknown token {nope}",
            ),
            (
                "format_version = 1\n[naming]\nfolder = \"{show\"",
                "unclosed",
            ),
            (
                "format_version = 1\n[[destinations]]\nroot = \"relative/dir\"",
                "must be an absolute path",
            ),
            (
                "format_version = 1\n[[destinations]]\nlabel = \"x\"",
                "missing field `root`",
            ),
            (
                "format_version = 1\n[production]\ndirector = \"x\"",
                "unknown field `director`",
            ),
            ("format_version = 1\n[settings", "TOML parse error"),
        ] {
            let e = Preset::from_toml(text, None).unwrap_err();
            assert!(e.message.contains(needle), "{text:?}: {e}");
        }
    }

    #[test]
    fn overlay_overrides_field_by_field() {
        let base = Preset::from_toml(FULL, None).unwrap();
        let mut flags = Preset::new();
        flags.production.camroll = Some("B002".into());
        flags.production.show = Some("Other Show".into());
        flags.settings.hashes = Some(HashList::single(HashAlgorithm::Xxh128));
        flags.reports.pdf = Some(PdfSetting::Off);
        let m = base.overlay(&flags);
        assert_eq!(m.production.camroll.as_deref(), Some("B002"));
        assert_eq!(m.production.show.as_deref(), Some("Other Show"));
        assert_eq!(m.production.operator.as_deref(), Some("Jane Doe"));
        assert_eq!(m.settings.hashes, flags.settings.hashes);
        assert_eq!(m.settings.parallel_files, base.settings.parallel_files);
        assert_eq!(m.reports.pdf, Some(PdfSetting::Off));
        assert_eq!(
            m.destinations, base.destinations,
            "kept when flags give none"
        );
        flags.destinations = vec![PresetDestination {
            label: None,
            root: "/x".into(),
        }];
        assert_eq!(
            base.overlay(&flags).destinations,
            flags.destinations,
            "replaced"
        );
    }

    #[test]
    fn save_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/p.toml");
        let p = Preset::from_toml(FULL, Some(&tmp.path().join("sub"))).unwrap();
        p.save(&path, false).unwrap();
        let e = p.save(&path, false).unwrap_err();
        assert!(e.message.contains("already exists"), "{e}");
        p.save(&path, true).unwrap();
        assert_eq!(Preset::load(&path).unwrap(), p);
    }

    #[test]
    fn shipped_example_is_valid() {
        let text = include_str!("../../../examples/bigshow.toml");
        let p = Preset::from_toml(text, Some(Path::new("/presets"))).unwrap();
        assert_eq!(p.destinations.len(), 2);
        assert_eq!(p.settings.hashes.as_ref().unwrap().as_slice().len(), 2);
        assert_eq!(Preset::from_toml(&p.to_toml(), None).unwrap(), p);
    }

    #[test]
    fn locate_names_and_paths() {
        assert_eq!(locate("./x.toml"), PathBuf::from("./x.toml"));
        assert_eq!(locate("x.toml"), PathBuf::from("x.toml"));
        assert!(locate("bigshow").ends_with("presets/bigshow.toml"));
    }
}
