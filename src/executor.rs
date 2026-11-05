//! Runs an Update Plan. Steps of one provider always run one after another; different
//! providers of the same stage may run in parallel. Every result is verified by re-reading
//! the installed version: an exit code 0 alone is never reported as "updated".

use crate::config;
use crate::history::{self, History, Record};
use crate::planner::Step;
use crate::providers::Provider;
use crate::shell::{CancelToken, Ctx, RunOpts, RunResult};
use crate::version;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl Control {
    /// Controlled stop: the running step finishes, nothing new starts.
    pub fn abort(&self) {
        self.abort.store(true, Ordering::SeqCst)
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
    }
    fn skipped(&self, i: usize) -> bool {
        self.skip.lock().unwrap().contains(&i)
    }
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
    let outcomes: Mutex<Vec<Outcome>> = Mutex::new(vec![Outcome::NotRun; steps.len()]);
    let mut stages: BTreeMap<usize, BTreeMap<&str, Vec<usize>>> = BTreeMap::new();
    for (i, s) in steps.iter().enumerate() {
        stages.entry(s.stage).or_default().entry(s.update.provider.as_str()).or_default().push(i);
    }
    let lane = |idx: &Vec<usize>| {
        for &i in idx {
            let o = run_step(ctx, providers, &steps[i], i, interactive, control, hist, events);
            outcomes.lock().unwrap()[i] = o;
        }
    };
    for (_, lanes) in stages {
        if control.aborted() {
            break;
        }
        if ctx.config.parallel && lanes.len() > 1 {
            std::thread::scope(|s| {
                for idx in lanes.values() {
                    s.spawn(move || lane(idx));
                }
            });
        } else {
            lanes.values().for_each(lane);
        }
    }
    outcomes.into_inner().unwrap()
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
    let env = if u.provider == "homebrew" { vec![("HOMEBREW_NO_AUTO_UPDATE", "1"), ("HOMEBREW_NO_ENV_HINTS", "1")] } else { vec![] };
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
    fn explains_sudo_failures() {
        let r = RunResult { status: 1, err: "sudo: a terminal is required to read the password".into(), ..Default::default() };
        assert!(explain(&r).starts_with("needs administrator password"));
    }
}
