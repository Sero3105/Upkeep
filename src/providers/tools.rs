//! CLI developer tools: inventory with version and who manages each binary.
//! Their updates come from the owning provider (brew, npm...), so nothing is double-counted.

use super::{Provider, origin};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;

pub struct Tools;

/// (command, version args, display name)
const TOOLS: &[(&str, &[&str], &str)] = &[
    ("git", &["--version"], "Git"),
    ("gh", &["--version"], "GitHub CLI"),
    ("claude", &["--version"], "Claude Code"),
    ("docker", &["--version"], "Docker CLI"),
    ("code", &["--version"], "VS Code CLI"),
    ("cmake", &["--version"], "CMake"),
    ("make", &["--version"], "Make"),
    ("ninja", &["--version"], "Ninja"),
    ("openssl", &["version"], "OpenSSL"),
    ("terraform", &["version"], "Terraform"),
    ("kubectl", &["version", "--client"], "kubectl"),
    ("helm", &["version", "--short"], "Helm"),
    ("curl", &["--version"], "curl"),
    ("wget", &["--version"], "wget"),
    ("jq", &["--version"], "jq"),
    ("ffmpeg", &["-version"], "FFmpeg"),
    ("yarn", &["--version"], "Yarn"),
    ("pnpm", &["--version"], "pnpm"),
    ("poetry", &["--version"], "Poetry"),
    ("pipx", &["--version"], "pipx"),
    ("uv", &["--version"], "uv"),
    ("bun", &["--version"], "Bun"),
    ("deno", &["--version"], "Deno"),
    ("go", &["version"], "Go"),
    ("cargo", &["--version"], "Cargo"),
    ("composer", &["--version"], "Composer"),
    ("mas", &["version"], "mas"),
];

impl Provider for Tools {
    fn id(&self) -> &'static str {
        "tools"
    }
    fn name(&self) -> &'static str {
        "CLI tools"
    }
    fn category(&self) -> Category {
        Category::Tools
    }
    fn available(&self, _ctx: &Ctx) -> bool {
        true
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let found: Vec<Option<Installed>> = std::thread::scope(|s| {
            let hs: Vec<_> = TOOLS
                .iter()
                .map(|(cmd, args, display)| {
                    s.spawn(move || {
                        let path = ctx.runner.which(cmd)?;
                        let mut argv = vec![path.as_str()];
                        argv.extend_from_slice(args);
                        let r = ctx.run(&argv, 15);
                        let text = format!("{}\n{}", r.out, r.err);
                        let ver = text.lines().find_map(version::extract).unwrap_or_else(|| "?".into());
                        let src = origin(&path);
                        let note = match src.as_str() {
                            "manual install" => Some("update not verifiable: unknown installer".to_string()),
                            s if s.contains("self-updating") => Some(format!("updates itself; force with `{cmd} update`")),
                            s if s.starts_with("bundled") => Some("updated together with its app".to_string()),
                            _ => None,
                        };
                        Some(Installed {
                            name: display.to_string(),
                            version: ver,
                            category: Category::Tools,
                            provider: "tools".into(),
                            source: src,
                            path: Some(path),
                            note,
                            key: Some(cmd.to_string()),
                        })
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        Ok(ScanResult { installed: found.into_iter().flatten().collect(), ..Default::default() })
    }
}
