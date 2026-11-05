//! Docker Desktop, engine and images. Read-only: Upkeep never removes images, volumes or
//! containers. Cleanup is only suggested, as a separate command the user runs explicitly.

use super::{Provider, plist_strings};
use crate::model::*;
use crate::shell::Ctx;
use serde_json::Value;
use std::path::Path;

pub struct Docker;

const DESKTOP: &str = "/Applications/Docker.app";

impl Provider for Docker {
    fn id(&self) -> &'static str {
        "docker"
    }
    fn name(&self) -> &'static str {
        "Docker"
    }
    fn category(&self) -> Category {
        Category::Docker
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("docker") || Path::new(DESKTOP).exists()
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let item = |name: &str, version: &str, source: &str, note: Option<String>| Installed {
            name: name.into(),
            version: version.into(),
            category: Category::Docker,
            provider: "docker".into(),
            source: source.into(),
            note,
            ..Default::default()
        };
        if Path::new(DESKTOP).exists() {
            let v = plist_strings(ctx, &format!("{DESKTOP}/Contents/Info.plist"), &["CFBundleShortVersionString"]);
            let ver = v[0].clone().unwrap_or_else(|| "?".into());
            res.installed.push(item("Docker Desktop", &ver, "Docker Desktop", Some("updates via Docker Desktop's updater (or `brew upgrade --cask docker` if cask-managed)".into())));
        }
        if !ctx.has("docker") {
            return Ok(res);
        }
        let r = ctx.run(&["docker", "version", "--format", "{{json .}}"], 20);
        let v: Value = r.out.lines().next().and_then(|l| serde_json::from_str(l).ok()).unwrap_or(Value::Null);
        if let Some(c) = v["Client"]["Version"].as_str() {
            res.installed.push(item("Docker CLI", c, "Docker", None));
        }
        let Some(engine) = v["Server"]["Version"].as_str() else {
            res.issues.push(HealthIssue::new(Severity::Info, "Docker daemon not running", "Docker", "Engine version and images were not checked").fix("Start Docker Desktop and re-run the check."));
            return Ok(res);
        };
        res.installed.push(item("Docker Engine", engine, "Docker", None));
        let imgs = ctx.out(&["docker", "images", "--format", "{{json .}}"], 30).unwrap_or_default();
        let (images, dangling) = parse_images(&imgs);
        for (name, tag, size) in &images {
            res.installed.push(item(name, tag, "Docker image", Some(format!("{size}; newer tags not verifiable without `docker pull`"))));
        }
        if dangling > 0 {
            res.issues.push(
                HealthIssue::new(Severity::Info, format!("{dangling} dangling Docker images"), "Docker", "Untagged layers left by rebuilds/pulls")
                    .consequence("Disk space inside the Docker VM.")
                    .fix("Review with `docker images -f dangling=true`, then `docker image prune` (destructive: Upkeep never runs it)."),
            );
        }
        Ok(res)
    }
}

/// `docker images --format '{{json .}}'` lines -> (repo, tag, size) + dangling count.
pub fn parse_images(text: &str) -> (Vec<(String, String, String)>, usize) {
    let mut out = vec![];
    let mut dangling = 0;
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let (repo, tag) = (v["Repository"].as_str().unwrap_or("<none>"), v["Tag"].as_str().unwrap_or("<none>"));
        if repo == "<none>" {
            dangling += 1;
            continue;
        }
        out.push((repo.to_string(), tag.to_string(), v["Size"].as_str().unwrap_or("?").to_string()));
    }
    (out, dangling)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_images_and_dangling() {
        let t = r#"{"Repository":"postgres","Tag":"16","Size":"432MB"}
{"Repository":"<none>","Tag":"<none>","Size":"1GB"}"#;
        let (imgs, d) = parse_images(t);
        assert_eq!(imgs.len(), 1);
        assert_eq!(d, 1);
    }
}
