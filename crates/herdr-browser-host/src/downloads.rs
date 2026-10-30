//! Where downloads land: `~/Downloads` (or `$HERDR_BROWSER_DOWNLOADS`, which tests set), never
//! overwriting a file that is already there.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The folder downloads go to.
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HERDR_BROWSER_DOWNLOADS").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Downloads")
}

/// A file name a page cannot use to climb out of the folder: no separators, no leading dots.
pub fn safe_name(suggested: &str) -> String {
    let base = suggested.rsplit(['/', '\\']).next().unwrap_or("");
    let base: String = base.chars().filter(|c| !c.is_control() && *c != ':').collect();
    let base = base.trim().trim_start_matches('.').trim();
    if base.is_empty() { "download".into() } else { base.into() }
}

/// `dir/name`, or `dir/name (1).ext`, `(2)`, ... when taken (as Chrome and Safari do).
pub fn unique_path(dir: &Path, suggested: &str) -> PathBuf {
    unique_path_except(dir, suggested, &HashSet::new())
}

/// Like `unique_path`, also skipping paths `reserved` for downloads still being written.
pub fn unique_path_except(dir: &Path, suggested: &str, reserved: &HashSet<PathBuf>) -> PathBuf {
    let taken = |p: &Path| p.exists() || reserved.contains(p);
    let name = safe_name(suggested);
    let first = dir.join(&name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !taken(p))
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_stay_in_the_folder() {
        let d = std::env::temp_dir().join(format!("herdr-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(unique_path(&d, "report.pdf"), d.join("report.pdf"));
        std::fs::write(d.join("report.pdf"), b"x").unwrap();
        assert_eq!(unique_path(&d, "report.pdf"), d.join("report (1).pdf"));
        std::fs::write(d.join("report (1).pdf"), b"x").unwrap();
        assert_eq!(unique_path(&d, "report.pdf"), d.join("report (2).pdf"));
        std::fs::write(d.join("notes"), b"x").unwrap();
        assert_eq!(unique_path(&d, "notes"), d.join("notes (1)"));
        assert_eq!(unique_path(&d, "../../etc/passwd"), d.join("passwd"));
        assert_eq!(unique_path(&d, ".hidden"), d.join("hidden"));
        assert_eq!(unique_path(&d, ""), d.join("download"));
        let reserved = HashSet::from([d.join("a.zip"), d.join("a (1).zip")]);
        assert_eq!(unique_path_except(&d, "a.zip", &reserved), d.join("a (2).zip"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
