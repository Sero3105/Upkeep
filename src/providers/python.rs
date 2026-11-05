use super::{Provider, argv};
use crate::model::*;
use crate::shell::Ctx;
use serde_json::Value;

/// Packages of the `python3` on PATH. Virtualenvs/conda envs are never touched, and
/// externally-managed interpreters (PEP 668, e.g. Homebrew's) are reported as manual.
pub struct Pip;

const MANAGED_CHECK: &str = "import sysconfig,os;print(os.path.exists(os.path.join(sysconfig.get_paths()['stdlib'],'EXTERNALLY-MANAGED')))";

impl Provider for Pip {
    fn id(&self) -> &'static str {
        "pip"
    }
    fn name(&self) -> &'static str {
        "pip (python3)"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("python3")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        if let Some(env) = ["VIRTUAL_ENV", "CONDA_PREFIX"].iter().find_map(|k| ctx.env.get(*k)) {
            res.warnings.push(format!("pip skipped: running inside an environment ({env}); Upkeep never modifies virtual environments"));
            return Ok(res);
        }
        let r = ctx.run(&["python3", "-m", "pip", "list", "--outdated", "--format=json", "--disable-pip-version-check"], 180);
        if !r.ok() {
            return Err(format!("pip list failed: {}", r.err.lines().last().unwrap_or("")));
        }
        let managed = ctx.out(&["python3", "-c", MANAGED_CHECK], 20).is_some_and(|o| o.trim() == "True");
        let v: Value = serde_json::from_str(&r.out).map_err(|e| format!("pip: bad JSON: {e}"))?;
        res.updates = parse(&v, managed);
        let purelib = ctx.out(&["python3", "-c", "import sysconfig;print(sysconfig.get_paths()['purelib'])"], 20).unwrap_or_default();
        if !managed && !super::writable(&purelib) {
            res.updates.iter_mut().for_each(|u| u.requires_admin = true);
        }
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let out = ctx.out(&["python3", "-m", "pip", "show", name], 30)?;
        out.lines().find_map(|l| l.strip_prefix("Version:")).map(|v| v.trim().to_string())
    }
}

pub fn parse(v: &Value, externally_managed: bool) -> Vec<Update> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p["name"].as_str()?;
            let (cur, new) = (p["version"].as_str()?, p["latest_version"].as_str()?);
            let pin = |ver: &str| argv(&["python3", "-m", "pip", "install", &format!("{name}=={ver}")]);
            Some(if externally_managed && name == "pip" {
                Update {
                    name: name.into(),
                    provider: "pip".into(),
                    category: Category::Packages,
                    current: cur.into(),
                    latest: new.into(),
                    source: "pip (externally managed Python)".into(),
                    support: Support::Manual,
                    hint: Some("pip ships with Homebrew's Python: it updates with `brew upgrade python@3.x`.".into()),
                    is_self_update: true,
                    ..Default::default()
                }
            } else if externally_managed {
                // Packages you installed into Homebrew's Python yourself (with --break-system-packages).
                let pin = |ver: &str| argv(&["python3", "-m", "pip", "install", "--break-system-packages", &format!("{name}=={ver}")]);
                Update {
                    name: name.into(),
                    provider: "pip".into(),
                    category: Category::Packages,
                    current: cur.into(),
                    latest: new.into(),
                    source: "pip (Homebrew Python)".into(),
                    support: Support::Supported,
                    command: pin(new),
                    rollback: pin(cur),
                    notes: vec!["externally managed Python (PEP 668)".into()],
                    ..Default::default()
                }
            } else {
                Update {
                    name: name.into(),
                    provider: "pip".into(),
                    category: Category::Packages,
                    current: cur.into(),
                    latest: new.into(),
                    source: "pip".into(),
                    support: Support::Supported,
                    command: pin(new),
                    rollback: pin(cur),
                    is_self_update: name == "pip",
                    ..Default::default()
                }
            })
        })
        .collect()
}

/// Tools installed with `uv tool install`.
pub struct Uv;

impl Provider for Uv {
    fn id(&self) -> &'static str {
        "uv"
    }
    fn name(&self) -> &'static str {
        "uv tools"
    }
    fn category(&self) -> Category {
        Category::Packages
    }
    fn after(&self) -> &'static [&'static str] {
        &["homebrew"]
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("uv")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let list = ctx.out(&["uv", "tool", "list"], 60).unwrap_or_default();
        for l in list.lines().filter(|l| !l.starts_with('-') && !l.trim().is_empty() && !l.starts_with("No tools")) {
            let mut it = l.split_whitespace();
            if let (Some(n), Some(v)) = (it.next(), it.next()) {
                res.installed.push(Installed {
                    name: n.into(),
                    version: v.trim_start_matches('v').into(),
                    category: Category::Packages,
                    provider: "uv".into(),
                    source: "uv tool".into(),
                    ..Default::default()
                });
            }
        }
        let r = ctx.run(&["uv", "tool", "list", "--outdated"], 120);
        if !r.ok() {
            res.warnings.push(format!("uv tool list --outdated failed (uv too old?): {}", r.err.lines().next().unwrap_or("")));
        }
        res.updates = parse_uv(&r.out);
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let list = ctx.out(&["uv", "tool", "list"], 60)?;
        list.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next() == Some(name)).then(|| it.next().map(|v| v.trim_start_matches('v').to_string())).flatten()
        })
    }
}

/// "ruff v0.6.0 [latest: 0.7.0]"
pub fn parse_uv(text: &str) -> Vec<Update> {
    text.lines()
        .filter_map(|l| {
            let (head, tail) = l.split_once("[latest:")?;
            let mut it = head.split_whitespace();
            let (name, cur) = (it.next()?, it.next()?.trim_start_matches('v'));
            let latest = tail.trim().trim_end_matches(']').trim().trim_start_matches('v');
            Some(Update {
                name: name.into(),
                provider: "uv".into(),
                category: Category::Packages,
                current: cur.into(),
                latest: latest.into(),
                source: "uv tool".into(),
                support: Support::Supported,
                command: argv(&["uv", "tool", "upgrade", name]),
                rollback: argv(&["uv", "tool", "install", "--force", &format!("{name}=={cur}")]),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};

    #[test]
    fn pip_respects_pep668_and_virtualenvs() {
        let v: Value = serde_json::from_str(r#"[{"name": "pip", "version": "26.2.1", "latest_version": "26.3"}, {"name": "rich", "version": "13.0", "latest_version": "14.0"}]"#).unwrap();
        let managed = parse(&v, true);
        assert!(managed[0].support == Support::Manual && managed[0].command.is_none(), "pip itself stays with brew");
        assert!(managed[1].command.as_ref().unwrap().contains(&"--break-system-packages".to_string()));
        let free = parse(&v, false);
        assert_eq!(free[1].command.as_ref().unwrap().join(" "), "python3 -m pip install rich==14.0");
        assert!(free[0].is_self_update);

        let mut c = ctx(MockRunner::new(&["python3"]));
        c.env.insert("VIRTUAL_ENV".into(), "/proj/.venv".into());
        let r = Pip.scan(&c).unwrap();
        assert!(r.updates.is_empty() && r.warnings[0].contains("never modifies"));
    }

    #[test]
    fn parses_uv_outdated() {
        let u = parse_uv("ruff v0.6.0 [latest: 0.7.0]\n- ruff\nblack v24.1.0 [latest: 24.2.0]\n");
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].current.as_str(), u[0].latest.as_str()), ("0.6.0", "0.7.0"));
        assert_eq!(u[0].rollback.as_ref().unwrap().join(" "), "uv tool install --force ruff==0.6.0");
    }
}
