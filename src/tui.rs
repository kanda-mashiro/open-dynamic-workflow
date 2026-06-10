//! Two-pane ratatui TUI for a codex-flow run.
//!
//!   LEFT  : workflow steps (phase) list, each with status + agent count.
//!   RIGHT : agents of the focused step (windowed for hundreds of rows).
//!   DETAIL: press ->/Enter on an agent to drill into a scrollable view of its
//!           prompt (input), streamed intermediate events, and final output.
//!
//! Keys: Up/Down move; ->/Enter drill in (Steps->Agents->Detail);
//!       <-/Esc go back; q quits. In Detail: Up/Down/PgUp/PgDn/Home/End scroll.
//!
//! The engine (deno_core, !Send) runs on its own OS thread and streams AppEvents
//! over an mpsc; this render loop owns all state, so no locking is needed.

use std::collections::{HashMap, HashSet};
use std::io::Stdout;

use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::buffer::Buffer;
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, StatefulWidget,
};
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::event::{AgentId, AgentStatus, AgentUpdate, AppEvent, StepId, StepStatus};
use crate::render_opt::{wrap_width, PaneCache, RenderScheduler, WrapCache};

struct AgentRow {
    step: StepId,
    label: String,
    /// One-level nesting group (the child workflow this agent belongs to).
    group: Option<String>,
    status: AgentStatus,
    tokens_in: u64,
    tokens_out: u64,
    detail: Vec<String>,
    /// Most recent note (best-effort failure reason for the exit summary).
    last_note: Option<String>,
}

struct StepRow {
    title: String,
    status: StepStatus,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Focus {
    Steps,
    Agents,
    Detail,
}

/// One visible row of the (possibly grouped) Agents pane. Index-only and Copy:
/// rows are re-derived per key inside 256-key drain bursts, so they must not
/// clone group-name Strings — a header resolves its name lazily via
/// `agents[first].group`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Row {
    /// Group header; `first` = index (into App.agents) of the group's first
    /// agent, which carries the group name.
    Header { first: usize },
    /// A plain agent row (ungrouped, or a member of an expanded group).
    Agent { idx: usize },
}

/// R7.4 rollup, taken literally from the spec: any Failed -> Failed; else any
/// non-Done (Running OR Pending) -> Running; all Done -> Done.
fn rollup(statuses: impl Iterator<Item = AgentStatus>) -> AgentStatus {
    let mut all_done = true;
    let mut any = false;
    for s in statuses {
        any = true;
        match s {
            AgentStatus::Failed => return AgentStatus::Failed,
            AgentStatus::Done => {}
            _ => all_done = false,
        }
    }
    if any && all_done {
        AgentStatus::Done
    } else {
        AgentStatus::Running
    }
}

struct App {
    steps: Vec<StepRow>,
    agents: Vec<AgentRow>,
    by_id: HashMap<AgentId, usize>,
    /// step index -> agent indices, maintained incrementally so rendering never
    /// rescans all agents (was O(total agents) per frame).
    step_agents: Vec<Vec<usize>>,
    focus: Focus,
    steps_sel: usize,
    agents_sel: usize,
    // usize (not u16): wrapped detail lines can exceed u16 for huge/CJK content
    // (codex M1 review finding #2); clamp to u16 only at the ratatui boundary.
    detail_scroll: usize,
    done: Option<Result<String, String>>,
    should_quit: bool,
    // Per-pane invalidation versions, bumped only on changes that pane depends
    // on, so the render layer can cache a pane whose inputs were stable this
    // frame. steps_ver: titles/statuses/counts/selection; detail_ver: the
    // drilled-into agent's content/selection/scroll.
    steps_ver: u64,
    detail_ver: u64,
    /// Workflow name from `export const meta`, shown in the status bar.
    meta_name: Option<String>,
    /// Collapsed nesting groups per step. HashSet<String> lets contains() take
    /// &str (Borrow), so the per-frame row derivation never allocates a key.
    collapsed: HashMap<StepId, HashSet<String>>,
    /// Bumped on every collapse/expand; joins the on_key invalidation tuple
    /// (a toggle changes no focus/selection, so the tuple alone misses it).
    collapse_gen: u64,
    /// The agent the Detail pane is drilled into, PINNED by index at Enter.
    /// Visible-row positions shift as agents spawn into earlier groups; a
    /// positional lookup would silently retarget — freezing the real agent's
    /// detail (stale wrap) exactly like the R1.5 bug. agents is append-only,
    /// so the index never dangles.
    detail_agent: Option<usize>,
}

impl App {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            agents: Vec::new(),
            by_id: HashMap::new(),
            step_agents: Vec::new(),
            focus: Focus::Steps,
            steps_sel: 0,
            agents_sel: 0,
            detail_scroll: 0,
            done: None,
            should_quit: false,
            steps_ver: 0,
            detail_ver: 0,
            meta_name: None,
            collapsed: HashMap::new(),
            collapse_gen: 0,
            detail_agent: None,
        }
    }

    /// Agent indices for the selected step (borrowed; no per-frame allocation).
    fn agents_of_selected_step(&self) -> &[usize] {
        self.step_agents
            .get(self.steps_sel)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Visible rows of the Agents pane for the selected step (R7.3): groups
    /// anchor a Header at their first-seen position with members folded under
    /// it (hidden when collapsed); ungrouped agents keep their spawn position.
    /// Owned Vec because the Enter arm needs &mut self while acting on a row.
    fn rows_for_selected_step(&self) -> Vec<Row> {
        let idxs = self.agents_of_selected_step();
        let collapsed = self.collapsed.get(&self.steps_sel);
        // Pass 1: top-level skeleton + per-group member lists (first-seen order).
        let mut skeleton: Vec<Row> = Vec::with_capacity(idxs.len());
        let mut group_order: Vec<&str> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        for &idx in idxs {
            match self.agents[idx].group.as_deref() {
                None => skeleton.push(Row::Agent { idx }),
                Some(g) => match group_order.iter().position(|&n| n == g) {
                    Some(k) => members[k].push(idx),
                    None => {
                        group_order.push(g);
                        members.push(vec![idx]);
                        skeleton.push(Row::Header { first: idx });
                    }
                },
            }
        }
        // Pass 2: expand each header with its members unless collapsed.
        let mut rows = Vec::with_capacity(idxs.len() + group_order.len());
        let mut next_group = 0usize;
        for row in skeleton {
            rows.push(row);
            if let Row::Header { first } = row {
                let g = self.agents[first].group.as_deref().unwrap_or("");
                let k = next_group;
                next_group += 1;
                if !collapsed.is_some_and(|set| set.contains(g)) {
                    rows.extend(members[k].iter().map(|&idx| Row::Agent { idx }));
                }
            }
        }
        rows
    }

    fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::StepDeclared { step, title } => {
                while self.steps.len() <= step {
                    self.steps.push(StepRow {
                        title: String::new(),
                        status: StepStatus::Pending,
                    });
                }
                while self.step_agents.len() <= step {
                    self.step_agents.push(Vec::new());
                }
                self.steps[step].title = title;
                self.steps_ver += 1;
            }
            AppEvent::StepStatus { step, status } => {
                if let Some(s) = self.steps.get_mut(step) {
                    s.status = status;
                }
                self.steps_ver += 1;
            }
            AppEvent::AgentSpawned {
                id,
                step,
                label,
                prompt,
                group,
            } => {
                // Capture the selected row's IDENTITY before the visible rows
                // shift: a spawn into an earlier EXPANDED group inserts a row
                // above the selection, and a positional agents_sel would drift
                // onto a different row — the next Enter would then pin the
                // wrong agent (codex M7 review #1). Row is index-only into the
                // append-only agents list, so the value itself is stable.
                let keep = (step == self.steps_sel)
                    .then(|| self.rows_for_selected_step().get(self.agents_sel).copied())
                    .flatten();
                let idx = self.agents.len();
                self.agents.push(AgentRow {
                    step,
                    label,
                    group,
                    status: AgentStatus::Pending,
                    tokens_in: 0,
                    tokens_out: 0,
                    detail: vec![
                        "-- INPUT (prompt) --------------------------".to_string(),
                        prompt,
                        String::new(),
                        "-- STREAM ----------------------------------".to_string(),
                    ],
                    last_note: None,
                });
                self.by_id.insert(id, idx);
                // An agent can reference a step no phase()/meta declared (a workflow
                // that calls agent() without any phase()). Synthesize a visible
                // Pending row so the left pane isn't empty (codex M4 review #4);
                // draw_steps renders the empty title gracefully.
                while self.steps.len() <= step {
                    self.steps.push(StepRow {
                        title: String::new(),
                        status: StepStatus::Pending,
                    });
                }
                // Maintain the step -> agents index incrementally.
                while self.step_agents.len() <= step {
                    self.step_agents.push(Vec::new());
                }
                self.step_agents[step].push(idx);
                // Re-locate the captured selection identity in the new rows.
                if let Some(keep) = keep {
                    if let Some(pos) = self
                        .rows_for_selected_step()
                        .iter()
                        .position(|r| *r == keep)
                    {
                        self.agents_sel = pos;
                    }
                }
                self.steps_ver += 1; // step's agent-count badge changed
            }
            AppEvent::Agent { id, update } => {
                if id == 0 {
                    return; // narrator note; ignored in TUI for now
                }
                let Some(&idx) = self.by_id.get(&id) else {
                    return;
                };
                // Compare against the PIN, not a positional lookup: rows shift
                // as agents spawn into earlier groups, and a positional miss
                // here would stop bumping detail_ver for the drilled agent —
                // freezing its Detail pane mid-stream (the R1.5 bug reborn).
                let sel = self.detail_agent;
                let a = &mut self.agents[idx];
                match update {
                    AgentUpdate::Status(s) => a.status = s,
                    AgentUpdate::Tokens { input, output } => {
                        a.tokens_in = input;
                        a.tokens_out = output;
                        a.detail.push(format!("[tokens] in={input} out={output}"));
                    }
                    AgentUpdate::Reasoning(t) => a.detail.push(format!("[reason] {t}")),
                    AgentUpdate::Command { command, output } => {
                        a.detail.push(format!("[cmd] {command}"));
                        if let Some(o) = output {
                            for l in o.lines() {
                                a.detail.push(format!("    {l}"));
                            }
                        }
                    }
                    AgentUpdate::FileChange(fc) => a.detail.push(format!("[file] {fc}")),
                    AgentUpdate::ToolCall(t) => a.detail.push(format!("[tool] {t}")),
                    AgentUpdate::WebSearch(q) => a.detail.push(format!("[web] {q}")),
                    AgentUpdate::Message(m) => a.detail.push(format!("[msg] {m}")),
                    AgentUpdate::Note(n) => {
                        a.detail.push(format!("[note] {n}"));
                        a.last_note = Some(n);
                    }
                    AgentUpdate::Final(fr) => {
                        a.detail.push(String::new());
                        a.detail
                            .push("-- FINAL -----------------------------------".to_string());
                        a.detail.push(fr);
                    }
                }
                // Cap detail buffer so a runaway-chatty agent can't make the
                // detail pane (wrapped Paragraph) slow to render. Keep the tail.
                const MAX_DETAIL: usize = 2000;
                if a.detail.len() > MAX_DETAIL {
                    let drop = a.detail.len() - MAX_DETAIL;
                    a.detail.drain(0..drop);
                }
                if Some(idx) == sel {
                    self.detail_ver += 1; // the drilled-into agent changed
                }
            }
            AppEvent::RunMeta { name, phases } => {
                if !name.is_empty() {
                    self.meta_name = Some(name);
                }
                // Pre-declare the phase skeleton: every phase shows as Pending up
                // front; phase() later flips each to Running/Done in place. Mirrors
                // StepDeclared's capacity logic, so the two are idempotent.
                for (i, title) in phases.into_iter().enumerate() {
                    while self.steps.len() <= i {
                        self.steps.push(StepRow {
                            title: String::new(),
                            status: StepStatus::Pending,
                        });
                    }
                    while self.step_agents.len() <= i {
                        self.step_agents.push(Vec::new());
                    }
                    // StepDeclared is authoritative for titles; only fill an empty
                    // slot here so a late meta can't clobber a real title for the
                    // top-level-promise form (codex M4 review #3).
                    if self.steps[i].title.is_empty() {
                        self.steps[i].title = title;
                    }
                }
                self.steps_ver += 1;
            }
            AppEvent::WorkflowDone(r) => self.done = Some(r),
            AppEvent::EngineError(e) => self.done = Some(Err(e)),
            AppEvent::Key(_) | AppEvent::Render => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        use KeyCode::*;
        // Bump the pane invalidation versions only when pane INPUTS change
        // (focus, a selection, or a collapse toggle): the cached wrap/steps
        // buffers depend on those, NOT on the scroll offset (applied per draw
        // from the cached lines). "Keys are human-rate" was wrong: terminals
        // turn a trackpad flick into hundreds of arrow keys (tmux: 3/notch +
        // momentum), and busting the wrap cache per key re-wrapped the whole
        // detail buffer each time — measured 35.9s for one 400-key burst (the
        // scroll-freeze bug). collapse_gen is in the tuple because a toggle
        // changes no focus/selection yet changes what panes must show.
        let before = (self.focus, self.steps_sel, self.agents_sel, self.collapse_gen);
        match (self.focus, k.code) {
            (_, Char('q')) => self.should_quit = true,
            (_, Char('c')) if k.modifiers.contains(KeyModifiers::CONTROL) => self.should_quit = true,

            (Focus::Steps, Up) => self.steps_sel = self.steps_sel.saturating_sub(1),
            (Focus::Steps, Down) => {
                if self.steps_sel + 1 < self.steps.len() {
                    self.steps_sel += 1;
                }
            }
            (Focus::Steps, Right | Enter) => {
                if !self.agents_of_selected_step().is_empty() {
                    self.focus = Focus::Agents;
                    self.agents_sel = 0;
                }
            }

            (Focus::Agents, Up) => self.agents_sel = self.agents_sel.saturating_sub(1),
            (Focus::Agents, Down) => {
                // Bound by VISIBLE rows: headers lengthen the list, collapsed
                // groups shorten it — the raw agent count is wrong both ways.
                let n = self.rows_for_selected_step().len();
                if self.agents_sel + 1 < n {
                    self.agents_sel += 1;
                }
            }
            (Focus::Agents, Left | Esc) => self.focus = Focus::Steps,
            (Focus::Agents, Right | Enter) => {
                match self.rows_for_selected_step().get(self.agents_sel).copied() {
                    Some(Row::Agent { idx }) => {
                        self.focus = Focus::Detail;
                        self.detail_scroll = 0;
                        self.detail_agent = Some(idx);
                    }
                    Some(Row::Header { first }) => {
                        // Toggle this group's collapse.
                        let g = self.agents[first].group.clone().unwrap_or_default();
                        let set = self.collapsed.entry(self.steps_sel).or_default();
                        if !set.remove(&g) {
                            set.insert(g);
                        }
                        self.collapse_gen = self.collapse_gen.wrapping_add(1);
                        // Collapsing shrinks the row list; clamp the stored
                        // selection now (write-back lesson, R1.5 ②) so a later
                        // Enter can't act on an out-of-range row.
                        let n = self.rows_for_selected_step().len();
                        self.agents_sel = self.agents_sel.min(n.saturating_sub(1));
                    }
                    None => {}
                }
            }

            (Focus::Detail, Left | Esc) => self.focus = Focus::Agents,
            (Focus::Detail, Up) => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            (Focus::Detail, Down) => self.detail_scroll = self.detail_scroll.saturating_add(1),
            (Focus::Detail, PageUp) => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            (Focus::Detail, PageDown) => self.detail_scroll = self.detail_scroll.saturating_add(10),
            (Focus::Detail, Home) => self.detail_scroll = 0,
            (Focus::Detail, End) => self.detail_scroll = usize::MAX,
            _ => {}
        }
        if (self.focus, self.steps_sel, self.agents_sel, self.collapse_gen) != before {
            self.steps_ver = self.steps_ver.wrapping_add(1);
            self.detail_ver = self.detail_ver.wrapping_add(1);
        }
    }
}

fn step_glyph(s: StepStatus) -> Span<'static> {
    match s {
        StepStatus::Pending => Span::styled(".", Style::default().fg(Color::DarkGray)),
        StepStatus::Running => Span::styled("*", Style::default().fg(Color::Yellow)),
        StepStatus::Done => Span::styled("v", Style::default().fg(Color::Green)),
        StepStatus::Failed => Span::styled("x", Style::default().fg(Color::Red)),
    }
}

fn agent_glyph(s: AgentStatus) -> Span<'static> {
    let c = match s {
        AgentStatus::Pending => Color::DarkGray,
        AgentStatus::Running => Color::Yellow,
        AgentStatus::Done => Color::Green,
        AgentStatus::Failed => Color::Red,
    };
    Span::styled("*", Style::default().fg(c))
}

fn pane_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render-side caches owned by the run loop (and the bench harness), kept OUT of
/// `App` so a pane's cache can be borrowed mutably while `App` is borrowed shared.
struct RenderState {
    steps_pane: PaneCache,
    last_steps_ver: u64,
    detail_wrap: WrapCache,
}

impl RenderState {
    fn new() -> Self {
        Self {
            steps_pane: PaneCache::new(),
            last_steps_ver: u64::MAX, // != any real steps_ver -> force first render
            detail_wrap: WrapCache::new(),
        }
    }
}

// `app` is mutable so draw_detail can write the scroll clamp back (see there).
fn draw(f: &mut ratatui::Frame, app: &mut App, rs: &mut RenderState) {
    let outer = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(f.area());
    let panes =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).split(outer[0]);
    // B.2: the Steps pane's inputs are stable while agents stream, so cache it —
    // a streaming frame skips rebuilding it. Volatile panes (Agents during a run)
    // stay uncached: a cache that misses every frame only adds a buffer copy on
    // top of the rebuild. See SPEC.md R1.3.
    let force = app.steps_ver != rs.last_steps_ver;
    let app_ro = &*app;
    rs.steps_pane
        .render_into(f.buffer_mut(), panes[0], force, |a, buf| render_steps_into(app_ro, a, buf));
    rs.last_steps_ver = app.steps_ver;
    if app.focus == Focus::Detail {
        draw_detail(f, app, rs, panes[1]);
    } else {
        draw_agents(f, app, panes[1]);
    }
    draw_status(f, app, outer[1]);
}

fn render_steps_into(app: &App, area: Rect, buf: &mut Buffer) {
    let items: Vec<ListItem> = app
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let count = app.agents.iter().filter(|a| a.step == i).count();
            ListItem::new(Line::from(vec![
                step_glyph(s.status),
                Span::raw(" "),
                Span::raw(if s.title.is_empty() {
                    format!("step {i}")
                } else {
                    s.title.clone()
                }),
                Span::styled(format!("  ({count})"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut st = ListState::default();
    if !app.steps.is_empty() {
        st.select(Some(app.steps_sel.min(app.steps.len() - 1)));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Steps ")
                .border_style(pane_border(app.focus == Focus::Steps)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    StatefulWidget::render(list, area, buf, &mut st);
}

fn draw_agents(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let idxs = app.agents_of_selected_step();
    let rows = app.rows_for_selected_step();
    let title = app
        .steps
        .get(app.steps_sel)
        .map(|s| format!(" Agents · {} ", s.title))
        .unwrap_or_else(|| " Agents ".to_string());
    // One O(n) pass collecting per-group statuses/token sums for header rows —
    // NOT a rescan per header (this pane is uncached and redrawn every frame).
    struct Agg {
        statuses: Vec<AgentStatus>,
        tin: u64,
        tout: u64,
    }
    let mut aggs: HashMap<&str, Agg> = HashMap::new();
    for &i in idxs {
        let a = &app.agents[i];
        if let Some(g) = a.group.as_deref() {
            let e = aggs.entry(g).or_insert(Agg {
                statuses: Vec::new(),
                tin: 0,
                tout: 0,
            });
            e.statuses.push(a.status);
            e.tin += a.tokens_in;
            e.tout += a.tokens_out;
        }
    }
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            Row::Header { first } => {
                let name = app.agents[first].group.as_deref().unwrap_or("");
                let agg = &aggs[name];
                let status = rollup(agg.statuses.iter().copied());
                let done = agg
                    .statuses
                    .iter()
                    .filter(|s| **s == AgentStatus::Done)
                    .count();
                let open = !app
                    .collapsed
                    .get(&app.steps_sel)
                    .is_some_and(|s| s.contains(name));
                let arrow = if open { "v" } else { ">" };
                ListItem::new(Line::from(vec![
                    agent_glyph(status),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<28}", truncate(&format!("{arrow} {name}"), 28)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("[{done}/{}]  {}/{}", agg.statuses.len(), agg.tin, agg.tout),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            }
            Row::Agent { idx } => {
                let a = &app.agents[idx];
                // Members indent INSIDE the fixed 28-col label field so the
                // token column stays aligned with ungrouped rows (whose span
                // sequence is byte-identical to the pre-M7 rendering).
                let label = if a.group.is_some() {
                    format!("  {}", truncate(&a.label, 26))
                } else {
                    truncate(&a.label, 28)
                };
                ListItem::new(Line::from(vec![
                    agent_glyph(a.status),
                    Span::raw(" "),
                    Span::raw(format!("{:<28}", label)),
                    Span::styled(
                        format!("{}/{}", a.tokens_in, a.tokens_out),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            }
        })
        .collect();
    let mut st = ListState::default();
    if !rows.is_empty() {
        st.select(Some(app.agents_sel.min(rows.len() - 1)));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(pane_border(app.focus == Focus::Agents)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_detail(f: &mut ratatui::Frame, app: &mut App, rs: &mut RenderState, area: Rect) {
    // The drilled agent is PINNED at Enter (stable index into append-only
    // agents) — a positional row lookup would retarget as rows shift.
    let Some(ai) = app.detail_agent else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return; // avoid a scrollbar panic on a zero-size area (codex review #3)
    }
    // B.3: pre-wrap to the inner width using DISPLAY width (CJK = 2 cols), cached
    // by (detail_ver, width). The scroll offset is NOT part of the cache key, so
    // a scroll key replays the cached wrap instead of rebuilding it.
    let inner_w = area.width.saturating_sub(2).max(1);
    let title = {
        let a = &app.agents[ai];
        format!(" Agent: {} [{}] ", a.label, status_text(a.status))
    };
    let detail_ver = app.detail_ver;
    let wrapped = {
        let detail = &app.agents[ai].detail;
        rs.detail_wrap.get(detail_ver, inner_w, || {
            detail.iter().flat_map(|l| wrap_width(l, inner_w)).collect()
        })
    };
    let total = wrapped.len();
    let view_h = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(view_h);
    let scroll = app.detail_scroll.min(max_scroll);
    // Write the clamp back: scrolling past the end must not bank invisible
    // overshoot that Up then unwinds press-by-press with nothing moving on
    // screen (half of the scroll-freeze bug). End (usize::MAX) lands on the
    // real max here too.
    app.detail_scroll = scroll;
    // Materialize ONLY the visible window: draw cost is O(view height), not
    // O(total wrapped lines), and pre-sliced lines need no Paragraph::scroll
    // (which also removes the old u16 clamp at the ratatui boundary).
    let lines: Vec<Line> = wrapped[scroll..(scroll + view_h).min(total)]
        .iter()
        .map(|l| Line::raw(l.as_str()))
        .collect();
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(pane_border(true)),
    );
    f.render_widget(p, area);
    let mut sb = ScrollbarState::new(total).position(scroll);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut sb,
    );
}

fn draw_status(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let hint = match app.focus {
        Focus::Steps => " Up/Dn steps | -> agents | q quit ",
        Focus::Agents => " Up/Dn agents | -> detail | <- steps | q quit ",
        Focus::Detail => " Up/Dn/PgUp/PgDn scroll | <- back | q quit ",
    };
    let right = match &app.done {
        Some(Ok(_)) => Span::styled(" DONE ", Style::default().fg(Color::Black).bg(Color::Green)),
        Some(Err(_)) => Span::styled(" ERROR ", Style::default().fg(Color::White).bg(Color::Red)),
        None => Span::styled(" running ", Style::default().fg(Color::Black).bg(Color::Yellow)),
    };
    let mut spans = Vec::new();
    if let Some(name) = &app.meta_name {
        spans.push(Span::styled(
            format!(" {name} "),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    spans.push(Span::raw("  "));
    spans.push(right);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}~")
    }
}

fn status_text(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Pending => "pending",
        AgentStatus::Running => "running",
        AgentStatus::Done => "done",
        AgentStatus::Failed => "failed",
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> std::io::Result<Term> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(out))
}

fn restore_terminal(mut term: Term) -> std::io::Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

/// Run the TUI on the current thread, consuming AppEvents from `rx`.
pub async fn run_tui(
    mut rx: mpsc::UnboundedReceiver<AppEvent>,
) -> std::io::Result<Option<Result<String, String>>> {
    let mut term = setup_terminal()?;
    let mut app = App::new();
    let mut rs = RenderState::new();
    let mut keys = EventStream::new();
    // B.1: a RenderScheduler decides WHEN to draw. Data (engine) events coalesce
    // into the next ~16ms (60fps) frame; a key event forces an immediate redraw
    // so navigation/scroll feel instant. This keeps the high-volume-stream
    // coalescing fix while halving input latency vs the old fixed 33ms ticker.
    let mut sched = RenderScheduler::new();
    let mut ticker = tokio::time::interval(RenderScheduler::FRAME);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Once the engine sender drops, rx.recv() returns None immediately+forever.
    // A naive select! spins that ready branch and STARVES keys — so `q` would
    // not quit after a workflow finished. Disable the rx branch once closed.
    let mut engine_open = true;
    term.draw(|f| draw(f, &mut app, &mut rs))?;

    loop {
        tokio::select! {
            // Drain the channel in bulk: apply the first event, then greedily
            // take everything already queued without waking per-event. This is
            // the key fix for "page lags when subagents update" — N queued
            // updates cost one redraw, not N.
            maybe = rx.recv(), if engine_open => {
                match maybe {
                    Some(ev) => {
                        app.apply(ev);
                        // Bounded bulk-drain: coalesce a burst into one redraw, but
                        // cap the batch so a continuously-busy producer can't starve
                        // keys/ticks. Leftover events win the rx branch again next
                        // iteration (codex M1 review finding #1).
                        const MAX_DRAIN: usize = 512;
                        for _ in 0..MAX_DRAIN {
                            match rx.try_recv() {
                                Ok(ev) => app.apply(ev),
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    engine_open = false;
                                    break;
                                }
                            }
                        }
                        sched.mark_dirty();
                    }
                    None => {
                        engine_open = false; // engine done; keep keys live
                        sched.mark_dirty();
                    }
                }
            }
            maybe = keys.next() => {
                // Coalesce a key burst into ONE redraw: a trackpad flick reaches
                // us as hundreds of arrow keys (tmux sends 3 per wheel notch and
                // momentum keeps feeding more); drawing once per key multiplied
                // a full redraw by the burst length and froze the UI. Handle the
                // awaited event, then drain whatever is already buffered.
                let mut next = maybe;
                const MAX_KEY_DRAIN: usize = 256;
                for _ in 0..MAX_KEY_DRAIN {
                    match next {
                        Some(Ok(CtEvent::Key(k))) => {
                            if k.kind != KeyEventKind::Release {
                                app.on_key(k);
                                sched.on_key();
                            }
                        }
                        Some(Ok(_)) => {} // resize etc: the draw below adapts
                        Some(Err(_)) | None => break,
                    }
                    if app.should_quit {
                        break;
                    }
                    // Non-blocking peek: pending (None) ends the burst; select!
                    // re-polls the stream with a real waker on the next loop.
                    match keys.next().now_or_never() {
                        Some(n) => next = n,
                        None => break,
                    }
                }
                // Immediate redraw: don't make the keypress wait for the next
                // frame boundary.
                if sched.should_draw_now() {
                    term.draw(|f| draw(f, &mut app, &mut rs))?;
                }
            }
            // Frame boundary: redraw only if a data event marked us dirty.
            _ = ticker.tick() => {
                if sched.should_draw_on_tick() {
                    term.draw(|f| draw(f, &mut app, &mut rs))?;
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    restore_terminal(term)?;
    // After leaving the alternate screen, surface a consolidated failure summary
    // (the live red glyphs are gone once we exit). Shares cli::format_failures
    // with the stdout front-end; per-agent errors stay inspectable in Detail.
    // Only summarize once the run actually finished — an early `q` quit aborts
    // with agents still running, where a partial list would mislead (codex #4).
    if app.done.is_some() {
        let failed: Vec<(String, Option<String>)> = app
            .agents
            .iter()
            .filter(|a| a.status == AgentStatus::Failed)
            .map(|a| {
                // Same group/label display identity as the stdout front-end.
                let label = match &a.group {
                    Some(g) => format!("{g}/{}", a.label),
                    None => a.label.clone(),
                };
                (label, a.last_note.clone())
            })
            .collect();
        if let Some(s) = crate::cli::format_failures(&failed) {
            eprintln!("{s}");
        }
    }
    Ok(app.done)
}

// ── Benchmark harness ─────────────────────────────────────────────────────
//
// Measures render cost on a deterministic synthetic load using ratatui's
// headless TestBackend (same draw path: widget build + buffer diff), so numbers
// are comparable and reproducible. Two scheduling strategies are compared on the
// IDENTICAL event stream:
//
//   PerEvent  (pre-optimization): draw() after EVERY applied event.
//   Coalesced (post-optimization): apply a burst, draw() once per ~33ms frame.
//
// Run: `codex-flow bench [agents] [events_per_agent]`  (defaults 16 x 200)
pub mod bench {
    use super::{draw, App, RenderState};
    use crate::event::{AgentId, AgentUpdate, AppEvent, StepStatus};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::{Duration, Instant};

    /// Build a deterministic event stream: one phase, `agents` agents, each
    /// emitting `per` streaming detail events, interleaved round-robin (mirrors
    /// real concurrent subagents).
    fn make_load(agents: usize, per: usize) -> Vec<AppEvent> {
        let mut evs = Vec::with_capacity(agents * (per + 2) + 2);
        evs.push(AppEvent::StepDeclared { step: 0, title: "Stress".into() });
        evs.push(AppEvent::StepStatus { step: 0, status: StepStatus::Running });
        for a in 0..agents {
            evs.push(AppEvent::AgentSpawned {
                id: (a + 1) as AgentId,
                step: 0,
                label: format!("stress-{a}"),
                prompt: format!("prompt for agent {a}"),
                group: None,
            });
        }
        // Round-robin streaming so the active set updates like real concurrency.
        for i in 0..per {
            for a in 0..agents {
                evs.push(AppEvent::Agent {
                    id: (a + 1) as AgentId,
                    update: AgentUpdate::Command {
                        command: format!("step {i} of agent {a}"),
                        output: Some(format!("line {i}: some streamed output text here")),
                    },
                });
            }
        }
        evs
    }

    struct Stats {
        strategy: &'static str,
        events: usize,
        draws: usize,
        total_draw: Duration,
        max_draw: Duration,
        wall: Duration,
    }

    fn report(s: &Stats) {
        let avg = if s.draws > 0 { s.total_draw / s.draws as u32 } else { Duration::ZERO };
        println!(
            "{{\"strategy\":\"{}\",\"events\":{},\"draws\":{},\"total_draw_ms\":{:.2},\"avg_draw_us\":{:.1},\"max_draw_us\":{:.1},\"wall_ms\":{:.2}}}",
            s.strategy,
            s.events,
            s.draws,
            s.total_draw.as_secs_f64() * 1e3,
            avg.as_secs_f64() * 1e6,
            s.max_draw.as_secs_f64() * 1e6,
            s.wall.as_secs_f64() * 1e3,
        );
    }

    /// PerEvent: draw after every event (the old loop's behavior).
    fn run_per_event(load: &[AppEvent], w: u16, h: u16) -> Stats {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut app = App::new();
        let mut rs = RenderState::new();
        let mut total = Duration::ZERO;
        let mut max = Duration::ZERO;
        let mut draws = 0usize;
        let wall0 = Instant::now();
        for ev in load.iter().cloned() {
            app.apply(ev);
            let t = Instant::now();
            term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
            let d = t.elapsed();
            total += d;
            if d > max { max = d; }
            draws += 1;
        }
        Stats { strategy: "PerEvent(old)", events: load.len(), draws, total_draw: total, max_draw: max, wall: wall0.elapsed() }
    }

    /// Coalesced: apply in bursts, draw once per frame (the new loop's behavior).
    /// `burst` = events applied between frames (simulates a 33ms frame's intake).
    fn run_coalesced(load: &[AppEvent], w: u16, h: u16, burst: usize) -> Stats {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut app = App::new();
        let mut rs = RenderState::new();
        let mut total = Duration::ZERO;
        let mut max = Duration::ZERO;
        let mut draws = 0usize;
        let wall0 = Instant::now();
        let mut applied_since = 0usize;
        for ev in load.iter().cloned() {
            app.apply(ev);
            applied_since += 1;
            if applied_since >= burst {
                let t = Instant::now();
                term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
                let d = t.elapsed();
                total += d;
                if d > max { max = d; }
                draws += 1;
                applied_since = 0;
            }
        }
        if applied_since > 0 {
            let t = Instant::now();
            term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
            let d = t.elapsed();
            total += d;
            if d > max { max = d; }
            draws += 1;
        }
        Stats { strategy: "Coalesced(new)", events: load.len(), draws, total_draw: total, max_draw: max, wall: wall0.elapsed() }
    }

    pub fn run(agents: usize, per: usize) {
        let (w, h) = (160u16, 45u16);
        let load = make_load(agents, per);
        eprintln!(
            "bench load: {} agents x {} events = {} total events, viewport {}x{}",
            agents, per, load.len(), w, h
        );
        // The new loop coalesces a frame's worth of intake; with ~33ms frames and
        // a fast fake codex, a burst of dozens-to-hundreds is realistic. Use the
        // per-agent count as a representative burst (one round of all agents).
        let burst = agents.max(1);
        let old = run_per_event(&load, w, h);
        let new = run_coalesced(&load, w, h, burst);
        report(&old);
        report(&new);
        let speedup = old.total_draw.as_secs_f64() / new.total_draw.as_secs_f64().max(1e-9);
        eprintln!(
            "draws: {} -> {} ({:.1}x fewer); total render time: {:.1}ms -> {:.1}ms ({:.1}x less)",
            old.draws, new.draws,
            old.draws as f64 / new.draws.max(1) as f64,
            old.total_draw.as_secs_f64() * 1e3,
            new.total_draw.as_secs_f64() * 1e3,
            speedup,
        );
    }
}

// ── Scroll-freeze regression tests ──────────────────────────────────────────
//
// Repro for the field bug "scroll to the bottom of Detail, keep scrolling →
// TUI freezes for a long time". Terminals translate wheel/trackpad scrolling
// into arrow-key bursts (tmux: 3 per notch, with momentum lasting seconds), and
// the run loop draws once per key — so the per-key cost and the scroll-offset
// semantics below are exactly what the user feels.
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// An App drilled into the Detail pane of one agent whose detail buffer is
    /// at the MAX_DETAIL cap with long CJK-mixed lines — the realistic shape of
    /// a codex agent that streamed for a while.
    fn heavy_app() -> App {
        let mut app = App::new();
        app.apply(AppEvent::StepDeclared { step: 0, title: "s".into() });
        app.apply(AppEvent::AgentSpawned {
            id: 1,
            step: 0,
            label: "big".into(),
            prompt: "p".into(),
            group: None,
        });
        for i in 0..2000 {
            app.apply(AppEvent::Agent {
                id: 1,
                update: AgentUpdate::Message(format!(
                    "第{i}行 混合中文与ASCII的较长流式输出内容 streamed output {}",
                    "x".repeat(90)
                )),
            });
        }
        app.on_key(key(KeyCode::Enter)); // Steps -> Agents
        app.on_key(key(KeyCode::Enter)); // Agents -> Detail
        assert_eq!(app.focus, Focus::Detail);
        app
    }

    fn spawn(app: &mut App, id: AgentId, label: &str, group: Option<&str>) {
        app.apply(AppEvent::AgentSpawned {
            id,
            step: 0,
            label: label.into(),
            prompt: "p".into(),
            group: group.map(str::to_string),
        });
    }

    /// One step with two groups and one flat agent, spawn order interleaved:
    /// a(g1), b(flat), c(g1), d(g2)  ->  agents idx 0..=3.
    fn grouped_app() -> App {
        let mut app = App::new();
        app.apply(AppEvent::StepDeclared { step: 0, title: "s".into() });
        spawn(&mut app, 1, "a", Some("g1"));
        spawn(&mut app, 2, "b", None);
        spawn(&mut app, 3, "c", Some("g1"));
        spawn(&mut app, 4, "d", Some("g2"));
        app
    }

    #[test]
    fn rollup_truth_table() {
        use AgentStatus::*;
        assert_eq!(rollup([Done, Done].into_iter()), Done);
        assert_eq!(rollup([Done, Failed, Running].into_iter()), Failed);
        assert_eq!(rollup([Done, Running].into_iter()), Running);
        // Spec literal (R7.4): an all-Pending group rolls up Running, NOT Pending.
        assert_eq!(rollup([Pending, Pending].into_iter()), Running);
    }

    #[test]
    fn rows_group_at_first_seen_members_folded_flat_interleaved() {
        let app = grouped_app();
        assert_eq!(
            app.rows_for_selected_step(),
            vec![
                Row::Header { first: 0 }, // g1 anchors where a first appeared
                Row::Agent { idx: 0 },    // a
                Row::Agent { idx: 2 },    // c folded under g1, out of spawn order
                Row::Agent { idx: 1 },    // flat b keeps its spawn position
                Row::Header { first: 3 }, // g2
                Row::Agent { idx: 3 },    // d
            ]
        );
    }

    #[test]
    fn header_enter_toggles_collapse_bumps_gen_and_bounds_down() {
        let mut app = grouped_app();
        app.on_key(key(KeyCode::Enter)); // Steps -> Agents
        assert_eq!(app.focus, Focus::Agents);
        // Down is bounded by VISIBLE rows (6), not agent count (4).
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.agents_sel, 5, "Down bound = rows.len()-1");
        for _ in 0..10 {
            app.on_key(key(KeyCode::Up));
        }
        let (d0, g0) = (app.detail_ver, app.collapse_gen);
        app.on_key(key(KeyCode::Enter)); // on the g1 header: toggle, NOT Detail
        assert_eq!(app.focus, Focus::Agents, "header Enter must not enter Detail");
        assert_eq!(app.rows_for_selected_step().len(), 4, "g1 members hidden");
        assert_ne!(app.collapse_gen, g0);
        assert_ne!(app.detail_ver, d0, "a collapse toggle must invalidate panes");
        app.on_key(key(KeyCode::Enter)); // expand again
        assert_eq!(app.rows_for_selected_step().len(), 6);
    }

    #[test]
    fn spawn_into_earlier_group_keeps_selection_identity() {
        // codex M7 review #1: the pin protects the DRILLED agent, but a merely
        // SELECTED row must not drift either when a spawn inserts a row above it.
        let mut app = grouped_app();
        app.on_key(key(KeyCode::Enter)); // -> Agents
        for _ in 0..3 {
            app.on_key(key(KeyCode::Down)); // select flat b (row 3)
        }
        assert_eq!(app.rows_for_selected_step()[app.agents_sel], Row::Agent { idx: 1 });
        spawn(&mut app, 9, "late", Some("g1")); // inserts a row ABOVE the selection
        assert_eq!(
            app.rows_for_selected_step()[app.agents_sel],
            Row::Agent { idx: 1 },
            "selection must stay on b, not drift onto the late spawner"
        );
        assert_eq!(app.agents_sel, 4, "b moved down one visible row");
    }

    #[test]
    fn detail_pin_survives_row_shift_from_earlier_group_spawn() {
        let mut app = grouped_app();
        app.on_key(key(KeyCode::Enter)); // -> Agents
        for _ in 0..3 {
            app.on_key(key(KeyCode::Down)); // rows: H(g1) a c -> b (flat) at row 3
        }
        app.on_key(key(KeyCode::Enter)); // -> Detail, pinned on b
        assert_eq!(app.focus, Focus::Detail);
        assert_eq!(app.detail_agent, Some(1), "pin = agents index of b");
        // A late agent spawns into the EARLIER group g1: visible rows shift.
        spawn(&mut app, 9, "late", Some("g1"));
        let d0 = app.detail_ver;
        app.apply(AppEvent::Agent {
            id: 2, // b
            update: AgentUpdate::Message("hi".into()),
        });
        assert_ne!(app.detail_ver, d0, "pinned agent's stream must bump detail_ver");
        let d1 = app.detail_ver;
        app.apply(AppEvent::Agent {
            id: 9, // the late g1 agent
            update: AgentUpdate::Message("x".into()),
        });
        assert_eq!(app.detail_ver, d1, "non-drilled agent must not invalidate Detail");
    }

    #[test]
    fn overscroll_clamps_and_up_responds_immediately() {
        let mut app = heavy_app();
        let mut rs = RenderState::new();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.on_key(key(KeyCode::End));
        term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
        let max = app.detail_scroll;
        assert!(
            max < usize::MAX,
            "a draw must clamp the stored scroll to the real content max"
        );
        assert!(max > 0, "heavy buffer must actually be scrollable");
        // Keep scrolling at the bottom: no invisible overshoot may accumulate.
        for _ in 0..50 {
            app.on_key(key(KeyCode::Down));
            term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
        }
        assert_eq!(
            app.detail_scroll, max,
            "scrolling past the bottom must not accumulate overshoot debt"
        );
        // One Up must move the view immediately (this is the felt 'stuck').
        app.on_key(key(KeyCode::Up));
        term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
        assert_eq!(app.detail_scroll, max - 1, "one Up at the bottom must move");
    }

    #[test]
    fn scroll_keys_do_not_invalidate_render_caches() {
        let mut app = heavy_app();
        let before = (app.steps_ver, app.detail_ver);
        for k in [KeyCode::Down, KeyCode::Down, KeyCode::PageDown, KeyCode::End, KeyCode::Up] {
            app.on_key(key(k));
        }
        assert_eq!(
            (app.steps_ver, app.detail_ver),
            before,
            "pure scroll must not bust the wrap/pane caches (a wheel burst would \
             re-wrap the whole buffer once per key)"
        );
        app.on_key(key(KeyCode::Esc)); // focus change: pane inputs really changed
        assert_ne!(
            (app.steps_ver, app.detail_ver),
            before,
            "selection/focus changes must still invalidate"
        );
    }

    #[test]
    fn bottom_scroll_burst_stays_responsive() {
        let mut app = heavy_app();
        let mut rs = RenderState::new();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.on_key(key(KeyCode::End));
        term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
        // A trackpad flick in tmux: hundreds of Down keys, the loop draws per key.
        let t0 = std::time::Instant::now();
        for _ in 0..400 {
            app.on_key(key(KeyCode::Down));
            term.draw(|f| draw(f, &mut app, &mut rs)).unwrap();
        }
        let elapsed = t0.elapsed();
        eprintln!("400 bottom-scroll key+draw cycles took {elapsed:?}");
        // Pre-fix this took 35.9s; locally it's ~0.5s. The bound leaves CI
        // headroom (shared 2-core runners measured ~1.06s) while still failing
        // hard on any reintroduced per-key re-wrap.
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "a wheel burst at the bottom must not freeze the UI: 400 key+draw \
             cycles took {elapsed:?}"
        );
    }
}
