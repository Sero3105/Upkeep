//! Inventory of language runtimes, each installed version listed separately.
//! Reports no updates itself (brew/rustup do): it feeds System Health and `upkeep list`.
//! Never removes anything: old versions may be needed by existing projects.

use super::{Provider, cellar_versions, subdirs};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;
use std::path::Path;

pub struct Runtimes;

impl Provider for Runtimes {
    fn id(&self) -> &'static str {
        "runtimes"
    }
    fn name(&self) -> &'static str {
        "Language runtimes"
    }
    fn category(&self) -> Category {
        Category::Runtimes
    }
    fn available(&self, _ctx: &Ctx) -> bool {
        true
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut found: Vec<Installed> = vec![];
        let mut add = |name: &str, ver: &str, source: &str, path: &str| {
            let ver = ver.trim().trim_start_matches('v');
            if !ver.is_empty() && !found.iter().any(|i| i.name == name && i.version == ver && i.source == source) {
                found.push(Installed {
                    name: name.into(),
                    version: ver.into(),
                    category: Category::Runtimes,
                    provider: "runtimes".into(),
                    source: source.into(),
                    path: Some(path.into()),
                    ..Default::default()
                });
            }
        };
        // Homebrew kegs: formula -> display name.
        for (base, display) in [
            ("openjdk", "Java"),
            ("python", "Python"),
            ("node", "Node.js"),
            ("go", "Go"),
            ("ruby", "Ruby"),
            ("php", "PHP"),
            ("rust", "Rust"),
            ("perl", "Perl"),
            ("erlang", "Erlang"),
            ("elixir", "Elixir"),
            ("deno", "Deno"),
            ("bun", "Bun"),
        ] {
            for (formula, v, p) in cellar_versions(ctx, base) {
                add(display, &v, &format!("Homebrew ({formula})"), &p);
            }
        }
        // Java VMs registered with macOS.
        let jh = ctx.run(&["/usr/libexec/java_home", "-V"], 15);
        for (v, vendor, p) in parse_java_home(&format!("{}\n{}", jh.err, jh.out)) {
            add("Java", &v, &vendor, &p);
        }
        // Version-manager directories: (display, dir, source).
        let dirs = [
            ("Java", "~/.sdkman/candidates/java", "SDKMAN"),
            ("Python", "~/.pyenv/versions", "pyenv"),
            ("Python", "/Library/Frameworks/Python.framework/Versions", "python.org installer"),
            ("Python", "~/.local/share/uv/python", "uv"),
            ("Node.js", "~/.nvm/versions/node", "nvm"),
            ("Node.js", "~/Library/Application Support/fnm/node-versions", "fnm"),
            ("Node.js", "~/.local/share/fnm/node-versions", "fnm"),
            ("Node.js", "~/.volta/tools/image/node", "Volta"),
            ("Go", "~/sdk", "go install golang.org/dl"),
            ("Ruby", "~/.rbenv/versions", "rbenv"),
            ("Ruby", "~/.rvm/rubies", "RVM"),
            ("Ruby", "~/.rubies", "ruby-install"),
        ];
        for (display, dir, source) in dirs {
            let dir = ctx.path(dir);
            for d in subdirs(&dir) {
                if d == "Current" || d.contains("system") {
                    continue;
                }
                if let Some(v) = version::extract(&d.replace('-', " ")).or_else(|| d.chars().all(|c| c.is_ascii_digit()).then(|| d.clone())) {
                    add(display, &v, source, &format!("{dir}/{d}"));
                }
            }
        }
        // Standalone installs, read without executing stubs (/usr/bin/python3 would pop up the CLT installer).
        if Path::new("/Library/Developer/CommandLineTools/usr/bin/python3").exists()
            && let Some(v) = ctx.out(&["/Library/Developer/CommandLineTools/usr/bin/python3", "--version"], 10).and_then(|o| version::extract(&o)) {
                add("Python", &v, "Xcode Command Line Tools", "/Library/Developer/CommandLineTools/usr/bin/python3");
            }
        for (display, bin, args, source) in [
            ("Node.js", "/usr/local/bin/node", "--version", "nodejs.org installer"),
            ("Ruby", "/usr/bin/ruby", "--version", "macOS"),
            ("Go", "/usr/local/go/bin/go", "version", "go.dev installer"),
        ] {
            let real = std::fs::canonicalize(bin).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if Path::new(bin).exists() && !real.contains("/Cellar/")
                && let Some(v) = ctx.out(&[bin, args], 10).and_then(|o| version::extract(&o)) {
                    add(display, &v, source, bin);
                }
        }
        if ctx.has("dotnet") {
            for l in ctx.out(&["dotnet", "--list-sdks"], 20).unwrap_or_default().lines() {
                if let Some((v, p)) = l.split_once(" [") {
                    add(".NET SDK", v, "dotnet", p.trim_end_matches(']'));
                }
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name).then(version::compare(&a.version, &b.version)));
        Ok(ScanResult { installed: found, ..Default::default() })
    }
}

/// `java_home -V` lines: `    25.0.2 (arm64) "Eclipse Adoptium" - "OpenJDK 25.0.2" /Library/Java/.../Home`
pub fn parse_java_home(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let path = l.rsplit(' ').next().filter(|p| p.starts_with('/'))?;
            let ver = l.split_whitespace().next().filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))?;
            let vendor = l.split('"').nth(1).unwrap_or("JDK");
            Some((ver.to_string(), vendor.to_string(), path.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};

    #[test]
    fn lists_every_java_and_cellar_version_separately() {
        let jh = "Matching Java Virtual Machines (3):\n    25.0.2 (arm64) \"Eclipse Adoptium\" - \"OpenJDK 25.0.2\" /Library/Java/JavaVirtualMachines/temurin-25.jdk/Contents/Home\n    21.0.5 (arm64) \"Oracle Corporation\" - \"Java SE 21.0.5\" /Library/Java/JavaVirtualMachines/jdk-21.jdk/Contents/Home\n    17.0.13 (arm64) \"Azul Systems, Inc.\" - \"Zulu 17\" /Library/Java/JavaVirtualMachines/zulu-17.jdk/Contents/Home\n/Library/Java/JavaVirtualMachines/temurin-25.jdk/Contents/Home\n";
        assert_eq!(parse_java_home(jh).len(), 3);

        let tmp = std::env::temp_dir().join(format!("upkeep-cellar-{}", std::process::id()));
        for d in ["python@3.12/3.12.14", "python@3.14/3.14.7", "node/25.1.0", "nodenv/1.0"] {
            std::fs::create_dir_all(tmp.join(d)).unwrap();
        }
        let mut c = ctx(MockRunner::new(&[]).on("/usr/libexec/java_home -V", 0, jh));
        c.cellars = vec![tmp.to_string_lossy().into()];
        let r = Runtimes.scan(&c).unwrap();
        let count = |n: &str| r.installed.iter().filter(|i| i.name == n).count();
        assert_eq!(count("Java"), 3);
        assert_eq!(count("Python"), 2);
        assert_eq!(count("Node.js"), 1, "nodenv is not node");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
