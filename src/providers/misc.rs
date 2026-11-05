use super::{Provider, argv, writable};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;
use serde_json::Value;

pub struct Rustup;

impl Provider for Rustup {
    fn id(&self) -> &'static str {
        "rustup"
    }
    fn name(&self) -> &'static str {
        "rustup"
    }
    fn category(&self) -> Category {
        Category::Runtimes
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("rustup")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&["rustup", "check"], 120);
        if r.out.trim().is_empty() {
            return Err(format!("rustup check failed: {}", r.err.trim()));
        }
        // Homebrew's rustup disables self-update; brew upgrades it instead.
        let brew_managed = ctx.runner.which("rustup").is_some_and(|p| super::origin(&p) == "Homebrew");
        Ok(parse_check(&r.out, brew_managed))
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let out = ctx.out(&["rustup", "check"], 120)?;
        out.lines().find(|l| l.starts_with(&format!("{name} - "))).and_then(|l| {
            let tail = l.rsplit(" : ").next()?;
            let tail = tail.rsplit("->").next()?;
            version::extract(tail)
        })
    }
}

/// "stable-aarch64-apple-darwin - Update available : 1.98.0 (x 2026-09-01) -> 1.99.1 (y 2026-10-13)"
pub fn parse_check(text: &str, brew_managed: bool) -> ScanResult {
    let mut res = ScanResult::default();
    for l in text.lines() {
        let Some((name, status)) = l.split_once(" - ") else { continue };
        let Some((state, vers)) = status.split_once(" : ") else { continue };
        let is_self = name == "rustup";
        let (cur, new) = match vers.split_once("->") {
            Some((a, b)) => (version::extract(a), version::extract(b)),
            None => (version::extract(vers), None),
        };
        let cur = cur.unwrap_or_else(|| "?".into());
        res.installed.push(Installed {
            name: if is_self { "rustup".into() } else { format!("Rust {name}") },
            version: cur.clone(),
            category: Category::Runtimes,
            provider: "rustup".into(),
            source: "rustup".into(),
            ..Default::default()
        });
        if !state.contains("Update available") || (is_self && brew_managed) {
            continue;
        }
        res.updates.push(Update {
            name: name.into(),
            provider: "rustup".into(),
            category: Category::Runtimes,
            current: cur,
            latest: new.unwrap_or_else(|| "?".into()),
            source: "rustup".into(),
            support: Support::Supported,
            command: if is_self { argv(&["rustup", "self", "update"]) } else { argv(&["rustup", "update", name]) },
            hint: Some("rustup channels move forward only; pin an exact version with `rustup toolchain install <ver>` if a project needs it.".into()),
            is_self_update: is_self,
            is_runtime: !is_self,
            ..Default::default()
        });
    }
    res
}

/// Binaries installed with `cargo install`, checked against the crates.io API.
pub struct Cargo;

impl Provider for Cargo {
    fn id(&self) -> &'static str {
        "cargo"
    }
    fn name(&self) -> &'static str {
        "cargo install"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew", "rustup"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("cargo")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let list = ctx.out(&["cargo", "install", "--list"], 30).unwrap_or_default();
        let crates = parse_install_list(&list);
        let latest: Vec<Option<String>> = std::thread::scope(|s| {
            let hs: Vec<_> = crates.iter().map(|(n, _)| s.spawn(move || crates_io_latest(ctx, n))).collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        for ((name, cur), new) in crates.into_iter().zip(latest) {
            res.installed.push(Installed {
                name: name.clone(),
                version: cur.clone(),
                category: Category::Packages,
                provider: "cargo".into(),
                source: "cargo install (crates.io)".into(),
                ..Default::default()
            });
            match new {
                Some(new) if version::is_newer(&new, &cur) => res.updates.push(Update {
                    command: argv(&["cargo", "install", "--locked", &name]),
                    rollback: argv(&["cargo", "install", "--locked", "--version", &cur, &name]),
                    name,
                    provider: "cargo".into(),
                    category: Category::Packages,
                    current: cur,
                    latest: new,
                    source: "crates.io".into(),
                    support: Support::Supported,
                    hint: Some("Builds from source: can take minutes.".into()),
                    ..Default::default()
                }),
                Some(_) => {}
                None => res.warnings.push(format!("cargo: could not query crates.io for {name}")),
            }
        }
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let list = ctx.out(&["cargo", "install", "--list"], 30)?;
        parse_install_list(&list).into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

/// Registry crates only ("name v1.2.3:"); path/git installs ("name v1 (/src):") are skipped.
pub fn parse_install_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.starts_with(char::is_whitespace) && l.ends_with(':') && !l.contains('('))
        .filter_map(|l| {
            let (n, v) = l.trim_end_matches(':').split_once(' ')?;
            Some((n.to_string(), v.trim_start_matches('v').to_string()))
        })
        .collect()
}

fn crates_io_latest(ctx: &Ctx, name: &str) -> Option<String> {
    let url = format!("https://crates.io/api/v1/crates/{name}");
    let out = ctx.out(&["curl", "-fsSL", "--proto", "=https", "--max-time", "10", "-A", "upkeep (macOS update manager)", &url], 15)?;
    let v: Value = serde_json::from_str(&out).ok()?;
    v["crate"]["max_stable_version"].as_str().map(String::from)
}

/// RubyGems. The macOS system Ruby (/usr/bin/gem) is never modified.
pub struct Gem;

impl Provider for Gem {
    fn id(&self) -> &'static str {
        "gem"
    }
    fn name(&self) -> &'static str {
        "RubyGems"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.runner.which("gem").is_some_and(|p| p != "/usr/bin/gem")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&["gem", "outdated"], 120);
        if !r.ok() {
            return Err(format!("gem outdated failed: {}", r.err.trim()));
        }
        let gemdir = ctx.out(&["gem", "env", "gemdir"], 30).unwrap_or_default();
        let admin = !writable(&gemdir);
        Ok(ScanResult { updates: parse_gem(&r.out, admin), ..Default::default() })
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        // "rake (13.2.1, 13.0.6)": newest first.
        let out = ctx.out(&["gem", "list", "--exact", name], 30)?;
        let l = out.lines().find(|l| l.starts_with(&format!("{name} (")))?;
        l[l.find('(')? + 1..].split([',', ')']).filter_map(version::extract).max_by(|a, b| version::compare(a, b))
    }
}

/// "rake (13.0.6 < 13.2.1)"
pub fn parse_gem(text: &str, admin: bool) -> Vec<Update> {
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(" (")?;
            let (cur, new) = rest.trim_end_matches(')').split_once(" < ")?;
            Some(Update {
                name: name.into(),
                provider: "gem".into(),
                category: Category::Packages,
                current: cur.into(),
                latest: new.into(),
                source: "RubyGems".into(),
                support: Support::Supported,
                command: argv(&["gem", "update", name]),
                // `gem update` keeps the old version installed: removing the new one restores it.
                rollback: argv(&["gem", "uninstall", name, "-v", new]),
                requires_admin: admin,
                ..Default::default()
            })
        })
        .collect()
}

/// Composer global packages (within the global composer.json constraints).
pub struct Composer;

impl Provider for Composer {
    fn id(&self) -> &'static str {
        "composer"
    }
    fn name(&self) -> &'static str {
        "Composer (global)"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("composer")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&["composer", "global", "outdated", "--direct", "--format=json", "--no-interaction"], 120);
        let v: Value = serde_json::from_str(&r.out).map_err(|_| format!("composer outdated failed: {}", r.err.lines().last().unwrap_or("")))?;
        let updates = v["installed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let (name, cur, new) = (p["name"].as_str()?, p["version"].as_str()?, p["latest"].as_str()?);
                Some(Update {
                    name: name.into(),
                    provider: "composer".into(),
                    category: Category::Packages,
                    current: cur.into(),
                    latest: new.into(),
                    source: "Composer (global)".into(),
                    support: Support::Supported,
                    command: argv(&["composer", "global", "update", name, "--no-interaction"]),
                    rollback: argv(&["composer", "global", "require", &format!("{name}:{cur}"), "--no-interaction"]),
                    notes: (p["latest-status"].as_str() == Some("update-possible")).then(|| "major update: may exceed composer.json constraint".to_string()).into_iter().collect(),
                    ..Default::default()
                })
            })
            .collect();
        Ok(ScanResult { updates, ..Default::default() })
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let out = ctx.out(&["composer", "global", "show", name, "--format=json", "--no-interaction"], 60)?;
        let v: Value = serde_json::from_str(&out).ok()?;
        v["versions"].as_array()?.first()?.as_str().map(|s| s.trim_start_matches('v').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustup_check() {
        let out = "stable-aarch64-apple-darwin - Update available : 1.98.0 (aaa 2026-09-01) -> 1.99.1 (bbb 2026-10-13)\n\
                   nightly-aarch64-apple-darwin - Up to date : 1.100.0-nightly (ccc 2026-11-01)\n\
                   rustup - Update available : 1.28.1 -> 1.28.2\n";
        let r = parse_check(out, true);
        assert_eq!(r.installed.len(), 3);
        assert_eq!(r.updates.len(), 1, "brew-managed rustup must not self-update");
        assert_eq!((r.updates[0].current.as_str(), r.updates[0].latest.as_str()), ("1.98.0", "1.99.1"));
        assert_eq!(parse_check(out, false).updates.len(), 2);
    }

    #[test]
    fn parses_cargo_and_gem() {
        let l = parse_install_list("ripgrep v14.1.0:\n    rg\nlocal v0.1.0 (/src/local):\n    local\n");
        assert_eq!(l, vec![("ripgrep".to_string(), "14.1.0".to_string())]);
        let g = parse_gem("rake (13.0.6 < 13.2.1)\nminitest (5.16.0 < 5.25.1)\n", false);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].rollback.as_ref().unwrap().join(" "), "gem uninstall rake -v 13.2.1");
    }
}
