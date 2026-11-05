use super::{Provider, argv};
use crate::model::*;
use crate::shell::Ctx;

/// macOS + Apple software via `softwareupdate`. Needs your administrator password (sudo, typed by
/// you in the terminal) and never restarts the Mac by itself: the report tells you to restart.
pub struct SoftwareUpdate;

impl Provider for SoftwareUpdate {
    fn id(&self) -> &'static str {
        "softwareupdate"
    }
    fn name(&self) -> &'static str {
        "macOS Software Update"
    }
    fn category(&self) -> Category {
        Category::System
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("softwareupdate")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let os = ctx.out(&["sw_vers", "-productVersion"], 10).unwrap_or_default().trim().to_string();
        let build = ctx.out(&["sw_vers", "-buildVersion"], 10).unwrap_or_default().trim().to_string();
        res.installed.push(Installed {
            name: "macOS".into(),
            version: os.clone(),
            category: Category::System,
            provider: self.id().into(),
            source: "Apple".into(),
            note: (!build.is_empty()).then(|| format!("build {build}")),
            ..Default::default()
        });
        let r = ctx.run(&["softwareupdate", "--list"], 240);
        if r.timed_out {
            return Err("softwareupdate timed out (network?)".into());
        }
        res.updates = parse(&format!("{}\n{}", r.out, r.err), &os);
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        if name.starts_with("macOS") {
            // Changes only after the restart that completes the update.
            return ctx.out(&["sw_vers", "-productVersion"], 10).map(|s| s.trim().to_string());
        }
        if name.contains("Command Line Tools") {
            let info = ctx.out(&["pkgutil", "--pkg-info=com.apple.pkg.CLTools_Executables"], 10)?;
            return info.lines().find_map(|l| l.strip_prefix("version:")).map(|v| v.trim().to_string());
        }
        None
    }
}

pub fn parse(text: &str, os_version: &str) -> Vec<Update> {
    let mut out = vec![];
    let mut label: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(lab) = l.strip_prefix("* Label:") {
            label = Some(lab.trim().to_string());
        } else if let (Some(lab), true) = (&label, l.starts_with("Title:")) {
            let field = |k: &str| {
                l.split(", ").find_map(|p| p.trim().strip_prefix(k).map(|v| v.trim().trim_end_matches(',').to_string()))
            };
            let title = field("Title:").unwrap_or_else(|| lab.clone());
            let restart = field("Action:").is_some_and(|a| a.contains("restart"));
            let is_os = title.starts_with("macOS");
            let security = ["Security", "XProtect", "Background"].iter().any(|k| title.contains(k));
            let mut u = Update {
                name: title.clone(),
                provider: "softwareupdate".into(),
                category: Category::System,
                current: if is_os { os_version.to_string() } else { "installed".into() },
                latest: field("Version:").unwrap_or_else(|| "?".into()),
                source: "Apple Software Update".into(),
                support: Support::Supported,
                command: argv(&["sudo", "softwareupdate", "--install", lab]),
                hint: Some("Alternative: System Settings → General → Software Update.".into()),
                requires_admin: true,
                requires_restart: restart,
                download_size: field("Size:").map(|s| human_kib(&s)),
                ..Default::default()
            };
            if security {
                u.notes.push("security update".into());
            }
            if field("Recommended:").as_deref() == Some("YES") {
                u.notes.push("recommended by Apple".into());
            }
            out.push(u);
            label = None;
        }
    }
    out
}

fn human_kib(s: &str) -> String {
    match s.trim_end_matches("KiB").trim().parse::<f64>() {
        Ok(k) if k >= 1024.0 * 1024.0 => format!("{:.1} GB", k / 1024.0 / 1024.0),
        Ok(k) if k >= 1024.0 => format!("{:.0} MB", k / 1024.0),
        Ok(k) => format!("{k:.0} KB"),
        Err(_) => s.to_string(),
    }
}

/// Mac App Store via `mas`.
pub struct Mas;

impl Provider for Mas {
    fn id(&self) -> &'static str {
        "mas"
    }
    fn name(&self) -> &'static str {
        "Mac App Store"
    }
    fn category(&self) -> Category {
        Category::Apps
    }
    fn available(&self, ctx: &Ctx) -> bool {
        ctx.has("mas")
    }
    fn missing_hint(&self) -> Option<&'static str> {
        Some("`mas` not installed: Mac App Store updates are not checked. Install with `brew install mas`.")
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let r = ctx.run(&["mas", "outdated"], 120);
        if !r.ok() {
            return Err(format!("mas outdated failed: {}", r.err.trim()));
        }
        Ok(ScanResult { updates: parse_mas(&r.out), ..Default::default() })
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        // "497799835  Xcode  (15.1)"
        let list = ctx.out(&["mas", "list"], 60)?;
        list.lines().find_map(|l| {
            let (_, rest) = l.trim().split_once(char::is_whitespace)?;
            let open = rest.rfind('(')?;
            (rest[..open].trim() == name).then(|| rest[open + 1..].trim_end().trim_end_matches(')').trim().to_string())
        })
    }
}

/// "497799835  Xcode  (15.0 -> 15.1)"
pub fn parse_mas(text: &str) -> Vec<Update> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (id, rest) = l.split_once(char::is_whitespace)?;
            let open = rest.rfind('(')?;
            let (cur, new) = rest[open + 1..].trim_end_matches(')').split_once("->")?;
            let title = rest[..open].trim().to_string();
            Some(Update {
                name: title.clone(),
                provider: "mas".into(),
                category: Category::Apps,
                current: cur.trim().into(),
                latest: new.trim().into(),
                source: format!("Mac App Store (id {id})"),
                support: Support::Supported,
                command: argv(&["mas", "upgrade", id]),
                hint: Some("The App Store cannot install older versions: rollback not available.".into()),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_softwareupdate() {
        let out = "Software Update Tool\n\nFinding available software\nSoftware Update found the following new or updated software:\n\
* Label: macOS Golden Gate 27.1-27B50\n\tTitle: macOS Golden Gate 27.1, Version: 27.1, Size: 7253587KiB, Recommended: YES, Action: restart,\n\
* Label: Command Line Tools for Xcode-27.1\n\tTitle: Command Line Tools for Xcode, Version: 27.1, Size: 912345KiB, Recommended: YES, \n";
        let u = parse(out, "27.0.1");
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].current.as_str(), u[0].latest.as_str()), ("27.0.1", "27.1"));
        assert!(u[0].requires_restart && u[0].requires_admin && u[0].support == Support::Supported);
        assert_eq!(u[0].command.as_ref().unwrap().join(" "), "sudo softwareupdate --install macOS Golden Gate 27.1-27B50");
        assert_eq!(u[0].download_size.as_deref(), Some("6.9 GB"));
        assert!(!u[1].requires_restart);
        assert_eq!(u[1].command.as_ref().unwrap()[3], "Command Line Tools for Xcode-27.1");
        assert!(parse("No new software available.", "27.0").is_empty());
    }

    #[test]
    fn parses_mas() {
        let u = parse_mas("497799835  Xcode  (15.0 -> 15.1)\n1295203466 Microsoft Remote Desktop (10.9.0 -> 10.9.1)\n");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].command.as_ref().unwrap().join(" "), "mas upgrade 497799835");
        assert_eq!(u[1].name, "Microsoft Remote Desktop");
    }
}
