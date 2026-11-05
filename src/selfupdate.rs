//! Upkeep keeps itself current: on launch, if its source directory changed since the installed
//! binary was built (or the git upstream has new commits), it rebuilds via install.sh and
//! re-executes the same command. Toggle: Settings → "Auto-update Upkeep".

use crate::config::{self, Config};
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const STATE: &str = "self-update.json";

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct State {
    /// Unix time of the last `git fetch` (network: at most every 6 hours).
    last_fetch: u64,
    /// Fingerprint of sources whose build failed: don't retry until they change.
    failed: String,
}

pub fn source_dir() -> Option<PathBuf> {
    let s = std::fs::read_to_string(config::path(config::SOURCE)).ok()?;
    let p = PathBuf::from(s.trim());
    p.join("Cargo.toml").exists().then_some(p)
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// Newest modification time of the files that make up the binary.
pub fn newest_source(dir: &Path) -> u64 {
    fn walk(p: &Path, best: &mut u64) {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, best);
            } else if path.extension().is_some_and(|x| x == "rs") {
                *best = (*best).max(mtime(&path));
            }
        }
    }
    let mut best = mtime(&dir.join("Cargo.toml")).max(mtime(&dir.join("Cargo.lock")));
    walk(&dir.join("src"), &mut best);
    best
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Commits on the upstream branch not yet pulled (0 without a remote).
fn behind(dir: &Path, state: &mut State) -> u32 {
    if git(dir, &["rev-parse", "--abbrev-ref", "@{u}"]).is_none() {
        return 0;
    }
    if config::now().saturating_sub(state.last_fetch) > 6 * 3600 {
        // shortcut: no timeout on fetch; git's own network timeouts apply. Fine for an interactive launch.
        let _ = git(dir, &["fetch", "--quiet"]);
        state.last_fetch = config::now();
    }
    git(dir, &["rev-list", "--count", "HEAD..@{u}"]).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Rebuild + re-exec when needed. Returns only if nothing changed or the update failed.
pub fn auto(cfg: &Config) {
    if !cfg.auto_self_update || !std::io::stderr().is_terminal() || std::env::var_os("UPKEEP_NO_SELF_UPDATE").is_some() {
        return;
    }
    let (Some(src), Ok(exe)) = (source_dir(), std::env::current_exe()) else { return };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    if exe.starts_with(&src) {
        return; // a development build (cargo run / target/): never replace it.
    }
    let mut state: State = std::fs::read_to_string(config::path(STATE)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let n = behind(&src, &mut state);
    let newest = newest_source(&src);
    let fingerprint = format!("{newest}:{}", git(&src, &["rev-parse", "@{u}"]).unwrap_or_default());
    let stale = n > 0 || newest > mtime(&exe);
    let save = |s: &State| {
        let _ = config::write_atomic(STATE, &serde_json::to_string(s).unwrap());
    };
    if !stale || state.failed == fingerprint {
        save(&state);
        return;
    }
    eprintln!("◆ Upkeep: new version available ({}), updating itself…", if n > 0 { format!("{n} new commit(s)") } else { "source changed".into() });
    if n > 0 && git(&src, &["pull", "--ff-only", "--quiet"]).is_none() {
        eprintln!("  git pull failed (local changes?): rebuilding the local sources instead.");
    }
    let prefix = exe.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let ok = Command::new(src.join("install.sh"))
        .current_dir(&src)
        .env("PREFIX", &prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        state.failed = fingerprint;
        save(&state);
        eprintln!("  build failed: keeping the current version (will retry when the sources change).");
        return;
    }
    state.failed.clear();
    save(&state);
    eprintln!("  done. Restarting…\n");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let err = Command::new(&exe).args(args).env("UPKEEP_NO_SELF_UPDATE", "1").exec();
    eprintln!("  could not restart ({err}): run the command again.");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_source_sees_nested_rs_files_only() {
        let d = std::env::temp_dir().join(format!("upkeep-self-{}", std::process::id()));
        std::fs::create_dir_all(d.join("src/x")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("src/x/a.rs"), "").unwrap();
        std::fs::write(d.join("src/notes.md"), "").unwrap();
        let t = newest_source(&d);
        assert!(t > 0 && t >= mtime(&d.join("src/x/a.rs")));
        let _ = std::fs::remove_dir_all(d);
    }
}
