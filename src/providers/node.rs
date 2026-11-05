use super::{Provider, argv, writable};
use crate::model::*;
use crate::shell::Ctx;
use serde_json::Value;

/// Global npm packages and npm itself. Project dependencies are never touched.
pub struct Npm;

impl Provider for Npm {
    fn id(&self) -> &'static str {
        "npm"
    }
    fn name(&self) -> &'static str {
        "npm (global)"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("npm")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let ls = ctx.run(&["npm", "ls", "-g", "--depth=0", "--json"], 60);
        let ls: Value = serde_json::from_str(&ls.out).unwrap_or(Value::Null);
        for (name, v) in ls["dependencies"].as_object().into_iter().flatten() {
            res.installed.push(Installed {
                name: name.clone(),
                version: v["version"].as_str().unwrap_or("?").into(),
                category: Category::Packages,
                provider: "npm".into(),
                source: "npm (global)".into(),
                ..Default::default()
            });
        }
        // Exit code 1 just means "something is outdated".
        let r = ctx.run(&["npm", "outdated", "-g", "--json"], 120);
        if r.out.trim().is_empty() {
            if !r.ok() {
                res.warnings.push(format!("npm outdated failed: {}", r.err.lines().next().unwrap_or("")));
            }
            return Ok(res);
        }
        let prefix = ctx.out(&["npm", "prefix", "-g"], 30).unwrap_or_default();
        let admin = !prefix.trim().is_empty() && !writable(&format!("{}/lib", prefix.trim()));
        let v: Value = serde_json::from_str(&r.out).map_err(|e| format!("npm outdated: bad JSON: {e}"))?;
        res.updates = parse_outdated(&v, "npm", admin);
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let r = ctx.run(&["npm", "ls", "-g", "--depth=0", "--json", name], 60);
        let v: Value = serde_json::from_str(&r.out).ok()?;
        v["dependencies"][name]["version"].as_str().map(String::from)
    }
}

/// `npm outdated -g --json` / `pnpm outdated -g --format json`: { name: { current, latest } }.
pub fn parse_outdated(v: &Value, provider: &str, admin: bool) -> Vec<Update> {
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, info)| {
            let current = info["current"].as_str()?;
            let latest = info["latest"].as_str()?;
            let install = if provider == "npm" { vec!["npm", "install", "-g"] } else { vec!["pnpm", "add", "-g"] };
            let with = |ver: &str| {
                let mut a = install.clone();
                let spec = format!("{name}@{ver}");
                a.push(&spec);
                argv(&a)
            };
            let mut u = Update {
                name: name.clone(),
                provider: provider.into(),
                category: Category::Packages,
                current: current.into(),
                latest: latest.into(),
                source: format!("{provider} (global)"),
                support: Support::Supported,
                command: with(latest),
                rollback: with(current),
                requires_admin: admin,
                is_self_update: name == provider,
                ..Default::default()
            };
            if info["location"].as_str().is_some_and(|l| l.starts_with("/opt/homebrew") || l.starts_with("/usr/local/Cellar")) && u.is_self_update {
                u.notes.push("bundled with Homebrew node: `brew upgrade node` resets it".into());
            }
            if info["isDeprecated"].as_bool() == Some(true) {
                u.notes.push("deprecated on the registry".into());
            }
            Some(u)
        })
        .collect()
}

pub struct Pnpm;

impl Provider for Pnpm {
    fn id(&self) -> &'static str {
        "pnpm"
    }
    fn name(&self) -> &'static str {
        "pnpm (global)"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew", "npm"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("pnpm")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&["pnpm", "outdated", "-g", "--format", "json"], 120);
        let v: Value = serde_json::from_str(r.out.trim()).unwrap_or(Value::Null);
        if !v.is_object() && !r.ok() {
            return Err(format!("pnpm outdated failed: {}", r.err.lines().next().unwrap_or("")));
        }
        Ok(ScanResult { updates: parse_outdated(&v, "pnpm", false), ..Default::default() })
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let v: Value = serde_json::from_str(&ctx.out(&["pnpm", "ls", "-g", "--depth=0", "--json"], 60)?).ok()?;
        v.as_array()?.iter().find_map(|p| p["dependencies"][name]["version"].as_str().map(String::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};

    #[test]
    fn npm_outdated_exit_1_is_not_an_error() {
        let out = r#"{"npm": {"current": "11.20.0", "wanted": "12.2.2", "latest": "12.2.2", "location": "/opt/homebrew/lib/node_modules/npm"},
                      "typescript": {"current": "5.4.0", "latest": "5.6.2", "location": "/opt/homebrew/lib/node_modules/typescript"}}"#;
        let c = ctx(MockRunner::new(&["npm"])
            .on("npm ls -g --depth=0 --json", 0, r#"{"dependencies": {"npm": {"version": "11.20.0"}, "typescript": {"version": "5.4.0"}}}"#)
            .on("npm outdated -g --json", 1, out)
            .on("npm prefix -g", 0, "/tmp\n"));
        let r = Npm.scan(&c).unwrap();
        assert_eq!(r.installed.len(), 2);
        assert_eq!(r.updates.len(), 2);
        let npm = r.updates.iter().find(|u| u.name == "npm").unwrap();
        assert!(npm.is_self_update && !npm.notes.is_empty());
        let ts = r.updates.iter().find(|u| u.name == "typescript").unwrap();
        assert_eq!(ts.command.as_ref().unwrap().join(" "), "npm install -g typescript@5.6.2");
        assert_eq!(ts.rollback.as_ref().unwrap().join(" "), "npm install -g typescript@5.4.0");
    }
}
