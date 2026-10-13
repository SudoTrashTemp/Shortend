//! The Offload form: what the user is editing, convertible to and from a [`Preset`] so the
//! GUI uses exactly the engine's preset/override rules.

use std::num::NonZeroU8;
use std::path::{Path, PathBuf};

use shortend_core::naming::{Template, Values};
use shortend_core::preset::{PdfSetting, Preset, PresetDestination};
use shortend_core::report::mhl::MhlVersion;
use shortend_core::{HashAlgorithm, HashList, JobSettings, JobSpec, PdfOptions, ProductionInfo};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DestRow {
    pub label: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub source: String,
    pub dests: Vec<DestRow>,
    /// Folder template; empty means the card's name.
    pub folder: String,
    pub primary: HashAlgorithm,
    /// Extra algorithms computed in the same read pass (never contains `primary`).
    pub extra: Vec<HashAlgorithm>,
    pub mhl_v2: bool,
    pub mhl_v11: bool,
    pub csv: bool,
    pub txt: bool,
    pub pdf: bool,
    pub logo: String,
    pub reread: bool,
    pub parallel: u8,
    pub stall_timeout: u32,
    pub show: String,
    pub episode: String,
    pub day: String,
    pub camroll: String,
    pub operator: String,
    pub company: String,
    pub notes: String,
}

impl Default for Form {
    fn default() -> Self {
        let d = JobSettings::default();
        Self {
            source: String::new(),
            dests: vec![DestRow::default()],
            folder: String::new(),
            primary: d.hashes.primary(),
            extra: Vec::new(),
            mhl_v2: true,
            mhl_v11: false,
            csv: true,
            txt: true,
            pdf: true,
            logo: String::new(),
            reread: d.source_reread,
            parallel: d.parallel_files.get(),
            stall_timeout: d.stall_timeout_secs,
            show: String::new(),
            episode: String::new(),
            day: String::new(),
            camroll: String::new(),
            operator: String::new(),
            company: String::new(),
            notes: String::new(),
        }
    }
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

impl Form {
    pub fn hashes(&self) -> HashList {
        let mut v = vec![self.primary];
        v.extend(
            HashAlgorithm::ALL
                .into_iter()
                .filter(|a| *a != self.primary && self.extra.contains(a)),
        );
        HashList::new(v).expect("primary first, no duplicates")
    }

    pub fn production(&self) -> ProductionInfo {
        ProductionInfo {
            show: opt(&self.show),
            episode: opt(&self.episode),
            shoot_day: opt(&self.day),
            camroll: opt(&self.camroll),
            operator: opt(&self.operator),
            company: opt(&self.company),
            notes: opt(&self.notes),
        }
    }

    /// Every setting as a preset (sources are not part of presets).
    pub fn to_preset(&self) -> Result<Preset, String> {
        let mut p = Preset::new();
        if let Some(f) = opt(&self.folder) {
            p.naming.folder = Some(Template::parse(&f).map_err(|e| e.to_string())?);
        }
        p.settings.hashes = Some(self.hashes());
        p.settings.source_reread = Some(self.reread);
        p.settings.parallel_files = NonZeroU8::new(self.parallel.max(1));
        p.settings.stall_timeout_secs = Some(self.stall_timeout);
        p.reports.csv = Some(self.csv);
        p.reports.txt = Some(self.txt);
        let mut mhl = Vec::new();
        if self.mhl_v2 {
            mhl.push(MhlVersion::V2);
        }
        if self.mhl_v11 {
            mhl.push(MhlVersion::V1_1);
        }
        p.reports.mhl = Some(mhl);
        p.reports.pdf = Some(if self.pdf {
            PdfSetting::On(PdfOptions {
                logo: opt(&self.logo).map(PathBuf::from),
            })
        } else {
            PdfSetting::Off
        });
        for (i, d) in self.dests.iter().enumerate() {
            let Some(root) = opt(&d.root) else { continue };
            let root = PathBuf::from(root);
            if !root.has_root() {
                return Err(format!(
                    "destination {}: '{}' is not a full path",
                    i + 1,
                    root.display()
                ));
            }
            p.destinations.push(PresetDestination {
                label: opt(&d.label),
                root,
            });
        }
        p.production = self.production();
        Ok(p)
    }

    pub fn to_spec(&self) -> Result<JobSpec, String> {
        let source = opt(&self.source).ok_or("choose a source card or folder")?;
        let mut spec = self.to_preset()?.job_spec(PathBuf::from(source));
        if spec.destinations.is_empty() {
            return Err("add at least one destination".into());
        }
        spec.settings.retries = JobSettings::default().retries;
        Ok(spec)
    }

    /// The form with `p` applied on top: everything the preset sets replaces the form's
    /// value; the source is kept.
    pub fn apply_preset(&self, p: &Preset) -> Form {
        let mut f = self.clone();
        if let Some(t) = &p.naming.folder {
            f.folder = t.to_string();
        }
        let s = &p.settings;
        if let Some(h) = &s.hashes {
            f.primary = h.primary();
            f.extra = h.as_slice()[1..].to_vec();
        }
        if let Some(v) = s.source_reread {
            f.reread = v;
        }
        if let Some(v) = s.parallel_files {
            f.parallel = v.get();
        }
        if let Some(v) = s.stall_timeout_secs {
            f.stall_timeout = v;
        }
        let r = &p.reports;
        if let Some(v) = r.csv {
            f.csv = v;
        }
        if let Some(v) = r.txt {
            f.txt = v;
        }
        if let Some(m) = &r.mhl {
            f.mhl_v2 = m.contains(&MhlVersion::V2);
            f.mhl_v11 = m.contains(&MhlVersion::V1_1);
        }
        match &r.pdf {
            Some(PdfSetting::Off) => f.pdf = false,
            Some(PdfSetting::On(o)) => {
                f.pdf = true;
                f.logo = o
                    .logo
                    .as_ref()
                    .map(|l| l.display().to_string())
                    .unwrap_or_default();
            }
            None => {}
        }
        if !p.destinations.is_empty() {
            f.dests = p
                .destinations
                .iter()
                .map(|d| DestRow {
                    label: d.label.clone().unwrap_or_default(),
                    root: d.root.display().to_string(),
                })
                .collect();
        }
        let x = &p.production;
        let set = |dst: &mut String, v: &Option<String>| {
            if let Some(v) = v {
                dst.clone_from(v);
            }
        };
        set(&mut f.show, &x.show);
        set(&mut f.episode, &x.episode);
        set(&mut f.day, &x.shoot_day);
        set(&mut f.camroll, &x.camroll);
        set(&mut f.operator, &x.operator);
        set(&mut f.company, &x.company);
        set(&mut f.notes, &x.notes);
        f
    }

    /// What the folder will look like, with `{seq}` shown as `#`s (the number is found when
    /// the job is checked). Errors are the template's own messages.
    pub fn folder_preview(&self) -> Result<String, String> {
        let card = Path::new(self.source.trim())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "CARD".to_owned());
        let Some(text) = opt(&self.folder) else {
            return Ok(card);
        };
        let t = Template::parse(&text).map_err(|e| e.message)?;
        let now = jiff::Zoned::now();
        let production = self.production();
        let e = t
            .expand(&Values {
                now: &now,
                production: &production,
                card: &card,
            })
            .map_err(|p| p.join("; "))?;
        let mut folder = e.folder(0);
        if let Some(slot) = &e.slot {
            let numbered = slot.name(0);
            let hashes = "#".repeat(slot.width.max(1));
            let shown = format!("{}{hashes}{}", slot.prefix, slot.suffix);
            folder = folder.replacen(&numbered, &shown, 1);
        }
        Ok(folder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trip_through_the_form() {
        let text = include_str!("../../../examples/bigshow.toml");
        let p = Preset::from_toml(text, Some(Path::new("/presets"))).unwrap();
        let f = Form::default().apply_preset(&p);
        assert_eq!(f.dests.len(), 2);
        assert_eq!(f.extra, [HashAlgorithm::Md5]);
        assert!(f.mhl_v2 && f.mhl_v11);
        let back = f.to_preset().unwrap();
        assert_eq!(back.naming, p.naming);
        assert_eq!(back.settings.hashes, p.settings.hashes);
        assert_eq!(back.destinations, p.destinations);
        assert_eq!(back.production, p.production);
    }

    #[test]
    fn preview_and_errors() {
        let mut f = Form {
            source: "/media/A001".into(),
            ..Form::default()
        };
        assert_eq!(f.folder_preview().unwrap(), "A001");
        f.folder = "{show}/{card}_{seq:3}".into();
        assert!(f.folder_preview().unwrap_err().contains("{show}"));
        f.show = "Big Show".into();
        assert_eq!(f.folder_preview().unwrap(), "Big Show/A001_###");
        f.folder = "{nope}".into();
        assert!(f.folder_preview().unwrap_err().contains("unknown token"));
        f.folder.clear();
        f.dests[0].root = "relative".into();
        assert!(f.to_spec().unwrap_err().contains("not a full path"));
    }

    #[test]
    fn hash_order_is_primary_then_extras() {
        let f = Form {
            primary: HashAlgorithm::Md5,
            extra: vec![
                HashAlgorithm::Sha1,
                HashAlgorithm::Md5,
                HashAlgorithm::Xxh64,
            ],
            ..Form::default()
        };
        assert_eq!(
            f.hashes().as_slice(),
            [
                HashAlgorithm::Md5,
                HashAlgorithm::Xxh64,
                HashAlgorithm::Sha1
            ]
        );
    }
}
