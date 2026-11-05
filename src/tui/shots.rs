//! README screenshots rendered by the real TUI code with demo data (never the user's machine).
//! Run: `cargo test screenshots -- --ignored` → assets/screens/*.svg (then ./scripts/screens.sh for PNG).

use super::*;
use crate::executor::Outcome;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, backend::TestBackend};

const FG: &str = "#c9d1d9";
const BG: &str = "#0d1117";

fn hex(c: Color, default: &str) -> String {
    let rgb = |r: u8, g: u8, b: u8| format!("#{r:02x}{g:02x}{b:02x}");
    match c {
        Color::Reset => default.into(),
        Color::Black => BG.into(),
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Indexed(n) if n >= 232 => {
            let v = 8 + 10 * (n - 232);
            rgb(v, v, v)
        }
        Color::Indexed(n) if n >= 16 => {
            let l = [0u8, 95, 135, 175, 215, 255];
            let i = n - 16;
            rgb(l[(i / 36) as usize], l[(i / 6 % 6) as usize], l[(i % 6) as usize])
        }
        _ => default.into(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Terminal buffer → SVG "window" on a gradient backdrop.
fn svg(buf: &Buffer, title: &str) -> String {
    let (w, h) = (buf.area.width as f64, buf.area.height as f64);
    let (cw, ch, fs) = (8.4, 18.0, 14.0);
    let (pad, bar) = (36.0, 34.0);
    let (ww, wh) = (w * cw + 32.0, h * ch + bar + 20.0);
    let (tw, th) = (ww + pad * 2.0, wh + pad * 2.0);
    let mut s = format!(
        // Square canvas, content centered: QuickLook scales squares predictably and `sips -c` crops around the center.
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{tw}" height="{tw}" viewBox="0 0 {tw} {tw}" data-crop="{th}">
<defs><linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#1f6feb"/><stop offset="1" stop-color="#8957e5"/></linearGradient>
<filter id="sh" x="-10%" y="-10%" width="120%" height="130%"><feDropShadow dx="0" dy="12" stdDeviation="14" flood-color="#000" flood-opacity=".45"/></filter></defs>
<rect width="{tw}" height="{tw}" fill="url(#bg)"/>
<g transform="translate({pad},{})"><rect width="{ww}" height="{wh}" rx="12" fill="{BG}" filter="url(#sh)"/>
<circle cx="20" cy="17" r="6" fill="#ff5f57"/><circle cx="40" cy="17" r="6" fill="#febc2e"/><circle cx="60" cy="17" r="6" fill="#28c840"/>
<text x="{}" y="22" fill="#8b949e" font-family="SF Mono, Menlo, monospace" font-size="13" text-anchor="middle">{}</text>
<g transform="translate(16,{bar})" font-family="SF Mono, Menlo, monospace" font-size="{fs}">
"##,
        (tw - th) / 2.0 + pad,
        ww / 2.0,
        esc(title)
    );
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let c = &buf[(x, y)];
            let (fg, bg, m) = (c.fg, c.bg, c.modifier);
            let start = x;
            let mut text = String::new();
            while x < buf.area.width {
                let d = &buf[(x, y)];
                if d.fg != fg || d.bg != bg || d.modifier != m {
                    break;
                }
                text.push_str(d.symbol());
                x += 1;
            }
            let (px, py) = (start as f64 * cw, y as f64 * ch);
            let n = (x - start) as f64;
            if bg != Color::Reset {
                s += &format!(r#"<rect x="{px}" y="{py}" width="{}" height="{ch}" fill="{}"/>"#, n * cw, hex(bg, BG));
            }
            // One <text> per word at its exact column: renderers collapse spaces, monospace keeps the rest aligned.
            let weight = if m.contains(Modifier::BOLD) { r#" font-weight="700""# } else { "" };
            let chars: Vec<char> = text.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == ' ' {
                    i += 1;
                    continue;
                }
                let j = chars[i..].iter().position(|c| *c == ' ').map_or(chars.len(), |k| i + k);
                let word: String = chars[i..j].iter().collect();
                if word.chars().all(|c| c == '│') {
                    // Box verticals as continuous lines: font glyphs leave gaps between rows.
                    for k in 0..j - i {
                        s += &format!(r#"<rect x="{}" y="{py}" width="1.2" height="{ch}" fill="{}"/>"#, px + (i + k) as f64 * cw + cw / 2.0 - 0.6, hex(fg, FG));
                    }
                    i = j;
                    continue;
                }
                s += &format!(r#"<text x="{}" y="{}" fill="{}"{weight}>{}</text>"#, px + i as f64 * cw, py + ch * 0.75, hex(fg, FG), esc(&word));
                i = j;
            }
            s.push('\n');
        }
    }
    s + "</g></g></svg>\n"
}

fn up(name: &str, provider: &str, cat: Category, cur: &str, new: &str, source: &str) -> Update {
    Update {
        name: name.into(),
        provider: provider.into(),
        category: cat,
        current: cur.into(),
        latest: new.into(),
        source: source.into(),
        support: Support::Supported,
        command: Some(vec!["true".into()]),
        ..Default::default()
    }
}

fn demo() -> App {
    let mut app = App::new(Config::default());
    let mut ups = vec![
        up("google-chrome", "homebrew", Category::Apps, "141.0.7390.66", "142.0.7444.60", "Homebrew cask"),
        up("visual-studio-code", "homebrew", Category::Apps, "1.104.2", "1.105.1", "Homebrew cask"),
        up("ffmpeg", "homebrew", Category::Homebrew, "7.1.1", "8.0", "Homebrew formula"),
        up("gh", "homebrew", Category::Homebrew, "2.81.0", "2.82.1", "Homebrew formula"),
        up("git", "homebrew", Category::Homebrew, "2.51.0", "2.51.1", "Homebrew formula"),
        up("node", "homebrew", Category::Homebrew, "24.9.0", "25.0.0", "Homebrew formula"),
        up("openssl@3", "homebrew", Category::Homebrew, "3.5.3", "3.6.0", "Homebrew formula"),
        up("python@3.13", "homebrew", Category::Homebrew, "3.13.7", "3.13.9", "Homebrew formula"),
        up("npm", "npm", Category::Packages, "11.6.0", "11.6.2", "npm (global)"),
        up("typescript", "npm", Category::Packages, "5.8.3", "5.9.3", "npm (global)"),
        up("ruff", "uv", Category::Packages, "0.13.3", "0.14.1", "uv tool"),
        up("stable-aarch64-apple-darwin", "rustup", Category::Runtimes, "1.89.0", "1.90.0", "rustup"),
    ];
    ups[5].is_runtime = true;
    ups[5].dependents = (0..7).map(|i| format!("pkg{i}")).collect();
    ups[6].dependents = (0..23).map(|i| format!("lib{i}")).collect();
    ups[4].dependencies = vec!["openssl@3".into()];
    ups[8].is_self_update = true;
    for u in ups.iter_mut().filter(|u| matches!(u.provider.as_str(), "npm" | "uv")) {
        u.rollback = Some(vec!["true".into()]);
    }
    let mut slack = up("Slack", "apps", Category::Apps, "4.46.99", "4.47.59", "Sparkle appcast (vendor feed)");
    slack.support = Support::Manual;
    slack.command = None;
    slack.hint = Some("Open Slack and use its “Check for Updates…” menu.".into());
    ups.push(slack);
    let mut mac = up("macOS Tahoe 26.1", "softwareupdate", Category::System, "26.0.1", "26.1", "Apple Software Update");
    mac.support = Support::Manual;
    mac.command = None;
    mac.requires_admin = true;
    mac.requires_restart = true;
    mac.download_size = Some("6.9 GB".into());
    mac.notes = vec!["recommended by Apple".into()];
    ups.push(mac);
    let st = |id: &str, name: &str, n: usize, u: usize, d: f64| ProviderStatus { id: id.into(), name: name.into(), available: true, enabled: true, duration: d, installed: n, updates: u, error: None };
    let mut providers = vec![
        st("homebrew", "Homebrew", 184, 8, 2.1),
        st("apps", "Applications", 41, 1, 1.4),
        st("softwareupdate", "macOS Software Update", 1, 1, 3.9),
        st("npm", "npm (global)", 9, 2, 1.2),
        st("pip", "pip (python3)", 0, 0, 0.9),
        st("uv", "uv tools", 4, 1, 0.3),
        st("rustup", "rustup", 2, 1, 0.8),
        st("runtimes", "Language runtimes", 9, 0, 0.2),
        st("tools", "CLI tools", 22, 0, 0.6),
        st("docker", "Docker", 14, 0, 0.4),
    ];
    providers.insert(2, ProviderStatus { id: "mas".into(), name: "Mac App Store".into(), enabled: true, ..Default::default() });
    let mut installed = vec![];
    for (cat, n) in [(Category::Apps, 41), (Category::Homebrew, 184), (Category::Runtimes, 9), (Category::Packages, 13), (Category::Tools, 22), (Category::Docker, 14), (Category::System, 1)] {
        for i in 0..n {
            installed.push(Installed { name: format!("x{i}"), category: cat, provider: if cat == Category::Apps { "apps".into() } else { "x".into() }, ..Default::default() });
        }
    }
    app.snap = Snapshot { date: config::now() - 120, installed, updates: ups, providers, ..Default::default() };
    app.reg = Arc::new(providers::registry().into_iter().filter(|p| app.snap.providers.iter().any(|s| s.id == p.id())).collect());
    app.health = Some(Report {
        score: 84,
        issues: vec![
            HealthIssue::new(Severity::Warning, "Multiple `node` on PATH", "node", "`node` runs /opt/homebrew/bin/node; also found: /usr/local/bin/node")
                .consequence("Updates may land in the copy that is not used; scripts can pick different versions.")
                .fix("Keep one installation (prefer the package-managed one) or reorder PATH deliberately."),
            HealthIssue::new(Severity::Warning, "Package managers out of date", "Package managers", "npm 11.6.0 → 11.6.2"),
            HealthIssue::new(Severity::Warning, "1 broken symlinks", "bin directories", "/usr/local/bin/terraform"),
            HealthIssue::new(Severity::Warning, "icu4c@76 is deprecated", "Homebrew: icu4c@76", "versioned formula"),
            HealthIssue::new(Severity::Info, "3 Java versions installed", "Java", "17.0.16 (Temurin), 21.0.8 (Temurin), 25 (Homebrew)"),
            HealthIssue::new(Severity::Info, "3 Python versions installed", "Python", "3.9.6 (Xcode CLT), 3.12.11, 3.13.7 (Homebrew)"),
            HealthIssue::new(Severity::Info, "4 duplicate PATH entries", "PATH", "/opt/homebrew/bin, ~/.local/bin, …"),
            HealthIssue::new(Severity::Info, "14 updates available", "Software", "See the Updates tab"),
            HealthIssue::new(Severity::Healthy, "PATH has no unsafe entries", "PATH", ""),
            HealthIssue::new(Severity::Healthy, "Developer tools installed", "Xcode CLT", ""),
            HealthIssue::new(Severity::Healthy, "Disk space OK", "Disk", "312.4 GB free (63%)"),
            HealthIssue::new(Severity::Healthy, "brew doctor: ready to brew", "Homebrew", ""),
        ],
    });
    app
}

fn shot(app: &mut App, name: &str, title: &str) {
    let mut t = Terminal::new(TestBackend::new(118, 30)).unwrap();
    t.draw(|f| view::draw(f, app)).unwrap();
    std::fs::create_dir_all("assets/screens").unwrap();
    std::fs::write(format!("assets/screens/{name}.svg"), svg(t.backend().buffer(), title)).unwrap();
}

#[test]
#[ignore = "writes README assets"]
fn screenshots() {
    let mut app = demo();
    app.tab = Tab::Dashboard;
    shot(&mut app, "dashboard", "upkeep");
    app.tab = Tab::Updates;
    for id in ["homebrew:gh", "homebrew:git", "homebrew:openssl@3", "npm:npm", "npm:typescript", "uv:ruff"] {
        app.sel.insert(id.into());
    }
    app.ustate.select(Some(4));
    shot(&mut app, "updates", "upkeep — Updates");
    app.tab = Tab::Health;
    app.hstate.select(Some(0));
    shot(&mut app, "health", "upkeep — System Health");
    app.tab = Tab::Updates;
    app.make_plan();
    shot(&mut app, "plan", "upkeep — Smart Update");
    app.start_exec_for_demo();
    shot(&mut app, "exec", "upkeep — Smart Update");
}

impl App {
    /// Frozen mid-run execution state (no thread, nothing executed).
    fn start_exec_for_demo(&mut self) {
        let steps = self.plan.as_ref().unwrap().executable(false);
        let n = steps.len();
        let mut outcomes = vec![None; n];
        outcomes[0] = Some(Outcome::AlreadyCurrent("2.82.1".into()));
        outcomes[1] = Some(Outcome::Success("3.6.0".into()));
        outcomes[2] = Some(Outcome::Success("2.51.1".into()));
        let log = [
            "▶ gh",
            "■ gh: already at 2.82.1",
            "▶ openssl@3",
            "openssl@3: ==> Upgrading openssl@3 3.5.3 -> 3.6.0",
            "openssl@3: ==> Pouring openssl@3--3.6.0.arm64_tahoe.bottle.tar.gz",
            "■ openssl@3: now 3.6.0",
            "▶ git",
            "git: ==> Upgrading git 2.51.0 -> 2.51.1",
            "■ git: now 2.51.1",
            "▶ npm",
            "npm: changed 14 packages in 3s",
            "npm: 31 packages are looking for funding",
        ];
        self.exec = Some(Exec {
            steps,
            outcomes,
            running: [3].into(),
            log: log.iter().map(|s| s.to_string()).collect(),
            control: Arc::new(Control::default()),
            done: false,
            started: Instant::now() - Duration::from_secs(47),
            elapsed: Duration::ZERO,
            state: ListState::default().with_selected(Some(3)),
        });
        self.tick = 3;
    }
}
