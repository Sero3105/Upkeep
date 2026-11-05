//! Interactive TUI. State + input handling here, drawing in `view`.
//! Scans, health checks and updates run on background threads and report through a channel,
//! so the interface never blocks.

mod view;

use crate::config::{self, Config};
use crate::engine::{self, ScanEvent};
use crate::executor::{self, Control, Event as ExecEvent, Outcome};
use crate::health::{self, Report};
use crate::history::{History, Record};
use crate::model::*;
use crate::planner::{self, Plan, Risk, Step};
use crate::providers::{self, Provider};
use crate::shell::Ctx;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Updates,
    Health,
    History,
    Smart,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 6] = [Tab::Dashboard, Tab::Updates, Tab::Health, Tab::History, Tab::Smart, Tab::Settings];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Updates => "Updates",
            Tab::Health => "System Health",
            Tab::History => "History",
            Tab::Smart => "Smart Update",
            Tab::Settings => "Settings",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Category,
    Name,
    Risk,
    Kind,
}

impl Sort {
    fn next(self) -> Sort {
        match self {
            Sort::Category => Sort::Name,
            Sort::Name => Sort::Risk,
            Sort::Risk => Sort::Kind,
            Sort::Kind => Sort::Category,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Sort::Category => "category",
            Sort::Name => "name",
            Sort::Risk => "risk",
            Sort::Kind => "auto/manual",
        }
    }
}

pub enum Modal {
    Execute,
    IncludeHigh,
    Abort,
    Exclude(String),
    Rollback(Box<Record>),
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputTarget {
    Updates,
    History,
}

pub struct Exec {
    pub steps: Vec<Step>,
    pub outcomes: Vec<Option<Outcome>>,
    pub running: HashSet<usize>,
    pub log: VecDeque<String>,
    pub control: Arc<Control>,
    pub done: bool,
    pub started: Instant,
    pub elapsed: Duration,
    pub state: ListState,
}

enum Msg {
    ScanFinished(ProviderStatus),
    ScanDone(Box<Snapshot>),
    Health(Box<Report>),
    Exec(ExecEvent),
    ExecDone(Vec<Outcome>),
    Rollback(Result<Record, String>),
}

pub struct App {
    pub ctx: Ctx,
    pub reg: Arc<Vec<Box<dyn Provider>>>,
    pub tab: Tab,
    pub snap: Snapshot,
    pub scanning: bool,
    pub live: HashMap<String, Option<ProviderStatus>>,
    pub health: Option<Report>,
    pub health_busy: bool,
    // Updates
    pub sel: HashSet<String>,
    pub ustate: TableState,
    pub ucat: Option<Category>,
    pub usearch: String,
    pub usort: Sort,
    // Health
    pub hstate: TableState,
    pub hsev: Option<Severity>,
    // History
    pub hist: Vec<Record>,
    pub rstate: TableState,
    pub rsearch: String,
    pub rfailed: bool,
    pub rcat: Option<Category>,
    pub detail: Option<Record>,
    pub dscroll: u16,
    // Smart update
    pub plan: Option<Plan>,
    pub include_high: bool,
    pub pstate: TableState,
    pub exec: Option<Exec>,
    pub rollback_busy: bool,
    // Settings
    pub sstate: ListState,
    // Overlays
    pub input: Option<InputTarget>,
    pub modal: Option<Modal>,
    pub help: bool,
    pub status: Option<(String, Instant)>,
    pub tick: usize,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    quit: bool,
}

pub fn run(cfg: Config) -> i32 {
    if !std::io::stdout().is_terminal() {
        eprintln!("upkeep: the TUI needs a terminal. Use `upkeep check` / `upkeep --help` for scripts.");
        return 2;
    }
    let mut app = App::new(cfg);
    app.start_scan();
    let mut term = ratatui::init();
    let mut result = 0;
    while !app.quit {
        if let Err(e) = term.draw(|f| view::draw(f, &mut app)) {
            result = 1;
            config::log(&format!("draw error: {e}"));
            break;
        }
        app.drain();
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => app.key(k),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        app.tick = app.tick.wrapping_add(1);
    }
    ratatui::restore();
    if let Some(e) = &app.exec {
        // Quit while running: the current step was allowed to finish, nothing new started.
        e.control.abort();
    }
    result
}

impl App {
    fn new(cfg: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        let hist = History::open().load().0;
        let snap = engine::load_cached().unwrap_or_default();
        App {
            ctx: Ctx::system(cfg),
            reg: Arc::new(providers::registry()),
            tab: Tab::Dashboard,
            snap,
            scanning: false,
            live: HashMap::new(),
            health: None,
            health_busy: false,
            sel: HashSet::new(),
            ustate: TableState::default().with_selected(Some(0)),
            ucat: None,
            usearch: String::new(),
            usort: Sort::Category,
            hstate: TableState::default().with_selected(Some(0)),
            hsev: None,
            hist,
            rstate: TableState::default().with_selected(Some(0)),
            rsearch: String::new(),
            rfailed: false,
            rcat: None,
            detail: None,
            dscroll: 0,
            plan: None,
            include_high: false,
            pstate: TableState::default().with_selected(Some(0)),
            exec: None,
            rollback_busy: false,
            sstate: ListState::default().with_selected(Some(0)),
            input: None,
            modal: None,
            help: false,
            status: None,
            tick: 0,
            tx,
            rx,
            quit: false,
        }
    }

    pub fn flash(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    pub fn busy(&self) -> bool {
        self.exec.as_ref().is_some_and(|e| !e.done) || self.rollback_busy
    }

    fn start_scan(&mut self) {
        if self.scanning || self.busy() {
            return;
        }
        self.scanning = true;
        self.live = self.reg.iter().filter(|p| self.ctx.config.enabled(p.id())).map(|p| (p.id().to_string(), None)).collect();
        let (ctx, reg, tx) = (self.ctx.clone(), self.reg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let progress = |e: ScanEvent| {
                let ScanEvent::Finished(st) = e;
                let _ = tx.send(Msg::ScanFinished(st.clone()));
            };
            let snap = engine::scan(&ctx, &reg, &progress);
            engine::save(&snap);
            let _ = tx.send(Msg::ScanDone(Box::new(snap)));
        });
    }

    fn start_health(&mut self) {
        self.health_busy = true;
        let (ctx, snap, tx) = (self.ctx.clone(), self.snap.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Health(Box::new(health::check(&ctx, &snap))));
        });
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::ScanFinished(st) => {
                    self.live.insert(st.id.clone(), Some(st));
                }
                Msg::ScanDone(s) => {
                    self.snap = *s;
                    self.scanning = false;
                    let ids: HashSet<String> = self.snap.updates.iter().map(Update::id).collect();
                    self.sel.retain(|id| ids.contains(id));
                    self.clamp_updates();
                    self.start_health();
                }
                Msg::Health(r) => {
                    self.health = Some(*r);
                    self.health_busy = false;
                }
                Msg::Exec(e) => {
                    let Some(x) = self.exec.as_mut() else { continue };
                    match e {
                        ExecEvent::Started(i) => {
                            x.running.insert(i);
                            x.log.push_back(format!("▶ {}", x.steps[i].update.name));
                        }
                        ExecEvent::Line(i, l) => x.log.push_back(format!("{}: {l}", x.steps[i].update.name)),
                        ExecEvent::Finished(i, o) => {
                            x.running.remove(&i);
                            x.log.push_back(format!("■ {}: {}", x.steps[i].update.name, o.detail()));
                            x.outcomes[i] = Some(o);
                        }
                    }
                    while x.log.len() > 500 {
                        x.log.pop_front();
                    }
                }
                Msg::ExecDone(out) => {
                    if let Some(x) = self.exec.as_mut() {
                        for (i, o) in out.into_iter().enumerate() {
                            x.outcomes[i].get_or_insert(o);
                        }
                        x.done = true;
                        x.elapsed = x.started.elapsed();
                    }
                    self.hist = History::open().load().0;
                    self.flash("Updates finished: report ready. Enter = rescan.");
                }
                Msg::Rollback(r) => {
                    self.rollback_busy = false;
                    self.hist = History::open().load().0;
                    match r {
                        Ok(rec) => self.flash(format!("Rollback {}: {} ({})", rec.name, rec.outcome, rec.detail.unwrap_or_default())),
                        Err(e) => self.flash(format!("Rollback failed: {e}")),
                    }
                }
            }
        }
    }

    // ---------- derived views ----------

    pub fn visible_updates(&self) -> Vec<&Update> {
        let q = self.usearch.to_lowercase();
        let mut v: Vec<&Update> = self
            .snap
            .updates
            .iter()
            .filter(|u| self.ucat.is_none_or(|c| u.category == c))
            .filter(|u| q.is_empty() || u.name.to_lowercase().contains(&q) || u.source.to_lowercase().contains(&q))
            .collect();
        match self.usort {
            Sort::Category => {}
            Sort::Name => v.sort_by_key(|u| u.name.to_lowercase()),
            Sort::Risk => v.sort_by_key(|u| std::cmp::Reverse(planner::assess(u).0)),
            Sort::Kind => v.sort_by_key(|u| u.support != Support::Supported),
        }
        v
    }

    pub fn visible_issues(&self) -> Vec<&HealthIssue> {
        self.health.as_ref().map(|h| h.issues.iter().filter(|i| self.hsev.is_none_or(|s| i.severity == s)).collect()).unwrap_or_default()
    }

    pub fn visible_history(&self) -> Vec<&Record> {
        let f = crate::history::Filter { category: self.rcat, search: self.rsearch.clone(), failed_only: self.rfailed };
        crate::history::filter(&self.hist, &f)
    }

    /// Settings rows: (label, value, description).
    pub fn settings_rows(&self) -> Vec<(String, bool, String)> {
        let c = &self.ctx.config;
        let mut v = vec![
            ("Parallel execution".to_string(), c.parallel, "Run different package managers at once (never two ops on one manager)".to_string()),
            ("Refresh indexes".to_string(), c.refresh_indexes, "`brew update` before checking (metadata only)".to_string()),
            ("Check Sparkle appcasts".to_string(), c.check_appcasts, "Read vendor update feeds of self-updating apps (HTTPS)".to_string()),
        ];
        for p in self.reg.iter() {
            v.push((format!("Provider: {}", p.name()), c.enabled(p.id()), format!("id `{}`", p.id())));
        }
        for e in &c.excluded {
            v.push((format!("Excluded: {e}"), true, "Space = include again".to_string()));
        }
        v
    }

    fn clamp_updates(&mut self) {
        let n = self.visible_updates().len();
        let i = self.ustate.selected().unwrap_or(0).min(n.saturating_sub(1));
        self.ustate.select(Some(i));
    }

    fn current_update(&self) -> Option<Update> {
        self.visible_updates().get(self.ustate.selected()?).map(|u| (*u).clone())
    }

    // ---------- input ----------

    fn key(&mut self, k: KeyEvent) {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if self.modal.is_some() {
            return self.modal_key(k, ctrl_c);
        }
        if let Some(t) = self.input {
            let buf = match t {
                InputTarget::Updates => &mut self.usearch,
                InputTarget::History => &mut self.rsearch,
            };
            match k.code {
                KeyCode::Enter => self.input = None,
                KeyCode::Esc => {
                    buf.clear();
                    self.input = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !ctrl_c => buf.push(c),
                _ => {}
            }
            self.clamp_updates();
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.detail.is_some() {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => self.dscroll = self.dscroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.dscroll += 1,
                KeyCode::PageDown => self.dscroll += 10,
                KeyCode::PageUp => self.dscroll = self.dscroll.saturating_sub(10),
                KeyCode::Char('R') => self.ask_rollback(),
                _ => self.detail = None,
            }
            return;
        }
        match k.code {
            _ if ctrl_c => self.ask_quit(),
            KeyCode::Char('q') => self.ask_quit(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(c @ '1'..='6') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab_index() + 1) % 6],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab_index() + 5) % 6],
            KeyCode::Char('r') if self.tab != Tab::Smart || self.exec.is_none() => {
                if self.busy() {
                    self.flash("Busy: wait for the running operation to finish.");
                } else {
                    self.start_scan();
                    self.flash("Rescanning…");
                }
            }
            _ => match self.tab {
                Tab::Dashboard => {
                    if k.code == KeyCode::Enter {
                        self.tab = Tab::Updates;
                    }
                }
                Tab::Updates => self.updates_key(k),
                Tab::Health => self.health_key(k),
                Tab::History => self.history_key(k),
                Tab::Smart => self.smart_key(k),
                Tab::Settings => self.settings_key(k),
            },
        }
    }

    fn tab_index(&self) -> usize {
        Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0)
    }

    fn ask_quit(&mut self) {
        if self.busy() {
            self.modal = Some(Modal::Quit);
        } else {
            self.quit = true;
        }
    }

    fn modal_key(&mut self, k: KeyEvent, ctrl_c: bool) {
        let yes = matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let no = ctrl_c || matches!(k.code, KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q'));
        if !yes && !no {
            return;
        }
        let m = self.modal.take().unwrap();
        if no {
            return;
        }
        match m {
            Modal::Execute => self.start_exec(),
            Modal::IncludeHigh => {
                self.include_high = true;
                self.flash("High-risk steps included.");
            }
            Modal::Abort => {
                if let Some(x) = &self.exec {
                    x.control.abort();
                    self.flash("Stopping after the current step…");
                }
            }
            Modal::Exclude(id) => {
                self.ctx.config.excluded.push(id.clone());
                self.save_config();
                self.snap.updates.retain(|u| u.id() != id);
                self.snap.excluded += 1;
                self.sel.remove(&id);
                self.clamp_updates();
                self.flash(format!("Excluded {id}. Undo in Settings."));
            }
            Modal::Rollback(rec) => self.start_rollback(*rec),
            Modal::Quit => {
                if let Some(x) = &self.exec {
                    x.control.abort();
                }
                self.quit = true;
            }
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.ctx.config.save() {
            self.flash(format!("Could not save settings: {e}"));
        }
    }

    fn nav(state: &mut TableState, k: KeyCode, n: usize) -> bool {
        let i = state.selected().unwrap_or(0);
        let last = n.saturating_sub(1);
        let j = match k {
            KeyCode::Up | KeyCode::Char('k') => i.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (i + 1).min(last),
            KeyCode::PageUp => i.saturating_sub(10),
            KeyCode::PageDown => (i + 10).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return false,
        };
        state.select(Some(j));
        true
    }

    fn updates_key(&mut self, k: KeyEvent) {
        let n = self.visible_updates().len();
        if Self::nav(&mut self.ustate, k.code, n) {
            return;
        }
        match k.code {
            KeyCode::Char(' ') => {
                let Some(u) = self.current_update() else { return };
                if u.support != Support::Supported {
                    self.flash(format!("Manual update: {}", u.hint.unwrap_or_default()));
                } else if !self.sel.remove(&u.id()) {
                    self.sel.insert(u.id());
                }
                Self::nav(&mut self.ustate, KeyCode::Down, n);
            }
            KeyCode::Char('a') => {
                // "Compatible" = Upkeep can run it here without a password and it is not high-risk.
                let ids: Vec<String> = self
                    .visible_updates()
                    .iter()
                    .filter(|u| u.support == Support::Supported && !u.requires_admin && planner::assess(u).0 != Risk::High)
                    .map(|u| u.id())
                    .collect();
                let n = ids.len();
                self.sel.extend(ids);
                self.flash(format!("Selected {n} compatible updates (high-risk and admin ones must be picked by hand)."));
            }
            KeyCode::Char('n') => self.sel.clear(),
            KeyCode::Char('/') => self.input = Some(InputTarget::Updates),
            KeyCode::Char('c') => {
                self.ucat = cycle_cat(self.ucat);
                self.clamp_updates();
            }
            KeyCode::Char('o') => self.usort = self.usort.next(),
            KeyCode::Char('x') => {
                if let Some(u) = self.current_update() {
                    self.modal = Some(Modal::Exclude(u.id()));
                }
            }
            KeyCode::Enter | KeyCode::Char('p') => self.make_plan(),
            KeyCode::Esc => {
                self.usearch.clear();
                self.ucat = None;
            }
            _ => {}
        }
    }

    fn make_plan(&mut self) {
        if self.busy() {
            self.flash("An update run is in progress.");
            return;
        }
        let mut chosen: Vec<Update> = self.snap.updates.iter().filter(|u| self.sel.contains(&u.id())).cloned().collect();
        if chosen.is_empty() {
            match self.current_update() {
                Some(u) => chosen.push(u),
                None => return,
            }
        }
        self.plan = Some(planner::build(&chosen, &self.snap.updates, &self.reg));
        self.include_high = false;
        self.exec = None;
        self.pstate.select(Some(0));
        self.tab = Tab::Smart;
    }

    fn health_key(&mut self, k: KeyEvent) {
        let n = self.visible_issues().len();
        if Self::nav(&mut self.hstate, k.code, n) {
            return;
        }
        if k.code == KeyCode::Char('f') {
            self.hsev = match self.hsev {
                None => Some(Severity::Critical),
                Some(Severity::Critical) => Some(Severity::Warning),
                Some(Severity::Warning) => Some(Severity::Info),
                Some(Severity::Info) => Some(Severity::Healthy),
                Some(Severity::Healthy) => None,
            };
            self.hstate.select(Some(0));
        }
    }

    fn history_key(&mut self, k: KeyEvent) {
        let n = self.visible_history().len();
        if Self::nav(&mut self.rstate, k.code, n) {
            return;
        }
        match k.code {
            KeyCode::Char('/') => self.input = Some(InputTarget::History),
            KeyCode::Char('f') => {
                self.rfailed = !self.rfailed;
                self.rstate.select(Some(0));
            }
            KeyCode::Char('c') => {
                self.rcat = cycle_cat(self.rcat);
                self.rstate.select(Some(0));
            }
            KeyCode::Enter => {
                self.detail = self.visible_history().get(self.rstate.selected().unwrap_or(0)).map(|r| (*r).clone());
                self.dscroll = 0;
            }
            KeyCode::Char('R') => self.ask_rollback(),
            KeyCode::Esc => {
                self.rsearch.clear();
                self.rfailed = false;
                self.rcat = None;
            }
            _ => {}
        }
    }

    fn ask_rollback(&mut self) {
        let rec = self.detail.clone().or_else(|| self.visible_history().get(self.rstate.selected().unwrap_or(0)).map(|r| (*r).clone()));
        let Some(rec) = rec else { return };
        if self.busy() {
            self.flash("Busy: wait for the running operation to finish.");
        } else if rec.rollback.is_none() {
            self.flash(format!("{} ({}): rollback not supported by this package manager.", rec.name, rec.provider));
        } else {
            self.detail = None;
            self.modal = Some(Modal::Rollback(Box::new(rec)));
        }
    }

    fn start_rollback(&mut self, rec: Record) {
        self.rollback_busy = true;
        self.flash(format!("Rolling back {} to {}…", rec.name, rec.from));
        let (ctx, reg, tx) = (self.ctx.clone(), self.reg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = executor::rollback(&ctx, &reg, &rec, false, &History::open(), &|_| {});
            let _ = tx.send(Msg::Rollback(r));
        });
    }

    fn smart_key(&mut self, k: KeyEvent) {
        if let Some(x) = self.exec.as_mut() {
            let n = x.steps.len();
            let mut ts = TableState::default().with_selected(x.state.selected());
            if Self::nav(&mut ts, k.code, n) {
                x.state.select(ts.selected());
                return;
            }
            match k.code {
                KeyCode::Char('s') if !x.done => {
                    if let Some(i) = x.state.selected()
                        && x.outcomes[i].is_none() {
                            x.control.skip(i);
                            let name = x.steps[i].update.name.clone();
                            self.flash(format!("{name}: will be skipped / stopped."));
                        }
                }
                KeyCode::Char('x') if !x.done => self.modal = Some(Modal::Abort),
                KeyCode::Enter | KeyCode::Esc if x.done => {
                    self.exec = None;
                    self.plan = None;
                    self.sel.clear();
                    self.tab = Tab::Dashboard;
                    self.start_scan();
                }
                _ => {}
            }
            return;
        }
        let Some(plan) = &self.plan else {
            if k.code == KeyCode::Enter {
                self.tab = Tab::Updates;
            }
            return;
        };
        let n = plan.steps.len();
        if Self::nav(&mut self.pstate, k.code, n) {
            return;
        }
        match k.code {
            KeyCode::Char('h') => {
                if self.include_high {
                    self.include_high = false;
                } else if plan.high_risk() > 0 {
                    self.modal = Some(Modal::IncludeHigh);
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                if plan.executable(self.include_high).is_empty() {
                    self.flash("Nothing executable: all steps are manual or high-risk (press h).");
                } else {
                    self.modal = Some(Modal::Execute);
                }
            }
            KeyCode::Esc => {
                self.plan = None;
                self.tab = Tab::Updates;
            }
            _ => {}
        }
    }

    fn start_exec(&mut self) {
        let Some(plan) = &self.plan else { return };
        let steps = plan.executable(self.include_high);
        let control = Arc::new(Control::default());
        self.exec = Some(Exec {
            outcomes: vec![None; steps.len()],
            steps: steps.clone(),
            running: HashSet::new(),
            log: VecDeque::new(),
            control: control.clone(),
            done: false,
            started: Instant::now(),
            elapsed: Duration::ZERO,
            state: ListState::default().with_selected(Some(0)),
        });
        let (ctx, reg, tx) = (self.ctx.clone(), self.reg.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let ev = |e: ExecEvent| {
                let _ = tx.send(Msg::Exec(e));
            };
            let out = executor::execute(&ctx, &reg, &steps, false, &control, &History::open(), &ev);
            let _ = tx.send(Msg::ExecDone(out));
        });
    }

    fn settings_key(&mut self, k: KeyEvent) {
        let rows = self.settings_rows();
        let mut ts = TableState::default().with_selected(self.sstate.selected());
        if Self::nav(&mut ts, k.code, rows.len()) {
            self.sstate.select(ts.selected());
            return;
        }
        if k.code != KeyCode::Char(' ') && k.code != KeyCode::Enter {
            return;
        }
        let i = self.sstate.selected().unwrap_or(0);
        let np = self.reg.len();
        let c = &mut self.ctx.config;
        match i {
            0 => c.parallel = !c.parallel,
            1 => c.refresh_indexes = !c.refresh_indexes,
            2 => c.check_appcasts = !c.check_appcasts,
            i if i < 3 + np => {
                let id = self.reg[i - 3].id().to_string();
                if c.enabled(&id) {
                    c.disabled_providers.push(id);
                } else {
                    c.disabled_providers.retain(|p| *p != id);
                }
            }
            i => {
                let e = i - 3 - np;
                if e < c.excluded.len() {
                    c.excluded.remove(e);
                    let n = rows.len() - 1;
                    self.sstate.select(Some(i.min(n.saturating_sub(1))));
                }
            }
        }
        self.save_config();
        self.flash("Saved. Press r to rescan with the new settings.");
    }
}

fn cycle_cat(c: Option<Category>) -> Option<Category> {
    match c {
        None => Some(Category::ALL[0]),
        Some(c) => Category::ALL.iter().position(|x| *x == c).and_then(|i| Category::ALL.get(i + 1).copied()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn sample() -> App {
        let mut app = App::new(Config::default());
        let up = |name: &str, provider: &str, cat, cur: &str, new: &str, support| Update {
            name: name.into(),
            provider: provider.into(),
            category: cat,
            current: cur.into(),
            latest: new.into(),
            source: provider.into(),
            support,
            command: (support == Support::Supported).then(|| vec!["true".into()]),
            ..Default::default()
        };
        app.snap = Snapshot {
            date: config::now(),
            updates: vec![
                up("gh", "homebrew", Category::Homebrew, "2.103.1", "2.103.3", Support::Supported),
                up("openjdk", "homebrew", Category::Homebrew, "26.0.2", "27.0.1", Support::Supported),
                up("macOS 27.2", "softwareupdate", Category::System, "27.0.1", "27.2", Support::Manual),
            ],
            ..Default::default()
        };
        app.snap.updates[1].is_runtime = true;
        app.health = Some(Report { score: 72, issues: vec![HealthIssue::new(Severity::Warning, "Literal ~ in PATH", "PATH", "~/.dotnet/tools").fix("Use an absolute path")] });
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| view::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn every_tab_renders_at_normal_and_minimum_size() {
        let mut app = sample();
        for tab in Tab::ALL {
            app.tab = tab;
            for (w, h) in [(120, 36), (60, 16), (40, 10)] {
                let s = render(&mut app, w, h);
                assert!(!s.trim().is_empty());
            }
        }
        app.tab = Tab::Updates;
        app.sel.insert("homebrew:gh".into());
        app.make_plan();
        assert!(app.plan.as_ref().is_some_and(|p| p.steps.len() == 1));
        app.modal = Some(Modal::Execute);
        let s = render(&mut app, 120, 36);
        assert!(s.contains("Run 1 update(s) now?"));
        if std::env::var("UPKEEP_DUMP").is_ok() {
            app.modal = None;
            for tab in [Tab::Dashboard, Tab::Updates, Tab::Smart] {
                app.tab = tab;
                println!("{}", render(&mut app, 120, 30));
            }
        }
    }

    #[test]
    fn select_all_skips_high_risk_and_manual() {
        let mut app = sample();
        app.tab = Tab::Updates;
        app.key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(app.sel.len(), 1, "only gh: openjdk is high-risk, macOS is manual");
    }
}

#[cfg(test)]
mod shots;
