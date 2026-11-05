//! Persistent update history (JSON Lines, append-only) and rollback.

use crate::config;
use crate::model::Category;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Record {
    pub id: String,
    /// Unix seconds.
    pub date: u64,
    pub name: String,
    pub provider: String,
    pub category: Category,
    pub from: String,
    pub to: String,
    /// "update" | "rollback"
    pub operation: String,
    pub command: Vec<String>,
    pub duration: f64,
    /// "success" | "unverified" | "already-current" | "failed" | "skipped"
    pub outcome: String,
    pub detail: Option<String>,
    /// argv that restores `from`, when supported.
    pub rollback: Option<Vec<String>>,
    pub output_tail: Vec<String>,
}

impl Record {
    pub fn failed(&self) -> bool {
        self.outcome == "failed"
    }
}

pub struct History {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Default)]
pub struct Filter {
    pub category: Option<Category>,
    pub search: String,
    pub failed_only: bool,
}

impl History {
    pub fn open() -> Self {
        History { path: config::path(config::HISTORY), lock: Mutex::new(()) }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        History { path, lock: Mutex::new(()) }
    }

    pub fn append(&self, r: &Record) -> Result<(), String> {
        let _g = self.lock.lock().unwrap();
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        // One write per line keeps concurrent appends from interleaving.
        let line = serde_json::to_string(r).unwrap() + "\n";
        f.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    /// Newest first. Corrupt lines are skipped and counted, never fatal.
    pub fn load(&self) -> (Vec<Record>, usize) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut bad = 0;
        let mut v: Vec<Record> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).map_err(|_| bad += 1).ok())
            .collect();
        v.reverse();
        (v, bad)
    }
}

pub fn filter<'a>(records: &'a [Record], f: &Filter) -> Vec<&'a Record> {
    let q = f.search.to_lowercase();
    records
        .iter()
        .filter(|r| f.category.is_none_or(|c| r.category == c))
        .filter(|r| !f.failed_only || r.failed())
        .filter(|r| q.is_empty() || r.name.to_lowercase().contains(&q) || r.provider.contains(&q))
        .collect()
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("{:x}{:02x}", t & 0xffff_ffff, N.fetch_add(1, Ordering::Relaxed) & 0xff)
}

/// Previous versions recorded for a package, newest first (for "consult previous versions").
pub fn previous_versions(records: &[Record], provider: &str, name: &str) -> Vec<String> {
    let mut v: Vec<String> = records
        .iter()
        .filter(|r| r.provider == provider && r.name == name && r.outcome == "success")
        .map(|r| r.from.clone())
        .collect();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_load_filter_roundtrip() {
        let p = std::env::temp_dir().join(format!("upkeep-hist-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let h = History::at(p.clone());
        let mk = |name: &str, outcome: &str, cat| Record { id: new_id(), name: name.into(), provider: "npm".into(), category: cat, outcome: outcome.into(), from: "1.0".into(), ..Default::default() };
        h.append(&mk("typescript", "success", Category::Packages)).unwrap();
        h.append(&mk("gh", "failed", Category::Homebrew)).unwrap();
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{garbage\n").unwrap();
        let (recs, bad) = h.load();
        assert_eq!((recs.len(), bad), (2, 1));
        assert_eq!(recs[0].name, "gh", "newest first");
        assert_eq!(filter(&recs, &Filter { failed_only: true, ..Default::default() }).len(), 1);
        assert_eq!(filter(&recs, &Filter { search: "TYPE".into(), ..Default::default() }).len(), 1);
        assert_eq!(filter(&recs, &Filter { category: Some(Category::Packages), ..Default::default() }).len(), 1);
        assert_eq!(previous_versions(&recs, "npm", "typescript"), vec!["1.0"]);
        assert_ne!(new_id(), new_id());
        let _ = std::fs::remove_file(&p);
    }
}
