//! Microsoft Office & co. through Microsoft AutoUpdate's official CLI (`msupdate`).
//! Its privileged helper installs updates: no password needed.

use super::apps::MSUPDATE;
use super::{Provider, plist_value};
use crate::model::*;
use crate::shell::Ctx;
use std::path::Path;

pub struct MicrosoftAutoUpdate;

impl Provider for MicrosoftAutoUpdate {
    fn id(&self) -> &'static str {
        "msupdate"
    }
    fn name(&self) -> &'static str {
        "Microsoft AutoUpdate"
    }
    fn category(&self) -> Category {
        Category::Apps
    }
    fn available(&self, _ctx: &Ctx) -> bool {
        Path::new(MSUPDATE).exists()
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&[MSUPDATE, "--list", "--format", "plist"], 180);
        if r.timed_out {
            return Err("msupdate timed out".into());
        }
        Ok(ScanResult { updates: parse(&r.out), ..Default::default() })
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        // Office reports full builds (16.90.24100611) = CFBundleVersion.
        let plist = format!("/Applications/{name}.app/Contents/Info.plist");
        super::plist_strings(ctx, &plist, &["CFBundleVersion"]).pop().flatten()
    }
}

/// `msupdate --list --format plist`: progress text, then an array of update dicts.
pub fn parse(out: &str) -> Vec<Update> {
    let Some(start) = out.find("<plist").or_else(|| out.find("<array")) else { return vec![] };
    out[start..]
        .split("<dict>")
        .skip(1)
        .filter_map(|d| {
            let id = plist_value(d, "Application ID")?;
            let path = plist_value(d, "ApplicationToBeUpdatedPath").unwrap_or_default();
            let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).or_else(|| plist_value(d, "Title"))?;
            let size = plist_value(d, "FullUpdaterSize").and_then(|s| s.parse::<f64>().ok()).map(|b| format!("{:.0} MB", b / 1e6));
            Some(Update {
                name,
                provider: "msupdate".into(),
                category: Category::Apps,
                current: plist_value(d, "Baseline Version").unwrap_or_else(|| "?".into()),
                latest: plist_value(d, "Update Version")?,
                source: "Microsoft AutoUpdate".into(),
                support: Support::Supported,
                command: super::argv(&[MSUPDATE, "--install", "--apps", &id, "--wait", "1800"]),
                hint: Some("Quit the app first; Microsoft AutoUpdate cannot roll back.".into()),
                download_size: size,
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_msupdate_plist() {
        let out = "\u{1b}[2KChecking for updates...<?xml version=\"1.0\"?><plist version=\"1.0\"><array><dict>\
            <key>Application ID</key><string>XCEL2019</string>\
            <key>ApplicationToBeUpdatedPath</key><string>/Applications/Microsoft Excel.app</string>\
            <key>Baseline Version</key><string>16.89.24091630</string>\
            <key>FullUpdaterSize</key><string>1031094487</string>\
            <key>Title</key><string>Excel Update 16.90.24100611</string>\
            <key>Update Version</key><string>16.90.24100611</string></dict></array></plist>";
        let u = parse(out);
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].name.as_str(), u[0].current.as_str(), u[0].latest.as_str()), ("Microsoft Excel", "16.89.24091630", "16.90.24100611"));
        assert!(u[0].command.as_ref().unwrap().contains(&"XCEL2019".to_string()));
        assert_eq!(u[0].download_size.as_deref(), Some("1031 MB"));
        assert!(parse("No updates available").is_empty());
    }
}
