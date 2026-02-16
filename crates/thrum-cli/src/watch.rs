//! Live TUI dashboard for pipeline observability.
//!
//! Subscribes to the `EventBus` broadcast channel and renders a split-pane
//! dashboard with per-agent panels, a scrollable output log, and a bottom
//! status bar showing aggregate counts.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use thrum_core::agent::AgentId;
use thrum_core::event::{EventKind, PipelineEvent};
use thrum_core::task::TaskId;
use thrum_db::task_store::TaskStore;
use thrum_runner::parallel::PipelineContext;

/// Per-agent state tracked by the TUI.
struct AgentPanel {
    agent_id: AgentId,
    task_id: TaskId,
    task_title: String,
    repo: String,
    stage: String,
    last_tool: String,
    insertions: u32,
    deletions: u32,
    files_changed: u32,
    log_lines: Vec<String>,
    started_at: Instant,
    finished: bool,
    success: Option<bool>,
}

impl AgentPanel {
    fn elapsed_display(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        let mins = secs / 60;
        let secs = secs % 60;
        format!("{mins}m{secs:02}s")
    }

    fn diff_summary(&self) -> String {
        format!(
            "+{} -{} ~{}",
            self.insertions, self.deletions, self.files_changed
        )
    }
}

/// Top-level TUI application state.
struct WatchApp {
    agents: HashMap<String, AgentPanel>,
    /// Ordered list of agent keys for stable rendering.
    agent_order: Vec<String>,
    /// Index of the currently selected agent panel (for log scrolling).
    selected: usize,
    /// Scroll offset within the selected agent's log.
    scroll_offset: usize,
    /// Engine-level log messages.
    engine_log: Vec<String>,
    /// Cached task status counts for the bottom bar.
    queue_pending: usize,
    queue_active: usize,
    queue_total: usize,
}

impl WatchApp {
    fn new() -> Self {
        Self {
            agents: HashMap::new(),
            agent_order: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            engine_log: Vec::new(),
            queue_pending: 0,
            queue_active: 0,
            queue_total: 0,
        }
    }

    /// Process a pipeline event and update internal state.
    fn handle_event(&mut self, event: &PipelineEvent) {
        match &event.kind {
            EventKind::AgentStarted {
                agent_id,
                task_id,
                repo,
            } => {
                let key = agent_id.0.clone();
                let panel = AgentPanel {
                    agent_id: agent_id.clone(),
                    task_id: task_id.clone(),
                    task_title: String::new(),
                    repo: repo.to_string(),
                    stage: "implementing".into(),
                    last_tool: String::new(),
                    insertions: 0,
                    deletions: 0,
                    files_changed: 0,
                    log_lines: vec![format!(
                        "[{}] Agent started",
                        event.timestamp.format("%H:%M:%S")
                    )],
                    started_at: Instant::now(),
                    finished: false,
                    success: None,
                };
                self.agents.insert(key.clone(), panel);
                if !self.agent_order.contains(&key) {
                    self.agent_order.push(key);
                }
            }

            EventKind::AgentOutput { agent_id, line, .. } => {
                if let Some(panel) = self.agents.get_mut(&agent_id.0) {
                    // Extract tool usage from Claude output lines
                    if (line.contains("Tool:") || line.contains("tool_use"))
                        && let Some(tool) = extract_tool_name(line)
                    {
                        panel.last_tool = tool;
                    }
                    panel.log_lines.push(line.clone());
                    // Cap log buffer to prevent unbounded growth
                    if panel.log_lines.len() > 5000 {
                        panel.log_lines.drain(..1000);
                    }
                }
            }

            EventKind::AgentFinished {
                agent_id,
                success,
                elapsed_secs,
                ..
            } => {
                if let Some(panel) = self.agents.get_mut(&agent_id.0) {
                    panel.finished = true;
                    panel.success = Some(*success);
                    let status = if *success { "OK" } else { "FAIL" };
                    panel
                        .log_lines
                        .push(format!("Agent finished: {status} ({elapsed_secs:.1}s)"));
                }
            }

            EventKind::TaskStateChange { task_id, to, .. } => {
                // Update stage for any agent working on this task
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel.stage = to.clone();
                    }
                }
            }

            EventKind::GateStarted { task_id, level } => {
                let stage = format!("{level}");
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel.stage = stage.clone();
                        panel.log_lines.push(format!("Gate started: {level}"));
                    }
                }
            }

            EventKind::GateOutput {
                task_id,
                check_name,
                line,
                ..
            } => {
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel.log_lines.push(format!("gate/{check_name}: {line}"));
                    }
                }
            }

            EventKind::GateCheckFinished {
                task_id,
                check_name,
                passed,
                ..
            } => {
                let status = if *passed { "PASS" } else { "FAIL" };
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel.log_lines.push(format!("gate/{check_name}: {status}"));
                    }
                }
            }

            EventKind::GateFinished {
                task_id,
                level,
                passed,
                duration_secs,
            } => {
                let status = if *passed { "PASS" } else { "FAIL" };
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel
                            .log_lines
                            .push(format!("{level}: {status} ({duration_secs:.1}s)"));
                    }
                }
            }

            EventKind::FileChanged {
                agent_id,
                path,
                kind,
                ..
            } => {
                if let Some(panel) = self.agents.get_mut(&agent_id.0) {
                    let tag = match kind {
                        thrum_core::event::FileChangeKind::Created => "created",
                        thrum_core::event::FileChangeKind::Modified => "modified",
                        thrum_core::event::FileChangeKind::Deleted => "deleted",
                    };
                    panel
                        .log_lines
                        .push(format!("file {tag}: {}", path.display()));
                }
            }

            EventKind::DiffUpdate {
                agent_id,
                files_changed,
                insertions,
                deletions,
                ..
            } => {
                if let Some(panel) = self.agents.get_mut(&agent_id.0) {
                    panel.files_changed = *files_changed;
                    panel.insertions = *insertions;
                    panel.deletions = *deletions;
                }
            }

            EventKind::EngineLog { level, message } => {
                let tag = match level {
                    thrum_core::event::LogLevel::Info => "INFO",
                    thrum_core::event::LogLevel::Warn => "WARN",
                    thrum_core::event::LogLevel::Error => "ERR ",
                };
                self.engine_log.push(format!("[{tag}] {message}"));
                if self.engine_log.len() > 200 {
                    self.engine_log.drain(..50);
                }
            }

            EventKind::CheckpointSaved { task_id, phase, .. } => {
                self.engine_log
                    .push(format!("[CKPT] {task_id} checkpoint saved at {phase}"));
            }

            EventKind::SessionContinued {
                task_id,
                session_id,
                ..
            } => {
                self.engine_log.push(format!(
                    "[SESSION] {task_id} continuing session {session_id}"
                ));
            }

            // -- Agent-to-agent coordination events --
            EventKind::FileConflictDetected {
                conflict, policy, ..
            } => {
                let policy_tag = match policy {
                    thrum_core::coordination::ConflictPolicy::WarnAndContinue => "warn",
                    thrum_core::coordination::ConflictPolicy::Serialize => "serialize",
                };
                self.engine_log.push(format!(
                    "[CONFLICT/{policy_tag}] {} between {} and {} on {}",
                    conflict.path.display(),
                    conflict.first_agent,
                    conflict.second_agent,
                    conflict.repo,
                ));
                // Also notify both agent panels
                for aid in [&conflict.first_agent, &conflict.second_agent] {
                    if let Some(panel) = self.agents.get_mut(&aid.0) {
                        panel
                            .log_lines
                            .push(format!("⚠ file conflict: {}", conflict.path.display()));
                    }
                }
            }

            EventKind::CrossAgentNotification {
                source, message, ..
            } => {
                self.engine_log
                    .push(format!("[NOTIFY] {source}: {message}"));
            }

            EventKind::SharedMemoryWrite {
                agent_id,
                key,
                value,
            } => {
                self.engine_log
                    .push(format!("[SHARED] {agent_id} set {key}={value}"));
            }

            EventKind::TaskConvergenceDetected {
                task_id,
                strategy,
                repeated_count,
            } => {
                self.engine_log.push(format!(
                    "[CONVERGENCE] {task_id}: strategy={strategy}, repeats={repeated_count}"
                ));
                // Also notify the agent panel working on this task
                for panel in self.agents.values_mut() {
                    if panel.task_id == *task_id {
                        panel.log_lines.push(format!(
                            "convergence detected: {strategy} (repeats={repeated_count})"
                        ));
                    }
                }
            }
        }
    }

    /// Refresh task counts from the database.
    fn refresh_queue_counts(&mut self, ctx: &PipelineContext) {
        let store = TaskStore::new(&ctx.db);
        if let Ok(counts) = store.status_counts() {
            let pending = counts.get("pending").copied().unwrap_or(0);
            let active = counts.get("claimed").copied().unwrap_or(0)
                + counts.get("implementing").copied().unwrap_or(0)
                + counts.get("reviewing").copied().unwrap_or(0)
                + counts.get("integrating").copied().unwrap_or(0);
            let total: usize = counts.values().sum();
            self.queue_pending = pending;
            self.queue_active = active;
            self.queue_total = total;
        }

        // Also populate task titles from DB for any agents missing them
        let task_store = TaskStore::new(&ctx.db);
        for panel in self.agents.values_mut() {
            if panel.task_title.is_empty()
                && let Ok(Some(task)) = task_store.get(&panel.task_id)
            {
                panel.task_title = task.title;
            }
        }
    }

    fn active_agent_count(&self) -> usize {
        self.agents.values().filter(|p| !p.finished).count()
    }

    fn idle_agent_count(&self) -> usize {
        self.agents.values().filter(|p| p.finished).count()
    }

    fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    fn scroll_down(&mut self) {
        if let Some(key) = self.agent_order.get(self.selected)
            && let Some(panel) = self.agents.get(key)
        {
            let max = panel.log_lines.len().saturating_sub(1);
            self.scroll_offset = (self.scroll_offset + 3).min(max);
        }
    }

    fn select_prev(&mut self) {
        if !self.agent_order.is_empty() {
            if self.selected > 0 {
                self.selected -= 1;
            } else {
                self.selected = self.agent_order.len() - 1;
            }
            self.scroll_offset = 0;
        }
    }

    fn select_next(&mut self) {
        if !self.agent_order.is_empty() {
            self.selected = (self.selected + 1) % self.agent_order.len();
            self.scroll_offset = 0;
        }
    }
}

/// Try to extract a tool name from an agent output line.
fn extract_tool_name(line: &str) -> Option<String> {
    // Common patterns in Claude CLI output
    for prefix in &["Tool: ", "tool_use: ", "Using tool: "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest.split_whitespace().next().unwrap_or(rest).to_string());
        }
    }
    if line.contains("tool_use") {
        // Try to find tool name in JSON-ish output
        if let Some(start) = line.find("\"name\":") {
            let rest = &line[start + 7..];
            let rest = rest.trim().trim_start_matches('"');
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Render the TUI to the terminal frame.
fn render(frame: &mut ratatui::Frame, app: &WatchApp) {
    let size = frame.area();

    // Main layout: agent panels on top, bottom status bar
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(size);

    render_agent_panels(frame, app, main_chunks[0]);
    render_bottom_bar(frame, app, main_chunks[1]);
}

/// Render the split-pane agent panels area.
fn render_agent_panels(frame: &mut ratatui::Frame, app: &WatchApp, area: Rect) {
    if app.agent_order.is_empty() {
        // No agents yet — show a waiting message
        let msg = Paragraph::new("Waiting for agents to start...")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" thrum watch "),
            );
        frame.render_widget(msg, area);
        return;
    }

    // Split available space evenly among active agents (up to 4 visible)
    let visible_agents: Vec<&str> = app.agent_order.iter().map(|s| s.as_str()).collect();
    let num_panels = visible_agents.len().min(4);

    if num_panels == 0 {
        return;
    }

    // Arrange panels in a grid: 1 → 1×1, 2 → 2×1, 3-4 → 2×2
    if num_panels <= 2 {
        let constraints: Vec<Constraint> = (0..num_panels)
            .map(|_| Constraint::Ratio(1, num_panels as u32))
            .collect();
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);

        for (i, key) in visible_agents.iter().take(num_panels).enumerate() {
            if let Some(panel) = app.agents.get(*key) {
                let is_selected = i == app.selected;
                render_single_panel(frame, panel, chunks[i], is_selected, app.scroll_offset);
            }
        }
    } else {
        // 2×2 grid
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(area);

        let top_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(rows[0]);

        let bottom_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(rows[1]);

        let slots = [top_cols[0], top_cols[1], bottom_cols[0], bottom_cols[1]];
        for (i, key) in visible_agents.iter().take(4).enumerate() {
            if let Some(panel) = app.agents.get(*key) {
                let is_selected = i == app.selected;
                render_single_panel(frame, panel, slots[i], is_selected, app.scroll_offset);
            }
        }
    }
}

/// Render a single agent panel with header info and scrollable log.
fn render_single_panel(
    frame: &mut ratatui::Frame,
    panel: &AgentPanel,
    area: Rect,
    is_selected: bool,
    scroll_offset: usize,
) {
    // Panel border style — highlight selected panel
    let border_style = if is_selected {
        Style::default().fg(Color::Cyan)
    } else if panel.finished {
        match panel.success {
            Some(true) => Style::default().fg(Color::Green),
            Some(false) => Style::default().fg(Color::Red),
            None => Style::default().fg(Color::DarkGray),
        }
    } else {
        Style::default().fg(Color::White)
    };

    let title_text = if panel.task_title.is_empty() {
        format!(" {} | {} ", panel.task_id, panel.agent_id)
    } else {
        // Truncate title to keep panel header readable
        let max_title_len = area.width.saturating_sub(20) as usize;
        let truncated: String = panel.task_title.chars().take(max_title_len).collect();
        format!(" {} {} ", panel.task_id, truncated)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title_text);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // Split inner area: 3-line header + scrollable log
    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    // Header: stage, last tool, diff stats, elapsed
    let status_icon = if panel.finished {
        match panel.success {
            Some(true) => "✓",
            Some(false) => "✗",
            None => "?",
        }
    } else {
        "▸"
    };

    let header_lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{status_icon} "),
                Style::default().fg(if panel.finished {
                    if panel.success.unwrap_or(false) {
                        Color::Green
                    } else {
                        Color::Red
                    }
                } else {
                    Color::Yellow
                }),
            ),
            Span::styled(&panel.stage, Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                panel.elapsed_display(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("repo: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&panel.repo),
            Span::raw("  "),
            Span::styled("diff: ", Style::default().fg(Color::DarkGray)),
            Span::styled(panel.diff_summary(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("tool: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if panel.last_tool.is_empty() {
                    "—"
                } else {
                    &panel.last_tool
                },
                Style::default().fg(Color::Magenta),
            ),
        ]),
    ];

    let header = Paragraph::new(header_lines);
    frame.render_widget(header, inner_chunks[0]);

    // Log area with scrolling (only for selected panel)
    let log_height = inner_chunks[1].height as usize;
    let total_lines = panel.log_lines.len();
    let effective_offset = if is_selected {
        scroll_offset.min(total_lines.saturating_sub(log_height))
    } else {
        // For non-selected panels, auto-scroll to bottom
        total_lines.saturating_sub(log_height)
    };

    let visible_lines: Vec<ListItem> = panel
        .log_lines
        .iter()
        .skip(effective_offset)
        .take(log_height)
        .map(|line| {
            let style = if line.contains("FAIL") || line.contains("error") {
                Style::default().fg(Color::Red)
            } else if line.contains("PASS") || line.contains("OK") {
                Style::default().fg(Color::Green)
            } else if line.starts_with("gate/") {
                Style::default().fg(Color::Blue)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(Span::styled(line.as_str(), style)))
        })
        .collect();

    let log_list = List::new(visible_lines);
    frame.render_widget(log_list, inner_chunks[1]);
}

/// Render the bottom status bar.
fn render_bottom_bar(frame: &mut ratatui::Frame, app: &WatchApp, area: Rect) {
    let active = app.active_agent_count();
    let idle = app.idle_agent_count();
    let total = active + idle;

    let status_line = Line::from(vec![
        Span::styled(" Agents: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{active}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" active ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{idle}"), Style::default().fg(Color::Yellow)),
        Span::styled(" idle ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{total}"), Style::default().fg(Color::White)),
        Span::styled(" total", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled("Queue: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{}", app.queue_pending),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(" pending ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", app.queue_active),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" active ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", app.queue_total),
            Style::default().fg(Color::White),
        ),
        Span::styled(" total", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled(
            "Ctrl+Q",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "←→",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" panel  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "↑↓",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll", Style::default().fg(Color::DarkGray)),
    ]);

    let bar = Paragraph::new(status_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(bar, area);
}

/// Set up the terminal for TUI rendering.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to normal mode.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Main entry point: run the watch TUI connected to the pipeline event bus.
pub async fn run_watch_tui(ctx: Arc<PipelineContext>) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut app = WatchApp::new();
    let mut rx = ctx.event_bus.subscribe();

    // Refresh queue counts from DB on a timer
    let mut last_db_refresh = Instant::now();
    let db_refresh_interval = Duration::from_secs(2);

    // Initial DB refresh
    app.refresh_queue_counts(&ctx);

    let tick_rate = Duration::from_millis(100);

    loop {
        // Draw
        terminal.draw(|frame| render(frame, &app))?;

        // Poll for crossterm input events with a short timeout
        if event::poll(tick_rate)?
            && let Event::Key(key) = event::read()?
        {
            match key {
                // Ctrl+Q or 'q' to quit
                KeyEvent {
                    code: KeyCode::Char('q'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('q'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    break;
                }
                // Arrow keys for navigation
                KeyEvent {
                    code: KeyCode::Up, ..
                } => app.scroll_up(),
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                } => app.scroll_down(),
                KeyEvent {
                    code: KeyCode::Left,
                    ..
                } => app.select_prev(),
                KeyEvent {
                    code: KeyCode::Right,
                    ..
                } => app.select_next(),
                // Page up/down for faster scrolling
                KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                } => {
                    for _ in 0..10 {
                        app.scroll_up();
                    }
                }
                KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                } => {
                    for _ in 0..10 {
                        app.scroll_down();
                    }
                }
                _ => {}
            }
        }

        // Drain all pending events from the broadcast channel
        loop {
            match rx.try_recv() {
                Ok(event) => app.handle_event(&event),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    app.engine_log
                        .push(format!("[WARN] Lagged: missed {n} events"));
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    break;
                }
            }
        }

        // Periodic DB refresh for queue counts
        if last_db_refresh.elapsed() >= db_refresh_interval {
            app.refresh_queue_counts(&ctx);
            last_db_refresh = Instant::now();
        }
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::event::{EventKind, FileChangeKind, LogLevel, OutputStream, PipelineEvent};
    use thrum_core::task::{GateLevel, RepoName};

    fn make_event(kind: EventKind) -> PipelineEvent {
        PipelineEvent::new(kind)
    }

    #[test]
    fn agent_started_creates_panel() {
        let mut app = WatchApp::new();
        let event = make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1-loom-TASK-0001".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        });
        app.handle_event(&event);

        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agent_order.len(), 1);
        assert_eq!(app.agent_order[0], "agent-1-loom-TASK-0001");

        let panel = app.agents.get("agent-1-loom-TASK-0001").unwrap();
        assert_eq!(panel.repo, "loom");
        assert_eq!(panel.stage, "implementing");
        assert!(!panel.finished);
    }

    #[test]
    fn agent_output_appends_to_log() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::AgentOutput {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            stream: OutputStream::Stdout,
            line: "compiling...".into(),
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert!(panel.log_lines.iter().any(|l| l == "compiling..."));
    }

    #[test]
    fn agent_finished_marks_done() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::AgentFinished {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            success: true,
            elapsed_secs: 42.0,
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert!(panel.finished);
        assert_eq!(panel.success, Some(true));
    }

    #[test]
    fn diff_update_tracks_stats() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::DiffUpdate {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            files_changed: 5,
            insertions: 100,
            deletions: 20,
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert_eq!(panel.insertions, 100);
        assert_eq!(panel.deletions, 20);
        assert_eq!(panel.files_changed, 5);
        assert_eq!(panel.diff_summary(), "+100 -20 ~5");
    }

    #[test]
    fn task_state_change_updates_stage() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::TaskStateChange {
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
            from: "implementing".into(),
            to: "reviewing".into(),
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert_eq!(panel.stage, "reviewing");
    }

    #[test]
    fn engine_log_captured() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::EngineLog {
            level: LogLevel::Warn,
            message: "budget running low".into(),
        }));

        assert_eq!(app.engine_log.len(), 1);
        assert!(app.engine_log[0].contains("budget running low"));
    }

    #[test]
    fn navigation_wraps_around() {
        let mut app = WatchApp::new();
        // Add 3 agents
        for i in 0..3 {
            app.handle_event(&make_event(EventKind::AgentStarted {
                agent_id: AgentId(format!("agent-{i}")),
                task_id: TaskId(i),
                repo: RepoName::new("loom"),
            }));
        }

        assert_eq!(app.selected, 0);
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 2);
        app.select_next();
        assert_eq!(app.selected, 0); // Wrapped

        app.select_prev();
        assert_eq!(app.selected, 2); // Wrapped backward
    }

    #[test]
    fn active_idle_counts() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-2".into()),
            task_id: TaskId(2),
            repo: RepoName::new("synth"),
        }));

        assert_eq!(app.active_agent_count(), 2);
        assert_eq!(app.idle_agent_count(), 0);

        app.handle_event(&make_event(EventKind::AgentFinished {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            success: true,
            elapsed_secs: 10.0,
        }));

        assert_eq!(app.active_agent_count(), 1);
        assert_eq!(app.idle_agent_count(), 1);
    }

    #[test]
    fn extract_tool_name_patterns() {
        assert_eq!(extract_tool_name("Tool: Read"), Some("Read".into()));
        assert_eq!(
            extract_tool_name(r#"{"name":"Write","type":"tool_use"}"#),
            Some("Write".into())
        );
        assert_eq!(extract_tool_name("regular output"), None);
    }

    #[test]
    fn file_changed_appends_to_log() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::FileChanged {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            path: "src/main.rs".into(),
            kind: FileChangeKind::Modified,
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert!(
            panel
                .log_lines
                .iter()
                .any(|l| l.contains("file modified: src/main.rs"))
        );
    }

    #[test]
    fn gate_events_update_panel() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));
        app.handle_event(&make_event(EventKind::GateStarted {
            task_id: TaskId(1),
            level: GateLevel::Quality,
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert!(panel.stage.contains("Quality"));
        assert!(panel.log_lines.iter().any(|l| l.contains("Gate started")));

        app.handle_event(&make_event(EventKind::GateCheckFinished {
            task_id: TaskId(1),
            level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            passed: true,
        }));

        let panel = app.agents.get("agent-1").unwrap();
        assert!(
            panel
                .log_lines
                .iter()
                .any(|l| l.contains("gate/cargo_test: PASS"))
        );
    }

    #[test]
    fn log_buffer_capped() {
        let mut app = WatchApp::new();
        app.handle_event(&make_event(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        }));

        // Push 6000 lines
        for i in 0..6000 {
            app.handle_event(&make_event(EventKind::AgentOutput {
                agent_id: AgentId("agent-1".into()),
                task_id: TaskId(1),
                stream: OutputStream::Stdout,
                line: format!("line {i}"),
            }));
        }

        let panel = app.agents.get("agent-1").unwrap();
        // Should have been trimmed (5000 cap minus 1000 drain = ~4000-5000 range)
        assert!(panel.log_lines.len() <= 5001);
    }
}
