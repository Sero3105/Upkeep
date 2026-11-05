//! Applications in /Applications and ~/Applications. For each app Upkeep picks the official
//! channel, in this order:
//!   App Store (→ `mas` provider) · Microsoft AutoUpdate (→ `msupdate` provider) ·
//!   Homebrew cask that owns it · the vendor's Sparkle feed · the Homebrew cask catalog.
//! Downloads always go through `upkeep __app-install`, which verifies bundle id, code signature,
//! Team ID (same developer as the installed app) and Gatekeeper before replacing anything.

use super::catalog::{self, Entry};
use super::{Provider, plist_strings, unescape_xml};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;
use std::collections::HashMap;
use std::path::Path;

pub struct Apps;

pub const CASKROOMS: [&str; 2] = ["/opt/homebrew/Caskroom", "/usr/local/Caskroom"];
pub const MSUPDATE: &str = "/Library/Application Support/Microsoft/MAU2.0/Microsoft AutoUpdate.app/Contents/MacOS/msupdate";

struct App {
    name: String,
    file: String,
    path: String,
    id: String,
    short: String,
    build: String,
    feed: Option<String>,
    source: &'static str,
}

/// Newest stable appcast item.
#[derive(Debug, PartialEq)]
pub struct Cast {
    pub build: String,
    pub short: String,
    pub url: Option<String>,
}

impl Provider for Apps {
    fn id(&self) -> &'static str {
        "apps"
    }
    fn name(&self) -> &'static str {
        "Applications"
    }
    fn category(&self) -> Category {
        Category::Apps
    }
    fn available(&self, _ctx: &Ctx) -> bool {
        true
    }

    fn scan(&self, ctx: &Ctx) -> Result<ScanResult, String> {
        let mut res = ScanResult::default();
        let apps = find_apps(ctx);
        let online = ctx.config.check_appcasts;
        let idx = if online && ctx.has("brew") {
            catalog::load(ctx).unwrap_or_else(|e| {
                res.warnings.push(e);
                vec![]
            })
        } else {
            vec![]
        };
        // Appcast checks hit the network: run them concurrently.
        let casts: Vec<Option<Cast>> = std::thread::scope(|s| {
            let hs: Vec<_> = apps
                .iter()
                .map(|a| {
                    s.spawn(move || {
                        let feed = a.feed.as_deref().filter(|f| f.starts_with("https://") && online)?;
                        latest_from_appcast(&ctx.out(&["curl", "-fsSL", "--proto", "=https", "--max-time", "10", feed], 15)?)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "upkeep".into());
        let ms = Path::new(MSUPDATE).exists();
    // Apps Microsoft AutoUpdate actually updates (VS Code, OneDrive… have their own updaters).
    const MAU_APPS: [&str; 7] = ["com.microsoft.Word", "com.microsoft.Excel", "com.microsoft.Powerpoint", "com.microsoft.Outlook", "com.microsoft.onenote.mac", "com.microsoft.teams2", "com.microsoft.edgemac"];

        let mut by_id: HashMap<&str, Vec<&App>> = HashMap::new();
        for (a, cast) in apps.iter().zip(casts) {
            by_id.entry(&a.id).or_default().push(a);
            let entry = catalog::lookup(&idx, &a.file, &a.id);
            let owned_cask = entry.filter(|e| CASKROOMS.iter().any(|r| Path::new(r).join(&e.token).is_dir()));
            let microsoft = ms && MAU_APPS.contains(&a.id.as_str());
            let (update, how) = plan_update(a, cast, entry, owned_cask.is_some(), microsoft, &exe);
            res.installed.push(Installed {
                name: a.name.clone(),
                version: a.short.clone(),
                category: Category::Apps,
                provider: "apps".into(),
                source: if owned_cask.is_some() { "Homebrew cask".into() } else if microsoft { "Microsoft AutoUpdate".into() } else { a.source.into() },
                path: Some(a.path.clone()),
                note: how,
                key: Some(a.id.clone()),
            });
            res.updates.extend(update);
        }
        for (id, list) in by_id.iter().filter(|(_, l)| l.len() > 1) {
            let paths: Vec<String> = list.iter().map(|a| format!("{} ({})", a.path, a.short)).collect();
            res.issues.push(
                HealthIssue::new(Severity::Warning, format!("Duplicate app: {}", list[0].name), format!("App {id}"), paths.join(", "))
                    .consequence("Spotlight/Dock may open the stale copy; wastes disk space.")
                    .fix("Keep one copy (usually /Applications) and move the other to the Trash."),
            );
        }
        Ok(res)
    }

    fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
        let file = format!("{name}.app");
        find_apps(ctx).into_iter().find(|a| a.file == file).map(|a| a.short)
    }
}

/// Which channel updates this app, and the note shown when none can be checked.
fn plan_update(a: &App, cast: Option<Cast>, entry: Option<&Entry>, cask_owned: bool, microsoft: bool, exe: &str) -> (Option<Update>, Option<String>) {
    if a.source == "Mac App Store" || microsoft {
        return (None, None); // `mas` / `msupdate` providers report these.
    }
    let newer = |build: &str, short: &str| if a.build.is_empty() || build.is_empty() { version::is_newer(short, &a.short) } else { version::is_newer(build, &a.build) };
    let base = |latest: String, source: &str| Update {
        name: a.name.clone(),
        provider: "apps".into(),
        category: Category::Apps,
        current: a.short.clone(),
        latest,
        source: source.into(),
        support: Support::Supported,
        ..Default::default()
    };
    let admin = crate::appinstall::needs_root(Path::new(&a.path));
    let install = |src: &str| {
        let backup = crate::appinstall::backup_path(&a.path, &a.short).to_string_lossy().into_owned();
        (Some(vec![exe.to_string(), "__app-install".into(), a.path.clone(), src.to_string()]), Some(vec![exe.to_string(), "__app-restore".into(), backup, a.path.clone()]))
    };

    if let Some(e) = entry.filter(|_| cask_owned) {
        // Cask-managed app that updates itself but fell behind: brew upgrades it (with --greedy).
        if version::is_newer(&e.version, &a.short) {
            let mut u = base(e.version.clone(), "Homebrew cask");
            u.command = super::argv(&["brew", "upgrade", "--cask", "--greedy", &e.token]);
            u.cask = Some(e.token.clone());
            u.requires_admin = e.pkg;
            u.hint = Some("Homebrew cannot downgrade casks: rollback not available.".into());
            return (Some(u), None);
        }
        return (None, None);
    }
    let feed_checked = cast.is_some();
    if let Some(c) = cast.filter(|c| newer(&c.build, &c.short)) {
        let latest = if c.short.is_empty() { c.build.clone() } else { c.short.clone() };
        let mut u = base(latest, "Sparkle feed (vendor)");
        match c.url.filter(|u| supported_archive(u)) {
            Some(url) => {
                (u.command, u.rollback) = install(&url);
                u.requires_admin = admin;
                u.hint = Some("Downloaded from the vendor feed; installed only if signed by the same developer and accepted by Gatekeeper.".into());
            }
            None => {
                u.support = Support::Manual;
                u.hint = Some(format!("Open {} and use its “Check for Updates…” menu.", a.name));
            }
        }
        return (Some(u), None);
    }
    if let Some(e) = entry {
        if version::is_newer(&e.version, &a.short) {
            let mut u = base(e.version.clone(), &format!("Homebrew catalog ({})", e.token));
            if e.pkg {
                // Installer packages can't be swapped bundle-by-bundle: let brew run the vendor .pkg.
                u.command = super::argv(&["brew", "install", "--cask", "--force", &e.token]);
                u.requires_admin = true;
                u.notes.push("installer package: the app becomes Homebrew-managed".into());
            } else {
                (u.command, u.rollback) = install(&format!("cask:{}", e.token));
                u.requires_admin = admin;
                u.hint = Some("Vendor download via Homebrew (checksum), then same-developer + Gatekeeper checks.".into());
            }
            u.cask = Some(e.token.clone());
            return (Some(u), None);
        }
        return (None, None);
    }
    let note = match a.source {
        "self-updating (Squirrel)" => "updates itself when opened (Electron/Squirrel): channel not readable from outside",
        "Sparkle" if a.feed.is_none() => "updates itself (Sparkle) but publishes no feed URL: use its “Check for Updates…” menu",
        _ if feed_checked => return (None, None),
        _ => "update not verifiable: no official update channel found",
    };
    (None, Some(note.into()))
}

fn supported_archive(url: &str) -> bool {
    let u = url.to_lowercase();
    let path = u.split(['?', '#']).next().unwrap_or("");
    [".zip", ".dmg", ".tar.gz", ".tar.xz", ".tar.bz2", ".tgz", ".tbz"].iter().any(|x| path.ends_with(x))
}

fn find_apps(ctx: &Ctx) -> Vec<App> {
    let mut bundles = vec![];
    for dir in &ctx.app_dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                bundles.push(p);
            } else if p.is_dir() && !p.ends_with("Utilities") {
                // Vendor folders like "/Applications/Epson Software/*.app"
                if let Ok(sub) = std::fs::read_dir(&p) {
                    bundles.extend(sub.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "app")));
                }
            }
        }
    }
    bundles.iter().filter_map(|b| read_app(ctx, b)).collect()
}

fn read_app(ctx: &Ctx, bundle: &Path) -> Option<App> {
    let plist = bundle.join("Contents/Info.plist");
    let v = plist_strings(ctx, &plist.to_string_lossy(), &["CFBundleIdentifier", "CFBundleShortVersionString", "CFBundleVersion", "SUFeedURL"]);
    let id = v[0].clone()?;
    if id.starts_with("com.apple.") {
        return None; // Updated with macOS itself (includes Safari web apps).
    }
    let has = |rel: &str| bundle.join(rel).exists();
    let feed = v[3].clone();
    let source = if has("Contents/_MASReceipt/receipt") {
        "Mac App Store"
    } else if feed.is_some() || has("Contents/Frameworks/Sparkle.framework") {
        "Sparkle"
    } else if has("Contents/Frameworks/Squirrel.framework") {
        "self-updating (Squirrel)"
    } else {
        "manual"
    };
    let build = v[2].clone().unwrap_or_default();
    Some(App {
        name: bundle.file_stem()?.to_string_lossy().into_owned(),
        file: bundle.file_name()?.to_string_lossy().into_owned(),
        path: bundle.to_string_lossy().into_owned(),
        id,
        short: v[1].clone().unwrap_or_else(|| build.clone()),
        build,
        feed,
        source,
    })
}

/// Newest non-channel item of a Sparkle appcast.
pub fn latest_from_appcast(xml: &str) -> Option<Cast> {
    xml.split("<item")
        .skip(1)
        .map(|item| &item[..item.find("</item>").unwrap_or(item.len())])
        .filter(|item| !item.contains("<sparkle:channel>"))
        .filter_map(|item| {
            let enclosure = item.find("<enclosure").map(|i| &item[i..]);
            let build = field(item, "sparkle:version")?;
            Some(Cast {
                build,
                short: field(item, "sparkle:shortVersionString").unwrap_or_default(),
                url: enclosure.and_then(|e| field(e, "url")),
            })
        })
        .max_by(|a, b| version::compare(&a.build, &b.build))
}

/// `<k>v</k>` element or `k="v"` attribute.
fn field(item: &str, k: &str) -> Option<String> {
    let open = format!("<{k}>");
    if let Some(i) = item.find(&open) {
        let rest = &item[i + open.len()..];
        return Some(unescape_xml(rest[..rest.find('<')?].trim()));
    }
    // Attribute: must follow whitespace (attributes are often on their own line).
    let attr = format!("{k}=\"");
    let mut from = 0;
    while let Some(off) = item[from..].find(&attr) {
        let at = from + off;
        if item[..at].ends_with(char::is_whitespace) {
            let i = at + attr.len();
            return Some(unescape_xml(&item[i..i + item[i..].find('"')?]));
        }
        from = at + attr.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};

    #[test]
    fn appcast_picks_newest_stable_item_with_url() {
        let xml = r#"<rss><channel>
            <item><title>2.0</title><enclosure url="https://x/a-2.0.zip" sparkle:version="200" sparkle:shortVersionString="2.0"/></item>
            <item><sparkle:version>310</sparkle:version><x sparkle:url="no"/><sparkle:shortVersionString>3.1</sparkle:shortVersionString><enclosure
                url="https://x/a-3.1.dmg" length="1"/></item>
            <item><sparkle:channel>beta</sparkle:channel><sparkle:version>400</sparkle:version></item>
        </channel></rss>"#;
        assert_eq!(latest_from_appcast(xml), Some(Cast { build: "310".into(), short: "3.1".into(), url: Some("https://x/a-3.1.dmg".into()) }));
        assert_eq!(latest_from_appcast("<rss/>"), None);
        assert!(supported_archive("https://x/a.dmg?x=1") && !supported_archive("https://x/a.pkg") && !supported_archive("https://x/a.delta"));
    }

    fn app(source: &'static str, feed: Option<&str>) -> App {
        App { name: "Foo".into(), file: "Foo.app".into(), path: "/Applications/Foo.app".into(), id: "com.x.foo".into(), short: "1.0".into(), build: "100".into(), feed: feed.map(String::from), source }
    }

    #[test]
    fn channel_priority() {
        let e = Entry { app: "Foo.app".into(), token: "foo".into(), version: "2.0".into(), ids: vec![], pkg: false };
        let cast = || Some(Cast { build: "200".into(), short: "2.0".into(), url: Some("https://v/foo.zip".into()) });
        // cask-owned → brew upgrade --greedy
        let (u, _) = plan_update(&app("Sparkle", Some("f")), cast(), Some(&e), true, false, "upkeep");
        assert_eq!(u.unwrap().command.unwrap().join(" "), "brew upgrade --cask --greedy foo");
        // Sparkle feed beats catalog, installs through the verifier, with rollback
        let (u, _) = plan_update(&app("Sparkle", Some("f")), cast(), Some(&e), false, false, "upkeep");
        let u = u.unwrap();
        assert_eq!(u.command.as_ref().unwrap()[1..].join(" "), "__app-install /Applications/Foo.app https://v/foo.zip");
        assert_eq!(u.rollback.as_ref().unwrap()[1], "__app-restore");
        // catalog only
        let (u, _) = plan_update(&app("manual", None), None, Some(&e), false, false, "upkeep");
        assert_eq!(u.unwrap().command.unwrap()[3], "cask:foo");
        // pkg cask → brew install --force, admin
        let pkg = Entry { pkg: true, ..e.clone() };
        let u = plan_update(&app("manual", None), None, Some(&pkg), false, false, "upkeep").0.unwrap();
        assert!(u.requires_admin && u.command.unwrap().contains(&"--force".to_string()));
        // App Store / Microsoft: other providers
        assert!(plan_update(&app("Mac App Store", None), cast(), Some(&e), false, false, "u").0.is_none());
        assert!(plan_update(&app("manual", None), None, Some(&e), false, true, "u").0.is_none());
        // nothing known → honest note
        let (u, note) = plan_update(&app("self-updating (Squirrel)", None), None, None, false, false, "u");
        assert!(u.is_none() && note.unwrap().contains("Squirrel"));
    }

    fn fake_app(root: &Path, dir: &str, name: &str, id: &str, ver: &str, extra: &[&str]) {
        let c = root.join(dir).join(format!("{name}.app/Contents"));
        std::fs::create_dir_all(&c).unwrap();
        let plist = format!(
            "<?xml version=\"1.0\"?><plist><dict><key>CFBundleIdentifier</key><string>{id}</string>\
             <key>CFBundleShortVersionString</key><string>{ver}</string><key>CFBundleVersion</key><string>{ver}</string></dict></plist>"
        );
        std::fs::write(c.join("Info.plist"), plist).unwrap();
        for e in extra {
            std::fs::create_dir_all(c.join(e).parent().unwrap()).unwrap();
            std::fs::write(c.join(e), "").unwrap();
        }
    }

    #[test]
    fn detects_sources_duplicates_and_skips_apple_apps() {
        let tmp = std::env::temp_dir().join(format!("upkeep-apps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        fake_app(&tmp, "A", "Store", "com.x.store", "1.0", &["_MASReceipt/receipt"]);
        fake_app(&tmp, "A", "Dup", "com.x.dup", "1.0", &[]);
        fake_app(&tmp, "B", "Dup", "com.x.dup", "0.9", &[]);
        fake_app(&tmp, "A", "Safari", "com.apple.Safari", "18.0", &[]);
        fake_app(&tmp, "A/Vendor", "Nested", "com.x.nested", "2.0", &["Frameworks/Sparkle.framework/x"]);
        let mut c = ctx(MockRunner::new(&[]));
        c.app_dirs = vec![tmp.join("A").to_string_lossy().into(), tmp.join("B").to_string_lossy().into()];
        let r = Apps.scan(&c).unwrap();
        let src = |n: &str| r.installed.iter().find(|i| i.name == n).map(|i| i.source.clone());
        assert_eq!(src("Store").as_deref(), Some("Mac App Store"));
        assert_eq!(src("Nested").as_deref(), Some("Sparkle"));
        assert_eq!(src("Safari"), None);
        assert_eq!(r.installed.len(), 4);
        assert!(r.issues.iter().any(|i| i.title == "Duplicate app: Dup"));
        assert_eq!(Apps.installed_version(&c, "Nested").as_deref(), Some("2.0"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
