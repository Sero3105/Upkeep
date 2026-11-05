use super::{Provider, plist_strings, unescape_xml};
use crate::model::*;
use crate::shell::Ctx;
use crate::version;
use std::collections::HashMap;
use std::path::Path;

pub struct Apps;

struct App {
    name: String,
    path: String,
    id: String,
    short: String,
    build: String,
    feed: Option<String>,
    source: &'static str,
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
        // Appcast checks hit the network: run them concurrently.
        let checks: Vec<Option<(String, String)>> = std::thread::scope(|s| {
            let handles: Vec<_> = apps
                .iter()
                .map(|a| {
                    s.spawn(move || {
                        let feed = a.feed.as_deref().filter(|f| f.starts_with("https://") && ctx.config.check_appcasts)?;
                        let xml = ctx.out(&["curl", "-fsSL", "--proto", "=https", "--max-time", "10", feed], 15)?;
                        latest_from_appcast(&xml)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok().flatten()).collect()
        });

        let mut by_id: HashMap<&str, Vec<&App>> = HashMap::new();
        for (a, latest) in apps.iter().zip(checks) {
            by_id.entry(&a.id).or_default().push(a);
            // App Store apps are checked by `mas`; cask-managed ones get re-labelled by the engine.
            let verifiable = a.source == "Mac App Store" || (a.feed.is_some() && ctx.config.check_appcasts);
            res.installed.push(Installed {
                name: a.name.clone(),
                version: a.short.clone(),
                category: Category::Apps,
                provider: "apps".into(),
                source: a.source.into(),
                path: Some(a.path.clone()),
                note: (!verifiable).then(|| "update not verifiable: no official update channel exposed".to_string()),
                key: Some(a.id.clone()),
            });
            if let Some((build, short)) = latest {
                let newer = if a.build.is_empty() { version::is_newer(&short, &a.short) } else { version::is_newer(&build, &a.build) };
                if newer {
                    res.updates.push(Update {
                        name: a.name.clone(),
                        provider: "apps".into(),
                        category: Category::Apps,
                        current: a.short.clone(),
                        latest: if short.is_empty() { build } else { short },
                        source: "Sparkle appcast (vendor feed)".into(),
                        support: Support::Manual,
                        hint: Some(format!("Open {} and use its “Check for Updates…” menu: the vendor's signed updater installs it.", a.name)),
                        ..Default::default()
                    });
                }
            }
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
        return None; // Updated with macOS itself.
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
        path: bundle.to_string_lossy().into_owned(),
        id,
        short: v[1].clone().unwrap_or_else(|| build.clone()),
        build,
        feed,
        source,
    })
}

/// Newest non-channel item of a Sparkle appcast: (sparkle:version, sparkle:shortVersionString).
pub fn latest_from_appcast(xml: &str) -> Option<(String, String)> {
    xml.split("<item")
        .skip(1)
        .map(|item| &item[..item.find("</item>").unwrap_or(item.len())])
        .filter(|item| !item.contains("<sparkle:channel>"))
        .filter_map(|item| {
            let build = field(item, "sparkle:version")?;
            Some((build, field(item, "sparkle:shortVersionString").unwrap_or_default()))
        })
        .max_by(|a, b| version::compare(&a.0, &b.0))
}

/// `<k>v</k>` element or `k="v"` attribute.
fn field(item: &str, k: &str) -> Option<String> {
    let open = format!("<{k}>");
    if let Some(i) = item.find(&open) {
        let rest = &item[i + open.len()..];
        return Some(unescape_xml(rest[..rest.find('<')?].trim()));
    }
    let attr = format!("{k}=\"");
    let i = item.find(&attr)? + attr.len();
    Some(unescape_xml(&item[i..i + item[i..].find('"')?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::mock::{MockRunner, ctx};

    #[test]
    fn appcast_picks_newest_stable_item() {
        let xml = r#"<rss><channel>
            <item><title>2.0</title><enclosure url="https://x/a.zip" sparkle:version="200" sparkle:shortVersionString="2.0"/></item>
            <item><sparkle:version>310</sparkle:version><sparkle:shortVersionString>3.1</sparkle:shortVersionString></item>
            <item><sparkle:channel>beta</sparkle:channel><sparkle:version>400</sparkle:version></item>
        </channel></rss>"#;
        assert_eq!(latest_from_appcast(xml), Some(("310".into(), "3.1".into())));
        assert_eq!(latest_from_appcast("<rss/>"), None);
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
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
