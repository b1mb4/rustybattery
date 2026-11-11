use crate::apps::{AppDeltaUsage, AppEnergyTracker};
use crate::battery::{read_battery_snapshot, BatterySnapshot, BatteryStatus};
use crate::db::{AppSummaryRow, BatteryHistoryPoint, Database};
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, stop_or_kill_app,
    OptimizationTarget,
};
use crate::service::{install_systemd_service, is_daemon_active, is_service_file_installed};
use crate::statusbar::send_status_notification;
use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Clear, Dataset, Gauge, GraphType, List, ListItem,
        Paragraph, Row, Sparkline, Table, TableState, Tabs, Wrap,
    },
    Frame, Terminal,
};
use std::io;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeWindow {
    SinceCharge,
    Last6Hours,
    Last24Hours,
    Last7Days,
}

impl TimeWindow {
    pub fn all() -> &'static [TimeWindow] {
        &[
            TimeWindow::SinceCharge,
            TimeWindow::Last6Hours,
            TimeWindow::Last24Hours,
            TimeWindow::Last7Days,
        ]
    }

    pub fn title(&self) -> &'static str {
        match self {
            TimeWindow::SinceCharge => "1: Since Full Charge",
            TimeWindow::Last6Hours => "2: Last 6 Hours",
            TimeWindow::Last24Hours => "3: Last 24 Hours",
            TimeWindow::Last7Days => "4: Last 7 Days",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            TimeWindow::SinceCharge => TimeWindow::Last6Hours,
            TimeWindow::Last6Hours => TimeWindow::Last24Hours,
            TimeWindow::Last24Hours => TimeWindow::Last7Days,
            TimeWindow::Last7Days => TimeWindow::SinceCharge,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            TimeWindow::SinceCharge => TimeWindow::Last7Days,
            TimeWindow::Last6Hours => TimeWindow::SinceCharge,
            TimeWindow::Last24Hours => TimeWindow::Last6Hours,
            TimeWindow::Last7Days => TimeWindow::Last24Hours,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    BatteryPct,
    EnergyWh,
    LivePowerW,
    LiveCpuPct,
    MemoryMb,
}

impl SortColumn {
    pub fn next(&self) -> Self {
        match self {
            SortColumn::BatteryPct => SortColumn::EnergyWh,
            SortColumn::EnergyWh => SortColumn::LivePowerW,
            SortColumn::LivePowerW => SortColumn::LiveCpuPct,
            SortColumn::LiveCpuPct => SortColumn::MemoryMb,
            SortColumn::MemoryMb => SortColumn::BatteryPct,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SortColumn::BatteryPct => "Battery %",
            SortColumn::EnergyWh => "Energy (Wh)",
            SortColumn::LivePowerW => "Live Power (W)",
            SortColumn::LiveCpuPct => "Live CPU %",
            SortColumn::MemoryMb => "Memory (MB)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryFilter {
    All,
    UserAppsOnly,
    SystemAndDesktop,
}

impl CategoryFilter {
    pub fn next(&self) -> Self {
        match self {
            CategoryFilter::All => CategoryFilter::UserAppsOnly,
            CategoryFilter::UserAppsOnly => CategoryFilter::SystemAndDesktop,
            CategoryFilter::SystemAndDesktop => CategoryFilter::All,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CategoryFilter::All => "All Apps & Services",
            CategoryFilter::UserAppsOnly => "User Apps Only",
            CategoryFilter::SystemAndDesktop => "Desktop & System",
        }
    }

    pub fn matches(&self, category: &str) -> bool {
        match self {
            CategoryFilter::All => true,
            CategoryFilter::UserAppsOnly => matches!(
                category,
                "Browser"
                    | "Development"
                    | "Productivity"
                    | "Terminal"
                    | "Audio & Media"
                    | "Social & Chat"
                    | "Gaming"
                    | "App"
            ),
            CategoryFilter::SystemAndDesktop => {
                !CategoryFilter::UserAppsOnly.matches(category)
            }
        }
    }
}

pub struct AppUi {
    pub db: Database,
    pub tracker: AppEnergyTracker,
    pub battery: BatterySnapshot,
    pub history: Vec<BatteryHistoryPoint>,
    pub apps: Vec<AppSummaryRow>,
    pub live_deltas: Vec<AppDeltaUsage>,
    pub selected_app_buckets: Vec<(i64, f64, f64)>,
    pub window: TimeWindow,
    pub sort_by: SortColumn,
    pub filter: CategoryFilter,
    pub show_charts: bool,
    pub table_state: TableState,
    pub daemon_running: bool,
    pub service_installed: bool,
    pub status_message: Option<(String, Instant)>,
    pub since_ts: i64,
    pub optimizer_modal: Option<(Vec<OptimizationTarget>, usize)>,
}

impl AppUi {
    pub fn new(db: Database) -> Result<Self> {
        let battery = read_battery_snapshot()?;
        db.ensure_initial_seed(&battery)?;
        db.record_battery_snapshot(&battery)?;

        let tracker = AppEnergyTracker::new();
        let mut ui = Self {
            db,
            tracker,
            battery,
            history: Vec::new(),
            apps: Vec::new(),
            live_deltas: Vec::new(),
            selected_app_buckets: Vec::new(),
            window: TimeWindow::SinceCharge,
            sort_by: SortColumn::BatteryPct,
            filter: CategoryFilter::All,
            show_charts: true,
            table_state: TableState::default(),
            daemon_running: false,
            service_installed: is_service_file_installed(),
            status_message: None,
            since_ts: 0,
            optimizer_modal: None,
        };
        ui.table_state.select(Some(0));
        ui.refresh_data(false)?;
        Ok(ui)
    }

    pub fn compute_since_ts(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        match self.window {
            TimeWindow::SinceCharge => self.db.get_last_full_charge_ts(),
            TimeWindow::Last6Hours => now - 6 * 3600,
            TimeWindow::Last24Hours => now - 24 * 3600,
            TimeWindow::Last7Days => now - 7 * 86400,
        }
    }

    pub fn refresh_data(&mut self, sample_live_tick: bool) -> Result<()> {
        if let Ok(snap) = read_battery_snapshot() {
            self.battery = snap;
        }

        self.daemon_running = is_daemon_active(&self.db);
        self.service_installed = is_service_file_installed();

        if sample_live_tick {
            let (deltas, _) = self.tracker.sample_delta(
                self.battery.power_w,
                self.battery.energy_full_wh,
                self.battery.status.is_discharging(),
            );
            // If the background daemon is NOT currently active, the TUI itself persists samples to SQLite!
            if !self.daemon_running {
                let _ = self.db.record_battery_snapshot(&self.battery);
                let _ = self.db.record_app_deltas(&deltas);
            }
            self.live_deltas = deltas;
        }

        self.since_ts = self.compute_since_ts();
        self.history = self.db.get_battery_history(self.since_ts).unwrap_or_default();

        let mut all_apps = self
            .db
            .get_app_breakdown(self.since_ts, &self.live_deltas)
            .unwrap_or_default();

        all_apps.retain(|r| self.filter.matches(&r.category) && (r.battery_pct >= 0.005 || r.live_cpu_pct >= 0.1 || r.memory_mb >= 15.0));

        // Recompute share_of_drain_pct within filtered view
        let total_drain: f64 = all_apps.iter().map(|a| a.battery_pct).sum();
        if total_drain > 0.0 {
            for a in &mut all_apps {
                a.share_of_drain_pct = (a.battery_pct / total_drain) * 100.0;
            }
        }

        all_apps.sort_by(|a, b| {
            let cmp = match self.sort_by {
                SortColumn::BatteryPct => b.battery_pct.partial_cmp(&a.battery_pct),
                SortColumn::EnergyWh => b.energy_wh.partial_cmp(&a.energy_wh),
                SortColumn::LivePowerW => b.live_power_w.partial_cmp(&a.live_power_w),
                SortColumn::LiveCpuPct => b.live_cpu_pct.partial_cmp(&a.live_cpu_pct),
                SortColumn::MemoryMb => b.memory_mb.partial_cmp(&a.memory_mb),
            };
            cmp.unwrap_or(std::cmp::Ordering::Equal)
        });

        self.apps = all_apps;

        if self.apps.is_empty() {
            self.table_state.select(None);
            self.selected_app_buckets.clear();
        } else {
            let idx = self
                .table_state
                .selected()
                .unwrap_or(0)
                .min(self.apps.len() - 1);
            self.table_state.select(Some(idx));
            self.update_selected_app_buckets();
        }

        Ok(())
    }

    pub fn update_selected_app_buckets(&mut self) {
        if let Some(idx) = self.table_state.selected() {
            if let Some(app) = self.apps.get(idx) {
                self.selected_app_buckets =
                    self.db.get_app_buckets(&app.app_name, self.since_ts);
                return;
            }
        }
        self.selected_app_buckets.clear();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.apps.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, (self.apps.len() as isize) - 1) as usize;
        self.table_state.select(Some(next));
        self.update_selected_app_buckets();
    }
}

pub fn run_tui(db: Database) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_tui_loop(&mut terminal, db);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    db: Database,
) -> Result<()> {
    let mut app = AppUi::new(db)?;
    let tick_rate = Duration::from_millis(2000);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if let Some((targets, sel_idx)) = &mut app.optimizer_modal {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.optimizer_modal = None;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !targets.is_empty() {
                                    *sel_idx = (*sel_idx + 1).min(targets.len() - 1);
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                *sel_idx = sel_idx.saturating_sub(1);
                            }
                            KeyCode::Char(' ') => {
                                if let Some(t) = targets.get_mut(*sel_idx) {
                                    t.selected = !t.selected;
                                }
                            }
                            KeyCode::Char('a') => {
                                let all_selected = targets.iter().all(|t| t.selected);
                                for t in targets.iter_mut() {
                                    t.selected = !all_selected;
                                }
                            }
                            KeyCode::Enter | KeyCode::Char('o') => {
                                let chosen = targets.clone();
                                app.optimizer_modal = None;
                                let report = execute_optimizations(&chosen);
                                app.refresh_data(true)?;
                                if !report.actions_taken.is_empty() {
                                    app.status_message = Some((
                                        format!(
                                            "⚡ Optimized {} target(s): freed {:.0} MB RAM, stopped {} threads (~{:.1} W saved)",
                                            report.actions_taken.len(),
                                            report.freed_memory_mb,
                                            report.stopped_pids,
                                            report.estimated_watts_saved
                                        ),
                                        Instant::now(),
                                    ));
                                } else if let Some(err) = report.failed_actions.first() {
                                    app.status_message = Some((err.clone(), Instant::now()));
                                } else {
                                    app.status_message = Some((
                                        "No optimization targets were selected.".to_string(),
                                        Instant::now(),
                                    ));
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                        KeyCode::PageDown => app.move_selection(8),
                        KeyCode::PageUp => app.move_selection(-8),
                        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                            app.window = app.window.next();
                            app.refresh_data(false)?;
                        }
                        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                            app.window = app.window.prev();
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('1') => {
                            app.window = TimeWindow::SinceCharge;
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('2') => {
                            app.window = TimeWindow::Last6Hours;
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('3') => {
                            app.window = TimeWindow::Last24Hours;
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('4') => {
                            app.window = TimeWindow::Last7Days;
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('s') => {
                            app.sort_by = app.sort_by.next();
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('f') => {
                            app.filter = app.filter.next();
                            app.refresh_data(false)?;
                        }
                        KeyCode::Char('c') => {
                            app.show_charts = !app.show_charts;
                        }
                        KeyCode::Char('o') => {
                            let targets = scan_optimization_targets(app.battery.power_w);
                            app.optimizer_modal = Some((targets, 0));
                        }
                        KeyCode::Char('z') => {
                            if let Some(idx) = app.table_state.selected() {
                                if let Some(sel) = app.apps.get(idx).cloned() {
                                    let next_freeze = !sel.is_frozen;
                                    match set_app_frozen(&sel.app_name, next_freeze) {
                                        Ok(msg) => {
                                            app.refresh_data(true)?;
                                            app.status_message = Some((msg, Instant::now()));
                                        }
                                        Err(e) => {
                                            app.status_message =
                                                Some((format!("Freeze error: {e}"), Instant::now()));
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('x') => {
                            if let Some(idx) = app.table_state.selected() {
                                if let Some(sel) = app.apps.get(idx).cloned() {
                                    match stop_or_kill_app(&sel.app_name) {
                                        Ok(msg) => {
                                            app.refresh_data(true)?;
                                            app.status_message = Some((msg, Instant::now()));
                                        }
                                        Err(e) => {
                                            app.status_message =
                                                Some((format!("Stop error: {e}"), Instant::now()));
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('r') => {
                            app.refresh_data(true)?;
                            app.status_message =
                                Some(("Refreshed live battery & app stats".to_string(), Instant::now()));
                        }
                        KeyCode::Char('n') => {
                            let _ = send_status_notification(&app.battery, &app.apps);
                            app.status_message = Some((
                                "🔔 Sent desktop notification with battery & top apps summary".to_string(),
                                Instant::now(),
                            ));
                        }
                        KeyCode::Char('i') => {
                            match install_systemd_service(5) {
                                Ok(msg) => {
                                    let first_line = msg.lines().next().unwrap_or("Service installed").to_string();
                                    app.status_message = Some((first_line, Instant::now()));
                                    app.service_installed = is_service_file_installed();
                                }
                                Err(e) => {
                                    app.status_message = Some((format!("Service error: {e}"), Instant::now()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.refresh_data(true)?;
            last_tick = Instant::now();
        }
    }
}

fn draw_ui(f: &mut Frame, app: &mut AppUi) {
    let size = f.area();

    let constraints = if app.show_charts && size.height >= 26 {
        vec![
            Constraint::Length(5),  // Top phone battery header card
            Constraint::Length(3),  // Time window tabs + KPI strip
            Constraint::Length(10), // Battery % & Wattage charts
            Constraint::Min(8),     // Per-app table + App inspector
            Constraint::Length(2),  // Footer keybindings
        ]
    } else {
        vec![
            Constraint::Length(5), // Top phone battery header card
            Constraint::Length(3), // Time window tabs + KPI strip
            Constraint::Min(8),    // Per-app table + App inspector
            Constraint::Length(2), // Footer keybindings
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(size);

    draw_battery_header(f, app, chunks[0]);
    draw_tabs_and_kpis(f, app, chunks[1]);

    if app.show_charts && size.height >= 26 {
        draw_charts(f, app, chunks[2]);
        draw_apps_section(f, app, chunks[3]);
        draw_footer(f, app, chunks[4]);
    } else {
        draw_apps_section(f, app, chunks[2]);
        draw_footer(f, app, chunks[3]);
    }

    if app.optimizer_modal.is_some() {
        draw_optimizer_modal(f, app, size);
    }
}

fn draw_battery_header(f: &mut Frame, app: &AppUi, area: Rect) {
    let b = &app.battery;

    let (bat_color, status_badge) = match b.status {
        BatteryStatus::Charging => (Color::Cyan, "⚡ CHARGING"),
        BatteryStatus::Full => (Color::Green, "✓ FULL CHARGE"),
        BatteryStatus::NotCharging => (Color::Blue, "🔌 PLUGGED IN"),
        BatteryStatus::Discharging => {
            if b.capacity_pct >= 50.0 {
                (Color::Green, "🔋 ON BATTERY")
            } else if b.capacity_pct >= 20.0 {
                (Color::Yellow, "🔋 ON BATTERY")
            } else {
                (Color::Red, "⚠ LOW BATTERY")
            }
        }
        BatteryStatus::Unknown => (Color::Gray, "• UNKNOWN"),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " 🦀 RustyBattery — Per-App Power Tracker for Linux ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(36),
            Constraint::Percentage(30),
        ])
        .split(inner);

    // Left: Big Battery Gauge
    let time_str = match b.time_remaining_mins {
        Some(mins) => {
            let h = mins / 60;
            let m = mins % 60;
            if b.status == BatteryStatus::Charging {
                format!("Full in {h}h {m:02}m")
            } else {
                format!("~{h}h {m:02}m left")
            }
        }
        None => " Calculating...".to_string(),
    };

    let gauge_label = format!(
        "{:.1}%  {}  ({})",
        b.capacity_pct, status_badge, time_str
    );

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::NONE))
        .gauge_style(
            Style::default()
                .fg(bat_color)
                .bg(Color::Rgb(30, 34, 42))
                .add_modifier(Modifier::BOLD),
        )
        .ratio((b.capacity_pct / 100.0).clamp(0.0, 1.0))
        .label(Span::styled(
            gauge_label,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));

    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(1)])
        .split(cols[0]);

    f.render_widget(gauge, left_rows[0]);

    let sub_info = Line::from(vec![
        Span::styled(" Energy: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.1} / {:.1} Wh", b.energy_now_wh, b.energy_full_wh),
            Style::default().fg(Color::Gray),
        ),
        Span::styled("  •  Voltage: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:.2} V", b.voltage_v), Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(sub_info), left_rows[1]);

    // Center: Live Power & Battery Health
    let power_color = if b.power_w > 18.0 {
        Color::LightRed
    } else if b.power_w > 11.0 {
        Color::Yellow
    } else {
        Color::LightGreen
    };

    let center_lines = vec![
        Line::from(vec![
            Span::styled("   Live Power: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2} W", b.power_w),
                Style::default()
                    .fg(power_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if b.ac_online { " (AC Connected)" } else { " (Battery Drain)" },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("   Health:     ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}%", b.health_pct),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " ({} cycles · {:.1} Wh design)",
                    b.cycle_count, b.energy_design_wh
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("   Hardware:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{} {} ({})", b.manufacturer, b.model_name, b.technology),
                Style::default().fg(Color::Gray),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(center_lines), cols[1]);

    // Right: Daemon / Collector Status & Last Full Charge
    let last_charge_ts = app.db.get_last_full_charge_ts();
    let hours_ago = ((chrono::Utc::now().timestamp() - last_charge_ts).max(0) as f64) / 3600.0;

    let daemon_span = if app.daemon_running {
        Span::styled(
            "● 24/7 Service Active",
            Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        )
    } else if app.service_installed {
        Span::styled(
            "◐ Service Installed (Live TUI)",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "◉ Live Collector ('i' install svc)",
            Style::default().fg(Color::Yellow),
        )
    };

    let right_lines = vec![
        Line::from(vec![
            Span::styled("Collector: ", Style::default().fg(Color::Gray)),
            daemon_span,
        ]),
        Line::from(vec![
            Span::styled("Last Full: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}h ago", hours_ago),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Sort By:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{} [s]", app.sort_by.label()),
                Style::default().fg(Color::LightCyan),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(right_lines).alignment(Alignment::Left),
        cols[2],
    );
}

fn draw_tabs_and_kpis(f: &mut Frame, app: &AppUi, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    let titles: Vec<Line> = TimeWindow::all()
        .iter()
        .map(|w| Line::from(w.title()))
        .collect();

    let selected_idx = TimeWindow::all()
        .iter()
        .position(|&w| w == app.window)
        .unwrap_or(0);

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" Time Range (Tab / 1-4) "),
        )
        .select(selected_idx)
        .style(Style::default().fg(Color::Gray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, cols[0]);

    // Calculate summary stats for selected window
    let total_app_pct: f64 = app.apps.iter().map(|a| a.battery_pct).sum();
    let total_app_wh: f64 = app.apps.iter().map(|a| a.energy_wh).sum();
    let avg_watts = if !app.history.is_empty() {
        let discharging: Vec<f64> = app
            .history
            .iter()
            .filter(|p| p.power_w > 0.5)
            .map(|p| p.power_w)
            .collect();
        if discharging.is_empty() {
            app.battery.power_w
        } else {
            discharging.iter().sum::<f64>() / (discharging.len() as f64)
        }
    } else {
        app.battery.power_w
    };

    let kpi_line = Line::from(vec![
        Span::styled(" Drain Used: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:.1}%", total_app_pct),
            Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({:.1} Wh)", total_app_wh),
            Style::default().fg(Color::Gray),
        ),
        Span::styled("  │  Avg Power: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.1} W", avg_watts),
            Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  View: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.filter.label(),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
    ]);

    let kpi_block = Paragraph::new(kpi_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Period Summary & Filter [f] "),
    );
    f.render_widget(kpi_block, cols[1]);
}

fn draw_charts(f: &mut Frame, app: &AppUi, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    // 1. Left Chart: Battery Level (%) Curve
    let mut pct_points: Vec<(f64, f64)> = app
        .history
        .iter()
        .map(|p| (p.timestamp as f64, p.capacity_pct))
        .collect();

    if pct_points.is_empty() {
        pct_points.push((app.battery.timestamp as f64 - 60.0, app.battery.capacity_pct));
        pct_points.push((app.battery.timestamp as f64, app.battery.capacity_pct));
    }

    let min_x = pct_points.first().map(|p| p.0).unwrap_or(0.0);
    let max_x = pct_points
        .last()
        .map(|p| p.0)
        .unwrap_or(1.0)
        .max(min_x + 60.0);
    let mid_x = (min_x + max_x) / 2.0;

    let fmt_ts = |ts: f64| -> String {
        Local
            .timestamp_opt(ts as i64, 0)
            .single()
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_default()
    };

    let datasets = vec![Dataset::default()
        .name("Battery %")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::LightGreen))
        .data(&pct_points)];

    let bat_chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " 📈 Battery Level (%) History [c: hide] ",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([min_x, max_x])
                .labels(vec![
                    Span::raw(fmt_ts(min_x)),
                    Span::raw(fmt_ts(mid_x)),
                    Span::raw(fmt_ts(max_x)),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, 100.0])
                .labels(vec![
                    Span::raw("  0%"),
                    Span::raw(" 50%"),
                    Span::raw("100%"),
                ]),
        );

    f.render_widget(bat_chart, cols[0]);

    // 2. Right Chart: Power Discharge Rate (Watts) Curve
    let mut watt_points: Vec<(f64, f64)> = app
        .history
        .iter()
        .map(|p| (p.timestamp as f64, p.power_w))
        .collect();

    if watt_points.is_empty() {
        watt_points.push((app.battery.timestamp as f64 - 60.0, app.battery.power_w));
        watt_points.push((app.battery.timestamp as f64, app.battery.power_w));
    }

    let max_watts = watt_points
        .iter()
        .map(|p| p.1)
        .fold(20.0_f64, f64::max)
        .ceil();

    let watt_datasets = vec![Dataset::default()
        .name("Watts")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::LightYellow))
        .data(&watt_points)];

    let watt_chart = Chart::new(watt_datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " ⚡ Power Draw (Watts) ",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([min_x, max_x])
                .labels(vec![Span::raw(fmt_ts(min_x)), Span::raw(fmt_ts(max_x))]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, max_watts])
                .labels(vec![
                    Span::raw("0W"),
                    Span::raw(format!("{:.0}W", max_watts / 2.0)),
                    Span::raw(format!("{:.0}W", max_watts)),
                ]),
        );

    f.render_widget(watt_chart, cols[1]);
}

fn category_icon_and_color(category: &str) -> (&'static str, Color) {
    match category {
        "Browser" => ("🌐", Color::LightCyan),
        "Development" => ("💻", Color::LightBlue),
        "Productivity" => ("📝", Color::LightMagenta),
        "Terminal" => ("⌨ ", Color::LightGreen),
        "Desktop UI" => ("🖥 ", Color::Yellow),
        "Audio & Media" => ("🔊", Color::Magenta),
        "Social & Chat" => ("💬", Color::Cyan),
        "Gaming" => ("🎮", Color::LightRed),
        "Connectivity" => ("📡", Color::Blue),
        "Background" => ("⚙ ", Color::Gray),
        "System" => ("🔧", Color::DarkGray),
        _ => ("📦", Color::White),
    }
}

fn format_duration_short(secs: f64) -> String {
    let total = secs.round() as u64;
    if total >= 3600 {
        format!("{}h {:02}m", total / 3600, (total % 3600) / 60)
    } else if total >= 60 {
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        format!("{}s", total)
    }
}

fn make_usage_bar(share_pct: f64, width: usize) -> String {
    let filled_exact = (share_pct.clamp(0.0, 100.0) / 100.0) * (width as f64);
    let full_blocks = filled_exact.floor() as usize;
    let rem = filled_exact - (full_blocks as f64);
    let mut s = String::with_capacity(width * 3);
    for _ in 0..full_blocks.min(width) {
        s.push('█');
    }
    if full_blocks < width {
        if rem >= 0.5 {
            s.push('▌');
        } else {
            s.push('░');
        }
        for _ in (full_blocks + 1)..width {
            s.push('░');
        }
    }
    s
}

fn draw_apps_section(f: &mut Frame, app: &mut AppUi, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    // 1. Left: Phone-Style Per-App Battery Usage Table
    let header_cells = [
        "#",
        "Application",
        "Category",
        "Battery Share",
        "Batt %",
        "Energy",
        "Live W",
        "CPU Time",
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::Rgb(24, 28, 36)))
        .height(1);

    let rows: Vec<Row> = app
        .apps
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let (icon, cat_color) = category_icon_and_color(&row.category);
            let bar = make_usage_bar(row.share_of_drain_pct, 10);

            let pct_color = if row.share_of_drain_pct >= 25.0 {
                Color::LightRed
            } else if row.share_of_drain_pct >= 10.0 {
                Color::LightYellow
            } else {
                Color::LightGreen
            };

            let name_display = if row.is_frozen {
                format!("❄ {} [FROZEN]", row.app_name)
            } else {
                format!("{} {}", icon, row.app_name)
            };

            let name_style = if row.is_frozen {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.pids_count == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };

            Row::new(vec![
                Cell::from(format!("{:>2}", i + 1)).style(Style::default().fg(Color::DarkGray)),
                Cell::from(name_display).style(name_style),
                Cell::from(row.category.clone()).style(Style::default().fg(cat_color)),
                Cell::from(format!("{} {:>4.0}%", bar, row.share_of_drain_pct))
                    .style(Style::default().fg(pct_color)),
                Cell::from(format!("{:>5.2}%", row.battery_pct))
                    .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Cell::from(format!("{:>5.2} Wh", row.energy_wh))
                    .style(Style::default().fg(Color::Gray)),
                Cell::from(format!("{:>4.2} W", row.live_power_w)).style(
                    if row.live_power_w >= 2.0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Cell::from(format!("{:>7}", format_duration_short(row.cpu_secs)))
                    .style(Style::default().fg(Color::Gray)),
            ])
        })
        .collect();

    let table_title = format!(
        " 🔋 Battery Usage by Application ({} apps) — Sorted by {} ",
        app.apps.len(),
        app.sort_by.label()
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(18),
            Constraint::Length(13),
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                table_title,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(42, 54, 72))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, cols[0], &mut app.table_state);

    // 2. Right: Phone-Style Selected App Inspector Card
    draw_app_inspector(f, app, cols[1]);
}

fn draw_app_inspector(f: &mut Frame, app: &AppUi, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " 🔍 App Battery Details ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(idx) = app.table_state.selected() else {
        f.render_widget(
            Paragraph::new("No application selected.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let Some(sel) = app.apps.get(idx) else {
        return;
    };

    let (icon, cat_color) = category_icon_and_color(&sel.category);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(4),
        ])
        .split(inner);

    let mwh = sel.energy_wh * 1000.0;
    let bar = make_usage_bar(sel.share_of_drain_pct, 16);

    let items = vec![
        ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {} {} ", icon, sel.app_name),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[{}]", sel.category),
                Style::default().fg(cat_color),
            ),
        ])),
        ListItem::new(Line::from("")),
        ListItem::new(Line::from(vec![
            Span::styled(" Share of Drain: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{} {:.1}%", bar, sel.share_of_drain_pct),
                Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Battery Used:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2}% of battery capacity", sel.battery_pct),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Energy Used:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2} Wh  ({:.0} mWh)", sel.energy_wh, mwh),
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Live Power:     ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2} W  (Peak: {:.2} W)", sel.live_power_w, sel.peak_power_w),
                Style::default().fg(Color::LightGreen),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Live CPU Load:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}%", sel.live_cpu_pct),
                Style::default().fg(Color::White),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Active Time:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!(
                    "{}  (CPU: {})",
                    format_duration_short(sel.active_secs),
                    format_duration_short(sel.cpu_secs)
                ),
                Style::default().fg(Color::White),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Memory & PIDs:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1} MB  ({} procs/threads)", sel.memory_mb, sel.pids_count),
                Style::default().fg(Color::Gray),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Systemd Unit:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sel.unit_hint.chars().take(28).collect::<String>(),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" App Controls:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                if sel.is_frozen {
                    "[z] Resume App  "
                } else {
                    "[z] Freeze (0% CPU)  "
                },
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "[x] Stop App",
                Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
            ),
        ])),
    ];

    f.render_widget(List::new(items), sections[0]);

    // Mini Sparkline of this app's recent energy buckets
    let spark_data: Vec<u64> = if app.selected_app_buckets.is_empty() {
        vec![(sel.battery_pct * 100.0).round() as u64; 8]
    } else {
        app.selected_app_buckets
            .iter()
            .map(|&(_, pct, _)| (pct * 1000.0).round().max(1.0) as u64)
            .collect()
    };

    let spark = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " Activity Trend ",
                    Style::default().fg(Color::DarkGray),
                )),
        )
        .data(&spark_data)
        .style(Style::default().fg(Color::Cyan));

    f.render_widget(spark, sections[1]);
}

fn draw_footer(f: &mut Frame, app: &AppUi, area: Rect) {
    if let Some((msg, ts)) = &app.status_message {
        if ts.elapsed() < Duration::from_secs(6) {
            let line = Line::from(vec![
                Span::styled(
                    " ✔ ",
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    msg.clone(),
                    Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
                ),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
    }

    let keys = Line::from(vec![
        Span::styled(
            " o",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": ⚡ Optimize Battery  ", Style::default().fg(Color::White)),
        Span::styled(
            "z",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": Freeze/Sleep App  ", Style::default().fg(Color::Gray)),
        Span::styled(
            "x",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": Stop App  ", Style::default().fg(Color::Gray)),
        Span::styled("Tab/1-4", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Time  ", Style::default().fg(Color::Gray)),
        Span::styled("s", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Sort  ", Style::default().fg(Color::Gray)),
        Span::styled("f", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Filter  ", Style::default().fg(Color::Gray)),
        Span::styled("c", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Charts  ", Style::default().fg(Color::Gray)),
        Span::styled("q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Quit", Style::default().fg(Color::Gray)),
    ]);

    f.render_widget(
        Paragraph::new(keys).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_optimizer_modal(f: &mut Frame, app: &AppUi, area: Rect) {
    let Some((targets, sel_idx)) = &app.optimizer_modal else {
        return;
    };

    let popup_width = area.width.saturating_sub(8).min(96);
    let popup_height = area.height.saturating_sub(4).min(24);
    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Rgb(18, 22, 30)))
        .title(Span::styled(
            " ⚡ Battery Optimizer — Turn Off Unused Services & Background Hogs ",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(inner);

    let total_sel_watts: f64 = targets
        .iter()
        .filter(|t| t.selected)
        .map(|t| t.estimated_watts)
        .sum();
    let total_sel_ram: f64 = targets
        .iter()
        .filter(|t| t.selected)
        .map(|t| t.memory_mb)
        .sum();
    let total_sel_count = targets.iter().filter(|t| t.selected).count();

    let header_line = Line::from(vec![
        Span::styled(
            format!(" Selected: {} item(s)", total_sel_count),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   •   Estimated Savings: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("~{:.1} W", total_sel_watts),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   •   RAM to Free: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:.0} MB", total_sel_ram),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header_line), chunks[0]);

    if targets.is_empty() {
        f.render_widget(
            Paragraph::new(" ✨ Your system is already clean! No unused background hogs detected.")
                .style(Style::default().fg(Color::LightGreen)),
            chunks[1],
        );
    } else {
        let items: Vec<ListItem> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let check = if t.selected { "[✓]" } else { "[ ]" };
                let is_curr = i == *sel_idx;
                let bg = if is_curr {
                    Color::Rgb(42, 54, 72)
                } else {
                    Color::Rgb(18, 22, 30)
                };
                let line = Line::from(vec![
                    Span::styled(
                        format!(" {} ", check),
                        Style::default()
                            .fg(if t.selected {
                                Color::LightGreen
                            } else {
                                Color::DarkGray
                            })
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<36}", t.display_name),
                        Style::default()
                            .fg(Color::White)
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" ~{:>4.2} W ", t.estimated_watts),
                        Style::default().fg(Color::LightYellow).bg(bg),
                    ),
                    Span::styled(
                        if t.memory_mb > 0.0 {
                            format!(" {:>5.0} MB  ({:>3} procs) ", t.memory_mb, t.pids_count)
                        } else {
                            " System Power Tunable ".to_string()
                        },
                        Style::default().fg(Color::Cyan).bg(bg),
                    ),
                    Span::styled(
                        format!("— {}", t.reason),
                        Style::default().fg(Color::Gray).bg(bg),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        f.render_widget(List::new(items), chunks[1]);
    }

    let help_line = Line::from(vec![
        Span::styled(
            " Enter / o",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": Optimize Selected Now   ", Style::default().fg(Color::White)),
        Span::styled("Space", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Toggle Item   ", Style::default().fg(Color::Gray)),
        Span::styled("a", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Select/Deselect All   ", Style::default().fg(Color::Gray)),
        Span::styled("Esc", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Cancel", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(help_line), chunks[2]);
}
