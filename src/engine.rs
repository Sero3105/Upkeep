//! Core engine: runs every provider scan concurrently (scans are read-only, so this is safe),
//! merges the results, removes cross-provider duplicates and caches the snapshot.

use crate::config;
use crate::model::*;
use crate::providers::Provider;
use crate::shell::Ctx;
use std::collections::HashSet;
use std::time::Instant;

pub enum ScanEvent<'a> {
    Finished(&'a ProviderStatus),
}

pub fn scan(ctx: &Ctx, providers: &[Box<dyn Provider>], progress: &(dyn Fn(ScanEvent) + Sync)) -> Snapshot {
    let results: Vec<(ProviderStatus, ScanResult)> = std::thread::scope(|s| {
        let hs: Vec<_> = providers
            .iter()
            .map(|p| {
                s.spawn(move || {
                    let mut st = ProviderStatus { id: p.id().into(), name: p.name().into(), enabled: ctx.config.enabled(p.id()), ..Default::default() };
                    let mut res = ScanResult::default();
                    if st.enabled {
                        st.available = p.available(ctx);
                        if st.available {
                            let t = Instant::now();
                            match p.scan(ctx) {
                                Ok(r) => res = r,
                                Err(e) => st.error = Some(e),
                            }
                            st.duration = t.elapsed().as_secs_f64();
                        } else if let Some(h) = p.missing_hint() {
                            res.issues.push(HealthIssue::new(Severity::Info, format!("{} not checked", p.name()), p.name(), "its command-line tool is not installed").fix(h));
                        }
                    }
                    st.installed = res.installed.len();
                    st.updates = res.updates.len();
                    progress(ScanEvent::Finished(&st));
                    (st, res)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("provider scan panicked")).collect()
    });

    let mut snap = Snapshot { date: config::now(), ..Default::default() };
    for (st, r) in results {
        if let Some(e) = &st.error {
            snap.warnings.push(format!("{}: {e}", st.name));
        }
        snap.warnings.extend(r.warnings.into_iter().map(|w| format!("{}: {w}", st.name)));
        snap.installed.extend(r.installed);
        snap.updates.extend(r.updates);
        snap.issues.extend(r.issues);
        snap.providers.push(st);
    }
    merge(&mut snap, &ctx.config);
    snap
}

/// Cross-provider cleanup, kept out of providers so they stay independent.
pub fn merge(snap: &mut Snapshot, cfg: &config::Config) {
    // Apps installed by a Homebrew cask: the cask is the update channel.
    let cask_paths: HashSet<String> = snap.installed.iter().filter(|i| i.source == "Homebrew cask").filter_map(|i| i.path.clone()).collect();
    let mut cask_apps = HashSet::new();
    for i in snap.installed.iter_mut().filter(|i| i.provider == "apps") {
        if i.path.as_ref().is_some_and(|p| cask_paths.contains(p)) {
            i.source = "Homebrew cask".into();
            i.note = None;
            cask_apps.insert(i.name.clone());
        }
    }
    // Same app reported by the Homebrew provider and by the apps provider: keep Homebrew's.
    let brew_casks: HashSet<String> = snap.updates.iter().filter(|u| u.provider == "homebrew" && u.source == "Homebrew cask").map(|u| u.name.clone()).collect();
    snap.updates.retain(|u| {
        u.provider != "apps" || !(u.cask.as_ref().is_some_and(|c| brew_casks.contains(c)) || (u.cask.is_none() && cask_apps.contains(&u.name) && !brew_casks.is_empty()))
    });
    // A source offering an *older* version (e.g. a reverted formula) is not an update: never run it.
    for u in snap.updates.iter_mut().filter(|u| crate::version::is_newer(&u.current, &u.latest)) {
        u.support = Support::Manual;
        u.command = None;
        u.notes.push("downgrade offered by the source".into());
        u.hint = Some(format!("Installed {} is newer than what {} ships now ({}): usually a reverted package. Nothing to do.", u.current, u.source, u.latest));
    }
    let before = snap.updates.len();
    snap.updates.retain(|u| !cfg.is_excluded(&u.id(), &u.name));
    snap.excluded = before - snap.updates.len();
    snap.updates.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

pub fn save(snap: &Snapshot) {
    if let Err(e) = config::write_atomic(config::SNAPSHOT, &serde_json::to_string(snap).unwrap()) {
        config::log(&format!("could not save snapshot: {e}"));
    }
}

pub fn load_cached() -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(config::path(config::SNAPSHOT)).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_relabels_cask_apps_and_applies_exclusions() {
        let mut snap = Snapshot::default();
        snap.installed.push(Installed { name: "google-chrome".into(), provider: "homebrew".into(), source: "Homebrew cask".into(), path: Some("/Applications/Google Chrome.app".into()), ..Default::default() });
        snap.installed.push(Installed { name: "Google Chrome".into(), provider: "apps".into(), source: "manual".into(), path: Some("/Applications/Google Chrome.app".into()), note: Some("update not verifiable".into()), ..Default::default() });
        snap.updates.push(Update { name: "google-chrome".into(), provider: "homebrew".into(), source: "Homebrew cask".into(), ..Default::default() });
        snap.updates.push(Update { name: "Google Chrome".into(), provider: "apps".into(), cask: Some("google-chrome".into()), ..Default::default() });
        snap.updates.push(Update { name: "Slack".into(), provider: "apps".into(), cask: Some("slack".into()), ..Default::default() });
        snap.updates.push(Update { name: "node".into(), provider: "homebrew".into(), ..Default::default() });
        snap.updates.push(Update { name: "openssl@3".into(), provider: "homebrew".into(), current: "3.6.4".into(), latest: "3.5.9".into(), support: Support::Supported, command: Some(vec!["brew".into()]), ..Default::default() });
        let cfg = config::Config { excluded: vec!["homebrew:node".into()], ..Default::default() };
        merge(&mut snap, &cfg);
        assert_eq!(snap.installed[1].source, "Homebrew cask");
        assert_eq!(snap.unverifiable(), 0);
        assert_eq!(snap.updates.len(), 3, "chrome deduped, slack kept, openssl kept as manual");
        assert_eq!(snap.updates[0].name, "google-chrome");
        let ossl = snap.updates.iter().find(|u| u.name == "openssl@3").unwrap();
        assert!(ossl.support == Support::Manual && ossl.command.is_none(), "downgrades are never executable");
        assert_eq!(snap.excluded, 1);
    }
}
