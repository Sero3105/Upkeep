mod appinstall;
mod cli;
mod config;
mod engine;
mod executor;
mod health;
mod history;
mod model;
mod planner;
mod providers;
mod selfupdate;
mod shell;
mod tui;
mod version;

use model::Category;

pub struct Args {
    pub cmd: String,
    pub categories: Vec<Category>,
    pub only: Vec<String>,
    pub json: bool,
    pub all: bool,
    pub failed: bool,
    pub search: String,
    pub show: Option<String>,
    pub rollback: Option<String>,
    pub limit: usize,
    pub no_refresh: bool,
}

const HELP: &str = "\
upkeep — universal software update & system health manager for macOS

USAGE
  upkeep [COMMAND] [OPTIONS]

COMMANDS
  (none)        Open the interactive TUI
  check         Scan the system and list available updates
  update        Interactive update: choose, review the plan, confirm, run
  plan          Build an update plan without running it
  list          List installed software and versions
  health        Run System Health diagnostics
  history       Show update history (also: --show ID, --rollback ID)
  doctor        Check Upkeep's own installation
  self-update   Rebuild and reinstall Upkeep from its source directory

OPTIONS
  -c, --category LIST   Only these categories (comma separated):
                        apps, homebrew, runtimes, packages, tools, docker, system
  -o, --only LIST       Only these package names (update / plan)
      --json            Machine-readable output (check, list, health, plan, history)
      --no-refresh      Skip `brew update` index refresh for this run
      --failed          history: only failed operations
  -s, --search TEXT     history: filter by name
  -n, --limit N         history: number of records (default 20)
      --all             history: everything
  -h, --help            Show this help
  -V, --version         Show version

Safety: nothing is installed without explicit confirmation; sudo runs only after you
confirm, with your password typed at the normal prompt; apps are replaced only by builds
signed by the same developer; projects, virtualenvs and Docker data are never touched.
Data: ~/.config/upkeep (override with UPKEEP_HOME).";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        cmd: String::new(),
        categories: vec![],
        only: vec![],
        json: false,
        all: false,
        failed: false,
        search: String::new(),
        show: None,
        rollback: None,
        limit: 20,
        no_refresh: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" | "help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            "-c" | "--category" => {
                for c in val("--category")?.split(',') {
                    a.categories.push(Category::parse(c.trim()).ok_or(format!("unknown category `{c}`"))?);
                }
            }
            "-o" | "--only" => a.only.extend(val("--only")?.split(',').map(|s| s.trim().to_string())),
            "-s" | "--search" => a.search = val("--search")?,
            "-n" | "--limit" => a.limit = val("--limit")?.parse().map_err(|_| "--limit needs a number")?,
            "--show" => a.show = Some(val("--show")?),
            "--rollback" => a.rollback = Some(val("--rollback")?),
            "--json" => a.json = true,
            "--all" => a.all = true,
            "--failed" => a.failed = true,
            "--no-refresh" => a.no_refresh = true,
            s if s.starts_with('-') => return Err(format!("unknown option `{s}` (see --help)")),
            s if a.cmd.is_empty() => a.cmd = s.to_string(),
            s => return Err(format!("unexpected argument `{s}`")),
        }
    }
    Ok(a)
}

fn main() {
    // Internal steps run by the executor (not part of the public CLI).
    let raw: Vec<String> = std::env::args().collect();
    if let Some(cmd) = raw.get(1).filter(|c| c.starts_with("__")) {
        let r = match (cmd.as_str(), raw.get(2), raw.get(3)) {
            ("__app-install", Some(app), Some(src)) => appinstall::install(app, src, raw.get(4).is_some_and(|f| f == "--check")),
            ("__app-restore", Some(backup), Some(app)) => appinstall::restore(backup, app),
            _ => Err(format!("bad internal command {cmd}")),
        };
        if let Err(e) = r {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("upkeep: {e}");
            std::process::exit(2);
        }
    };
    if args.cmd == "help" {
        println!("{HELP}");
        return;
    }
    if args.cmd == "version" {
        println!("upkeep {}", model::VERSION);
        return;
    }
    let mut cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) if args.cmd == "doctor" => {
            eprintln!("warning: config unreadable ({e}), using defaults");
            config::Config::default()
        }
        Err(e) => {
            eprintln!("upkeep: invalid config: {e}\nFix or delete it (`upkeep doctor` shows the path).");
            std::process::exit(1);
        }
    };
    if args.no_refresh {
        cfg.refresh_indexes = false;
    }
    if !matches!(args.cmd.as_str(), "doctor" | "self-update") {
        selfupdate::auto(&cfg);
    }
    let code = match args.cmd.as_str() {
        "" => tui::run(cfg),
        "check" => cli::check(cfg, &args),
        "list" => cli::list(cfg, &args),
        "health" => cli::health(cfg, &args),
        "history" => cli::history(cfg, &args),
        "plan" => cli::plan(cfg, &args),
        "update" => cli::update(cfg, &args),
        "doctor" => cli::doctor(cfg),
        "self-update" => cli::self_update(),
        other => {
            eprintln!("upkeep: unknown command `{other}` (see --help)");
            2
        }
    };
    std::process::exit(code);
}
