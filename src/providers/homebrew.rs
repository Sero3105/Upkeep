use super::{Provider, argv, is_runtime_name};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;
use serde_json::Value;
use std::collections::HashMap;

pub struct Homebrew;

const QUIET: [(&str, &str); 3] = [("HOMEBREW_NO_AUTO_UPDATE", "1"), ("HOMEBREW_NO_ENV_HINTS", "1"), ("HOMEBREW_NO_COLOR", "1")];

fn brew(ctx: &Ctx, args: &[&str], secs: u64) -> crate::shell::RunResult {
    let mut a = vec!["brew"];
    a.extend_from_slice(args);
    ctx.runner.run(&a, &crate::shell::RunOpts { timeout: std::time::Duration::from_secs(secs), env: QUIET.to_vec(), ..Default::default() })
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

impl Provider for Homebrew {
    fn id(&self) -> &'static str {
        "homebrew"
    }
    fn name(&self) -> &'static str {
        "Homebrew"
    }
    fn category(&self) -> Category {
        Category::Homebrew
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("brew")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        if ctx.config.refresh_indexes {
            let r = brew(ctx, &["update", "--quiet"], 300);
            if !r.ok() {
                res.warnings.push(format!("brew update failed, results may be stale: {}", r.err.lines().last().unwrap_or("")));
            }
        }
        let info = brew(ctx, &["info", "--json=v2", "--installed"], 180);
        if !info.ok() {
            return Err(format!("brew info failed: {}", info.err.trim()));
        }
        let info: Value = serde_json::from_str(&info.out).map_err(|e| format!("brew info: bad JSON: {e}"))?;
        let outdated = brew(ctx, &["outdated", "--json=v2"], 180);
        let outdated: Value = serde_json::from_str(&outdated.out).map_err(|e| format!("brew outdated: bad JSON: {e}"))?;
        parse(&info, &outdated, &mut res);

        // Dependencies nothing needs any more (read-only dry run).
        let auto = brew(ctx, &["autoremove", "--dry-run"], 60);
        let unused: Vec<&str> = auto.out.lines().filter(|l| !l.starts_with("==>") && !l.trim().is_empty()).map(str::trim).collect();
        if auto.ok() && !unused.is_empty() {
            res.issues.push(
                HealthIssue::new(Severity::Info, format!("{} unused Homebrew dependencies", unused.len()), "Homebrew", unused.join(", "))
                    .consequence("Disk space used by formulae no installed package needs.")
                    .fix("Review, then run `brew autoremove` (Upkeep never removes packages itself)."),
            );
        }
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        for kind in ["--formula", "--cask"] {
            let r = brew(ctx, &["list", "--versions", kind, name], 30);
            // "name 1.2 1.3": several kegs can coexist; the newest is the active one after upgrade.
            if let Some(v) = r.out.split_whitespace().skip(1).max_by(|a, b| version::compare(a, b)) {
                return Some(v.to_string());
            }
        }
        None
    }
}

pub fn parse(info: &Value, outdated: &Value, res: &mut ScanResult) {
    let empty = vec![];
    let formulae = info["formulae"].as_array().unwrap_or(&empty);
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    for f in formulae {
        let name = f["name"].as_str().unwrap_or_default().to_string();
        let ver = f["linked_keg"].as_str().map(String::from).or_else(|| {
            f["installed"].as_array().and_then(|a| a.last()).and_then(|i| i["version"].as_str()).map(String::from)
        });
        let on_request = f["installed"].as_array().and_then(|a| a.last()).is_some_and(|i| i["installed_on_request"].as_bool() == Some(true));
        deps.insert(name.clone(), strs(&f["dependencies"]));
        res.installed.push(Installed {
            name: name.clone(),
            version: ver.unwrap_or_else(|| "?".into()),
            category: Category::Homebrew,
            provider: "homebrew".into(),
            source: if on_request { "Homebrew formula".into() } else { "Homebrew formula (dependency)".into() },
            note: f["desc"].as_str().map(String::from),
            ..Default::default()
        });
        let reason = f["deprecation_reason"].as_str().or(f["disable_reason"].as_str()).unwrap_or("no reason given");
        if f["disabled"].as_bool() == Some(true) {
            res.issues.push(
                HealthIssue::new(Severity::Critical, format!("{name} is disabled upstream"), format!("Homebrew: {name}"), reason)
                    .consequence("No more updates or security fixes; it may stop installing.")
                    .fix(format!("Find a replacement, then `brew uninstall {name}`.")),
            );
        } else if f["deprecated"].as_bool() == Some(true) {
            res.issues.push(
                HealthIssue::new(Severity::Warning, format!("{name} is deprecated"), format!("Homebrew: {name}"), reason)
                    .consequence("It will be disabled in a future Homebrew release.")
                    .fix("Plan a migration to a maintained alternative."),
            );
        }
    }
    let mut cask_meta: HashMap<String, (bool, Option<String>, bool)> = HashMap::new();
    for c in info["casks"].as_array().unwrap_or(&empty) {
        let token = c["token"].as_str().unwrap_or_default().to_string();
        let arts = c["artifacts"].as_array().cloned().unwrap_or_default();
        let needs_admin = arts.iter().any(|a| a.get("pkg").is_some() || a.get("installer").is_some());
        let app = arts.iter().find_map(|a| a["app"].as_array().and_then(|x| x.first()).and_then(|x| x.as_str()).map(String::from));
        let deprecated = c["deprecated"].as_bool() == Some(true) || c["disabled"].as_bool() == Some(true);
        res.installed.push(Installed {
            name: token.clone(),
            version: c["installed"].as_str().unwrap_or("?").to_string(),
            category: Category::Apps,
            provider: "homebrew".into(),
            source: "Homebrew cask".into(),
            path: app.as_ref().map(|a| format!("/Applications/{a}")),
            note: c["auto_updates"].as_bool().filter(|b| *b).map(|_| "app updates itself".to_string()),
            key: None,
        });
        if deprecated {
            res.issues.push(
                HealthIssue::new(Severity::Warning, format!("cask {token} is deprecated"), format!("Homebrew cask: {token}"), "Marked deprecated/disabled upstream")
                    .fix("Check the vendor site for an alternative distribution."),
            );
        }
        cask_meta.insert(token, (needs_admin, app, deprecated));
    }

    let mut dependents: HashMap<&str, Vec<String>> = HashMap::new();
    for (n, ds) in &deps {
        for d in ds {
            dependents.entry(d.as_str()).or_default().push(n.clone());
        }
    }
    for o in outdated["formulae"].as_array().unwrap_or(&empty) {
        let name = o["name"].as_str().unwrap_or_default().to_string();
        let current = o["installed_versions"].as_array().and_then(|a| a.last()).and_then(|v| v.as_str()).unwrap_or("?");
        let mut u = Update {
            name: name.clone(),
            provider: "homebrew".into(),
            category: Category::Homebrew,
            current: current.into(),
            latest: o["current_version"].as_str().unwrap_or("?").into(),
            source: "Homebrew formula".into(),
            support: Support::Supported,
            command: argv(&["brew", "upgrade", "--formula", &name]),
            prefetch: argv(&["brew", "fetch", "--formula", &name]),
            hint: Some("Homebrew cannot downgrade: rollback not available.".into()),
            dependencies: deps.get(&name).cloned().unwrap_or_default().into_iter().filter(|d| deps.contains_key(d)).collect(),
            dependents: dependents.get(name.as_str()).cloned().unwrap_or_default(),
            is_self_update: false,
            is_runtime: is_runtime_name(&name),
            ..Default::default()
        };
        u.dependents.sort();
        if o["pinned"].as_bool() == Some(true) {
            u.support = Support::Manual;
            u.command = None;
            u.notes.push("pinned".into());
            u.hint = Some(format!("Pinned on purpose. To update: `brew unpin {name} && brew upgrade {name}`."));
        }
        res.updates.push(u);
    }
    for o in outdated["casks"].as_array().unwrap_or(&empty) {
        let name = o["name"].as_str().unwrap_or_default().to_string();
        let (admin, app, deprecated) = cask_meta.get(&name).cloned().unwrap_or((false, None, false));
        let mut u = Update {
            name: name.clone(),
            provider: "homebrew".into(),
            category: Category::Apps,
            current: o["installed_versions"].as_array().and_then(|a| a.last()).and_then(|v| v.as_str()).unwrap_or("?").into(),
            latest: o["current_version"].as_str().unwrap_or("?").into(),
            source: "Homebrew cask".into(),
            support: Support::Supported,
            command: argv(&["brew", "upgrade", "--cask", &name]),
            prefetch: argv(&["brew", "fetch", "--cask", &name]),
            hint: Some("Homebrew cannot downgrade casks: rollback not available.".into()),
            requires_admin: admin,
            ..Default::default()
        };
        if let Some(a) = app {
            u.notes.push(format!("replaces /Applications/{a}; quit the app first"));
            u.app_path = Some(format!("/Applications/{a}"));
        }
        if admin {
            u.notes.push("installer package: needs administrator password".into());
        }
        if deprecated {
            u.notes.push("deprecated cask".into());
        }
        res.updates.push(u);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};
    use serde_json::json;

    fn fixtures() -> (Value, Value) {
        let info = json!({
            "formulae": [
                {"name": "glib", "dependencies": ["pcre2"], "linked_keg": "2.88.3", "deprecated": false, "disabled": false,
                 "installed": [{"version": "2.88.3", "installed_on_request": false}]},
                {"name": "pcre2", "dependencies": [], "linked_keg": "10.44", "installed": [{"version": "10.44"}]},
                {"name": "cairo", "dependencies": ["glib"], "linked_keg": "1.18.4", "installed": [{"version": "1.18.4", "installed_on_request": true}]},
                {"name": "harfbuzz", "dependencies": ["glib", "cairo"], "linked_keg": "14.4.0", "installed": [{"version": "14.4.0"}]},
                {"name": "oldtool", "dependencies": [], "deprecated": true, "deprecation_reason": "unmaintained", "installed": [{"version": "1.0"}]}
            ],
            "casks": [
                {"token": "zoom", "installed": "6.0", "version": "6.1", "artifacts": [{"pkg": ["Zoom.pkg"]}]},
                {"token": "google-chrome", "installed": "150.0", "auto_updates": true, "artifacts": [{"app": ["Google Chrome.app"]}]}
            ]
        });
        let outdated = json!({
            "formulae": [
                {"name": "glib", "installed_versions": ["2.88.3"], "current_version": "2.90.1", "pinned": false},
                {"name": "pcre2", "installed_versions": ["10.44"], "current_version": "10.45", "pinned": true}
            ],
            "casks": [
                {"name": "zoom", "installed_versions": ["6.0"], "current_version": "6.1"},
                {"name": "google-chrome", "installed_versions": ["150.0"], "current_version": "157.0"}
            ]
        });
        (info, outdated)
    }

    #[test]
    fn parses_updates_dependencies_and_issues() {
        let (info, outdated) = fixtures();
        let mut r = ScanResult::default();
        parse(&info, &outdated, &mut r);
        assert_eq!(r.installed.len(), 7);
        let glib = r.updates.iter().find(|u| u.name == "glib").unwrap();
        assert_eq!((glib.current.as_str(), glib.latest.as_str()), ("2.88.3", "2.90.1"));
        assert_eq!(glib.dependencies, vec!["pcre2"]);
        assert_eq!(glib.dependents, vec!["cairo", "harfbuzz"]);
        assert_eq!(glib.support, Support::Supported);
        let pcre = r.updates.iter().find(|u| u.name == "pcre2").unwrap();
        assert_eq!(pcre.support, Support::Manual, "pinned formulae are never auto-upgraded");
        let zoom = r.updates.iter().find(|u| u.name == "zoom").unwrap();
        assert!(zoom.requires_admin && zoom.category == Category::Apps);
        assert!(r.issues.iter().any(|i| i.title.contains("oldtool") && i.severity == Severity::Warning));
        let chrome = r.installed.iter().find(|i| i.name == "google-chrome").unwrap();
        assert_eq!(chrome.path.as_deref(), Some("/Applications/Google Chrome.app"));
    }

    #[test]
    fn verifies_installed_version_via_brew_list() {
        let c = ctx(MockRunner::new(&["brew"])
            .on("brew list --versions --formula gh", 0, "gh 2.103.1 2.103.3\n")
            .on("brew list --versions --formula zoom", 1, "")
            .on("brew list --versions --cask zoom", 0, "zoom 6.1\n"));
        assert_eq!(Homebrew.installed_version(&c, "gh").as_deref(), Some("2.103.3"));
        assert_eq!(Homebrew.installed_version(&c, "zoom").as_deref(), Some("6.1"));
    }
}
