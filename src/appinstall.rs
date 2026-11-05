//! Installs a self-updating (Sparkle) app's update from its vendor feed, the way Sparkle would,
//! but with stricter checks before touching anything:
//!   HTTPS download → unpack (zip / dmg / tar) → same bundle id → valid code signature →
//!   SAME Team ID as the installed app → Gatekeeper/notarization → newer version.
//! The old bundle is kept as a backup (rollback = put it back). Runs as `upkeep __app-install`.

use crate::config;
use crate::version;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> Result<String, String> {
    let out = Command::new(args[0]).args(&args[1..]).stdin(Stdio::inherit()).output().map_err(|e| format!("{}: {e}", args[0]))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    } else {
        Err(format!("`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Plain command, or the same through `sudo` when the user can't write there.
/// sudo inherits the terminal, so the password prompt is the normal one.
fn run_maybe_sudo(args: &[&str], needs_root: bool) -> Result<String, String> {
    if !needs_root {
        return run(args);
    }
    let mut a = vec!["sudo"];
    a.extend_from_slice(args);
    let st = Command::new("sudo").args(&a[1..]).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(String::new()) } else { Err(format!("`{}` failed", a.join(" "))) }
}

fn plist(app: &Path, key: &str) -> Option<String> {
    let s = crate::providers::plist_strings(&crate::shell::Ctx::system(Default::default()), &app.join("Contents/Info.plist").to_string_lossy(), &[key]);
    s.into_iter().next().flatten()
}

fn team_id(app: &Path) -> Option<String> {
    let out = Command::new("codesign").args(["-dv", "--verbose=2"]).arg(app).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).map(str::to_string).filter(|t| t != "not set")
}

fn find_app(dir: &Path, bundle_id: &str, depth: u8) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "app") {
            if plist(&p, "CFBundleIdentifier").as_deref() == Some(bundle_id) {
                return Some(p);
            }
        } else if depth > 0 && p.is_dir() && !p.is_symlink()
            && let Some(f) = find_app(&p, bundle_id, depth - 1) {
                return Some(f);
            }
    }
    None
}

pub fn writable(p: &Path) -> bool {
    crate::providers::writable(&p.to_string_lossy())
}

/// Replacing a bundle needs root only if someone else owns it or /Applications is locked.
/// Read-only bundles you own (common from DMGs) just get `chmod u+w`.
pub fn needs_root(app: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mine = std::fs::symlink_metadata(app).is_ok_and(|m| m.uid() == unsafe { libc::getuid() });
    !mine || !writable(app.parent().unwrap_or(Path::new("/")))
}

/// rm -rf that also works on read-only bundles.
fn remove(p: &Path, root: bool) -> Result<String, String> {
    let s = p.to_string_lossy();
    let _ = run_maybe_sudo(&["chmod", "-R", "u+w", &s], root);
    run_maybe_sudo(&["rm", "-rf", &s], root)
}

/// Where the previous version is kept for rollback.
pub fn backup_path(app: &str, version: &str) -> PathBuf {
    let name = Path::new(app).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    config::root().join("backups").join(format!("{name}-{version}.app"))
}

struct Cleanup {
    dir: PathBuf,
    mount: Option<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(m) = &self.mount {
            let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(m).status();
        }
        let _ = remove(&self.dir, false);
    }
}

/// `source`: an https URL (vendor feed) or `cask:<token>` (vendor download fetched by Homebrew,
/// which checks the cask's sha256 when it has one).
pub fn install(app: &str, source: &str, check_only: bool) -> Result<String, String> {
    let app = Path::new(app);
    let cask = source.strip_prefix("cask:");
    if cask.is_none() && !source.starts_with("https://") {
        return Err("refusing non-HTTPS download".into());
    }
    let id = plist(app, "CFBundleIdentifier").ok_or("installed app has no bundle id")?;
    let old_ver = plist(app, "CFBundleShortVersionString").unwrap_or_default();
    let old_build = plist(app, "CFBundleVersion").unwrap_or_default();
    let old_team = team_id(app).ok_or("installed app is not signed by a developer: refusing to replace it automatically")?;

    let mut tmp = Cleanup { dir: std::env::temp_dir().join(format!("upkeep-app-{}", crate::history::new_id())), mount: None };
    std::fs::create_dir_all(&tmp.dir).map_err(|e| e.to_string())?;
    let file = match cask {
        Some(token) => {
            println!("Fetching cask {token} with Homebrew");
            let st = Command::new("brew").args(["fetch", "--cask", token]).env("HOMEBREW_NO_AUTO_UPDATE", "1").status().map_err(|e| e.to_string())?;
            if !st.success() {
                return Err(format!("brew fetch --cask {token} failed"));
            }
            PathBuf::from(run(&["brew", "--cache", "--cask", token])?.trim())
        }
        None => {
            let f = tmp.dir.join("download");
            println!("Downloading {source}");
            run(&["curl", "-fL", "--proto", "=https", "--retry", "2", "--max-time", "1800", "-sS", "-o", &f.to_string_lossy(), source])?;
            f
        }
    };

    let head = std::fs::read(&file).map(|b| b[..b.len().min(8)].to_vec()).unwrap_or_default();
    let lower = format!("{source} {}", file.display()).to_lowercase();
    let unpacked = tmp.dir.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let search = if head.starts_with(b"PK") {
        run(&["ditto", "-x", "-k", &file.to_string_lossy(), &unpacked.to_string_lossy()])?;
        unpacked.clone()
    } else if lower.contains(".dmg") || !head.starts_with(b"xar!") && run(&["hdiutil", "imageinfo", &file.to_string_lossy()]).is_ok() {
        let mnt = tmp.dir.join("mnt");
        run(&["hdiutil", "attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint", &mnt.to_string_lossy(), &file.to_string_lossy()])?;
        tmp.mount = Some(mnt.clone());
        mnt
    } else if lower.contains(".tar") || lower.ends_with(".tgz") || lower.ends_with(".tbz") {
        run(&["tar", "-xf", &file.to_string_lossy(), "-C", &unpacked.to_string_lossy()])?;
        unpacked.clone()
    } else {
        return Err("unsupported package format (installer .pkg?): update it from the app's own menu".into());
    };
    let new = find_app(&search, &id, 2).ok_or(format!("download contains no app with bundle id {id}"))?;

    println!("Verifying signature of {}", new.display());
    run(&["codesign", "--verify", "--deep", "--strict", &new.to_string_lossy()])?;
    let new_team = team_id(&new).ok_or("downloaded app is not signed")?;
    if new_team != old_team {
        return Err(format!("Team ID mismatch: installed app signed by {old_team}, download by {new_team}. Not installing."));
    }
    run(&["spctl", "--assess", "--type", "execute", &new.to_string_lossy()]).map_err(|e| format!("Gatekeeper rejected the download: {e}"))?;
    let new_ver = plist(&new, "CFBundleShortVersionString").unwrap_or_default();
    let new_build = plist(&new, "CFBundleVersion").unwrap_or_default();
    let newer = if old_build.is_empty() || new_build.is_empty() { version::is_newer(&new_ver, &old_ver) } else { version::is_newer(&new_build, &old_build) };
    if !newer {
        return Err(format!("download is not newer ({new_ver} vs installed {old_ver})"));
    }
    println!("Verified: {new_ver}, Team ID {new_team}, notarized");
    if check_only {
        return Ok(format!("ready to install {new_ver}"));
    }

    let exe = app.join("Contents/MacOS");
    if Command::new("pgrep").arg("-f").arg(exe.to_string_lossy().as_ref()).stdout(Stdio::null()).status().is_ok_and(|s| s.success()) {
        return Err(format!("{} is running: quit it and retry", app.file_stem().unwrap_or_default().to_string_lossy()));
    }
    let backup = backup_path(&app.to_string_lossy(), &old_ver);
    if let Some(dir) = backup.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if backup.exists() {
        remove(&backup, false)?;
    }
    let root = needs_root(app);
    // Previous version → backup (copy, so a failure below never leaves the app missing).
    run_maybe_sudo(&["ditto", &app.to_string_lossy(), &backup.to_string_lossy()], false).or_else(|_| run_maybe_sudo(&["ditto", &app.to_string_lossy(), &backup.to_string_lossy()], true))?;
    // Replace atomically-ish: new bundle next to the old one, then swap.
    let staged = app.with_extension("app.upkeep-new");
    run_maybe_sudo(&["ditto", &new.to_string_lossy(), &staged.to_string_lossy()], root)?;
    let old_tmp = app.with_extension("app.upkeep-old");
    run_maybe_sudo(&["mv", &app.to_string_lossy(), &old_tmp.to_string_lossy()], root)?;
    if let Err(e) = run_maybe_sudo(&["mv", &staged.to_string_lossy(), &app.to_string_lossy()], root) {
        let _ = run_maybe_sudo(&["mv", &old_tmp.to_string_lossy(), &app.to_string_lossy()], root);
        return Err(e);
    }
    remove(&old_tmp, root)?;
    prune_backups(&backup);
    println!("Installed {new_ver}; previous version kept at {}", backup.display());
    Ok(new_ver)
}

/// Keep only the newest backup per app: targeted, not a full software archive.
fn prune_backups(keep: &Path) {
    let Some(dir) = keep.parent() else { return };
    let stem = keep.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let prefix = stem.rsplit_once('-').map(|(p, _)| format!("{p}-")).unwrap_or_default();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with(&prefix) && n != stem {
            let _ = remove(&e.path(), false);
        }
    }
}

/// Rollback: put the backed-up bundle back in place.
pub fn restore(backup: &str, app: &str) -> Result<String, String> {
    let (backup, app) = (Path::new(backup), Path::new(app));
    if !backup.exists() {
        return Err(format!("no backup at {}", backup.display()));
    }
    let root = needs_root(app);
    let old_tmp = app.with_extension("app.upkeep-old");
    run_maybe_sudo(&["mv", &app.to_string_lossy(), &old_tmp.to_string_lossy()], root)?;
    if let Err(e) = run_maybe_sudo(&["ditto", &backup.to_string_lossy(), &app.to_string_lossy()], root) {
        let _ = run_maybe_sudo(&["mv", &old_tmp.to_string_lossy(), &app.to_string_lossy()], root);
        return Err(e);
    }
    remove(&old_tmp, root)?;
    let v = plist(app, "CFBundleShortVersionString").unwrap_or_default();
    println!("Restored {} {v}", app.display());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_plain_http_and_unsigned_apps() {
        assert!(install("/nonexistent.app", "http://x/y.zip", true).unwrap_err().contains("HTTPS"));
        assert!(!install("/nonexistent.app", "cask:foo", true).unwrap_err().contains("HTTPS"));
        assert!(install("/nonexistent.app", "https://x/y.zip", true).is_err());
    }

    #[test]
    fn backup_names_and_pruning() {
        let b = backup_path("/Applications/TablePlus.app", "6.1");
        assert!(b.ends_with("backups/TablePlus-6.1.app"));
        let dir = std::env::temp_dir().join(format!("upkeep-prune-{}", std::process::id()));
        for n in ["Foo-1.0.app", "Foo-2.0.app", "Foobar-1.0.app"] {
            std::fs::create_dir_all(dir.join(n)).unwrap();
        }
        prune_backups(&dir.join("Foo-2.0.app"));
        assert!(!dir.join("Foo-1.0.app").exists() && dir.join("Foo-2.0.app").exists() && dir.join("Foobar-1.0.app").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
