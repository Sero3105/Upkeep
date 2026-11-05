//! Index of the public Homebrew cask catalog (formulae.brew.sh/api/cask.json): which cask ships
//! which `.app`, and its current version. Lets Upkeep find updates for apps installed by hand.
//! Refreshed at most once a day; only the small index is kept on disk.

use crate::config;
use crate::shell::Ctx;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const URL: &str = "https://formulae.brew.sh/api/cask.json";
const FILE: &str = "cask-index.json";
/// Bump when `Entry` changes meaning: older caches are rebuilt instead of trusted.
const FORMAT: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// Bundle file name, e.g. "Google Chrome.app".
    pub app: String,
    pub token: String,
    /// Version without the build suffix ("4.98.0,245422" -> "4.98.0").
    pub version: String,
    /// Bundle ids the cask names (uninstall `quit`), used to disambiguate.
    pub ids: Vec<String>,
    /// Installs through a .pkg: needs an administrator password.
    pub pkg: bool,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    format: u32,
    date: u64,
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct Raw {
    token: String,
    version: String,
    #[serde(default)]
    artifacts: Vec<Value>,
    #[serde(default)]
    disabled: bool,
}

/// All strings at any depth (zap/uninstall stanzas nest arrays and objects).
fn strings_deep(v: &Value) -> Vec<String> {
    match v {
        Value::Object(m) => m.values().flat_map(strings_deep).collect(),
        Value::Array(a) => a.iter().flat_map(strings_deep).collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        _ => vec![],
    }
}

pub fn parse(json: &str) -> Result<Vec<Entry>, String> {
    let raw: Vec<Raw> = serde_json::from_str(json).map_err(|e| format!("cask catalog: bad JSON: {e}"))?;
    let mut out = vec![];
    for c in raw.into_iter().filter(|c| !c.disabled && c.version != "latest") {
        let pkg = c.artifacts.iter().any(|a| a.get("pkg").is_some() || a.get("installer").is_some());
        // Every reverse-DNS token the cask mentions (quit ids, ~/Library/Preferences/<id>.plist…):
        // evidence of which app it really is.
        let mut ids: Vec<String> = c
            .artifacts
            .iter()
            .filter_map(|a| a.get("uninstall").or_else(|| a.get("zap")))
            .flat_map(strings_deep)
            .flat_map(|s| s.split(['/', '*', ' ', ':']).map(str::to_string).collect::<Vec<_>>())
            .filter(|t| t.matches('.').count() >= 2 && t.starts_with(|ch: char| ch.is_ascii_alphabetic()))
            .collect();
        ids.sort();
        ids.dedup();
        let version = c.version.split(',').next().unwrap_or("").to_string();
        for a in &c.artifacts {
            // "app": ["Name.app"] or ["Name.app", {"target": "Other.app"}]
            // Index both the bundle name and its `target` rename: users may have either.
            let Some(spec) = a.get("app") else { continue };
            let mut names = strings(spec);
            names.extend(spec.as_array().and_then(|v| v.iter().find_map(|x| x.get("target").and_then(Value::as_str))).map(String::from));
            for app in names.into_iter().filter(|s| s.ends_with(".app")) {
                let app = app.rsplit('/').next().unwrap_or(&app).to_string();
                out.push(Entry { app, token: c.token.clone(), version: version.clone(), ids: ids.clone(), pkg });
            }
        }
    }
    Ok(out)
}

/// Cached index; refreshed when older than a day. A stale index beats none if the network fails.
pub fn load(ctx: &Ctx) -> Result<Vec<Entry>, String> {
    let cached: Option<Cache> = std::fs::read_to_string(config::path(FILE)).ok().and_then(|s| serde_json::from_str(&s).ok());
    if let Some(c) = &cached
        && c.format == FORMAT && config::now().saturating_sub(c.date) < 86_400 {
            return Ok(c.entries.clone());
        }
    let fresh = ctx.out(&["curl", "-fsSL", "--compressed", "--proto", "=https", "--max-time", "90", URL], 100).ok_or_else(|| "could not download the Homebrew cask catalog".to_string()).and_then(|s| parse(&s));
    match (fresh, cached) {
        (Ok(entries), _) => {
            let _ = config::write_atomic(FILE, &serde_json::to_string(&Cache { format: FORMAT, date: config::now(), entries: entries.clone() }).unwrap());
            Ok(entries)
        }
        (Err(_), Some(c)) => Ok(c.entries),
        (Err(e), None) => Err(e),
    }
}

/// Cask for an installed bundle: one that mentions this bundle id wins; casks that mention
/// only *other* apps' ids are excluded; then a unique non-variant (`@beta`…) cask with the same
/// file name. Ambiguous → None (never guess).
pub fn lookup<'a>(idx: &'a [Entry], file: &str, bundle_id: &str) -> Option<&'a Entry> {
    let prefix = format!("{bundle_id}.");
    let names_it = |e: &Entry| e.ids.iter().any(|i| i == bundle_id || i.starts_with(&prefix));
    let cands: Vec<&Entry> = idx.iter().filter(|e| e.app == file && !e.token.contains('@')).collect();
    if let Some(e) = cands.iter().find(|e| names_it(e)) {
        return Some(e);
    }
    let plain: Vec<&&Entry> = cands.iter().filter(|e| e.ids.is_empty()).collect();
    (plain.len() == 1).then(|| *plain[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"[
      {"token": "google-chrome", "version": "157.0.8103.39", "artifacts": [{"app": ["Google Chrome.app"]}, {"zap": [{"quit": "com.google.Chrome"}]}]},
      {"token": "docker-desktop", "version": "4.98.0,245422", "artifacts": [{"app": ["Docker.app"]}]},
      {"token": "telegram", "version": "13.2.0,283458", "artifacts": [{"app": ["Telegram.app"]}, {"uninstall": [{"quit": ["ru.keepcoder.Telegram"]}]}]},
      {"token": "telegram-desktop", "version": "7.3.5", "artifacts": [{"app": ["Telegram.app", {"target": "Telegram Desktop.app"}]}, {"zap": [{"trash": ["~/Library/Preferences/com.tdesktop.Telegram.plist"]}]}]},
      {"token": "telegram@beta", "version": "13.3", "artifacts": [{"app": ["Telegram.app"]}]},
      {"token": "zoom", "version": "6.1", "artifacts": [{"pkg": ["Zoom.pkg"]}, {"app": ["zoom.us.app"]}]},
      {"token": "nightly", "version": "latest", "artifacts": [{"app": ["Nightly.app"]}]},
      {"token": "old", "version": "1", "disabled": true, "artifacts": [{"app": ["Old.app"]}]},
      {"token": "renamed", "version": "2", "artifacts": [{"app": ["Thing.app", {"target": "Renamed Thing.app"}]}]}
    ]"#;

    #[test]
    fn parses_and_disambiguates() {
        let idx = parse(JSON).unwrap();
        assert_eq!(lookup(&idx, "Docker.app", "com.docker.docker").unwrap().version, "4.98.0");
        assert_eq!(lookup(&idx, "Telegram.app", "ru.keepcoder.Telegram").unwrap().token, "telegram");
        assert_eq!(lookup(&idx, "Telegram.app", "com.tdesktop.Telegram").unwrap().token, "telegram-desktop", "evidence from zap paths");
        assert_eq!(lookup(&idx, "Telegram Desktop.app", "com.tdesktop.Telegram").unwrap().token, "telegram-desktop");
        assert!(lookup(&idx, "Telegram.app", "org.other").is_none(), "both casks name other apps: don't guess");
        assert!(lookup(&idx, "zoom.us.app", "us.zoom.xos").unwrap().pkg);
        assert!(lookup(&idx, "Nightly.app", "x").is_none() && lookup(&idx, "Old.app", "x").is_none());
        assert!(lookup(&idx, "Renamed Thing.app", "x").is_some() && lookup(&idx, "Thing.app", "x").is_some());
    }
}
