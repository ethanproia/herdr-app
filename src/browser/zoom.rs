//! Page zoom (I10): Chrome's steps, cmd-plus / cmd-minus / cmd-0, remembered per host name in
//! `$HERDR_APP_HOME/browser/zoom.json` (`{ "example.com": 1.0, ... }`, Chrome zoom levels).

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Chrome's zoom presets (factors): 25 % to 500 %.
pub const STEPS: [f64; 17] = [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0];

/// A factor as a Chrome zoom level (`1.2^level = factor`).
pub fn level_of(factor: f64) -> f64 {
    factor.ln() / 1.2f64.ln()
}

pub fn factor_of(level: f64) -> f64 {
    1.2f64.powf(level)
}

/// The next preset in or out from `level` (from 100 % in is 110 %, out is 90 %).
pub fn step(level: f64, zoom_in: bool) -> f64 {
    let f = factor_of(level);
    let next = if zoom_in { STEPS.iter().copied().find(|s| *s > f + 0.001) } else { STEPS.iter().rev().copied().find(|s| *s < f - 0.001) };
    level_of(next.unwrap_or(f))
}

/// "125%" (rounded), as the bar shows it.
pub fn percent(level: f64) -> String {
    format!("{}%", (factor_of(level) * 100.0).round() as i64)
}

/// The host name zoom is remembered under (None for about:, data:, file: pages).
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.rsplit_once(':').filter(|(h, p)| !h.ends_with(']') || p.is_empty()).map_or(host, |(h, p)| if p.bytes().all(|b| b.is_ascii_digit()) { h } else { host });
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Zoom levels per host, saved as JSON.
#[derive(Debug, Default)]
pub struct ZoomPrefs {
    path: Option<PathBuf>,
    levels: BTreeMap<String, f64>,
}

impl ZoomPrefs {
    pub fn load(path: PathBuf) -> Self {
        let levels = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { path: Some(path), levels }
    }

    /// The remembered level for `url`'s host (0 = 100 %).
    pub fn level_for(&self, url: &str) -> f64 {
        host_of(url).and_then(|h| self.levels.get(&h).copied()).unwrap_or(0.0)
    }

    /// Remember `level` for `url`'s host (100 % forgets it) and save.
    pub fn set(&mut self, url: &str, level: f64) {
        let Some(host) = host_of(url) else { return };
        if level.abs() < 1e-6 {
            self.levels.remove(&host);
        } else {
            self.levels.insert(host, level);
        }
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, serde_json::to_string_pretty(&self.levels).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_chromes_presets() {
        let pct = |l: f64| percent(l);
        let mut l = 0.0;
        let mut seen = vec![];
        for _ in 0..4 {
            l = step(l, true);
            seen.push(pct(l));
        }
        assert_eq!(seen, ["110%", "125%", "150%", "175%"]);
        let mut l = 0.0;
        let mut seen = vec![];
        for _ in 0..3 {
            l = step(l, false);
            seen.push(pct(l));
        }
        assert_eq!(seen, ["90%", "80%", "75%"]);
        assert_eq!(pct(step(level_of(5.0), true)), "500%", "stops at the top");
        assert_eq!(pct(step(level_of(0.25), false)), "25%", "and the bottom");
        assert_eq!(pct(0.0), "100%");
    }

    #[test]
    fn hosts_are_names_without_ports_or_users() {
        assert_eq!(host_of("https://Example.com/a?b").as_deref(), Some("example.com"));
        assert_eq!(host_of("http://127.0.0.1:8080/x").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_of("https://me@docs.rs:443/").as_deref(), Some("docs.rs"));
        assert_eq!(host_of("data:text/html,x"), None);
        assert_eq!(host_of("about:blank"), None);
    }

    #[test]
    fn levels_are_remembered_per_host_and_saved() {
        let dir = std::env::temp_dir().join(format!("hz-{}", std::process::id()));
        let path = dir.join("zoom.json");
        let _ = std::fs::remove_file(&path);
        let mut z = ZoomPrefs::load(path.clone());
        z.set("https://example.com/a", level_of(1.25));
        z.set("https://other.test/", level_of(0.9));
        assert_eq!(percent(z.level_for("https://example.com/other/page")), "125%");
        let again = ZoomPrefs::load(path.clone());
        assert_eq!(percent(again.level_for("https://example.com/")), "125%", "survives a restart");
        assert_eq!(percent(again.level_for("https://other.test/x")), "90%");
        assert_eq!(again.level_for("https://new.test/"), 0.0);
        let mut z = again;
        z.set("https://example.com/", 0.0);
        assert_eq!(ZoomPrefs::load(path).level_for("https://example.com/"), 0.0, "100 % forgets the host");
    }
}
