use super::{App, InputTarget, Modal, Tab};
use crate::config;
use crate::executor::Outcome;
use crate::model::*;
use crate::planner::{self, Risk};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table, Wrap};

const ACCENT: Color = Color::Indexed(75);
const OK: Color = Color::Indexed(114);
const WARN: Color = Color::Indexed(179);
const ERR: Color = Color::Indexed(203);
const DIM: Color = Color::Indexed(244);
const MAG: Color = Color::Indexed(176);
const SEL: Color = Color::Indexed(237);
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(DIM))
}
fn fg(s: impl Into<String>, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(c))
}
fn bold(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().add_modifier(Modifier::BOLD))
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Indexed(240)))
        .title(Line::from(vec![Span::raw(" "), bold(title.into()).fg(ACCENT), Span::raw(" ")]))
}

fn risk_span(r: Risk) -> Span<'static> {
    match r {
        Risk::Low => fg(" LOW ", OK),
        Risk::Medium => fg(" MED ", WARN),
        Risk::High => Span::styled(" HIGH", Style::new().fg(ERR).add_modifier(Modifier::BOLD)),
    }
}

fn sev_span(s: Severity) -> Span<'static> {
    match s {
        Severity::Critical => Span::styled("● CRIT", Style::new().fg(ERR).add_modifier(Modifier::BOLD)),
        Severity::Warning => fg("● WARN", WARN),
        Severity::Info => fg("● INFO", ACCENT),
        Severity::Healthy => fg("● OK  ", OK),
    }
}

fn bar(ratio: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let full = ((ratio.clamp(0.0, 1.0)) * width as f64).round() as usize;
    vec![fg("━".repeat(full), color), fg("━".repeat(width - full), Color::Indexed(238))]
}

fn health_color(score: u8) -> Color {
    match score {
        75.. => OK,
        50..75 => WARN,
        _ => ERR,
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 60 || area.height < 16 {
        let msg = Paragraph::new(vec![Line::from(bold("upkeep")), Line::from(dim(format!("Terminal too small: {}×{} (need 60×16)", area.width, area.height)))]).wrap(Wrap { trim: true });
        f.render_widget(msg, area);
        return;
    }
    let [head, body, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    header(f, head, app);
    match app.tab {
        Tab::Dashboard => dashboard(f, body, app),
        Tab::Updates => updates(f, body, app),
        Tab::Health => health(f, body, app),
        Tab::History => history(f, body, app),
        Tab::Smart => smart(f, body, app),
        Tab::Settings => settings(f, body, app),
    }
    footer(f, foot, app);
    if app.detail.is_some() {
        detail(f, area, app);
    }
    if app.help {
        help(f, area);
    }
    if app.modal.is_some() {
        modal(f, area, app);
    }
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let mut l = vec![Span::styled(" ◆ UPKEEP ", Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)), dim(format!(" v{} ", VERSION))];
    if app.scanning {
        l.push(fg(format!(" {} scanning ", SPIN[app.tick % 10]), ACCENT));
    }
    if app.busy() {
        l.push(fg(format!(" {} updating ", SPIN[app.tick % 10]), MAG));
    }
    let mut tabs = vec![Span::raw(" ")];
    for (i, t) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, if area.width < 100 { t.title().split(' ').next().unwrap_or("") } else { t.title() });
        tabs.push(if *t == app.tab { Span::styled(label, Style::new().fg(ACCENT).bg(SEL).add_modifier(Modifier::BOLD)) } else { dim(label) });
    }
    f.render_widget(Paragraph::new(vec![Line::from(l), Line::from(tabs)]), area);
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some((s, t)) = &app.status
        && t.elapsed().as_secs() < 6 {
            f.render_widget(Paragraph::new(Line::from(vec![fg(" ● ", MAG), Span::raw(s.clone())])), area);
            return;
        }
    let keys: &[(&str, &str)] = match (app.tab, app.input.is_some()) {
        (_, true) => &[("type", "filter"), ("enter", "done"), ("esc", "clear")],
        (Tab::Dashboard, _) => &[("1-6", "sections"), ("enter", "updates"), ("r", "rescan"), ("?", "help"), ("q", "quit")],
        (Tab::Updates, _) => &[("space", "select"), ("a", "all compatible"), ("enter", "plan"), ("/", "search"), ("c", "category"), ("o", "sort"), ("x", "exclude"), ("?", "help")],
        (Tab::Health, _) => &[("↑↓", "move"), ("f", "severity"), ("r", "re-check"), ("?", "help")],
        (Tab::History, _) => &[("enter", "details"), ("R", "rollback"), ("/", "search"), ("f", "failed"), ("c", "category"), ("?", "help")],
        (Tab::Smart, _) if app.exec.as_ref().is_some_and(|e| e.done) => &[("enter", "finish & rescan"), ("↑↓", "move")],
        (Tab::Smart, _) if app.exec.is_some() => &[("s", "skip/stop step"), ("x", "abort"), ("↑↓", "move")],
        (Tab::Smart, _) => &[("enter", "execute"), ("h", "include high risk"), ("esc", "back"), ("?", "help")],
        (Tab::Settings, _) => &[("space", "toggle"), ("r", "rescan"), ("?", "help")],
    };
    let mut l = vec![Span::raw(" ")];
    for (k, v) in keys {
        l.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Indexed(245))));
        l.push(dim(format!(" {v}  ")));
    }
    f.render_widget(Paragraph::new(Line::from(l)), area);
}

fn card(f: &mut Frame, area: Rect, title: &str, value: String, color: Color, sub: Vec<Span<'static>>) {
    let p = Paragraph::new(vec![Line::from(Span::styled(value, Style::new().fg(color).add_modifier(Modifier::BOLD))), Line::from(sub)]).block(panel(title).padding(Padding::horizontal(1)));
    f.render_widget(p, area);
}

fn dashboard(f: &mut Frame, area: Rect, app: &App) {
    let s = &app.snap;
    let auto = s.updates.iter().filter(|u| u.support == Support::Supported).count();
    let apps = s.installed.iter().filter(|i| i.category == Category::Apps && i.provider == "apps").count();
    let tools = s.installed.iter().filter(|i| i.category == Category::Tools).count();
    let tool_ups = s.updates.iter().filter(|u| matches!(u.category, Category::Tools | Category::Packages | Category::Homebrew | Category::Runtimes)).count();
    let narrow = area.width < 90;
    let [cards, info, rest] = Layout::vertical([Constraint::Length(if narrow { 8 } else { 4 }), Constraint::Length(2), Constraint::Min(0)]).areas(area);
    let cells: Vec<Rect> = if narrow {
        let [a, b] = Layout::vertical([Constraint::Length(4), Constraint::Length(4)]).areas(cards);
        let [a1, a2] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(a);
        let [b1, b2] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(b);
        vec![a1, a2, b1, b2]
    } else {
        Layout::horizontal([Constraint::Percentage(25); 4]).split(cards).to_vec()
    };
    card(f, cells[0], "Updates", s.updates.len().to_string(), if s.updates.is_empty() { OK } else { WARN }, vec![fg(format!("{auto} auto"), OK), dim(" · "), fg(format!("{} manual", s.updates.len() - auto), WARN)]);
    card(f, cells[1], "Applications", apps.to_string(), ACCENT, vec![dim(format!("{} not verifiable", s.unverifiable()))]);
    card(f, cells[2], "Dev tools", tools.to_string(), ACCENT, vec![dim(format!("{tool_ups} package/tool updates"))]);
    match &app.health {
        Some(h) => {
            let mut sub = bar(h.score as f64 / 100.0, 10, health_color(h.score));
            sub.push(dim(format!(" {}", h.label())));
            card(f, cells[3], "System Health", format!("{}/100", h.score), health_color(h.score), sub)
        }
        None => card(f, cells[3], "System Health", if app.health_busy { SPIN[app.tick % 10].to_string() } else { "—".into() }, DIM, vec![dim(if app.health_busy { "checking…" } else { "pending" })]),
    }
    let mut l = vec![dim(" Last check: "), Span::raw(config::fmt_time(s.date)), dim(format!(" ({})", config::ago(s.date)))];
    if s.excluded > 0 {
        l.push(dim(format!(" · {} excluded", s.excluded)));
    }
    if !s.warnings.is_empty() {
        l.push(fg(format!("  ! {} scan warnings (see Health)", s.warnings.len()), WARN));
    }
    f.render_widget(Paragraph::new(Line::from(l)), info);

    let [left, right] = if narrow {
        Layout::vertical([Constraint::Percentage(50); 2]).areas(rest)
    } else {
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(rest)
    };
    let max = Category::ALL.iter().map(|c| s.updates.iter().filter(|u| u.category == *c).count()).max().unwrap_or(1).max(1);
    let rows: Vec<Row> = Category::ALL
        .iter()
        .map(|c| {
            let n = s.installed.iter().filter(|i| i.category == *c).count();
            let u = s.updates.iter().filter(|x| x.category == *c).count();
            let mut b = bar(u as f64 / max as f64, 14, if u == 0 { OK } else { WARN });
            b.insert(0, Span::raw(format!("{u:>3} ")));
            Row::new(vec![Cell::from(c.title()), Cell::from(Line::from(dim(n.to_string()))), Cell::from(Line::from(b))])
        })
        .collect();
    let t = Table::new(rows, [Constraint::Length(14), Constraint::Length(9), Constraint::Min(10)])
        .header(Row::new(vec!["Category", "Installed", "Updates"]).style(Style::new().fg(DIM)))
        .block(panel("Overview"));
    f.render_widget(t, left);

    let mut provs: Vec<ListItem> = vec![];
    for p in app.reg.iter() {
        let id = p.id();
        let line = if !app.ctx.config.enabled(id) {
            Line::from(vec![dim("○ "), dim(p.name()), dim("  disabled")])
        } else if app.scanning {
            match app.live.get(id) {
                Some(Some(st)) => status_line(st),
                _ => Line::from(vec![fg(format!("{} ", SPIN[app.tick % 10]), ACCENT), Span::raw(p.name()), dim("  scanning…")]),
            }
        } else {
            match s.providers.iter().find(|x| x.id == id) {
                Some(st) => status_line(st),
                None => Line::from(vec![dim("· "), dim(p.name())]),
            }
        };
        provs.push(ListItem::new(line));
    }
    f.render_widget(List::new(provs).block(panel("Providers")), right);
}

fn status_line(st: &ProviderStatus) -> Line<'static> {
    if let Some(e) = &st.error {
        return Line::from(vec![fg("✗ ", ERR), Span::raw(st.name.clone()), fg(format!("  {}", e.chars().take(60).collect::<String>()), ERR)]);
    }
    if !st.available {
        return Line::from(vec![dim("○ "), dim(st.name.clone()), dim("  not installed")]);
    }
    let ups = match st.updates {
        0 => fg("  up to date", OK),
        1 => fg("  1 update", WARN),
        n => fg(format!("  {n} updates"), WARN),
    };
    Line::from(vec![fg("✓ ", OK), Span::raw(st.name.clone()), dim(format!("  {} items", st.installed)), ups, dim(format!("  {:.1}s", st.duration))])
}

fn updates(f: &mut Frame, area: Rect, app: &mut App) {
    let show_detail = area.height >= 20;
    let [list, det] = Layout::vertical([Constraint::Min(5), Constraint::Length(if show_detail { 9 } else { 0 })]).areas(area);
    let wide = area.width >= 110;
    let v = app.visible_updates();
    let rows: Vec<Row> = v
        .iter()
        .map(|u| {
            let sel = app.sel.contains(&u.id());
            let mark = if u.support != Support::Supported { dim(" · ") } else if sel { fg("[✓]", OK) } else { dim("[ ]") };
            let (risk, _) = planner::assess(u);
            let kind = if u.support == Support::Supported { fg("auto", OK) } else { fg("manual", WARN) };
            let mut cells = vec![
                Cell::from(Line::from(mark)),
                Cell::from(u.name.clone()),
                Cell::from(Line::from(dim(u.current.clone()))),
                Cell::from(Line::from(vec![fg("→ ", DIM), bold(u.latest.clone())])),
                Cell::from(Line::from(risk_span(risk))),
                Cell::from(Line::from(kind)),
                Cell::from(Line::from(dim(u.category.title()))),
            ];
            if wide {
                let mut extra = String::new();
                if u.requires_admin {
                    extra.push_str("⚠ admin ");
                }
                if u.requires_restart {
                    extra.push_str("⟳ restart ");
                }
                cells.push(Cell::from(Line::from(vec![fg(extra, ERR), dim(u.source.clone())])));
            }
            Row::new(cells)
        })
        .collect();
    let mut widths = vec![Constraint::Length(3), Constraint::Min(16), Constraint::Length(14), Constraint::Length(16), Constraint::Length(5), Constraint::Length(6), Constraint::Length(12)];
    if wide {
        widths.push(Constraint::Min(18));
    }
    let auto = v.iter().filter(|u| u.support == Support::Supported).count();
    let search = match (app.input == Some(InputTarget::Updates), app.usearch.is_empty()) {
        (true, _) => format!(" · search: {}▌", app.usearch),
        (false, false) => format!(" · search: {}", app.usearch),
        _ => String::new(),
    };
    let title = format!(
        "Updates {} · {auto} auto · {} selected · {} · sort {}{search}",
        v.len(),
        app.sel.len(),
        app.ucat.map(|c| c.title()).unwrap_or("all categories"),
        app.usort.title()
    );
    let empty = v.is_empty();
    let current = app.ustate.selected().and_then(|i| v.get(i)).map(|u| (*u).clone());
    let mut header = vec!["", "Name", "Installed", "Available", "Risk", "Kind", "Category"];
    if wide {
        header.push("Source");
    }
    let t = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(DIM)))
        .row_highlight_style(Style::new().bg(SEL).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌")
        .block(panel(title));
    f.render_stateful_widget(t, list, &mut app.ustate);
    if empty {
        let msg = if app.scanning { "Scanning…" } else if app.snap.date == 0 { "No scan yet: press r." } else { "✓ Nothing to update here." };
        f.render_widget(Paragraph::new(fg(msg, OK)), centered(list, msg.chars().count() as u16 + 2, 1));
    }
    if show_detail {
        update_detail(f, det, current.as_ref());
    }
}

fn update_detail(f: &mut Frame, area: Rect, u: Option<&Update>) {
    let Some(u) = u else {
        f.render_widget(panel("Details"), area);
        return;
    };
    let (risk, why) = planner::assess(u);
    let na = || dim("not available");
    let mut lines = vec![
        Line::from(vec![bold(u.name.clone()), dim(format!("  {} → {}  ", u.current, u.latest)), risk_span(risk), dim(format!("  {}", why.join("; ")))]),
        Line::from(vec![dim("Source: "), Span::raw(u.source.clone()), dim("   Download: "), u.download_size.clone().map(Span::raw).unwrap_or_else(na), dim("   Admin: "), if u.requires_admin { fg("yes", ERR) } else { Span::raw("no") }, dim("   Restart: "), if u.requires_restart { fg("yes", ERR) } else { Span::raw("no") }]),
        Line::from(vec![dim("Rollback: "), if u.rollback.is_some() { fg("supported", OK) } else { fg("not available", WARN) }, dim("   Status: "), if u.support == Support::Supported { fg("supported by Upkeep", OK) } else { fg("manual update", WARN) }]),
    ];
    if !u.dependencies.is_empty() {
        lines.push(Line::from(vec![dim("Depends on: "), Span::raw(u.dependencies.join(", "))]));
    }
    if !u.dependents.is_empty() {
        lines.push(Line::from(vec![dim("Used by: "), Span::raw(format!("{} ({})", u.dependents.join(", "), u.dependents.len()))]));
    }
    if !u.notes.is_empty() {
        lines.push(Line::from(vec![fg("Notes: ", WARN), Span::raw(u.notes.join("; "))]));
    }
    if let Some(h) = &u.hint {
        lines.push(Line::from(vec![dim("How: "), Span::raw(h.clone())]));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel("Details")), area);
}

fn health(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(rep) = app.health.clone() else {
        let msg = if app.health_busy || app.scanning { format!("{} Running diagnostics…", SPIN[app.tick % 10]) } else { "Press r to run a check.".into() };
        f.render_widget(Paragraph::new(fg(msg, ACCENT)).block(panel("System Health")), area);
        return;
    };
    let [top, list, det] = Layout::vertical([Constraint::Length(3), Constraint::Min(4), Constraint::Length(if area.height >= 22 { 7 } else { 0 })]).areas(area);
    let mut g = vec![bold(format!(" {}/100 ", rep.score)).fg(health_color(rep.score)), dim(format!("{}  ", rep.label()))];
    g.extend(bar(rep.score as f64 / 100.0, (top.width as usize).saturating_sub(70).clamp(10, 40), health_color(rep.score)));
    g.push(Span::raw("   "));
    for s in Severity::ALL {
        g.push(sev_span(s));
        g.push(Span::raw(format!(" {}  ", rep.count(s))));
    }
    f.render_widget(Paragraph::new(Line::from(g)).block(panel("System Health")), top);
    let v = app.visible_issues();
    let rows: Vec<Row> = v
        .iter()
        .map(|i| Row::new(vec![Cell::from(Line::from(sev_span(i.severity))), Cell::from(i.title.clone()), Cell::from(Line::from(dim(i.component.clone())))]))
        .collect();
    let cur = app.hstate.selected().and_then(|i| v.get(i)).map(|i| (*i).clone());
    let t = Table::new(rows, [Constraint::Length(7), Constraint::Min(30), Constraint::Length(24)])
        .row_highlight_style(Style::new().bg(SEL))
        .highlight_symbol("▌")
        .block(panel(format!("Findings · {}", app.hsev.map(|s| s.title()).unwrap_or("all severities"))));
    f.render_stateful_widget(t, list, &mut app.hstate);
    if det.height > 0 {
        let lines = match cur {
            Some(i) => vec![
                Line::from(vec![sev_span(i.severity), Span::raw(" "), bold(i.title.clone()), dim(format!("  [{}]", i.component))]),
                Line::from(vec![dim("Why:  "), Span::raw(i.reason.clone())]),
                Line::from(vec![dim("Risk: "), Span::raw(if i.consequence.is_empty() { "—".into() } else { i.consequence.clone() })]),
                Line::from(vec![fg("Fix:  ", OK), Span::raw(if i.fix.is_empty() { "—".into() } else { i.fix.clone() }), dim("  (never applied automatically)")]),
            ],
            None => vec![],
        };
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel("Details")), det);
    }
}

fn outcome_span(o: &str) -> Span<'static> {
    match o {
        "success" => fg("✓ success", OK),
        "failed" => fg("✗ failed", ERR),
        "unverified" => fg("≈ unverified", WARN),
        "already-current" => dim("= current"),
        "skipped" => fg("↷ skipped", WARN),
        o => dim(o.to_string()),
    }
}

fn history(f: &mut Frame, area: Rect, app: &mut App) {
    let v = app.visible_history();
    let rows: Vec<Row> = v
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(Line::from(dim(config::fmt_time(r.date)))),
                Cell::from(Line::from(vec![Span::raw(r.name.clone()), if r.operation == "rollback" { fg(" ↺", MAG) } else { Span::raw("") }])),
                Cell::from(Line::from(vec![dim(r.from.clone()), dim(" → "), Span::raw(r.to.clone())])),
                Cell::from(Line::from(outcome_span(&r.outcome))),
                Cell::from(Line::from(dim(r.provider.clone()))),
                Cell::from(Line::from(if r.rollback.is_some() { fg("↺", OK) } else { dim("–") })),
                Cell::from(Line::from(dim(format!("{:.0}s", r.duration)))),
            ])
        })
        .collect();
    let failed = app.hist.iter().filter(|r| r.failed()).count();
    let search = if app.input == Some(InputTarget::History) { format!(" · search: {}▌", app.rsearch) } else if !app.rsearch.is_empty() { format!(" · search: {}", app.rsearch) } else { String::new() };
    let title = format!(
        "History {} of {} · {failed} failed{}{} {search}",
        v.len(),
        app.hist.len(),
        if app.rfailed { " · failed only" } else { "" },
        app.rcat.map(|c| format!(" · {}", c.title())).unwrap_or_default()
    );
    let empty = v.is_empty();
    let t = Table::new(rows, [Constraint::Length(16), Constraint::Min(16), Constraint::Length(28), Constraint::Length(12), Constraint::Length(10), Constraint::Length(2), Constraint::Length(5)])
        .header(Row::new(vec!["Date", "Software", "Version", "Outcome", "Manager", "", "Time"]).style(Style::new().fg(DIM)))
        .row_highlight_style(Style::new().bg(SEL))
        .highlight_symbol("▌")
        .block(panel(title));
    f.render_stateful_widget(t, area, &mut app.rstate);
    if empty {
        let msg = "No operations recorded yet.";
        f.render_widget(Paragraph::new(dim(msg)), centered(area, msg.len() as u16 + 2, 1));
    }
}

fn detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = &app.detail else { return };
    let box_ = centered(area, 100, area.height.saturating_sub(4));
    let prev = crate::history::previous_versions(&app.hist, &r.provider, &r.name);
    let mut lines = vec![
        Line::from(vec![bold(r.name.clone()), dim(format!("  #{}  {}", r.id, r.provider))]),
        Line::from(vec![dim("Date:       "), Span::raw(config::fmt_time(r.date))]),
        Line::from(vec![dim("Operation:  "), Span::raw(format!("{} {} → {}", r.operation, r.from, r.to))]),
        Line::from(vec![dim("Command:    "), Span::raw(r.command.join(" "))]),
        Line::from(vec![dim("Outcome:    "), outcome_span(&r.outcome), Span::raw(format!("  {}", r.detail.clone().unwrap_or_default()))]),
        Line::from(vec![dim("Duration:   "), Span::raw(format!("{:.1}s", r.duration))]),
        Line::from(vec![dim("Rollback:   "), r.rollback.as_ref().map(|c| fg(c.join(" "), OK)).unwrap_or_else(|| fg("not supported", WARN))]),
    ];
    if !prev.is_empty() {
        lines.push(Line::from(vec![dim("Previous:   "), Span::raw(prev.join(", "))]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(dim("── output (tail) ──")));
    lines.extend(r.output_tail.iter().map(|l| Line::from(Span::raw(l.clone()))));
    f.render_widget(Clear, box_);
    let p = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((app.dscroll, 0))
        .block(panel("Operation details").title_bottom(Line::from(dim(" ↑↓ scroll · R rollback · any key close "))));
    f.render_widget(p, box_);
}

fn smart(f: &mut Frame, area: Rect, app: &mut App) {
    if app.exec.is_some() {
        return exec_view(f, area, app);
    }
    let Some(plan) = app.plan.clone() else {
        let t = vec![
            Line::from(bold("No plan yet")),
            Line::from(""),
            Line::from(dim("1. Open Updates (2) and select items with space, or press a for all compatible ones.")),
            Line::from(dim("2. Press enter: Upkeep resolves dependencies, orders the steps and rates the risk.")),
            Line::from(dim("3. Review, then confirm. Nothing runs without your explicit confirmation.")),
        ];
        f.render_widget(Paragraph::new(t).block(panel("Smart Update")), area);
        return;
    };
    let warn_h = if plan.warnings.is_empty() && plan.manual.is_empty() { 0 } else { (plan.warnings.len() + plan.manual.len() + 2).min(8) as u16 };
    let [top, list, warn] = Layout::vertical([Constraint::Length(3), Constraint::Min(4), Constraint::Length(warn_h)]).areas(area);
    let exec_n = plan.executable(app.include_high).len();
    let summary = Line::from(vec![
        bold(format!(" {} steps ", plan.steps.len())),
        dim(format!("· {} stages · ", plan.stages())),
        fg(format!("{} high risk", plan.high_risk()), if plan.high_risk() > 0 { ERR } else { DIM }),
        dim(" · "),
        fg(format!("{} need admin", plan.admin()), if plan.admin() > 0 { WARN } else { DIM }),
        dim(format!(" · {} with rollback · ", plan.with_rollback())),
        if app.include_high { fg("high risk INCLUDED", ERR) } else { dim("high risk excluded (h)") },
        dim(" → "),
        fg(format!("{exec_n} will run"), OK),
    ]);
    f.render_widget(Paragraph::new(summary).block(panel("Update Plan")), top);
    let mut last = None;
    let rows: Vec<Row> = plan
        .steps
        .iter()
        .map(|s| {
            let lane = (s.stage, s.update.provider.clone());
            let stage = if last.as_ref() != Some(&lane) { fg(format!("{}·{}", s.stage + 1, s.update.provider), ACCENT) } else { dim("") };
            last = Some(lane);
            let skipped = s.risk == Risk::High && !app.include_high;
            let mut why = s.reasons.join("; ");
            if let Some(by) = &s.linked_by {
                why = format!("needed by {by}; {why}");
            }
            let name = if skipped { dim(format!("{} (excluded)", s.update.name)) } else { Span::raw(s.update.name.clone()) };
            Row::new(vec![
                Cell::from(Line::from(stage)),
                Cell::from(Line::from(risk_span(s.risk))),
                Cell::from(Line::from(name)),
                Cell::from(Line::from(vec![dim(s.update.current.clone()), dim(" → "), Span::raw(s.update.latest.clone())])),
                Cell::from(Line::from(if s.update.rollback.is_some() { fg("↺", OK) } else { dim("✗") })),
                Cell::from(Line::from(vec![if s.update.requires_admin { fg("⚠ admin ", ERR) } else { Span::raw("") }, dim(why)])),
            ])
        })
        .collect();
    let t = Table::new(rows, [Constraint::Length(14), Constraint::Length(5), Constraint::Length(24), Constraint::Length(24), Constraint::Length(2), Constraint::Min(30)])
        .header(Row::new(vec!["Stage", "Risk", "Package", "Version", "↺", "Why"]).style(Style::new().fg(DIM)))
        .row_highlight_style(Style::new().bg(SEL))
        .highlight_symbol("▌")
        .block(panel("Ordered steps (same stage: different managers may run in parallel)"));
    f.render_stateful_widget(t, list, &mut app.pstate);
    if warn_h > 0 {
        let mut l: Vec<Line> = plan.warnings.iter().map(|w| Line::from(vec![fg("! ", WARN), Span::raw(w.clone())])).collect();
        l.extend(plan.manual.iter().map(|u| Line::from(vec![fg("✋ ", WARN), bold(u.name.clone()), dim(format!(" {} → {}  ", u.current, u.latest)), dim(u.hint.clone().unwrap_or_default())])));
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }).block(panel("Notes & manual steps")), warn);
    }
}

fn outcome_icon(o: Option<&Outcome>, running: bool, tick: usize) -> Span<'static> {
    match o {
        _ if running => fg(SPIN[tick % 10], MAG),
        None => dim("◌"),
        Some(Outcome::Success(_)) => fg("✓", OK),
        Some(Outcome::Unverified(_)) => fg("≈", WARN),
        Some(Outcome::AlreadyCurrent(_)) => dim("="),
        Some(Outcome::Failed(_)) => fg("✗", ERR),
        Some(Outcome::Skipped(_)) => fg("↷", WARN),
        Some(Outcome::NotRun) => dim("·"),
    }
}

fn exec_view(f: &mut Frame, area: Rect, app: &mut App) {
    let tick = app.tick;
    let Some(x) = app.exec.as_mut() else { return };
    let total = x.steps.len();
    let finished = x.outcomes.iter().filter(|o| o.is_some()).count();
    let elapsed = if x.done { x.elapsed } else { x.started.elapsed() };
    let [top, mid] = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).areas(area);
    let mut g = vec![bold(format!(" {finished}/{total} ")), dim(format!("{:.0}s  ", elapsed.as_secs_f64()))];
    g.extend(bar(finished as f64 / total.max(1) as f64, (top.width as usize).saturating_sub(30).min(60), if x.done { OK } else { MAG }));
    if x.done {
        let c = |k: &str| x.outcomes.iter().flatten().filter(|o| o.key() == k).count();
        g.push(Span::raw("  "));
        g.extend([fg(format!("✓ {} ", c("success")), OK), fg(format!("≈ {} ", c("unverified")), WARN), dim(format!("= {} ", c("already-current"))), fg(format!("✗ {} ", c("failed")), ERR), fg(format!("↷ {} ", c("skipped")), WARN), dim(format!("· {}", c("not-run")))]);
    } else if x.control.aborted() {
        g.push(fg("  stopping after current step…", WARN));
    }
    f.render_widget(Paragraph::new(Line::from(g)).block(panel(if x.done { "Report" } else { "Executing" })), top);
    let [steps, log] = if mid.width >= 110 { Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(mid) } else { Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid) };
    let items: Vec<ListItem> = x
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let o = x.outcomes[i].as_ref();
            ListItem::new(Line::from(vec![
                outcome_icon(o, x.running.contains(&i), tick),
                Span::raw(format!(" {} ", s.update.name)),
                dim(format!("{} → {} ", s.update.current, s.update.latest)),
                o.map(|o| dim(o.detail())).unwrap_or_else(|| dim("")),
            ]))
        })
        .collect();
    f.render_stateful_widget(List::new(items).highlight_style(Style::new().bg(SEL)).highlight_symbol("▌").block(panel("Steps")), steps, &mut x.state);
    let h = log.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = x.log.iter().skip(x.log.len().saturating_sub(h)).map(|l| Line::from(dim(l.clone()))).collect();
    f.render_widget(Paragraph::new(lines).block(panel("Live output")), log);
}

fn settings(f: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .settings_rows()
        .into_iter()
        .map(|(label, on, desc)| {
            let mark = if label.starts_with("Excluded") { fg("✕ ", WARN) } else if on { fg("● ", OK) } else { dim("○ ") };
            ListItem::new(Line::from(vec![mark, Span::raw(format!("{label:<34}")), dim(desc)]))
        })
        .collect();
    let title = format!("Settings · {}", config::path(config::CONFIG).display());
    f.render_stateful_widget(List::new(items).highlight_style(Style::new().bg(SEL)).highlight_symbol("▌").block(panel(title)), area, &mut app.sstate);
}

fn help(f: &mut Frame, area: Rect) {
    let rows = [
        ("Global", "1-6 / tab  sections · r rescan · ? help · q quit"),
        ("Updates", "space select · a all compatible · n none · / search · c category · o sort · x exclude · enter plan"),
        ("Health", "f filter by severity · ↑↓ details"),
        ("History", "enter details · R rollback · / search · f failed only · c category"),
        ("Smart", "enter execute · h include high-risk · esc back · while running: s skip/stop · x abort"),
        ("Settings", "space toggle provider/option · excluded items: space to include again"),
        ("", ""),
        ("Safety", "Nothing runs without confirmation. Steps needing sudo pause the TUI"),
        ("", "and run in the terminal, where you type your password as usual."),
    ];
    let lines: Vec<Line> = rows.iter().map(|(a, b)| Line::from(vec![fg(format!("{a:<10}"), ACCENT), Span::raw(*b)])).collect();
    let r = centered(area, 104, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel("Keys")), r);
}

fn modal(f: &mut Frame, area: Rect, app: &App) {
    let Some(m) = &app.modal else { return };
    let (title, mut body): (&str, Vec<Line>) = match m {
        Modal::Execute => {
            let plan = app.plan.as_ref().unwrap();
            let steps = plan.executable(app.include_high);
            let mut b = vec![Line::from(bold(format!("Run {} update(s) now?", steps.len())))];
            b.extend(steps.iter().take(8).map(|s| Line::from(vec![Span::raw("  "), risk_span(s.risk), Span::raw(format!(" {} {} → {}", s.update.name, s.update.current, s.update.latest))])));
            if steps.len() > 8 {
                b.push(Line::from(dim(format!("  … and {} more", steps.len() - 8))));
            }
            let no_rb = steps.iter().filter(|s| s.update.rollback.is_none()).count();
            if no_rb > 0 {
                b.push(Line::from(fg(format!("{no_rb} of them cannot be rolled back."), WARN)));
            }
            if steps.iter().any(|s| s.update.requires_admin) {
                b.push(Line::from(fg("Needs sudo: the TUI will pause and you type your password in the terminal.", WARN)));
            }
            ("Confirm", b)
        }
        Modal::IncludeHigh => {
            let plan = app.plan.as_ref().unwrap();
            let mut b = vec![Line::from(fg("Include these HIGH-RISK steps?", ERR))];
            b.extend(plan.steps.iter().filter(|s| s.risk == Risk::High).map(|s| Line::from(vec![bold(format!("  {} ", s.update.name)), dim(s.reasons.join("; "))])));
            ("High risk", b)
        }
        Modal::Abort => ("Abort", vec![Line::from("Stop after the current step? Remaining steps will not run.")]),
        Modal::Exclude(id) => ("Exclude", vec![Line::from(format!("Never offer updates for `{id}`?")), Line::from(dim("Reversible in Settings."))]),
        Modal::Rollback(r) => (
            "Rollback",
            vec![
                Line::from(bold(format!("Roll {} back {} → {}?", r.name, r.to, r.from))),
                Line::from(dim(format!("Runs: {}", r.rollback.clone().unwrap_or_default().join(" ")))),
            ],
        ),
        Modal::Quit => ("Quit", vec![Line::from("Updates are running. Quit and stop after the current step?")]),
    };
    body.push(Line::from(""));
    body.push(Line::from(vec![Span::styled(" y ", Style::new().fg(Color::Black).bg(OK)), dim(" confirm   "), Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Indexed(245))), dim(" / esc cancel")]));
    let r = centered(area, 76, body.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }).block(panel(title).border_style(Style::new().fg(WARN))), r);
}
