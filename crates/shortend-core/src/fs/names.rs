//! Filename legality across filesystems and case-collision detection.

use std::collections::HashMap;

use super::FsType;
use super::relpath::RelPath;

/// Filesystems that follow Windows naming rules (no `<>:"\|?*`, no reserved device names,
/// no trailing dot or space, 255 UTF-16 units per name). On Windows every volume does.
/// Linux `fuseblk` mounts are almost always ntfs-3g or exfat-fuse, so they get the same,
/// stricter rules: refusing a legal name is recoverable, a failed write mid-offload is not.
pub fn windows_naming(fs: &FsType) -> bool {
    cfg!(windows)
        || matches!(
            fs,
            FsType::ExFat | FsType::Vfat | FsType::Ntfs | FsType::Smb
        )
        || matches!(fs, FsType::Other(name) if name == "fuseblk")
}

/// Why `name` (a single path component) can't be created on `fs`, if it can't.
pub fn component_problem(name: &str, fs: &FsType) -> Option<String> {
    if windows_naming(fs) {
        if name.encode_utf16().count() > 255 {
            return Some(format!("name longer than 255 UTF-16 units (limit on {fs})"));
        }
        if let Some(c) = name.chars().find(|&c| "<>:\"\\|?*".contains(c) || c < ' ') {
            return Some(format!("character {c:?} is not allowed on {fs}"));
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some(format!("trailing dot or space is not allowed on {fs}"));
        }
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if reserved {
            return Some(format!("'{stem}' is a reserved device name on {fs}"));
        }
    } else if name.len() > 255 {
        return Some(format!("name longer than 255 bytes (limit on {fs})"));
    }
    None
}

/// Pairs of paths that would map to the same entry on a filesystem that ignores case and/or
/// Unicode normalization (HFS+, APFS: "é" precomposed and "e" + combining accent are one name).
pub fn case_collisions<'a>(
    paths: impl IntoIterator<Item = &'a RelPath>,
    ignore_case: bool,
    ignore_normalization: bool,
) -> Vec<(RelPath, RelPath)> {
    use unicode_normalization::UnicodeNormalization;
    let mut seen: HashMap<String, &RelPath> = HashMap::new();
    let mut out = Vec::new();
    for p in paths {
        let mut key = p.as_str().to_owned();
        if ignore_normalization {
            key = key.nfd().collect();
        }
        if ignore_case {
            key = key.to_lowercase();
        }
        match seen.get(&key) {
            Some(first) => out.push(((*first).clone(), p.clone())),
            None => {
                seen.insert(key, p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exfat_rules() {
        let fs = FsType::ExFat;
        assert_eq!(component_problem("A001C001.mxf", &fs), None);
        assert!(component_problem("a:b", &fs).is_some());
        assert!(component_problem("trailing.", &fs).is_some());
        assert!(component_problem("CON", &fs).is_some());
        assert!(component_problem("com1.txt", &fs).is_some());
        assert_eq!(component_problem("COM0", &fs), None);
        assert_eq!(component_problem("CONSOLE.log", &fs), None);
        assert!(component_problem(&"é".repeat(256), &fs).is_some());
        assert_eq!(component_problem(&"é".repeat(255), &fs), None);
    }

    #[test]
    fn ext4_rules() {
        if cfg!(windows) {
            return;
        }
        let fs = FsType::Ext4;
        assert_eq!(component_problem("a:b?", &fs), None);
        assert!(component_problem(&"é".repeat(128), &fs).is_some()); // 256 bytes
    }

    #[test]
    fn collisions() {
        let a = RelPath::new("Clip/A.MOV").unwrap();
        let b = RelPath::new("clip/a.mov").unwrap();
        let c = RelPath::new("clip/b.mov").unwrap();
        let found = case_collisions([&a, &b, &c], true, false);
        assert_eq!(found, vec![(a.clone(), b.clone())]);
        assert!(case_collisions([&a, &b, &c], false, false).is_empty());
        let nfc = RelPath::new("caf\u{e9}.mov").unwrap();
        let nfd = RelPath::new("cafe\u{301}.mov").unwrap();
        assert_eq!(
            case_collisions([&nfc, &nfd], false, true),
            vec![(nfc.clone(), nfd.clone())]
        );
        assert!(case_collisions([&nfc, &nfd], true, false).is_empty());
    }
}
