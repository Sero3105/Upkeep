//! Smart Update: risk analysis, dependency expansion and ordering into an Update Plan.

use crate::model::*;
use crate::providers::Provider;
use crate::version::{Bump, bump};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// Risk level and the reasons behind it.
pub fn assess(u: &Update) -> (Risk, Vec<String>) {
    let mut risk = Risk::Low;
    let mut why = vec![];
    let mut raise = |r: Risk, reason: String, risk: &mut Risk| {
        *risk = (*risk).max(r);
        why.push(reason);
    };
    let b = bump(&u.current, &u.latest);
    if crate::version::is_newer(&u.current, &u.latest) {
        raise(Risk::Medium, format!("target {} is older than installed {}: downgrade", u.latest, u.current), &mut risk);
    }
    if u.requires_admin {
        raise(Risk::High, "needs administrator privileges".into(), &mut risk);
    }
    if u.requires_restart {
        raise(Risk::High, "requires a restart".into(), &mut risk);
    }
    match b {
        Bump::Major | Bump::Minor if u.is_runtime => raise(Risk::High, format!("language runtime {} → {}: projects may break", u.current, u.latest), &mut risk),
        Bump::Major => raise(Risk::Medium, format!("major version change {} → {}", u.current, u.latest), &mut risk),
        Bump::Unknown => raise(Risk::Medium, "versions not comparable".into(), &mut risk),
        _ => {}
    }
    if u.dependents.len() >= 5 {
        raise(Risk::Medium, format!("{} installed packages depend on it", u.dependents.len()), &mut risk);
    }
    if u.notes.iter().any(|n| n.contains("externally managed")) {
        raise(Risk::Medium, "package inside Homebrew's Python (PEP 668): may conflict with Homebrew".into(), &mut risk);
    }
    if u.notes.iter().any(|n| n.contains("deprecated")) {
        raise(Risk::Medium, "deprecated upstream".into(), &mut risk);
    }
    if why.is_empty() {
        why.push(match b {
            Bump::Minor => "minor update".into(),
            Bump::Patch => "patch update".into(),
            _ => "routine update".into(),
        });
    }
    (risk, why)
}

#[derive(Serialize, Clone, Debug)]
pub struct Step {
    pub update: Update,
    pub risk: Risk,
    pub reasons: Vec<String>,
    /// Added automatically because a selected update needs it.
    pub linked_by: Option<String>,
    /// Depth in the provider `after` graph: groups the plan for display. The executor schedules
    /// on the finer per-step dependency graph.
    pub stage: usize,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Selected but not executable by Upkeep: shown with instructions.
    pub manual: Vec<Update>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn high_risk(&self) -> usize {
        self.steps.iter().filter(|s| s.risk == Risk::High).count()
    }
    pub fn admin(&self) -> usize {
        self.steps.iter().filter(|s| s.update.requires_admin).count()
    }
    pub fn with_rollback(&self) -> usize {
        self.steps.iter().filter(|s| s.update.rollback.is_some()).count()
    }
    pub fn stages(&self) -> usize {
        self.steps.iter().map(|s| s.stage + 1).max().unwrap_or(0)
    }
    /// High-risk steps only run after their own explicit confirmation.
    pub fn executable(&self, include_high: bool) -> Vec<Step> {
        self.steps.iter().filter(|s| include_high || s.risk != Risk::High).cloned().collect()
    }
}

/// An update plus who pulled it in (None = chosen by the user).
type Pick = (Update, Option<String>);

pub fn build(selected: &[Update], all: &[Update], providers: &[Box<dyn Provider>]) -> Plan {
    let mut plan = Plan::default();
    let by_id: HashMap<String, &Update> = all.iter().map(|u| (u.id(), u)).collect();
    let mut chosen: Vec<Pick> = vec![];
    let mut seen = HashSet::new();
    let mut queue: Vec<Pick> = selected.iter().map(|u| (u.clone(), None)).collect();
    // Expand outdated dependencies the package manager would upgrade anyway, so the plan shows them.
    while let Some((u, by)) = queue.pop() {
        if !seen.insert(u.id()) {
            continue;
        }
        if u.support == Support::Supported {
            for d in &u.dependencies {
                if let Some(dep) = by_id.get(&format!("{}:{d}", u.provider))
                    && !seen.contains(&dep.id()) {
                        queue.push(((*dep).clone(), Some(u.name.clone())));
                    }
            }
        }
        // Chosen by the user too: not an automatic addition.
        let by = if selected.iter().any(|s| s.id() == u.id()) { None } else { by };
        chosen.push((u, by));
    }
    let (exec, manual): (Vec<_>, Vec<_>) = chosen.into_iter().partition(|(u, _)| u.support == Support::Supported && u.command.is_some());
    plan.manual = manual.into_iter().map(|(u, _)| u).collect();

    // Stage = depth in the provider `after` graph, counting only providers present in the plan.
    let present: HashSet<String> = exec.iter().map(|(u, _)| u.provider.clone()).collect();
    let after: HashMap<&str, &[&str]> = providers.iter().map(|p| (p.id(), p.after())).collect();
    fn depth(p: &str, after: &HashMap<&str, &[&str]>, present: &HashSet<String>, guard: usize) -> usize {
        if guard > 16 {
            return 0;
        }
        after.get(p).map_or(0, |a| a.iter().filter(|x| present.contains(**x)).map(|x| depth(x, after, present, guard + 1) + 1).max().unwrap_or(0))
    }

    let mut lanes: BTreeMap<(usize, String), Vec<Pick>> = BTreeMap::new();
    for (u, by) in exec {
        lanes.entry((depth(&u.provider, &after, &present, 0), u.provider.clone())).or_default().push((u, by));
    }
    for ((stage, provider), items) in lanes {
        for (u, by) in order_lane(items, &mut plan.warnings, &provider) {
            let (risk, reasons) = assess(&u);
            plan.steps.push(Step { update: u, risk, reasons, linked_by: by, stage });
        }
    }
    conflicts(&mut plan);
    plan
}

/// Self-update first, then dependencies before dependents (Kahn), alphabetical otherwise.
fn order_lane(mut items: Vec<Pick>, warnings: &mut Vec<String>, provider: &str) -> Vec<Pick> {
    items.sort_by(|a, b| b.0.is_self_update.cmp(&a.0.is_self_update).then(a.0.name.cmp(&b.0.name)));
    let names: HashSet<String> = items.iter().map(|(u, _)| u.name.clone()).collect();
    let mut done: HashSet<String> = HashSet::new();
    let mut out = vec![];
    while !items.is_empty() {
        let ready = items.iter().position(|(u, _)| u.is_self_update || u.dependencies.iter().all(|d| !names.contains(d) || done.contains(d)));
        let i = ready.unwrap_or_else(|| {
            warnings.push(format!("{provider}: dependency cycle, remaining steps keep alphabetical order"));
            0
        });
        let it = items.remove(i);
        done.insert(it.0.name.clone());
        out.push(it);
    }
    out
}

fn conflicts(plan: &mut Plan) {
    let has = |p: &str, n: &str| plan.steps.iter().any(|s| s.update.provider == p && (s.update.name == n || n == "*"));
    let mut w = vec![];
    if has("homebrew", "node") && plan.steps.iter().any(|s| s.update.provider == "npm" && s.update.is_self_update) {
        w.push("`brew upgrade node` reinstalls its bundled npm: the npm self-update runs after it and is re-applied.".to_string());
    }
    for rt in ["node", "python@3", "ruby"] {
        let upgrading = plan.steps.iter().any(|s| s.update.provider == "homebrew" && s.update.name.starts_with(rt));
        let pkg = match rt {
            "node" => "npm",
            "ruby" => "gem",
            _ => "pip",
        };
        if upgrading && has(pkg, "*") {
            w.push(format!("{rt} is upgraded in an earlier stage; {pkg} packages run afterwards against the new runtime."));
        }
    }
    if plan.admin() > 0 {
        w.push(format!("{} step(s) need your administrator password: Upkeep runs them in the terminal so you can type it (sudo).", plan.admin()));
    }
    plan.warnings.extend(w);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::registry;

    fn up(provider: &str, name: &str, cur: &str, new: &str) -> Update {
        Update {
            name: name.into(),
            provider: provider.into(),
            current: cur.into(),
            latest: new.into(),
            support: Support::Supported,
            command: Some(vec!["true".into()]),
            ..Default::default()
        }
    }

    #[test]
    fn risk_levels() {
        assert_eq!(assess(&up("homebrew", "gh", "2.103.1", "2.103.3")).0, Risk::Low);
        assert_eq!(assess(&up("npm", "npm", "11.20.0", "12.2.2")).0, Risk::Medium);
        let mut py = up("homebrew", "python@3.13", "3.13.1", "3.14.0");
        py.is_runtime = true;
        assert_eq!(assess(&py).0, Risk::High);
        let mut admin = up("homebrew", "zoom", "6.0", "6.0.1");
        admin.requires_admin = true;
        let (r, why) = assess(&admin);
        assert_eq!(r, Risk::High);
        assert!(why[0].contains("administrator"));
        let mut shared = up("homebrew", "glib", "2.88.3", "2.88.4");
        shared.dependents = (0..6).map(|i| format!("d{i}")).collect();
        assert_eq!(assess(&shared).0, Risk::Medium);
        let (r, why) = assess(&up("homebrew", "openssl@3", "3.6.4", "3.5.9"));
        assert_eq!(r, Risk::Medium);
        assert!(why[0].contains("downgrade"));
    }

    #[test]
    fn plan_links_deps_orders_lanes_and_separates_manual() {
        let mut glib = up("homebrew", "glib", "2.88", "2.90");
        glib.dependencies = vec!["pcre2".into()];
        let pcre = up("homebrew", "pcre2", "10.44", "10.45");
        let ts = up("npm", "typescript", "5.4.0", "5.6.2");
        let mut npm = up("npm", "npm", "11.0.0", "11.1.0");
        npm.is_self_update = true;
        let mut mac = up("softwareupdate", "macOS 27.1", "27.0", "27.1");
        mac.support = Support::Manual;
        mac.command = None;
        let all = vec![glib.clone(), pcre, ts.clone(), npm.clone(), mac.clone()];
        let plan = build(&[ts, glib, npm, mac], &all, &registry());
        let order: Vec<&str> = plan.steps.iter().map(|s| s.update.name.as_str()).collect();
        assert_eq!(order, vec!["pcre2", "glib", "npm", "typescript"]);
        assert_eq!(plan.steps[0].linked_by.as_deref(), Some("glib"));
        assert_eq!(plan.steps[0].stage, 0);
        assert_eq!(plan.steps[2].stage, 1, "npm runs after homebrew");
        assert_eq!(plan.manual.len(), 1);
        assert_eq!(plan.stages(), 2);
        let both = build(&[all[0].clone(), all[1].clone()], &all, &registry());
        assert!(both.steps.iter().all(|s| s.linked_by.is_none()), "user-selected deps are not 'linked'");
    }

    #[test]
    fn high_risk_needs_explicit_inclusion() {
        let mut big = up("homebrew", "node", "24.0", "25.0");
        big.is_runtime = true;
        let small = up("homebrew", "gh", "2.1", "2.2");
        let plan = build(&[big.clone(), small.clone()], &[big, small], &registry());
        assert_eq!(plan.high_risk(), 1);
        assert_eq!(plan.executable(false).len(), 1);
        assert_eq!(plan.executable(true).len(), 2);
    }
}
