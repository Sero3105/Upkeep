//! System Health: diagnostics only. Nothing here changes the system; every fix is a suggestion.

use crate::model::*;
use crate::shell::{Ctx, expand_tilde};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    pub score: u8,
    pub issues: Vec<HealthIssue>,
}

impl Report {
    pub fn count(&self, s: Severity) -> usize {
        self.issues.iter().filter(|i| i.severity == s).count()
    }
    pub fn label(&self) -> &'static str {
        match self.score {
            90.. => "Excellent",
            75..90 => "Good",
            50..75 => "Fair",
            _ => "Poor",
        }
    }
}

pub fn score(issues: &[HealthIssue]) -> u8 {
    let penalty: u32 = issues
        .iter()
        .map(|i| match i.severity {
            Severity::Critical => 20,
            Severity::Warning => 4,
            Severity::Info | Severity::Healthy => 0,
        })
        .sum();
    100u32.saturating_sub(penalty) as u8
}

pub fn check(ctx: &Ctx, snap: &Snapshot) -> Report {
    let mut v: Vec<HealthIssue> = vec![];
    let path = ctx.env.get("PATH").cloned().unwrap_or_default();
    v.extend(path_issues(&path, &ctx.home));
    v.extend(broken_symlinks(ctx));
    v.extend(shadowed(&path, &ctx.home));
    v.extend(multiple_runtimes(snap));
    v.extend(updates_summary(snap));
    v.extend(snap.issues.iter().cloned());
    for w in &snap.warnings {
        v.push(HealthIssue::new(Severity::Warning, "Check incomplete", w.split(':').next().unwrap_or("scan"), w.clone()).consequence("Some updates or problems may be missing from this report.").fix("Run `upkeep doctor`, then retry the check."));
    }
    v.extend(dev_tools(ctx));
    v.extend(disk(ctx));
    v.extend(env_vars(ctx));
    v.extend(brew_doctor(ctx));
    v.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.title.cmp(&b.title)));
    Report { score: score(&v), issues: v }
}

pub fn path_issues(path: &str, home: &str) -> Vec<HealthIssue> {
    let mut out = vec![];
    let entries: Vec<&str> = path.split(':').collect();
    let mut seen = HashSet::new();
    let dups: Vec<&str> = entries.iter().filter(|e| !e.is_empty() && !seen.insert(**e)).copied().collect::<HashSet<_>>().into_iter().collect();
    if !dups.is_empty() {
        let mut d = dups.clone();
        d.sort();
        out.push(
            HealthIssue::new(Severity::Info, format!("{} duplicate PATH entries", d.len()), "PATH", d.join(", "))
                .consequence("Slower command lookup and confusing precedence; usually from shell files sourcing the same setup twice.")
                .fix("Remove the repeated `export PATH=...` lines from ~/.zshrc / ~/.zprofile (or use `typeset -U path` in zsh)."),
        );
    }
    let tilde: Vec<&str> = entries.iter().filter(|e| e.starts_with('~')).copied().collect();
    if !tilde.is_empty() {
        out.push(
            HealthIssue::new(Severity::Warning, "Literal ~ in PATH", "PATH", tilde.join(", "))
                .consequence("`~` is not expanded inside PATH: commands in that directory are not found by most programs.")
                .fix(format!("Use the absolute path instead (e.g. {}), check /etc/paths.d and your shell files.", expand_tilde(tilde[0], home))),
        );
    }
    // Apple-managed entries (cryptexes, /Library/Apple) legitimately come and go.
    let apple = |e: &str| e.starts_with("/var/run/com.apple") || e.starts_with("/System/") || e.starts_with("/Library/Apple/");
    let missing: Vec<&str> = entries.iter().filter(|e| !e.is_empty() && !e.starts_with('~') && !apple(e) && !Path::new(e).is_dir()).copied().collect::<HashSet<_>>().into_iter().collect();
    if !missing.is_empty() {
        let mut m = missing.clone();
        m.sort();
        out.push(
            HealthIssue::new(Severity::Warning, format!("{} PATH entries do not exist", m.len()), "PATH", m.join(", "))
                .consequence("Leftovers of uninstalled software; harmless but they hide real problems.")
                .fix("Remove them from your shell configuration."),
        );
    }
    if entries.iter().any(|e| e.is_empty() || *e == ".") {
        out.push(
            HealthIssue::new(Severity::Critical, "Current directory in PATH", "PATH", "PATH contains `.` or an empty entry")
                .consequence("Running a command inside a downloaded folder can execute a planted binary.")
                .fix("Remove `.` / `::` from PATH."),
        );
    }
    let pos = |d: &str| entries.iter().position(|e| *e == d);
    if let (Some(sys), Some(brew)) = (pos("/usr/bin"), pos("/opt/homebrew/bin"))
        && sys < brew {
            out.push(
                HealthIssue::new(Severity::Warning, "System binaries shadow Homebrew", "PATH", "/usr/bin comes before /opt/homebrew/bin")
                    .consequence("Old Apple versions of git, python3, ruby... run instead of the Homebrew ones you keep updated.")
                    .fix("Put `eval \"$(/opt/homebrew/bin/brew shellenv)\"` early in ~/.zprofile."),
            );
        }
    if out.is_empty() {
        out.push(HealthIssue::new(Severity::Healthy, "PATH is clean", "PATH", "No duplicates, missing or unsafe entries"));
    }
    out
}

pub fn broken_symlinks(ctx: &Ctx) -> Vec<HealthIssue> {
    let dirs = ["/opt/homebrew/bin", "/usr/local/bin", "~/.local/bin", "~/bin"];
    let mut broken = vec![];
    for d in dirs {
        let d = ctx.path(d);
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && std::fs::metadata(&p).is_err() {
                broken.push(p.to_string_lossy().into_owned());
            }
        }
    }
    if broken.is_empty() {
        return vec![HealthIssue::new(Severity::Healthy, "No broken symlinks", "bin directories", "All links in bin directories resolve")];
    }
    broken.sort();
    let shown: Vec<&str> = broken.iter().take(10).map(String::as_str).collect();
    vec![
        HealthIssue::new(Severity::Warning, format!("{} broken symlinks", broken.len()), "bin directories", shown.join(", "))
            .consequence("Commands that look installed fail with \"no such file\".")
            .fix("In /opt/homebrew run `brew cleanup --prune-prefix`; elsewhere remove the dead link with `rm <path>`."),
    ]
}

/// Commands found more than once on PATH with different real targets.
pub fn shadowed(path: &str, home: &str) -> Vec<HealthIssue> {
    let cmds = ["python3", "pip3", "node", "npm", "java", "ruby", "gem", "git", "openssl", "docker", "go", "cargo", "php", "claude"];
    let mut out = vec![];
    for c in cmds {
        let mut targets: Vec<(String, String)> = vec![];
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let p = format!("{}/{c}", expand_tilde(dir, home));
            if let Ok(real) = std::fs::canonicalize(&p) {
                let real = real.to_string_lossy().into_owned();
                if !targets.iter().any(|(_, r)| *r == real) {
                    targets.push((p, real));
                }
            }
        }
        // /usr/bin stubs for python3/git/java just forward to the developer tools.
        let real: Vec<&(String, String)> = targets.iter().filter(|(p, _)| !p.starts_with("/usr/bin/")).collect();
        if real.len() > 1 {
            let list: Vec<String> = real.iter().map(|(p, _)| p.clone()).collect();
            out.push(
                HealthIssue::new(Severity::Warning, format!("Multiple `{c}` on PATH"), c, format!("`{c}` runs {}; also found: {}", list[0], list[1..].join(", ")))
                    .consequence("Updates may land in the copy that is not used; scripts can pick different versions.")
                    .fix("Keep one installation (prefer the package-managed one) or reorder PATH deliberately."),
            );
        }
    }
    out
}

pub fn multiple_runtimes(snap: &Snapshot) -> Vec<HealthIssue> {
    let mut by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for i in snap.installed.iter().filter(|i| i.category == Category::Runtimes && i.provider == "runtimes") {
        by.entry(&i.name).or_default().push(format!("{} ({})", i.version, i.source));
    }
    by.into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(n, v)| {
            HealthIssue::new(Severity::Info, format!("{} {n} versions installed", v.len()), n.to_string(), v.join(", "))
                .consequence("Fine if projects need them; otherwise disk space and confusion over which one runs.")
                .fix("Keep the ones your projects use. Remove others only with the tool that installed them (Upkeep never removes runtimes).")
        })
        .collect()
}

fn updates_summary(snap: &Snapshot) -> Vec<HealthIssue> {
    let mut out = vec![];
    let pm: Vec<&Update> = snap.updates.iter().filter(|u| u.is_self_update).collect();
    if !pm.is_empty() {
        let names: Vec<String> = pm.iter().map(|u| format!("{} {} → {}", u.name, u.current, u.latest)).collect();
        out.push(
            HealthIssue::new(Severity::Warning, "Package managers out of date", "Package managers", names.join(", "))
                .consequence("Older managers miss security fixes and may resolve packages differently.")
                .fix("Update them first from the Updates tab."),
        );
    }
    let security: Vec<&Update> = snap.updates.iter().filter(|u| u.notes.iter().any(|n| n.contains("security"))).collect();
    if !security.is_empty() {
        out.push(
            HealthIssue::new(Severity::Critical, "Security updates pending", "macOS", security.iter().map(|u| u.name.as_str()).collect::<Vec<_>>().join(", "))
                .consequence("Known vulnerabilities stay exploitable.")
                .fix("Install via System Settings → General → Software Update."),
        );
    }
    let n = snap.updates.len();
    out.push(match n {
        0 => HealthIssue::new(Severity::Healthy, "Everything up to date", "Software", "No pending updates found"),
        1..=20 => HealthIssue::new(Severity::Info, format!("{n} updates available"), "Software", "See the Updates tab").fix("Review and apply from Updates / Smart Update."),
        _ => HealthIssue::new(Severity::Warning, format!("{n} updates available"), "Software", "Many components are behind")
            .consequence("Large backlogs make each upgrade riskier.")
            .fix("Apply low-risk updates first from Smart Update."),
    });
    out
}

fn dev_tools(ctx: &Ctx) -> Vec<HealthIssue> {
    if ctx.run(&["xcode-select", "-p"], 10).ok() {
        vec![HealthIssue::new(Severity::Healthy, "Developer tools installed", "Xcode CLT", "xcode-select points to a valid toolchain")]
    } else {
        vec![
            HealthIssue::new(Severity::Critical, "Xcode Command Line Tools missing", "Xcode CLT", "`xcode-select -p` failed")
                .consequence("git, compilers and Homebrew builds don't work.")
                .fix("Run `xcode-select --install`."),
        ]
    }
}

fn disk(ctx: &Ctx) -> Vec<HealthIssue> {
    let Ok(c) = std::ffi::CString::new(ctx.home.as_str()) else { return vec![] };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
        return vec![];
    }
    let free = s.f_bavail as f64 / s.f_blocks as f64;
    let gb = s.f_bavail as f64 * s.f_frsize as f64 / 1e9;
    let msg = format!("{gb:.1} GB free ({:.0}%)", free * 100.0);
    vec![match free {
        f if f < 0.05 => HealthIssue::new(Severity::Critical, "Disk almost full", "Disk", msg).consequence("Updates fail midway; macOS needs room for upgrades.").fix("Free space before updating (large caches: ~/Library/Caches, Docker images, Xcode DerivedData)."),
        f if f < 0.15 => HealthIssue::new(Severity::Warning, "Low disk space", "Disk", msg).consequence("Big updates (macOS, Xcode) may not fit.").fix("Free some space."),
        _ => HealthIssue::new(Severity::Healthy, "Disk space OK", "Disk", msg),
    }]
}

fn env_vars(ctx: &Ctx) -> Vec<HealthIssue> {
    ["JAVA_HOME", "GOROOT", "PYTHONHOME", "ANDROID_HOME"]
        .iter()
        .filter_map(|k| {
            let v = ctx.env.get(*k)?;
            (!Path::new(&expand_tilde(v, &ctx.home)).is_dir()).then(|| {
                HealthIssue::new(Severity::Warning, format!("{k} points to a missing directory"), *k, v.clone())
                    .consequence("Builds pick no or the wrong toolchain.")
                    .fix(if *k == "JAVA_HOME" { "Use `export JAVA_HOME=$(/usr/libexec/java_home -v <version>)`.".to_string() } else { format!("Fix or remove `{k}` in your shell files.") })
            })
        })
        .collect()
}

fn brew_doctor(ctx: &Ctx) -> Vec<HealthIssue> {
    if !ctx.has("brew") {
        return vec![];
    }
    let r = ctx.run(&["brew", "doctor"], 120);
    if r.ok() {
        return vec![HealthIssue::new(Severity::Healthy, "brew doctor: ready to brew", "Homebrew", "No problems reported")];
    }
    let text = format!("{}\n{}", r.out, r.err);
    text.lines()
        .filter_map(|l| l.strip_prefix("Warning: "))
        .map(|w| HealthIssue::new(Severity::Warning, format!("brew doctor: {}", w.trim_end_matches('.')), "Homebrew", w).fix("Run `brew doctor` for the full explanation and fix."))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_problems() {
        let path = "/usr/bin:/opt/homebrew/bin:/usr/bin:~/.dotnet/tools:/definitely/missing:.";
        let v = path_issues(path, "/Users/x");
        let titles: Vec<&str> = v.iter().map(|i| i.title.as_str()).collect();
        assert!(titles.contains(&"1 duplicate PATH entries"));
        assert!(titles.contains(&"Literal ~ in PATH"));
        assert!(titles.iter().any(|t| t.contains("do not exist")));
        assert!(titles.contains(&"Current directory in PATH"));
        assert!(titles.contains(&"System binaries shadow Homebrew"));
        assert_eq!(path_issues("/bin:/usr/bin", "/")[0].severity, Severity::Healthy);
    }

    #[test]
    fn finds_broken_symlinks_and_shadowing() {
        let tmp = std::env::temp_dir().join(format!("upkeep-health-{}", std::process::id()));
        let (a, b) = (tmp.join("a"), tmp.join("bin"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", b.join("dead")).unwrap();
        std::fs::write(a.join("node"), "").unwrap();
        std::fs::write(b.join("node"), "").unwrap();
        let mut ctx = crate::shell::mock::ctx(crate::shell::mock::MockRunner::new(&[]));
        ctx.home = tmp.to_string_lossy().into();
        let bl = broken_symlinks(&ctx);
        assert_eq!(bl[0].severity, Severity::Warning);
        let path = format!("{}:{}", a.display(), b.display());
        assert!(shadowed(&path, "/").iter().any(|i| i.title == "Multiple `node` on PATH"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scoring_and_runtimes() {
        let mut snap = Snapshot::default();
        for v in ["17.0.1", "21.0.2", "25.0.2"] {
            snap.installed.push(Installed { name: "Java".into(), version: v.into(), category: Category::Runtimes, provider: "runtimes".into(), ..Default::default() });
        }
        let m = multiple_runtimes(&snap);
        assert_eq!(m[0].title, "3 Java versions installed");
        let issues = vec![
            HealthIssue::new(Severity::Critical, "a", "", ""),
            HealthIssue::new(Severity::Warning, "b", "", ""),
            HealthIssue::new(Severity::Healthy, "c", "", ""),
        ];
        assert_eq!(score(&issues), 76);
        assert_eq!(Report { score: 76, issues }.label(), "Good");
    }
}
