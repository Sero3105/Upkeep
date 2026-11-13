//! Runs an Update Plan. Each step starts as soon as the steps it depends on are done (its
//! package dependencies, its manager's self-update, providers it must run `after`), up to a
//! job limit derived from CPU cores and current load. Steps that share a tool (brew, npm, pip…)
//! never overlap, because those tools lock or corrupt shared state; their downloads (`prefetch`)
//! run ahead in parallel instead, so the serialized part is mostly unpacking. Admin steps in a
//! terminal run alone so the `sudo` prompt is readable. Every result is verified by re-reading
//! the installed version: an exit code 0 alone is never reported as "updated".

use crate::config;
use crate::history::{self, History, Record};
use crate::planner::Step;
use crate::providers::Provider;
use crate::shell::{CancelToken, Ctx, RunOpts, RunResult};
use crate::version;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Set by the CLI's SIGINT handler.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Verified: the installed version is now the target.
    Success(String),
    /// Command succeeded but the result could not be confirmed.
    Unverified(String),
    AlreadyCurrent(String),
    Failed(String),
    Skipped(String),
    NotRun,
}

impl Outcome {
    pub fn key(&self) -> &'static str {
        match self {
            Outcome::Success(_) => "success",
            Outcome::Unverified(_) => "unverified",
            Outcome::AlreadyCurrent(_) => "already-current",
            Outcome::Failed(_) => "failed",
            Outcome::Skipped(_) => "skipped",
            Outcome::NotRun => "not-run",
        }
    }
    pub fn detail(&self) -> String {
        match self {
            Outcome::Success(v) => format!("now {v}"),
            Outcome::AlreadyCurrent(v) => format!("already at {v}"),
            Outcome::Unverified(s) | Outcome::Failed(s) | Outcome::Skipped(s) => s.clone(),
            Outcome::NotRun => "not run (interrupted)".into(),
        }
    }
}

pub enum Event {
    Started(usize),
    Line(usize, String),
    Finished(usize, Outcome),
}

#[derive(Default)]
pub struct Control {
    abort: AtomicBool,
    skip: Mutex<HashSet<usize>>,
    running: Mutex<HashMap<usize, CancelToken>>,
    fetching: Mutex<HashMap<usize, CancelToken>>,
}

impl Control {
    /// Controlled stop: running steps finish, nothing new starts, downloads ahead are dropped.
    pub fn abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
        self.fetching.lock().unwrap().values().for_each(CancelToken::cancel);
    }
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst)
    }
    /// Skip a pending step, or stop it if it is running.
    pub fn skip(&self, i: usize) {
        self.skip.lock().unwrap().insert(i);
        if let Some(t) = self.running.lock().unwrap().get(&i) {
            t.cancel();
        }
        if let Some(t) = self.fetching.lock().unwrap().get(&i) {
            t.cancel();
        }
    }
    fn skipped(&self, i: usize) -> bool {
        self.skip.lock().unwrap().contains(&i)
    }
}

/// Upper bound for the automatic job limit: past this, more parallel downloads just split the
/// same bandwidth.
const MAX_AUTO_JOBS: usize = 8;

/// How many steps may run at once. `running` = steps of ours already running, so our own
/// work doesn't count as somebody else's load.
pub fn job_limit(cfg: &config::Config, running: usize) -> usize {
    if !cfg.parallel {
        return 1;
    }
    if cfg.jobs > 0 {
        return cfg.jobs;
    }
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let busy = load_avg().map_or(0.0, |l| (l - running as f64).max(0.0));
    let free = (cores as f64 - busy).floor().max(0.0) as usize;
    free.clamp(2, cores.clamp(2, MAX_AUTO_JOBS))
}

/// 1-minute load average.
fn load_avg() -> Option<f64> {
    let mut l = [0f64; 3];
    (unsafe { libc::getloadavg(l.as_mut_ptr(), 3) } >= 1).then_some(l[0])
}

/// The program a command runs (`sudo x` → `x`, paths → basename): steps sharing it share its lock.
fn tool(cmd: Option<&Vec<String>>) -> String {
    let c = cmd.map(Vec::as_slice).unwrap_or_default();
    let c = if c.first().is_some_and(|s| s == "sudo") { &c[1..] } else { c };
    c.first().map(|s| s.rsplit('/').next().unwrap_or(s).to_string()).unwrap_or_default()
}

/// For each step, the earlier steps it must wait for. Only earlier indices: the plan is already
/// topologically ordered, and this keeps the graph acyclic whatever the input.
fn dependencies(providers: &[Box<dyn Provider>], steps: &[Step]) -> Vec<Vec<usize>> {
    let after: HashMap<&str, &[&str]> = providers.iter().map(|p| (p.id(), p.after())).collect();
    // Transitive: npm after homebrew after X still waits for X when homebrew has no steps.
    let before = |p: &str| {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut todo: Vec<&str> = after.get(p).copied().unwrap_or_default().to_vec();
        while let Some(x) = todo.pop() {
            if seen.insert(x) {
                todo.extend(after.get(x).copied().unwrap_or_default());
            }
        }
        seen
    };
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let u = &s.update;
            let pre = before(&u.provider);
            (0..i)
                .filter(|&j| {
                    let t = &steps[j].update;
                    if t.provider == u.provider {
                        t.is_self_update || u.dependencies.contains(&t.name)
                    } else {
                        pre.contains(t.provider.as_str())
                    }
                })
                .collect()
        })
        .collect()
}

#[derive(Default)]
struct Sched {
    pending: Vec<bool>,
    done: Vec<bool>,
    /// Prefetch finished (or not needed): the install may start.
    fetched: Vec<bool>,
    tools: HashMap<String, usize>,
    running: usize,
    /// An admin step owns the terminal.
    exclusive: bool,
}

pub fn execute(
    ctx: &Ctx,
    providers: &[Box<dyn Provider>],
    steps: &[Step],
    interactive: bool,
    control: &Control,
    hist: &History,
    events: &(dyn Fn(Event) + Sync),
) -> Vec<Outcome> {
    let n = steps.len();
    let outcomes: Mutex<Vec<Outcome>> = Mutex::new(vec![Outcome::NotRun; n]);
    let deps = dependencies(providers, steps);
    let tools: Vec<String> = steps.iter().map(|s| tool(s.update.command.as_ref())).collect();
    let parallel = ctx.config.parallel;
    let prefetch: Vec<usize> = if parallel { (0..n).filter(|&i| steps[i].update.prefetch.is_some()).collect() } else { vec![] };
    let st = Mutex::new(Sched {
        pending: vec![true; n],
        done: vec![false; n],
        fetched: (0..n).map(|i| !prefetch.contains(&i)).collect(),
        ..Default::default()
    });
    let cv = Condvar::new();
    let solo = |i: usize| interactive && steps[i].update.requires_admin;

    // Next step allowed to start now, in plan order.
    let ready = |s: &Sched, i: usize| {
        if !s.pending[i] || s.exclusive || !deps[i].iter().all(|&j| s.done[j]) {
            return false;
        }
        if control.skipped(i) {
            return true; // resolves immediately, no slot needed
        }
        let free_tool = steps[i].update.independent || s.tools.get(&tools[i]).copied().unwrap_or(0) == 0;
        s.fetched[i] && free_tool && (!solo(i) || s.running == 0)
    };

    let worker = || {
        loop {
            let i = {
                let mut s = st.lock().unwrap();
                loop {
                    if control.aborted() || !s.pending.contains(&true) {
                        cv.notify_all();
                        return;
                    }
                    let room = s.running < job_limit(&ctx.config, s.running);
                    if let Some(i) = (0..n).find(|&i| ready(&s, i) && (room || control.skipped(i))) {
                        s.pending[i] = false;
                        s.running += 1;
                        *s.tools.entry(tools[i].clone()).or_default() += 1;
                        s.exclusive = solo(i);
                        break i;
                    }
                    // Timeout: load average and abort flags change without a notification.
                    s = cv.wait_timeout(s, Duration::from_millis(500)).unwrap().0;
                }
            };
            let o = run_step(ctx, providers, &steps[i], i, interactive, control, hist, events);
            outcomes.lock().unwrap()[i] = o;
            let mut s = st.lock().unwrap();
            s.done[i] = true;
            s.running -= 1;
            *s.tools.get_mut(&tools[i]).unwrap() -= 1;
            if solo(i) {
                s.exclusive = false;
            }
            cv.notify_all();
        }
    };

    let next_fetch = AtomicUsize::new(0);
    let fetcher = || {
        while let Some(&i) = prefetch.get(next_fetch.fetch_add(1, Ordering::SeqCst)) {
            if !control.aborted() && !control.skipped(i) {
                fetch(ctx, &steps[i], i, control, events);
            }
            st.lock().unwrap().fetched[i] = true;
            cv.notify_all();
        }
    };

    // Enough threads for the highest limit; `job_limit` decides, live, how many actually work.
    let workers = match (parallel, ctx.config.jobs) {
        (false, _) => 1,
        (true, 0) => MAX_AUTO_JOBS,
        (true, j) => j,
    };
    let fetchers = job_limit(&ctx.config, 0).min(prefetch.len());
    std::thread::scope(|s| {
        for _ in 0..fetchers {
            s.spawn(fetcher);
        }
        for _ in 0..workers.min(n.max(1)) {
            s.spawn(worker);
        }
    });
    outcomes.into_inner().unwrap()
}

/// Download ahead of the install. Errors are only noted: the install downloads by itself.
fn fetch(ctx: &Ctx, step: &Step, i: usize, control: &Control, events: &(dyn Fn(Event) + Sync)) {
    let Some(cmd) = &step.update.prefetch else { return };
    let token = CancelToken::default();
    control.fetching.lock().unwrap().insert(i, token.clone());
    let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
    let r = ctx.runner.run(
        &argv,
        &RunOpts { timeout: Duration::from_secs(ctx.config.command_timeout), env: tool_env(&step.update.provider, cmd), cancel: Some(token), ..Default::default() },
    );
    control.fetching.lock().unwrap().remove(&i);
    if r.cancelled {
        return;
    }
    if r.ok() {
        events(Event::Line(i, "downloaded ahead of install".into()));
    } else {
        config::log(&format!("prefetch failed: {}: {}", cmd.join(" "), explain(&r)));
        events(Event::Line(i, "download ahead failed; the install will download it".into()));
    }
}

/// Keep Homebrew from refreshing its index (or printing hints) on every command.
fn tool_env(provider: &str, cmd: &[String]) -> Vec<(&'static str, &'static str)> {
    if provider == "homebrew" || cmd.first().is_some_and(|c| c == "brew") {
        vec![("HOMEBREW_NO_AUTO_UPDATE", "1"), ("HOMEBREW_NO_ENV_HINTS", "1")]
    } else {
        vec![]
    }
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    ctx: &Ctx,
    providers: &[Box<dyn Provider>],
    step: &Step,
    i: usize,
    interactive: bool,
    control: &Control,
    hist: &History,
    events: &(dyn Fn(Event) + Sync),
) -> Outcome {
    let u = &step.update;
    if control.aborted() {
        return Outcome::NotRun;
    }
    let provider = providers.iter().find(|p| p.id() == u.provider);
    let mut rec = Record {
        id: history::new_id(),
        date: config::now(),
        name: u.name.clone(),
        provider: u.provider.clone(),
        category: u.category,
        from: u.current.clone(),
        to: u.latest.clone(),
        operation: "update".into(),
        command: u.command.clone().unwrap_or_default(),
        ..Default::default()
    };
    let finish = |rec: &mut Record, o: Outcome| {
        rec.outcome = o.key().into();
        rec.detail = Some(o.detail());
        if matches!(o, Outcome::Success(_) | Outcome::Unverified(_)) {
            rec.rollback = u.rollback.clone();
        }
        if let Err(e) = hist.append(rec) {
            config::log(&format!("history write failed: {e}"));
        }
        config::log(&format!("{} {} {} -> {}: {}", rec.provider, rec.name, rec.from, rec.to, o.detail()));
        events(Event::Finished(i, o.clone()));
        o
    };
    if control.skipped(i) {
        return finish(&mut rec, Outcome::Skipped("skipped by user".into()));
    }
    if u.requires_admin && !interactive {
        return finish(&mut rec, Outcome::Skipped("needs administrator password: run `upkeep update` in a terminal".into()));
    }
    let Some(cmd) = u.command.clone() else {
        return finish(&mut rec, Outcome::Skipped("no command: manual update".into()));
    };
    events(Event::Started(i));
    // Already done (e.g. pulled in by an earlier dependency upgrade, or a previous interrupted run).
    if let Some(v) = provider.and_then(|p| p.installed_version(ctx, &u.name))
        && !version::is_newer(&u.latest, &v) {
            return finish(&mut rec, Outcome::AlreadyCurrent(v));
        }

    let token = CancelToken::default();
    control.running.lock().unwrap().insert(i, token.clone());
    let tail = Mutex::new(VecDeque::new());
    let on_line = |l: &str| {
        events(Event::Line(i, l.to_string()));
        let mut t = tail.lock().unwrap();
        t.push_back(l.to_string());
        if t.len() > 200 {
            t.pop_front();
        }
    };
    config::log(&format!("run: {}", cmd.join(" ")));
    let started = Instant::now();
    let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
    let env = tool_env(&u.provider, &cmd);
    let r = ctx.runner.run(
        &argv,
        &RunOpts {
            timeout: Duration::from_secs(ctx.config.command_timeout),
            env,
            on_line: Some(&on_line),
            cancel: Some(token),
            detach_tty: !interactive,
        },
    );
    control.running.lock().unwrap().remove(&i);
    if r.status == 0 && ctx.config.strip_quarantine
        && let Some(app) = &u.app_path {
            let x = ctx.runner.run(&["xattr", "-dr", "com.apple.quarantine", app], &RunOpts { timeout: Duration::from_secs(60), ..Default::default() });
            if !x.ok() {
                config::log(&format!("could not remove quarantine from {app}: {}", x.err.trim()));
            }
        }
    rec.duration = started.elapsed().as_secs_f64();
    rec.output_tail = tail.into_inner().unwrap().into();

    let o = if r.cancelled {
        Outcome::Skipped("stopped by user".into())
    } else if r.timed_out {
        Outcome::Failed(format!("timed out after {}s", ctx.config.command_timeout))
    } else if r.status != 0 {
        Outcome::Failed(explain(&r))
    } else {
        match provider.and_then(|p| p.installed_version(ctx, &u.name)) {
            Some(v) if !version::is_newer(&u.latest, &v) => Outcome::Success(v),
            Some(v) if v != u.current => Outcome::Unverified(format!("now {v}, expected {}", u.latest)),
            Some(v) if u.requires_restart => Outcome::Unverified(format!("installed: restart the Mac to finish (still {v} until then)")),
            Some(v) => Outcome::Unverified(format!("command succeeded but version is still {v}")),
            None => Outcome::Unverified("exit code 0; version could not be re-checked".into()),
        }
    };
    finish(&mut rec, o)
}

fn explain(r: &RunResult) -> String {
    let src = if r.err.trim().is_empty() { &r.out } else { &r.err };
    let lines: Vec<&str> = src.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    let admin = ["a terminal is required", "password is required", "sudo:"].iter().any(|k| src.contains(k));
    if admin {
        format!("needs administrator password: run `upkeep update` in a terminal ({tail})")
    } else {
        format!("exit {}: {tail}", r.status)
    }
}

/// Re-run the recorded rollback command and verify the old version is back.
pub fn rollback(ctx: &Ctx, providers: &[Box<dyn Provider>], rec: &Record, interactive: bool, hist: &History, on_line: &(dyn Fn(&str) + Sync)) -> Result<Record, String> {
    let cmd = rec.rollback.clone().ok_or("this operation has no supported rollback")?;
    let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
    config::log(&format!("rollback: {}", cmd.join(" ")));
    let started = Instant::now();
    let r = ctx.runner.run(
        &argv,
        &RunOpts { timeout: Duration::from_secs(ctx.config.command_timeout), on_line: Some(on_line), detach_tty: !interactive, ..Default::default() },
    );
    let verified = providers.iter().find(|p| p.id() == rec.provider).and_then(|p| p.installed_version(ctx, &rec.name));
    let o = if !r.ok() {
        Outcome::Failed(explain(&r))
    } else {
        match verified {
            Some(v) if version::compare(&v, &rec.from) == std::cmp::Ordering::Equal => Outcome::Success(v),
            Some(v) => Outcome::Unverified(format!("now {v}, expected {}", rec.from)),
            None => Outcome::Unverified("exit code 0; version could not be re-checked".into()),
        }
    };
    let new = Record {
        id: history::new_id(),
        date: config::now(),
        name: rec.name.clone(),
        provider: rec.provider.clone(),
        category: rec.category,
        from: rec.to.clone(),
        to: rec.from.clone(),
        operation: "rollback".into(),
        command: cmd,
        duration: started.elapsed().as_secs_f64(),
        outcome: o.key().into(),
        detail: Some(o.detail()),
        rollback: None,
        output_tail: r.out.lines().rev().take(50).map(String::from).collect::<Vec<_>>().into_iter().rev().collect(),
    };
    hist.append(&new)?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::planner::{Risk, Step};
    use crate::shell::mock::{MockRunner, ctx};

    /// Provider whose installed version comes from `fake-ver <name>`.
    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn name(&self) -> &'static str {
            "fake"
        }
        fn category(&self) -> Category {
            Category::Packages
        }
        fn available(&self, _: &Ctx) -> bool {
            true
        }
        fn scan(&self, _: &Ctx) -> Result<ScanResult, String> {
            Ok(ScanResult::default())
        }
        fn installed_version(&self, ctx: &Ctx, name: &str) -> Option<String> {
            ctx.out(&["fake-ver", name], 5).map(|s| s.trim().to_string())
        }
    }

    fn step(name: &str, admin: bool) -> Step {
        Step {
            update: Update {
                name: name.into(),
                provider: "fake".into(),
                current: "1.0".into(),
                latest: "2.0".into(),
                support: Support::Supported,
                command: Some(vec!["fake-up".into(), name.into()]),
                rollback: Some(vec!["fake-down".into(), name.into()]),
                requires_admin: admin,
                ..Default::default()
            },
            risk: Risk::Low,
            reasons: vec![],
            linked_by: None,
            stage: 0,
        }
    }

    fn hist() -> (History, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("upkeep-exec-{}-{}.jsonl", std::process::id(), history::new_id()));
        (History::at(p.clone()), p)
    }

    #[test]
    fn verifies_results_and_records_history() {
        let runner = MockRunner::new(&[])
            // ok: 1.0 before, 2.0 after
            .on("fake-ver ok", 0, "1.0").on("fake-ver ok", 0, "2.0").on("fake-up ok", 0, "done")
            // done: already current, must not run the command
            .on("fake-ver done", 0, "2.0")
            // bad: command fails
            .on("fake-ver bad", 0, "1.0").on("fake-up bad", 1, "")
            // same: exit 0 but nothing changed
            .on("fake-ver same", 0, "1.0").on("fake-up same", 0, "");
        let c = ctx(runner);
        let steps = vec![step("ok", false), step("done", false), step("bad", false), step("same", false), step("root", true)];
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        let out = execute(&c, &providers, &steps, false, &Control::default(), &h, &|_| {});
        assert_eq!(out[0], Outcome::Success("2.0".into()));
        assert_eq!(out[1], Outcome::AlreadyCurrent("2.0".into()));
        assert!(matches!(out[2], Outcome::Failed(_)));
        assert!(matches!(out[3], Outcome::Unverified(_)), "exit 0 without version change is not a success");
        assert!(matches!(&out[4], Outcome::Skipped(s) if s.contains("administrator")), "no sudo in non-interactive mode");
        let (recs, _) = h.load();
        assert_eq!(recs.len(), 5);
        let ok = recs.iter().find(|r| r.name == "ok").unwrap();
        assert_eq!(ok.rollback.as_ref().unwrap()[0], "fake-down");
        assert!(recs.iter().find(|r| r.name == "bad").unwrap().rollback.is_none());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn skip_and_abort() {
        let c = ctx(MockRunner::new(&[]).on("fake-ver a", 0, "1.0").on("fake-ver b", 0, "1.0"));
        let steps = vec![step("a", false), step("b", false)];
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        let ctl = Control::default();
        ctl.skip(0);
        ctl.abort();
        let out = execute(&c, &providers, &steps, false, &ctl, &h, &|_| {});
        assert_eq!(out, vec![Outcome::NotRun, Outcome::NotRun], "abort wins over everything");
        let ctl = Control::default();
        ctl.skip(0);
        let out = execute(&c, &providers, &steps[..1], false, &ctl, &h, &|_| {});
        assert!(matches!(out[0], Outcome::Skipped(_)));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rollback_runs_recorded_command_and_verifies() {
        let c = ctx(MockRunner::new(&[]).on("fake-down ok", 0, "").on("fake-ver ok", 0, "1.0"));
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        let rec = Record { name: "ok".into(), provider: "fake".into(), from: "1.0".into(), to: "2.0".into(), rollback: Some(vec!["fake-down".into(), "ok".into()]), ..Default::default() };
        let r = rollback(&c, &providers, &rec, false, &h, &|_| {}).unwrap();
        assert_eq!((r.outcome.as_str(), r.operation.as_str(), r.to.as_str()), ("success", "rollback", "1.0"));
        assert!(rollback(&c, &providers, &Record::default(), false, &h, &|_| {}).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn strips_quarantine_only_when_enabled_and_command_succeeded() {
        let m = std::sync::Arc::new(MockRunner::new(&[]).on("fake-ver ok", 0, "1.0").on("fake-ver ok", 0, "2.0").on("fake-up ok", 0, "").on("xattr -dr com.apple.quarantine /Applications/Ok.app", 0, ""));
        let mut c = ctx(MockRunner::new(&[]));
        c.runner = m.clone();
        let mut s = step("ok", false);
        s.update.app_path = Some("/Applications/Ok.app".into());
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        execute(&c, &providers, std::slice::from_ref(&s), false, &Control::default(), &h, &|_| {});
        assert!(!m.calls.lock().unwrap().iter().any(|k| k.starts_with("xattr")), "off by default");
        c.config.strip_quarantine = true;
        m.responses.lock().unwrap().insert("fake-ver ok".into(), VecDeque::from([RunResult { out: "1.0".into(), ..Default::default() }, RunResult { out: "2.0".into(), ..Default::default() }]));
        execute(&c, &providers, std::slice::from_ref(&s), false, &Control::default(), &h, &|_| {});
        assert!(m.calls.lock().unwrap().iter().any(|k| k == "xattr -dr com.apple.quarantine /Applications/Ok.app"));
        let _ = std::fs::remove_file(p);
    }

    /// Records which commands overlap in time. Every command takes 40ms.
    #[derive(Default)]
    struct Timeline {
        live: Mutex<Vec<String>>,
        overlaps: Mutex<Vec<(String, String)>>,
        order: Mutex<Vec<String>>,
    }
    impl crate::shell::Runner for Timeline {
        fn run(&self, argv: &[&str], _: &RunOpts) -> RunResult {
            let key = argv.join(" ");
            if argv[0] == "fake-ver" {
                return RunResult { out: "1.0".into(), ..Default::default() };
            }
            {
                let mut live = self.live.lock().unwrap();
                self.overlaps.lock().unwrap().extend(live.iter().map(|o| (o.clone(), key.clone())));
                live.push(key.clone());
                self.order.lock().unwrap().push(key.clone());
            }
            std::thread::sleep(Duration::from_millis(40));
            self.live.lock().unwrap().retain(|k| *k != key);
            RunResult::default()
        }
        fn which(&self, _: &str) -> Option<String> {
            None
        }
    }

    fn timed(steps: &[Step], jobs: usize) -> std::sync::Arc<Timeline> {
        let t = std::sync::Arc::new(Timeline::default());
        let mut c = ctx(MockRunner::new(&[]));
        c.runner = t.clone();
        c.config.jobs = jobs;
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        execute(&c, &providers, steps, false, &Control::default(), &h, &|_| {});
        let _ = std::fs::remove_file(p);
        t
    }

    fn overlap(t: &Timeline, a: &str, b: &str) -> bool {
        t.overlaps.lock().unwrap().iter().any(|(x, y)| (x.contains(a) && y.contains(b)) || (x.contains(b) && y.contains(a)))
    }

    #[test]
    fn same_tool_never_overlaps_but_downloads_and_independent_steps_do() {
        let mut steps: Vec<Step> = ["a", "b", "c"].iter().map(|n| step(n, false)).collect();
        for s in &mut steps {
            s.update.prefetch = Some(vec!["fake-fetch".into(), s.update.name.clone()]);
        }
        let mut x = step("x", false);
        x.update.command = Some(vec!["/bin/app-install".into(), "x".into()]);
        x.update.independent = true;
        let mut y = x.clone();
        y.update.name = "y".into();
        y.update.command = Some(vec!["/bin/app-install".into(), "y".into()]);
        steps.extend([x, y]);
        let t = timed(&steps, 4);
        assert!(!overlap(&t, "fake-up a", "fake-up b") && !overlap(&t, "fake-up b", "fake-up c") && !overlap(&t, "fake-up a", "fake-up c"), "one brew-like tool at a time");
        assert!(overlap(&t, "fake-fetch", "fake-fetch"), "downloads run side by side");
        assert!(overlap(&t, "app-install x", "app-install y"), "independent steps run side by side");
        let order = t.order.lock().unwrap();
        let at = |k: &str| order.iter().position(|o| o == k).unwrap();
        for n in ["a", "b", "c"] {
            assert!(at(&format!("fake-fetch {n}")) < at(&format!("fake-up {n}")), "install waits for its download");
        }
    }

    #[test]
    fn serial_when_parallel_is_off() {
        let mut x = step("x", false);
        x.update.independent = true;
        let mut y = x.clone();
        y.update.name = "y".into();
        y.update.command = Some(vec!["fake-up".into(), "y".into()]);
        let t = std::sync::Arc::new(Timeline::default());
        let mut c = ctx(MockRunner::new(&[]));
        c.runner = t.clone();
        c.config.parallel = false;
        let (h, p) = hist();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake)];
        execute(&c, &providers, &[x, y], false, &Control::default(), &h, &|_| {});
        assert!(t.overlaps.lock().unwrap().is_empty());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn dependency_graph() {
        let reg = crate::providers::registry();
        let mk = |p: &str, n: &str| {
            let mut s = step(n, false);
            s.update.provider = p.into();
            s
        };
        let mut glib = mk("homebrew", "glib");
        glib.update.dependencies = vec!["pcre2".into()];
        let mut npm = mk("npm", "npm");
        npm.update.is_self_update = true;
        let steps = vec![mk("homebrew", "pcre2"), glib, mk("homebrew", "gh"), npm, mk("npm", "typescript"), mk("mas", "Xcode")];
        let d = dependencies(&reg, &steps);
        assert_eq!(d[1], vec![0], "glib waits for pcre2");
        assert!(d[2].is_empty(), "gh waits for nobody");
        assert_eq!(d[3], vec![0, 1, 2], "npm runs after homebrew");
        assert_eq!(d[4], vec![0, 1, 2, 3], "packages wait for the manager's self-update");
        assert!(d[5].is_empty());
        assert_eq!(tool(Some(&vec!["sudo".into(), "softwareupdate".into()])), "softwareupdate");
        assert_eq!(tool(Some(&vec!["/Library/x/msupdate".into()])), "msupdate");
    }

    #[test]
    fn job_limit_respects_settings() {
        let mut c = config::Config::default();
        let auto = job_limit(&c, 0);
        assert!((2..=MAX_AUTO_JOBS).contains(&auto));
        c.jobs = 3;
        assert_eq!(job_limit(&c, 0), 3);
        c.parallel = false;
        assert_eq!(job_limit(&c, 0), 1);
    }

    #[test]
    fn explains_sudo_failures() {
        let r = RunResult { status: 1, err: "sudo: a terminal is required to read the password".into(), ..Default::default() };
        assert!(explain(&r).starts_with("needs administrator password"));
    }
}
