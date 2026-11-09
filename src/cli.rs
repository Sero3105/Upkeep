//! Non-interactive commands and the line-based interactive `upkeep update`.

use crate::config::{self, Config};
use crate::engine::{self, ScanEvent};
use crate::executor::{self, Control, Event, Outcome};
use crate::health;
use crate::history::{self, History};
use crate::model::*;
use crate::planner::{self, Plan, Risk};
use crate::providers::{self, Provider};
use crate::shell::Ctx;
use crate::Args;
use std::io::{BufRead, IsTerminal, Write};

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, s: impl AsRef<str>) -> String {
    if color() { format!("\x1b[{code}m{}\x1b[0m", s.as_ref()) } else { s.as_ref().to_string() }
}
fn bold(s: impl AsRef<str>) -> String {
    paint("1", s)
}
fn dim(s: impl AsRef<str>) -> String {
    paint("2", s)
}
fn green(s: impl AsRef<str>) -> String {
    paint("32", s)
}
fn yellow(s: impl AsRef<str>) -> String {
    paint("33", s)
}
fn red(s: impl AsRef<str>) -> String {
    paint("31", s)
}
fn cyan(s: impl AsRef<str>) -> String {
    paint("36", s)
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n > w { s.chars().take(w.saturating_sub(1)).collect::<String>() + "…" } else { format!("{s}{}", " ".repeat(w - n)) }
}

fn risk_label(r: Risk) -> String {
    match r {
        Risk::Low => green("LOW "),
        Risk::Medium => yellow("MED "),
        Risk::High => red("HIGH"),
    }
}

/// Scan with a compact progress line on stderr.
pub fn scan(ctx: &Ctx, providers: &[Box<dyn Provider>], quiet: bool) -> Snapshot {
    let tty = std::io::stderr().is_terminal() && !quiet;
    let done = std::sync::Mutex::new(0usize);
    let total = providers.iter().filter(|p| ctx.config.enabled(p.id())).count();
    let progress = |e: ScanEvent| {
        let ScanEvent::Finished(st) = e;
        if tty {
            let mut d = done.lock().unwrap();
            *d += 1;
            eprint!("\r\x1b[2K{} {}/{} {}", cyan("◆ scanning"), *d, total, dim(&st.name));
        }
    };
    let snap = engine::scan(ctx, providers, &progress);
    if tty {
        eprint!("\r\x1b[2K");
    }
    engine::save(&snap);
    snap
}

fn in_scope(args: &Args, cat: Category, name: &str) -> bool {
    (args.categories.is_empty() || args.categories.contains(&cat)) && (args.only.is_empty() || args.only.iter().any(|o| o == name))
}

fn print_warnings(snap: &Snapshot) {
    for w in &snap.warnings {
        eprintln!("{} {w}", yellow("!"));
    }
}

pub fn check(cfg: Config, args: &Args) -> i32 {
    let ctx = Ctx::system(cfg);
    let snap = scan(&ctx, &providers::registry(), args.json);
    let ups: Vec<&Update> = snap.updates.iter().filter(|u| in_scope(args, u.category, &u.name)).collect();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&ups).unwrap());
        return 0;
    }
    print_updates(&ups);
    let auto = ups.iter().filter(|u| u.support == Support::Supported).count();
    println!(
        "\n{} {} updates · {} ready to install · {} by hand{}",
        bold("Σ"),
        ups.len(),
        green(auto.to_string()),
        yellow((ups.len() - auto).to_string()),
        if snap.excluded > 0 { dim(format!(" · {} excluded", snap.excluded)) } else { String::new() }
    );
    print_warnings(&snap);
    if auto > 0 {
        println!("{}", dim("Run `upkeep update` to choose and apply, or `upkeep plan` to preview."));
    }
    0
}

fn print_updates(ups: &[&Update]) {
    if ups.is_empty() {
        println!("{}", green("✓ Everything is up to date."));
        return;
    }
    let mut last = None;
    let mut last_hint = None;
    for (i, u) in ups.iter().enumerate() {
        if last != Some(u.category) {
            println!("\n{}", bold(u.category.title()));
            last = Some(u.category);
        }
        let (risk, _) = planner::assess(u);
        let kind = if u.support == Support::Supported { green("ready  ") } else { yellow("by hand") };
        println!(
            "  {:>3}  {} {} {} {}  {} {}{}",
            dim(format!("{}", i + 1)),
            pad(&u.name, 28),
            pad(&u.current, 14),
            dim("→"),
            pad(&u.latest, 14),
            kind,
            risk_label(risk),
            dim(format!("  {}{}", u.source, u.download_size.as_ref().map(|s| format!(" · {s}")).unwrap_or_default()))
        );
        // Same advice for a run of packages (e.g. PEP 668 pip): print it once.
        if u.support == Support::Manual && u.hint.is_some() && u.hint != last_hint {
            println!("       {}", dim(format!("↳ {}", u.hint.as_deref().unwrap_or(""))));
        }
        last_hint = u.hint.clone();
    }
}

pub fn list(cfg: Config, args: &Args) -> i32 {
    let ctx = Ctx::system(cfg);
    let snap = scan(&ctx, &providers::registry(), args.json);
    let items: Vec<&Installed> = snap.installed.iter().filter(|i| in_scope(args, i.category, &i.name)).collect();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
        return 0;
    }
    for cat in Category::ALL {
        let mut v: Vec<&&Installed> = items.iter().filter(|i| i.category == cat).collect();
        if v.is_empty() {
            continue;
        }
        v.sort_by_key(|i| i.name.to_lowercase());
        println!("\n{} {}", bold(cat.title()), dim(format!("({})", v.len())));
        for i in v {
            println!("  {} {} {}{}", pad(&i.name, 30), pad(&i.version, 16), dim(&i.source), i.note.as_ref().map(|n| dim(format!(" · {n}"))).unwrap_or_default());
        }
    }
    print_warnings(&snap);
    0
}

pub fn health(cfg: Config, args: &Args) -> i32 {
    let ctx = Ctx::system(cfg);
    let snap = scan(&ctx, &providers::registry(), args.json);
    let rep = health::check(&ctx, &snap);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rep).unwrap());
        return 0;
    }
    let s = rep.score;
    let bar = format!("{}{}", "█".repeat(s as usize / 5), "░".repeat(20 - s as usize / 5));
    let col = if s >= 75 { green(&bar) } else if s >= 50 { yellow(&bar) } else { red(&bar) };
    println!("\n{}  {col} {} {}", bold("System Health"), bold(format!("{s}/100")), dim(rep.label()));
    for sev in Severity::ALL {
        let v: Vec<&HealthIssue> = rep.issues.iter().filter(|i| i.severity == sev).collect();
        if v.is_empty() {
            continue;
        }
        let head = match sev {
            Severity::Critical => red("● Critical"),
            Severity::Warning => yellow("● Warning"),
            Severity::Info => cyan("● Info"),
            Severity::Healthy => green("● Healthy"),
        };
        println!("\n{head} {}", dim(format!("({})", v.len())));
        for i in v {
            println!("  {} {}", bold(&i.title), dim(format!("[{}]", i.component)));
            if sev != Severity::Healthy {
                println!("    {} {}", dim("why:"), i.reason);
                if !i.consequence.is_empty() {
                    println!("    {} {}", dim("risk:"), i.consequence);
                }
                if !i.fix.is_empty() {
                    println!("    {} {}", dim("fix:"), i.fix);
                }
            }
        }
    }
    println!("\n{}", dim("Upkeep never applies these fixes automatically."));
    0
}

pub fn history(cfg: Config, args: &Args) -> i32 {
    let h = History::open();
    let (recs, bad) = h.load();
    if bad > 0 {
        eprintln!("{} {bad} unreadable history lines skipped", yellow("!"));
    }
    let find = |id: &str| recs.iter().find(|r| r.id == id).cloned();
    if let Some(id) = &args.show {
        let Some(r) = find(id) else {
            eprintln!("no history record `{id}`");
            return 1;
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
            return 0;
        }
        println!("{} {} ({})", bold(&r.name), dim(&r.id), r.provider);
        println!("  date:      {}", config::fmt_time(r.date));
        println!("  operation: {} {} → {}", r.operation, r.from, r.to);
        println!("  command:   {}", r.command.join(" "));
        println!("  outcome:   {} — {}", r.outcome, r.detail.clone().unwrap_or_default());
        println!("  duration:  {:.1}s", r.duration);
        println!("  rollback:  {}", r.rollback.as_ref().map(|c| c.join(" ")).unwrap_or_else(|| "not supported".into()));
        let prev = history::previous_versions(&recs, &r.provider, &r.name);
        if !prev.is_empty() {
            println!("  previous versions: {}", prev.join(", "));
        }
        if !r.output_tail.is_empty() {
            println!("\n{}", dim("── output (tail) ──"));
            r.output_tail.iter().for_each(|l| println!("  {l}"));
        }
        return 0;
    }
    if let Some(id) = &args.rollback {
        let Some(r) = find(id) else {
            eprintln!("no history record `{id}`");
            return 1;
        };
        let Some(cmd) = &r.rollback else {
            eprintln!("{} {} has no supported rollback ({}).", red("✗"), r.name, r.provider);
            return 1;
        };
        println!("Rollback {} {} → {}\n  command: {}", bold(&r.name), r.to, r.from, cmd.join(" "));
        if !confirm("Proceed? [y/N] ") {
            println!("Cancelled.");
            return 0;
        }
        let ctx = Ctx::system(cfg);
        let out = |l: &str| println!("  {}", dim(l));
        return match executor::rollback(&ctx, &providers::registry(), &r, true, &h, &out) {
            Ok(n) => {
                println!("{} rollback {}: {}", if n.outcome == "success" { green("✓") } else { yellow("≈") }, n.outcome, n.detail.unwrap_or_default());
                i32::from(n.outcome == "failed")
            }
            Err(e) => {
                eprintln!("{} {e}", red("✗"));
                1
            }
        };
    }
    let f = history::Filter { category: args.categories.first().copied(), search: args.search.clone(), failed_only: args.failed };
    let rows = history::filter(&recs, &f);
    let rows: Vec<_> = if args.all { rows } else { rows.into_iter().take(args.limit).collect() };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return 0;
    }
    if rows.is_empty() {
        println!("No history yet.");
        return 0;
    }
    for r in rows {
        let o = match r.outcome.as_str() {
            "success" => green("✓ success  "),
            "failed" => red("✗ failed   "),
            "unverified" => yellow("≈ unverified"),
            "already-current" => dim("= current  "),
            _ => dim(format!("· {:<9}", r.outcome)),
        };
        println!(
            "{} {} {} {} {} {}{}",
            dim(config::fmt_time(r.date)),
            pad(&r.name, 24),
            pad(&format!("{} → {}", r.from, r.to), 26),
            o,
            dim(pad(&r.provider, 10)),
            dim(&r.id),
            if r.operation == "rollback" { cyan(" ↺") } else { String::new() }
        );
    }
    println!("{}", dim("Details: upkeep history --show ID · rollback: upkeep history --rollback ID"));
    0
}

fn print_plan(plan: &Plan) {
    if plan.steps.is_empty() && plan.manual.is_empty() {
        println!("{}", green("Nothing to do."));
        return;
    }
    println!(
        "\n{}  {} steps · {} stages · {} high risk · {} need admin · {} with rollback",
        bold("Update Plan"),
        plan.steps.len(),
        plan.stages(),
        plan.high_risk(),
        plan.admin(),
        plan.with_rollback()
    );
    let mut cur = None;
    for (i, s) in plan.steps.iter().enumerate() {
        if cur != Some((s.stage, s.update.provider.clone())) {
            println!("\n  {} {}", cyan(format!("Stage {} ·", s.stage + 1)), bold(&s.update.provider));
            cur = Some((s.stage, s.update.provider.clone()));
        }
        let u = &s.update;
        println!(
            "  {:>3}. {} {} {} → {} {}{}",
            i + 1,
            risk_label(s.risk),
            pad(&u.name, 26),
            pad(&u.current, 12),
            pad(&u.latest, 12),
            if u.rollback.is_some() { dim("↺ rollback") } else { dim("✗ no rollback") },
            if u.requires_admin { red(" ⚠ admin") } else { String::new() }
        );
        let mut why = s.reasons.join("; ");
        if let Some(by) = &s.linked_by {
            why = format!("dependency of {by}; {why}");
        }
        if !u.notes.is_empty() {
            why = format!("{why}; {}", u.notes.join("; "));
        }
        println!("        {}", dim(why));
    }
    if !plan.manual.is_empty() {
        println!("\n  {}", yellow("Manual (Upkeep will not run these)"));
        for u in &plan.manual {
            println!("   • {} {} → {}  {}", u.name, u.current, u.latest, dim(u.hint.clone().unwrap_or_default()));
        }
    }
    for w in &plan.warnings {
        println!("  {} {w}", yellow("!"));
    }
}

pub fn plan(cfg: Config, args: &Args) -> i32 {
    let ctx = Ctx::system(cfg);
    let reg = providers::registry();
    let snap = scan(&ctx, &reg, args.json);
    let sel: Vec<Update> = snap.updates.iter().filter(|u| in_scope(args, u.category, &u.name)).cloned().collect();
    let plan = planner::build(&sel, &snap.updates, &reg);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
    } else {
        print_plan(&plan);
        println!("\n{}", dim("Preview only: nothing was changed. Use `upkeep update` to execute."));
    }
    0
}

fn prompt(q: &str) -> String {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

fn confirm(q: &str) -> bool {
    matches!(prompt(q).to_lowercase().as_str(), "y" | "yes" | "s" | "si" | "sì")
}

/// "1,3-5" / "all" -> indexes (0-based). Unknown tokens are an error, not ignored.
pub fn parse_selection(s: &str, n: usize) -> Result<Vec<usize>, String> {
    if s.eq_ignore_ascii_case("all") || s == "*" {
        return Ok((0..n).collect());
    }
    let mut out = vec![];
    for tok in s.split([',', ' ']).filter(|t| !t.is_empty()) {
        let (a, b) = tok.split_once('-').unwrap_or((tok, tok));
        let unclear = || format!("I didn't understand `{tok}`: use numbers like 1,3-5, or `all`.");
        let (a, b): (usize, usize) = (a.parse().map_err(|_| unclear())?, b.parse().map_err(|_| unclear())?);
        if a == 0 || b > n || a > b {
            return Err(format!("`{tok}` is not in the list: use numbers from 1 to {n}."));
        }
        out.extend(a - 1..b);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

extern "C" fn on_sigint(_: libc::c_int) {
    executor::INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn update(cfg: Config, args: &Args) -> i32 {
    if !std::io::stdin().is_terminal() {
        eprintln!("upkeep update needs an interactive terminal: every update requires explicit confirmation.");
        return 2;
    }
    let ctx = Ctx::system(cfg);
    let reg = providers::registry();
    let snap = scan(&ctx, &reg, false);
    let ups: Vec<&Update> = snap.updates.iter().filter(|u| in_scope(args, u.category, &u.name)).collect();
    print_updates(&ups);
    print_warnings(&snap);
    if ups.is_empty() {
        return 0;
    }
    let sel = loop {
        println!("\n{}", bold("Which updates do you want?"));
        println!("{}", dim("Type the numbers from the list (like 1,3-5), or `all`. Press Enter alone to cancel."));
        let s = prompt("> ");
        if s.is_empty() {
            println!("Cancelled: nothing changed.");
            return 0;
        }
        match parse_selection(&s, ups.len()) {
            Ok(v) => break v,
            Err(e) => eprintln!("{} {e}", red("✗")),
        }
    };
    let chosen: Vec<Update> = sel.iter().map(|&i| ups[i].clone()).collect();
    let plan = planner::build(&chosen, &snap.updates, &reg);
    print_plan(&plan);
    if plan.steps.is_empty() {
        return 0;
    }
    let mut include_high = false;
    if plan.high_risk() > 0 {
        println!("\n{} {} high-risk step(s):", red("⚠"), plan.high_risk());
        for s in plan.steps.iter().filter(|s| s.risk == Risk::High) {
            println!("   • {} — {}", bold(&s.update.name), s.reasons.join("; "));
        }
        include_high = prompt("Type `yes` to install them too (anything else leaves them out): ") == "yes";
        if !include_high {
            println!("{}", dim("High-risk updates left out."));
        }
    }
    let steps = plan.executable(include_high);
    if steps.is_empty() {
        println!("Nothing left to run.");
        return 0;
    }
    if !confirm(&format!("\nRun {} update(s) now? [y/N] ", steps.len())) {
        println!("Cancelled: nothing changed.");
        return 0;
    }
    let out = run_in_terminal(&ctx, &reg, &steps);
    report(&steps, &out)
}

/// Run steps in the plain terminal: live output, `sudo` asks for your password the normal way,
/// Ctrl-C stops after the current step.
pub fn run_in_terminal(ctx: &Ctx, reg: &[Box<dyn Provider>], steps: &[planner::Step]) -> Vec<Outcome> {
    executor::INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
    if steps.iter().any(|s| s.update.requires_admin) {
        println!("{}", yellow("Some steps use sudo: type your password when asked."));
    }
    println!("{}", dim("Ctrl-C stops after the current step."));
    let names: Vec<String> = steps.iter().map(|s| s.update.name.clone()).collect();
    let n = steps.len();
    let events = |e: Event| match e {
        Event::Started(i) => println!("\n{} [{}/{n}] {}", cyan("▶"), i + 1, bold(&names[i])),
        Event::Line(_, l) => println!("    {}", dim(l)),
        Event::Finished(i, o) => println!("  {} {} — {}", icon(&o), names[i], o.detail()),
    };
    let out = executor::execute(ctx, reg, steps, true, &Control::default(), &History::open(), &events);
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    out
}

fn icon(o: &Outcome) -> String {
    match o {
        Outcome::Success(_) => green("✓"),
        Outcome::Unverified(_) => yellow("≈"),
        Outcome::AlreadyCurrent(_) => dim("="),
        Outcome::Failed(_) => red("✗"),
        Outcome::Skipped(_) => yellow("↷"),
        Outcome::NotRun => dim("·"),
    }
}

pub fn report(steps: &[planner::Step], out: &[Outcome]) -> i32 {
    let names: Vec<String> = steps.iter().map(|s| s.update.name.clone()).collect();
    let c = |k: &str| out.iter().filter(|o| o.key() == k).count();
    println!(
        "\n{}  {} updated · {} unverified · {} already current · {} failed · {} skipped · {} not run",
        bold("Report"),
        green(c("success").to_string()),
        yellow(c("unverified").to_string()),
        c("already-current"),
        red(c("failed").to_string()),
        c("skipped"),
        c("not-run")
    );
    for (n, o) in names.iter().zip(out).filter(|(_, o)| !matches!(o, Outcome::Success(_) | Outcome::AlreadyCurrent(_))) {
        println!("  {} {n}: {}", icon(o), o.detail());
    }
    if steps.iter().zip(out).any(|(s, o)| s.update.requires_restart && matches!(o, Outcome::Success(_) | Outcome::Unverified(_))) {
        println!("{}", yellow("⟳ Restart the Mac to finish installing the system update(s). Upkeep never restarts it for you."));
    }
    println!("{}", dim("Saved to history: `upkeep history`."));
    i32::from(c("failed") > 0)
}

pub fn doctor(cfg: Config) -> i32 {
    let ok = |b: bool| if b { green("✓") } else { red("✗") };
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let ctx = Ctx::system(cfg.clone());
    let on_path = ctx.runner.which("upkeep");
    let same = on_path.as_ref().and_then(|p| std::fs::canonicalize(p).ok()) == std::fs::canonicalize(&exe).ok();
    let root = config::root();
    let writable = std::fs::create_dir_all(&root).is_ok() && providers::writable(&root.to_string_lossy());
    let (recs, bad) = History::open().load();
    let mut fails = 0;
    let mut line = |good: bool, what: &str, detail: String| {
        fails += i32::from(!good);
        println!("  {} {} {}", ok(good), pad(what, 22), dim(detail));
    };
    println!("{} {}\n", bold("upkeep doctor"), dim(format!("v{}", crate::model::VERSION)));
    line(true, "binary", exe.clone());
    line(std::env::consts::ARCH == "aarch64", "architecture", format!("{} (native Apple Silicon build: {})", std::env::consts::ARCH, std::env::consts::ARCH == "aarch64"));
    line(same, "`upkeep` on PATH", on_path.unwrap_or_else(|| "not found — add the install dir to PATH".into()));
    line(writable, "data directory", root.display().to_string());
    line(Config::load().is_ok(), "config", format!("{} (valid JSON)", config::path(config::CONFIG).display()));
    line(bad == 0, "history", format!("{} records, {bad} unreadable lines", recs.len()));
    line(std::io::stdout().is_terminal(), "terminal", format!("TERM={}", std::env::var("TERM").unwrap_or_default()));
    let src = std::fs::read_to_string(config::path(config::SOURCE)).unwrap_or_default();
    line(!src.trim().is_empty(), "self-update source", if src.trim().is_empty() { "not recorded (install with install.sh)".into() } else { format!("{} (auto-update {})", src.trim(), if cfg.auto_self_update { "on" } else { "off" }) });
    println!("\n{}", bold("Providers"));
    for p in providers::registry() {
        let en = cfg.enabled(p.id());
        let av = p.available(&ctx);
        let state = if !en { dim("disabled") } else if av { green("available") } else { dim("not installed") };
        println!("  {} {} {} {state}", if av && en { green("●") } else { dim("○") }, pad(p.name(), 24), dim(pad(p.category().title(), 14)));
    }
    if fails == 0 { 0 } else { 1 }
}

pub fn self_update() -> i32 {
    let src = std::fs::read_to_string(config::path(config::SOURCE)).unwrap_or_default();
    let src = src.trim();
    if src.is_empty() || !std::path::Path::new(src).join("Cargo.toml").exists() {
        eprintln!("Source directory unknown. Re-run ./install.sh from the Upkeep repository.");
        return 1;
    }
    let git = std::path::Path::new(src).join(".git").exists();
    println!("Self-update from {src}:\n  {}  cargo build --release\n  install over {}", if git { "git pull --ff-only" } else { "(no git: rebuild local sources)" }, std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    if !confirm("Proceed? [y/N] ") {
        return 0;
    }
    let run = |args: &[&str]| std::process::Command::new(args[0]).args(&args[1..]).current_dir(src).status().is_ok_and(|s| s.success());
    if git && !run(&["git", "pull", "--ff-only"]) {
        eprintln!("{} git pull failed; nothing installed.", red("✗"));
        return 1;
    }
    if !run(&["./install.sh"]) {
        eprintln!("{} build/install failed; previous version kept.", red("✗"));
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_parsing() {
        assert_eq!(parse_selection("1,3-5", 6).unwrap(), vec![0, 2, 3, 4]);
        assert_eq!(parse_selection("all", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("2 2", 3).unwrap(), vec![1]);
        assert!(parse_selection("0", 3).is_err());
        assert!(parse_selection("4", 3).is_err());
        assert!(parse_selection("x", 3).is_err());
    }
}
