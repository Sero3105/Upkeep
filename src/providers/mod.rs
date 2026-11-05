//! Providers: one per package manager / software source. Adding a new one = implement
//! `Provider` and list it in `registry()`; the core never special-cases a provider.

mod apps;
mod docker;
mod homebrew;
mod macos;
mod misc;
mod node;
mod python;
mod runtimes;
mod tools;

use crate::model::{Category, ScanResult};
use crate::shell::Ctx;

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn category(&self) -> Category;
    /// Providers whose updates must finish first (npm after homebrew: node may change underneath).
    fn after(&self) -> &'static [&'static str] {
        &[]
    }
    fn available(&self, ctx: &Ctx) -> bool;
    /// Shown in Health when the provider is unavailable but would be useful.
    fn missing_hint(&self) -> Option<&'static str> {
        None
    }
    /// Read-only: never changes the system (except index refresh, see Config::refresh_indexes).
    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String>;
    /// Installed version right now, for pre-checks and post-update verification. None = can't verify.
    fn installed_version(&self, _ctx: &Ctx, _name: &str) -> Option<String> {
        None
    }
}

pub fn registry() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(homebrew::Homebrew),
        Box::new(apps::Apps),
        Box::new(macos::Mas),
        Box::new(macos::SoftwareUpdate),
        Box::new(node::Npm),
        Box::new(node::Pnpm),
        Box::new(python::Pip),
        Box::new(python::Uv),
        Box::new(misc::Rustup),
        Box::new(misc::Cargo),
        Box::new(misc::Gem),
        Box::new(misc::Composer),
        Box::new(runtimes::Runtimes),
        Box::new(tools::Tools),
        Box::new(docker::Docker),
    ]
}

/// Formula names that are language runtimes (version jumps can break projects).
pub fn is_runtime_name(name: &str) -> bool {
    let base = name.split('@').next().unwrap_or(name);
    matches!(
        base,
        "python" | "node" | "openjdk" | "go" | "ruby" | "php" | "rust" | "perl" | "erlang" | "elixir" | "dotnet" | "deno" | "bun" | "lua" | "julia"
    )
}

/// Who manages a binary, from where it really lives.
pub fn origin(path: &str) -> String {
    let real = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
    let p = real.as_str();
    if p.contains("/Cellar/") || p.contains("/Caskroom/") || p.starts_with("/opt/homebrew/") {
        "Homebrew".into()
    } else if p.contains("/node_modules/") {
        "npm (global)".into()
    } else if p.contains("/.cargo/") {
        "cargo".into()
    } else if p.contains("/.rustup/") {
        "rustup".into()
    } else if p.contains("/.local/share/claude/") {
        "native installer (self-updating)".into()
    } else if p.contains("/.docker/") || p.starts_with("/Applications/Docker.app") {
        "Docker Desktop".into()
    } else if let Some(i) = p.find(".app/") {
        let app = p[..i].rsplit('/').next().unwrap_or("app");
        format!("bundled with {app}.app")
    } else if p.starts_with("/Library/Developer/CommandLineTools") || p.starts_with("/Applications/Xcode") {
        "Xcode / Command Line Tools".into()
    } else if p.starts_with("/usr/bin/") || p.starts_with("/bin/") || p.starts_with("/System/") || p.starts_with("/usr/sbin/") {
        "macOS".into()
    } else {
        "manual install".into()
    }
}

/// Top-level string values of an Info.plist. XML is parsed directly; binary plists go through `plutil`.
pub fn plist_strings(ctx: &Ctx, file: &str, keys: &[&str]) -> Vec<Option<String>> {
    let raw = std::fs::read(file).unwrap_or_default();
    let xml = if raw.starts_with(b"bplist") {
        ctx.out(&["plutil", "-convert", "xml1", "-o", "-", file], 10).unwrap_or_default()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    keys.iter().map(|k| plist_value(&xml, k)).collect()
}

pub fn plist_value(xml: &str, key: &str) -> Option<String> {
    let tag = format!("<key>{key}</key>");
    let rest = &xml[xml.find(&tag)? + tag.len()..];
    let rest = rest.trim_start();
    let body = rest.strip_prefix("<string>")?;
    Some(unescape_xml(&body[..body.find("</string>")?]).trim().to_string())
}

pub fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Versions installed in Homebrew Cellars for formulae named `base` or `base@N`.
pub fn cellar_versions(ctx: &Ctx, base: &str) -> Vec<(String, String, String)> {
    let mut out = vec![];
    for cellar in &ctx.cellars {
        let Ok(rd) = std::fs::read_dir(cellar) else { continue };
        for f in rd.flatten() {
            let formula = f.file_name().to_string_lossy().into_owned();
            if formula != base && !formula.starts_with(&format!("{base}@")) {
                continue;
            }
            for v in subdirs(&f.path().to_string_lossy()) {
                out.push((formula.clone(), v.clone(), format!("{cellar}/{formula}/{v}")));
            }
        }
    }
    out
}

/// Sorted names of the visible subdirectories of `dir`.
pub fn subdirs(dir: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Is `path` writable by the current user (decides if an update would need sudo).
pub fn writable(path: &str) -> bool {
    std::ffi::CString::new(path.trim()).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

pub fn argv(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plist_values() {
        let xml = "<dict>\n\t<key>CFBundleShortVersionString</key>\n\t<string>2.3 &amp; co</string>\n\t<key>X</key><true/></dict>";
        assert_eq!(plist_value(xml, "CFBundleShortVersionString").as_deref(), Some("2.3 & co"));
        assert_eq!(plist_value(xml, "X"), None);
        assert_eq!(plist_value(xml, "Missing"), None);
    }

    #[test]
    fn runtime_names() {
        assert!(is_runtime_name("python@3.12") && is_runtime_name("node") && is_runtime_name("openjdk@21"));
        assert!(!is_runtime_name("gh") && !is_runtime_name("nodenv"));
    }

    #[test]
    fn origins() {
        assert_eq!(origin("/usr/bin/true"), "macOS");
        assert_eq!(origin("/nope/Visual Studio Code.app/Contents/Resources/app/bin/code"), "bundled with Visual Studio Code.app");
        assert_eq!(origin("/nope/x"), "manual install");
    }
}
