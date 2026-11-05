use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// All state lives in one directory; `UPKEEP_HOME` overrides it (tests, portable setups).
pub fn root() -> PathBuf {
    match std::env::var("UPKEEP_HOME") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config/upkeep"),
    }
}

pub fn path(file: &str) -> PathBuf {
    root().join(file)
}

pub const CONFIG: &str = "config.json";
pub const HISTORY: &str = "history.jsonl";
pub const SNAPSHOT: &str = "last-scan.json";
pub const LOG: &str = "upkeep.log";
pub const SOURCE: &str = "source-dir";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub disabled_providers: Vec<String>,
    /// Update ids ("homebrew:node") or bare names never offered.
    pub excluded: Vec<String>,
    /// Run independent package managers concurrently (never two ops on the same one).
    pub parallel: bool,
    /// `brew update` before checking. Refreshes metadata only, installs nothing.
    pub refresh_indexes: bool,
    /// Read Sparkle appcasts (HTTPS) of self-updating apps.
    pub check_appcasts: bool,
    /// Per-operation timeout in seconds.
    pub command_timeout: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            disabled_providers: vec![],
            excluded: vec![],
            parallel: true,
            refresh_indexes: true,
            check_appcasts: true,
            command_timeout: 1800,
        }
    }
}

impl Config {
    /// Missing file -> defaults. Corrupt file -> error: never silently overwrite user settings.
    pub fn load() -> Result<Config, String> {
        let p = path(CONFIG);
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        write_atomic(CONFIG, &serde_json::to_string_pretty(self).unwrap())
    }

    pub fn is_excluded(&self, id: &str, name: &str) -> bool {
        self.excluded.iter().any(|e| e == id || e == name)
    }

    pub fn enabled(&self, provider: &str) -> bool {
        !self.disabled_providers.iter().any(|p| p == provider)
    }
}

pub fn write_atomic(file: &str, data: &str) -> Result<(), String> {
    let dir = root();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{file}.tmp"));
    std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, dir.join(file))).map_err(|e| format!("{file}: {e}"))
}

/// Persistent log of every command Upkeep runs that changes something.
pub fn log(msg: &str) {
    let _ = std::fs::create_dir_all(root());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path(LOG)) {
        let _ = writeln!(f, "{} {msg}", fmt_time(now()));
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Local time "2026-11-05 18:33".
pub fn fmt_time(secs: u64) -> String {
    if secs == 0 {
        return "never".into();
    }
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub fn ago(secs: u64) -> String {
    if secs == 0 {
        return "never".into();
    }
    let d = now().saturating_sub(secs);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let c: Config = serde_json::from_str(r#"{"parallel": false, "excluded": ["homebrew:node"]}"#).unwrap();
        assert!(!c.parallel);
        assert!(c.refresh_indexes);
        assert_eq!(c.command_timeout, 1800);
        assert!(c.is_excluded("homebrew:node", "node"));
        assert!(!c.is_excluded("npm:node", "nodemon"));
    }

    #[test]
    fn corrupt_config_is_an_error() {
        assert!(serde_json::from_str::<Config>("{nope").is_err());
    }
}
