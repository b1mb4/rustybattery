use crate::apps::{AppDeltaUsage, AppEnergyTracker};
use crate::autopilot::{
    config_file_path, get_autopilot_log, is_app_whitelisted, is_autopilot_enabled,
    load_autopilot_config, save_autopilot_config, set_autopilot_enabled, toggle_app_whitelist,
    AutopilotConfig, SmartAutopilot,
};
use crate::battery::{read_battery_snapshot, BatterySnapshot, BatteryStatus};
use crate::db::{AppSummaryRow, BatteryHistoryPoint, Database, PeriodDrainStats};
use crate::optimize::{
    execute_optimizations, scan_optimization_targets, set_app_frozen, stop_or_kill_app,
    OptimizationTarget,
};
use crate::service::{install_systemd_service, is_daemon_active, is_service_file_installed};
use crate::statusbar::send_status_notification;
use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
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

    pub fn short_title(&self) -> &'static str {
        match self {
            TimeWindow::SinceCharge => "1:Charge",
            TimeWindow::Last6Hours => "2:6h",
            TimeWindow::Last24Hours => "3:24h",
            TimeWindow::Last7Days => "4:7d",
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
    FocusedPct,
    EnergyWh,
    LivePowerW,
    LiveCpuPct,
    MemoryMb,
}

impl SortColumn {
    pub fn next(&self) -> Self {
        match self {
            SortColumn::BatteryPct => SortColumn::FocusedPct,
            SortColumn::FocusedPct => SortColumn::EnergyWh,
            SortColumn::EnergyWh => SortColumn::LivePowerW,
            SortColumn::LivePowerW => SortColumn::LiveCpuPct,
            SortColumn::LiveCpuPct => SortColumn::MemoryMb,
            SortColumn::MemoryMb => SortColumn::BatteryPct,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SortColumn::BatteryPct => "Battery %",
            SortColumn::FocusedPct => "Focused %",
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

    pub fn short_label(&self) -> &'static str {
        match self {
            CategoryFilter::All => "All",
            CategoryFilter::UserAppsOnly => "User Apps",
            CategoryFilter::SystemAndDesktop => "System",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelVisibility {
    Auto,
    ForceShow,
    ForceHide,
}

impl PanelVisibility {
    pub fn toggle(&self, currently_visible: bool) -> Self {
        if currently_visible {
            PanelVisibility::ForceHide
        } else {
            PanelVisibility::ForceShow
        }
    }
}

pub struct AppUi {
    pub db: Database,
    pub tracker: AppEnergyTracker,
    pub autopilot: SmartAutopilot,
    pub battery: BatterySnapshot,
    pub history: Vec<BatteryHistoryPoint>,
    pub raw_apps: Vec<AppSummaryRow>,
    pub apps: Vec<AppSummaryRow>,
    pub live_deltas: Vec<AppDeltaUsage>,
    pub selected_app_buckets: Vec<(i64, f64, f64)>,
    pub window: TimeWindow,
    pub sort_by: SortColumn,
    pub filter: CategoryFilter,
    pub charts_mode: PanelVisibility,
    pub details_mode: PanelVisibility,
    pub details_modal_open: bool,
    pub search_query: String,
    pub search_active: bool,
    pub tabs_area: Rect,
    pub filter_area: Rect,
    pub table_area: Rect,
    pub table_state: TableState,
    pub daemon_running: bool,
    pub service_installed: bool,
    pub autopilot_enabled: bool,
    pub autopilot_config: AutopilotConfig,
    pub autopilot_logs: Vec<String>,
    pub drain_stats: PeriodDrainStats,
    pub status_message: Option<(String, Instant)>,
    pub since_ts: i64,
    pub last_full_charge_ts: i64,
    pub optimizer_modal: Option<(Vec<OptimizationTarget>, usize)>,
    pub autopilot_modal: Option<usize>,
}

impl AppUi {
    pub fn new(db: Database) -> Result<Self> {
        let battery = read_battery_snapshot()?;
        db.ensure_initial_seed(&battery)?;
        db.record_battery_snapshot(&battery)?;

        let autopilot_enabled = is_autopilot_enabled(&db);
        let autopilot_config = load_autopilot_config();
        let autopilot_logs = get_autopilot_log(&db);
        let last_full_charge_ts = db.get_last_full_charge_ts();
        let tracker = AppEnergyTracker::new();
        let mut ui = Self {
            db,
            tracker,
            autopilot: SmartAutopilot::new(),
            battery,
            history: Vec::new(),
            raw_apps: Vec::new(),
            apps: Vec::new(),
            live_deltas: Vec::new(),
            selected_app_buckets: Vec::new(),
            window: TimeWindow::SinceCharge,
            sort_by: SortColumn::BatteryPct,
            filter: CategoryFilter::All,
            charts_mode: PanelVisibility::Auto,
            details_mode: PanelVisibility::Auto,
            details_modal_open: false,
            search_query: String::new(),
            search_active: false,
            tabs_area: Rect::default(),
            filter_area: Rect::default(),
            table_area: Rect::default(),
            table_state: TableState::default(),
            daemon_running: false,
            service_installed: is_service_file_installed(),
            autopilot_enabled,
            autopilot_config,
            autopilot_logs,
            drain_stats: PeriodDrainStats::default(),
            status_message: None,
            since_ts: 0,
            last_full_charge_ts,
            optimizer_modal: None,
            autopilot_modal: None,
        };
        ui.table_state.select(Some(0));
        ui.refresh_data(false)?;
        Ok(ui)
    }

    pub fn compute_since_ts(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        match self.window {
            TimeWindow::SinceCharge => self.last_full_charge_ts,
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
        self.autopilot_enabled = is_autopilot_enabled(&self.db);
        self.autopilot_config = load_autopilot_config();

        if sample_live_tick {
            let (deltas, _) = self.tracker.sample_delta(
                self.battery.power_w,
                self.battery.energy_full_wh,
                self.battery.status.is_discharging(),
            );
            // If the background daemon is NOT currently active, the TUI itself persists samples & evaluates Autopilot!
            if !self.daemon_running {
                let _ = self.db.record_battery_snapshot(&self.battery);
                let _ = self.db.record_app_deltas(&deltas);
                self.autopilot.evaluate(&self.db, &self.battery, &deltas);
            }
            self.live_deltas = deltas;
        }

        self.last_full_charge_ts = self.db.get_last_full_charge_ts();
        self.since_ts = self.compute_since_ts();
        self.history = self.db.get_battery_history(self.since_ts).unwrap_or_default();
        self.drain_stats = self.db.get_period_drain_stats(self.since_ts).unwrap_or_default();
        self.autopilot_logs = get_autopilot_log(&self.db);

        self.raw_apps = self
            .db
            .get_app_breakdown(self.since_ts, &self.live_deltas)
            .unwrap_or_default();
        self.apply_filter_and_sort();
        Ok(())
    }

    /// Re-filters and re-sorts `self.raw_apps` in memory without querying SQLite or sysfs.
    pub fn apply_filter_and_sort(&mut self) {
        let mut all_apps: Vec<AppSummaryRow> = self
            .raw_apps
            .iter()
            .filter(|r| {
                self.filter.matches(&r.category)
                    && (r.battery_pct >= 0.005 || r.live_cpu_pct >= 0.1 || r.memory_mb >= 15.0)
            })
            .cloned()
            .collect();

        // Recompute share_of_drain_pct within category-filtered view (before search query filter)
        let total_drain: f64 = all_apps.iter().map(|a| a.battery_pct).sum();
        if total_drain > 0.0 {
            for a in &mut all_apps {
                a.share_of_drain_pct = (a.battery_pct / total_drain) * 100.0;
            }
        }

        let query = self.search_query.trim().to_lowercase();
        if !query.is_empty() {
            all_apps.retain(|r| {
                r.app_name.to_lowercase().contains(&query)
                    || r.category.to_lowercase().contains(&query)
            });
        }

        all_apps.sort_by(|a, b| {
            let cmp = match self.sort_by {
                SortColumn::BatteryPct => b.battery_pct.partial_cmp(&a.battery_pct),
                SortColumn::FocusedPct => b.focused_battery_pct.partial_cmp(&a.focused_battery_pct),
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_tui_loop(&mut terminal, db);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
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
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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

                    if let Some(sel_idx) = &mut app.autopilot_modal {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('A') => {
                                app.autopilot_modal = None;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                *sel_idx = (*sel_idx + 1).min(8);
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                *sel_idx = sel_idx.saturating_sub(1);
                            }
                            KeyCode::Left
                            | KeyCode::Char('h')
                            | KeyCode::Right
                            | KeyCode::Char('l')
                            | KeyCode::Char(' ')
                            | KeyCode::Enter => {
                                let inc = !matches!(key.code, KeyCode::Left | KeyCode::Char('h'));
                                let cfg = &mut app.autopilot_config;
                                match *sel_idx {
                                    0 => {
                                        cfg.enabled = !cfg.enabled;
                                        app.autopilot_enabled = cfg.enabled;
                                        let _ = set_autopilot_enabled(&app.db, cfg.enabled);
                                    }
                                    1 => cfg.auto_switch_power_profile = !cfg.auto_switch_power_profile,
                                    2 => cfg.auto_freeze_enabled = !cfg.auto_freeze_enabled,
                                    3 => {
                                        if inc {
                                            cfg.idle_freeze_mins = (cfg.idle_freeze_mins + 5).min(120);
                                        } else {
                                            cfg.idle_freeze_mins = cfg.idle_freeze_mins.saturating_sub(5).max(5);
                                        }
                                    }
                                    4 => {
                                        if inc {
                                            cfg.idle_power_threshold_w =
                                                ((cfg.idle_power_threshold_w + 0.1).min(5.0) * 10.0).round() / 10.0;
                                        } else {
                                            cfg.idle_power_threshold_w =
                                                ((cfg.idle_power_threshold_w - 0.1).max(0.1) * 10.0).round() / 10.0;
                                        }
                                    }
                                    5 => {
                                        if inc {
                                            cfg.low_battery_pct = (cfg.low_battery_pct + 5.0).min(80.0);
                                        } else {
                                            cfg.low_battery_pct = (cfg.low_battery_pct - 5.0)
                                                .max(cfg.critical_battery_pct + 5.0);
                                        }
                                    }
                                    6 => {
                                        if inc {
                                            cfg.critical_battery_pct = (cfg.critical_battery_pct + 5.0)
                                                .min(cfg.low_battery_pct - 5.0);
                                        } else {
                                            cfg.critical_battery_pct =
                                                (cfg.critical_battery_pct - 5.0).max(5.0);
                                        }
                                    }
                                    7 => cfg.dim_kbd_on_low_battery = !cfg.dim_kbd_on_low_battery,
                                    8 => cfg.s2idle_sleep_saver = !cfg.s2idle_sleep_saver,
                                    _ => {}
                                }
                                let _ = save_autopilot_config(cfg);
                                app.status_message = Some((
                                    format!("Saved Smart Saver rules to {}", config_file_path().display()),
                                    Instant::now(),
                                ));
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if app.details_modal_open {
                        match key.code {
                            KeyCode::Esc
                            | KeyCode::Enter
                            | KeyCode::Char('d')
                            | KeyCode::Char('q') => {
                                app.details_modal_open = false;
                            }
                            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                            KeyCode::Char(c @ ('z' | 'x' | 'w')) => {
                                app.handle_selected_app_action(c)?;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if app.search_active {
                        match key.code {
                            KeyCode::Esc => {
                                app.search_query.clear();
                                app.search_active = false;
                                app.apply_filter_and_sort();
                            }
                            KeyCode::Enter => {
                                app.search_active = false;
                            }
                            KeyCode::Backspace => {
                                app.search_query.pop();
                                app.apply_filter_and_sort();
                            }
                            KeyCode::Down => app.move_selection(1),
                            KeyCode::Up => app.move_selection(-1),
                            KeyCode::Char(c) => {
                                app.search_query.push(c);
                                app.apply_filter_and_sort();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    let term_size = terminal.size().unwrap_or_default();
                    let term_rect = Rect::new(0, 0, term_size.width, term_size.height);

                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Esc => {
                            if !app.search_query.is_empty() {
                                app.search_query.clear();
                                app.apply_filter_and_sort();
                            } else {
                                return Ok(());
                            }
                        }
                        KeyCode::Char('/') => {
                            app.search_active = true;
                        }
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
                            app.apply_filter_and_sort();
                        }
                        KeyCode::Char('f') => {
                            app.filter = app.filter.next();
                            app.apply_filter_and_sort();
                        }
                        KeyCode::Char('c') => {
                            let currently_shown = should_show_charts(&app, term_rect);
                            app.charts_mode = app.charts_mode.toggle(currently_shown);
                        }
                        KeyCode::Enter => {
                            if !should_show_details_panel(&app, term_rect) {
                                app.details_modal_open = !app.details_modal_open;
                            }
                        }
                        KeyCode::Char('d') => {
                            if term_rect.width < 120 {
                                app.details_modal_open = !app.details_modal_open;
                            } else {
                                let currently_shown = should_show_details_panel(&app, term_rect);
                                app.details_mode = app.details_mode.toggle(currently_shown);
                            }
                        }
                        KeyCode::Char('o') => {
                            let targets = scan_optimization_targets(app.battery.power_w);
                            app.optimizer_modal = Some((targets, 0));
                        }
                        KeyCode::Char(c @ ('z' | 'x' | 'w')) => {
                            app.handle_selected_app_action(c)?;
                        }
                        KeyCode::Char('a') => {
                            let next_state = !app.autopilot_enabled;
                            if let Ok(msg) = set_autopilot_enabled(&app.db, next_state) {
                                app.autopilot_enabled = next_state;
                                app.autopilot_config.enabled = next_state;
                                let _ = save_autopilot_config(&app.autopilot_config);
                                app.status_message = Some((format!("🤖 {msg}"), Instant::now()));
                            }
                        }
                        KeyCode::Char('A') => {
                            app.autopilot_config = load_autopilot_config();
                            app.autopilot_modal = Some(0);
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
                Event::Mouse(mouse) => {
                    let col = mouse.column;
                    let row = mouse.row;
                    let in_rect = |r: Rect| -> bool {
                        col >= r.x
                            && col < r.x.saturating_add(r.width)
                            && row >= r.y
                            && row < r.y.saturating_add(r.height)
                    };

                    if app.optimizer_modal.is_some() {
                        if let Some((targets, sel_idx)) = &mut app.optimizer_modal {
                            match mouse.kind {
                                MouseEventKind::ScrollDown => {
                                    if !targets.is_empty() {
                                        *sel_idx = (*sel_idx + 1).min(targets.len() - 1);
                                    }
                                }
                                MouseEventKind::ScrollUp => {
                                    *sel_idx = sel_idx.saturating_sub(1);
                                }
                                MouseEventKind::Down(MouseButton::Right) => {
                                    app.optimizer_modal = None;
                                }
                                _ => {}
                            }
                        }
                        continue;
                    }

                    if app.autopilot_modal.is_some() {
                        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
                            app.autopilot_modal = None;
                        }
                        continue;
                    }

                    if app.details_modal_open {
                        match mouse.kind {
                            MouseEventKind::ScrollDown => app.move_selection(1),
                            MouseEventKind::ScrollUp => app.move_selection(-1),
                            MouseEventKind::Down(MouseButton::Right) => {
                                app.details_modal_open = false;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match mouse.kind {
                        MouseEventKind::ScrollDown => app.move_selection(1),
                        MouseEventKind::ScrollUp => app.move_selection(-1),
                        MouseEventKind::Down(MouseButton::Left) => {
                            if in_rect(app.tabs_area) && app.tabs_area.width >= 4 {
                                let rel_x = col.saturating_sub(app.tabs_area.x);
                                let seg = ((rel_x as usize * 4) / (app.tabs_area.width as usize))
                                    .min(3);
                                app.window = TimeWindow::all()[seg];
                                app.refresh_data(false)?;
                            } else if in_rect(app.filter_area) {
                                app.filter = app.filter.next();
                                app.apply_filter_and_sort();
                            } else if in_rect(app.table_area) {
                                let header_end = app.table_area.y.saturating_add(2);
                                let bottom_edge = app
                                    .table_area
                                    .y
                                    .saturating_add(app.table_area.height.saturating_sub(1));
                                if row >= header_end && row < bottom_edge {
                                    let clicked = app.table_state.offset()
                                        + row.saturating_sub(header_end) as usize;
                                    if clicked < app.apps.len() {
                                        app.table_state.select(Some(clicked));
                                        app.update_selected_app_buckets();
                                    }
                                }
                            }
                        }
                        MouseEventKind::Down(MouseButton::Right) => {
                            if in_rect(app.table_area) {
                                let header_end = app.table_area.y.saturating_add(2);
                                let bottom_edge = app
                                    .table_area
                                    .y
                                    .saturating_add(app.table_area.height.saturating_sub(1));
                                if row >= header_end && row < bottom_edge {
                                    let clicked = app.table_state.offset()
                                        + row.saturating_sub(header_end) as usize;
                                    if clicked < app.apps.len() {
                                        app.table_state.select(Some(clicked));
                                        app.update_selected_app_buckets();
                                        let term_size = terminal.size().unwrap_or_default();
                                        let term_rect =
                                            Rect::new(0, 0, term_size.width, term_size.height);
                                        if !should_show_details_panel(&app, term_rect) {
                                            app.details_modal_open = true;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.refresh_data(true)?;
            last_tick = Instant::now();
        }
    }
}

fn should_show_charts(app: &AppUi, size: Rect) -> bool {
    match app.charts_mode {
        PanelVisibility::Auto => size.width >= 120 && size.height >= 28,
        PanelVisibility::ForceShow => size.height >= 19,
        PanelVisibility::ForceHide => false,
    }
}

fn should_show_details_panel(app: &AppUi, size: Rect) -> bool {
    match app.details_mode {
        PanelVisibility::Auto => size.width >= 120,
        PanelVisibility::ForceShow => size.width >= 96,
        PanelVisibility::ForceHide => false,
    }
}

fn draw_ui(f: &mut Frame, app: &mut AppUi) {
    let size = f.area();
    let show_charts = should_show_charts(app, size);
    let header_height = if size.height < 18 || size.width < 72 {
        3
    } else {
        5
    };
    let footer_height = if size.height < 16 { 1 } else { 2 };

    let constraints = if show_charts {
        let chart_h = if size.height >= 30 { 10 } else { 8 };
        vec![
            Constraint::Length(header_height), // Top phone battery header card
            Constraint::Length(3),             // Time window tabs + KPI strip
            Constraint::Length(chart_h),       // Battery % & Wattage charts
            Constraint::Min(7),                // Per-app table (+ optional App inspector)
            Constraint::Length(footer_height), // Footer keybindings
        ]
    } else {
        vec![
            Constraint::Length(header_height), // Top phone battery header card
            Constraint::Length(3),             // Time window tabs + KPI strip
            Constraint::Min(7),                // Per-app table (+ optional App inspector)
            Constraint::Length(footer_height), // Footer keybindings
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(size);

    draw_battery_header(f, app, chunks[0]);
    draw_tabs_and_kpis(f, app, chunks[1]);

    if show_charts {
        draw_charts(f, app, chunks[2]);
        draw_apps_section(f, app, chunks[3], size);
        draw_footer(f, app, chunks[4], size);
    } else {
        draw_apps_section(f, app, chunks[2], size);
        draw_footer(f, app, chunks[3], size);
    }

    if app.details_modal_open {
        draw_app_details_modal(f, app, size);
    }

    if app.optimizer_modal.is_some() {
        draw_optimizer_modal(f, app, size);
    }

    if app.autopilot_modal.is_some() {
        draw_autopilot_modal(f, app, size);
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

    let title_text = if area.width < 75 {
        " 🦀 RustyBattery "
    } else {
        " 🦀 RustyBattery — Per-App Power Tracker for Linux "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title_text,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

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
        None => "N/A".to_string(),
    };

    let power_color = if b.power_w > 18.0 {
        Color::LightRed
    } else if b.power_w > 11.0 {
        Color::Yellow
    } else {
        Color::LightGreen
    };

    // Ultra-compact 1-line header for short or narrow tiled windows
    if inner.height <= 1 || area.width < 72 {
        let compact_line = Line::from(vec![
            Span::styled(
                format!(" {:.1}% {} ", b.capacity_pct, status_badge),
                Style::default().fg(bat_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("({time_str})  •  "), Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.2} W", b.power_w),
                Style::default().fg(power_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  •  Smart Saver: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if app.autopilot_enabled { "ON [a]" } else { "OFF [a]" },
                Style::default().fg(if app.autopilot_enabled {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                }),
            ),
        ]);
        f.render_widget(Paragraph::new(compact_line), inner);
        return;
    }

    // 2-column layout for medium/half-screen tiling (72..108 cols), 3-column for wide (>= 108 cols)
    let is_wide_header = area.width >= 108;
    let cols = if is_wide_header {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(36),
                Constraint::Percentage(30),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
            .split(inner)
    };

    // Left: Big Battery Gauge
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
            format!("{:.1}/{:.1} Wh", b.energy_now_wh, b.energy_full_wh),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(" • Health: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.0}% ({}c)", b.health_pct, b.cycle_count),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    f.render_widget(Paragraph::new(sub_info), left_rows[1]);

    let last_charge_ts = app.last_full_charge_ts;
    let hours_ago = ((chrono::Utc::now().timestamp() - last_charge_ts).max(0) as f64) / 3600.0;

    let daemon_span = if app.daemon_running {
        Span::styled(
            "● 24/7 Service",
            Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        )
    } else if app.service_installed {
        Span::styled(
            "◐ Svc Installed",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "◉ Live [i:svc]",
            Style::default().fg(Color::Yellow),
        )
    };

    let autopilot_span = if app.autopilot_enabled {
        Span::styled(
            "● ON [a]",
            Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "○ OFF [a]",
            Style::default().fg(Color::DarkGray),
        )
    };

    if is_wide_header {
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

        let right_lines = vec![
            Line::from(vec![
                Span::styled("Collector:   ", Style::default().fg(Color::Gray)),
                daemon_span,
            ]),
            Line::from(vec![
                Span::styled("Smart Saver: ", Style::default().fg(Color::Gray)),
                autopilot_span,
                Span::styled(
                    format!(" · Full {:.1}h ago", hours_ago),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(vec![
                Span::styled("Sort By:     ", Style::default().fg(Color::Gray)),
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
    } else {
        // Half-screen 2nd column: merges Live Power + Smart Saver + Sort By cleanly
        let mid_lines = vec![
            Line::from(vec![
                Span::styled(" Power: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:.2} W", b.power_w),
                    Style::default().fg(power_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  •  ", Style::default().fg(Color::DarkGray)),
                daemon_span,
            ]),
            Line::from(vec![
                Span::styled(" Smart Saver: ", Style::default().fg(Color::Gray)),
                autopilot_span,
                Span::styled(
                    format!(" · {:.1}h", hours_ago),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Sort: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{} [s]", app.sort_by.label()),
                    Style::default().fg(Color::LightCyan),
                ),
                Span::styled(" • Details [Enter/d]", Style::default().fg(Color::DarkGray)),
            ]),
        ];
        f.render_widget(Paragraph::new(mid_lines), cols[1]);
    }
}

fn draw_tabs_and_kpis(f: &mut Frame, app: &mut AppUi, area: Rect) {
    let is_compact = area.width < 115;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if is_compact {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        } else {
            [Constraint::Percentage(42), Constraint::Percentage(58)]
        })
        .split(area);
    app.tabs_area = cols[0];
    app.filter_area = cols[1];

    let titles: Vec<Line> = TimeWindow::all()
        .iter()
        .map(|w| {
            Line::from(if is_compact {
                w.short_title()
            } else {
                w.title()
            })
        })
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
                .title(if is_compact {
                    " Range (Tab/1-4) "
                } else {
                    " Time Range (Tab / 1-4) "
                }),
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

    let sot_secs = if app.drain_stats.sot_secs > 0 {
        app.drain_stats.sot_secs
    } else {
        app.drain_stats.awake_total_secs
    };
    let sot_label = format_duration_short(sot_secs as f64);
    let sleep_label = if app.drain_stats.sleep_secs >= 60 {
        format!(
            "{:.1}%/h ({})",
            app.drain_stats.sleep_drain_pct_per_hr,
            format_duration_short(app.drain_stats.sleep_secs as f64)
        )
    } else {
        "0m".to_string()
    };

    let filter_text = if cols[1].width >= 88 {
        app.filter.label()
    } else {
        app.filter.short_label()
    };

    let kpi_line = if is_compact || cols[1].width < 68 {
        Line::from(vec![
            Span::styled(" Drain:", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}%", total_app_pct),
                Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ SoT:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sot_label,
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ Sleep:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if app.drain_stats.sleep_secs >= 60 {
                    format!("{:.1}%/h", app.drain_stats.sleep_drain_pct_per_hr)
                } else {
                    "0m".to_string()
                },
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.filter.short_label(),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Drain: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}%", total_app_pct),
                Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({:.1}Wh)", total_app_wh),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(" │ Avg:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}W", avg_watts),
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ SoT:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sot_label,
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ Sleep:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sleep_label,
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                filter_text,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ])
    };

    let kpi_block = Paragraph::new(kpi_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(if is_compact {
                " Summary & Filter [f] "
            } else {
                " Period Summary & Filter [f] "
            }),
    );
    f.render_widget(kpi_block, cols[1]);
}

fn draw_charts(f: &mut Frame, app: &AppUi, area: Rect) {
    let show_two_charts = area.width >= 92;
    let cols = if show_two_charts {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

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
    if !show_two_charts {
        return;
    }

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

fn draw_apps_section(f: &mut Frame, app: &mut AppUi, area: Rect, total_size: Rect) {
    let show_details = should_show_details_panel(app, total_size);

    let (table_outer_area, details_area) = if show_details {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    let show_search_bar = app.search_active || !app.search_query.is_empty();
    let table_area = if show_search_bar {
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(4)])
            .split(table_outer_area);
        let cursor = if app.search_active { "▌" } else { "" };
        let search_line = Line::from(vec![
            Span::styled(" 🔍 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{}{}", app.search_query, cursor),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if app.search_active {
                    "   (Enter: apply · Esc: clear)"
                } else {
                    "   (/: edit · Esc: clear)"
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let search_box = Paragraph::new(search_line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.search_active {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .title(Span::styled(
                    " Search Apps & Categories [/] ",
                    Style::default().fg(Color::LightCyan),
                )),
        );
        f.render_widget(search_box, v[0]);
        v[1]
    } else {
        table_outer_area
    };
    app.table_area = table_area;

    // Adaptive column tier based on the actual width available to the table
    let table_w = table_area.width;
    let col_tier = if table_w >= 88 {
        2 // Full 9 columns
    } else if table_w >= 66 {
        1 // Medium 7 columns (drops Category & CPU Time for clean half-screen reading)
    } else {
        0 // Compact 6 columns (essential stats for narrow tiles)
    };

    let header_labels: Vec<&str> = match col_tier {
        2 => vec![
            "#",
            "Application",
            "Category",
            "Battery Share",
            "Batt %",
            "Focused %",
            "Energy",
            "Live W",
            "CPU Time",
        ],
        1 => vec![
            "#",
            "Application",
            "Battery Share",
            "Batt %",
            "Focused %",
            "Energy",
            "Live W",
        ],
        _ => vec!["#", "Application", "Share", "Batt %", "Focused %", "Live W"],
    };

    let header = Row::new(header_labels.into_iter().map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    }))
    .style(Style::default().bg(Color::Rgb(24, 28, 36)))
    .height(1);

    let rows: Vec<Row> = app
        .apps
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let (icon, cat_color) = category_icon_and_color(&row.category);
            let bar_w = if col_tier == 2 {
                9
            } else if col_tier == 1 {
                8
            } else {
                5
            };
            let bar = make_usage_bar(row.share_of_drain_pct, bar_w);

            let pct_color = if row.share_of_drain_pct >= 25.0 {
                Color::LightRed
            } else if row.share_of_drain_pct >= 10.0 {
                Color::LightYellow
            } else {
                Color::LightGreen
            };

            let whitelisted = is_app_whitelisted(&app.autopilot_config, &row.app_name);
            let shield = if whitelisted { "🛡 " } else { "" };
            let name_display = if row.is_frozen {
                format!("❄ {shield}{} [FROZEN]", row.app_name)
            } else if row.is_focused {
                format!("🎯 {shield}{} {}", icon, row.app_name)
            } else {
                format!("{shield}{} {}", icon, row.app_name)
            };

            let name_style = if row.is_frozen {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.is_focused {
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else if row.pids_count == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };

            let focused_style = if row.focused_battery_pct > 0.005 {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let live_w_cell = Cell::from(format!("{:>4.2} W", row.live_power_w)).style(
                if row.live_power_w >= 2.0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            );

            match col_tier {
                2 => Row::new(vec![
                    Cell::from(format!("{:>2}", i + 1)).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(name_display).style(name_style),
                    Cell::from(row.category.clone()).style(Style::default().fg(cat_color)),
                    Cell::from(format!("{} {:>4.0}%", bar, row.share_of_drain_pct))
                        .style(Style::default().fg(pct_color)),
                    Cell::from(format!("{:>5.2}%", row.battery_pct))
                        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Cell::from(format!("{:>6.2}%", row.focused_battery_pct)).style(focused_style),
                    Cell::from(format!("{:>5.2} Wh", row.energy_wh))
                        .style(Style::default().fg(Color::Gray)),
                    live_w_cell,
                    Cell::from(format!("{:>7}", format_duration_short(row.cpu_secs)))
                        .style(Style::default().fg(Color::Gray)),
                ]),
                1 => Row::new(vec![
                    Cell::from(format!("{:>2}", i + 1)).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(name_display).style(name_style),
                    Cell::from(format!("{} {:>4.0}%", bar, row.share_of_drain_pct))
                        .style(Style::default().fg(pct_color)),
                    Cell::from(format!("{:>5.2}%", row.battery_pct))
                        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Cell::from(format!("{:>6.2}%", row.focused_battery_pct)).style(focused_style),
                    Cell::from(format!("{:>5.2} Wh", row.energy_wh))
                        .style(Style::default().fg(Color::Gray)),
                    live_w_cell,
                ]),
                _ => Row::new(vec![
                    Cell::from(format!("{:>2}", i + 1)).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(name_display).style(name_style),
                    Cell::from(format!("{:>4.0}%", row.share_of_drain_pct))
                        .style(Style::default().fg(pct_color)),
                    Cell::from(format!("{:>5.2}%", row.battery_pct))
                        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Cell::from(format!("{:>6.2}%", row.focused_battery_pct)).style(focused_style),
                    live_w_cell,
                ]),
            }
        })
        .collect();

    let table_title = if show_details {
        format!(
            " 🔋 Battery Usage by Application ({} apps) — Sorted by {} ",
            app.apps.len(),
            app.sort_by.label()
        )
    } else {
        format!(
            " 🔋 Battery Usage ({} apps) — Sort: {} [Enter/d: App Details · c: Charts] ",
            app.apps.len(),
            app.sort_by.label()
        )
    };

    let widths: Vec<Constraint> = match col_tier {
        2 => vec![
            Constraint::Length(3),
            Constraint::Min(17),
            Constraint::Length(12),
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(8),
        ],
        1 => vec![
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(14),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
        _ => vec![
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(7),
        ],
    };

    let table = Table::new(rows, widths)
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

    f.render_stateful_widget(table, table_area, &mut app.table_state);

    // 2. Right: Phone-Style Selected App Inspector Card (only when wide or toggled on)
    if let Some(insp_area) = details_area {
        draw_app_inspector(f, app, insp_area, false);
    }
}

fn draw_app_details_modal(f: &mut Frame, app: &AppUi, area: Rect) {
    let popup_width = area.width.saturating_sub(6).min(64);
    let popup_height = area.height.saturating_sub(4).min(20);
    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);
    draw_app_inspector(f, app, popup_area, true);
}

fn draw_app_inspector(f: &mut Frame, app: &AppUi, area: Rect, is_modal: bool) {
    let title = if is_modal {
        " 🔍 App Battery Details [Esc / Enter / d: Close] "
    } else {
        " 🔍 App Battery Details [d: hide] "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default().fg(if is_modal {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        )
        .style(if is_modal {
            Style::default().bg(Color::Rgb(18, 22, 30))
        } else {
            Style::default()
        })
        .title(Span::styled(
            title,
            Style::default()
                .fg(if is_modal { Color::LightCyan } else { Color::White })
                .add_modifier(Modifier::BOLD),
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
    let bg_pct = (sel.battery_pct - sel.focused_battery_pct).max(0.0);

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
            if sel.is_focused {
                Span::styled(
                    " 🎯 FOCUSED",
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
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
                format!("{:.2}% total", sel.battery_pct),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled(" Focused Drain:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!(
                    "{:.2}% ({}) · Bg {:.2}%",
                    sel.focused_battery_pct,
                    format_duration_short(sel.focused_secs),
                    bg_pct
                ),
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
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
                    "[z] Resume  "
                } else {
                    "[z] Freeze  "
                },
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if is_app_whitelisted(&app.autopilot_config, &sel.app_name) {
                    "[w] Unwhitelist  "
                } else {
                    "[w] Whitelist  "
                },
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "[x] Stop",
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

fn draw_footer(f: &mut Frame, app: &AppUi, area: Rect, total_size: Rect) {
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

    let keys = if total_size.width < 85 {
        Line::from(vec![
            Span::styled(" o", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
            Span::styled(":Opt ", Style::default().fg(Color::White)),
            Span::styled("a", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled(":Smart ", Style::default().fg(Color::Gray)),
            Span::styled("A", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled(":Rules ", Style::default().fg(Color::Gray)),
            Span::styled("w", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled(":Whitelist ", Style::default().fg(Color::Gray)),
            Span::styled("Enter/d", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Details ", Style::default().fg(Color::Gray)),
            Span::styled("/", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Search ", Style::default().fg(Color::Gray)),
            Span::styled("c", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Chart ", Style::default().fg(Color::Gray)),
            Span::styled("z", Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Freeze ", Style::default().fg(Color::Gray)),
            Span::styled("s", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Sort ", Style::default().fg(Color::Gray)),
            Span::styled("q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":Quit", Style::default().fg(Color::Gray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " o",
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ⚡ Optimize  ", Style::default().fg(Color::White)),
            Span::styled(
                "a",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": Smart Saver  ", Style::default().fg(Color::Gray)),
            Span::styled(
                "A",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": Rules  ", Style::default().fg(Color::Gray)),
            Span::styled(
                "w",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": Whitelist  ", Style::default().fg(Color::Gray)),
            Span::styled(
                "Enter/d",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": App Details  ", Style::default().fg(Color::Gray)),
            Span::styled(
                "z",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": Freeze  ", Style::default().fg(Color::Gray)),
            Span::styled(
                "x",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": Stop  ", Style::default().fg(Color::Gray)),
            Span::styled("Tab/1-4", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Time  ", Style::default().fg(Color::Gray)),
            Span::styled("s", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Sort  ", Style::default().fg(Color::Gray)),
            Span::styled("/", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Search  ", Style::default().fg(Color::Gray)),
            Span::styled("f", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Filter  ", Style::default().fg(Color::Gray)),
            Span::styled("c", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Charts  ", Style::default().fg(Color::Gray)),
            Span::styled("q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(": Quit", Style::default().fg(Color::Gray)),
        ])
    };

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

fn draw_autopilot_modal(f: &mut Frame, app: &AppUi, area: Rect) {
    let Some(sel_idx) = app.autopilot_modal else {
        return;
    };

    let popup_width = area.width.saturating_sub(6).min(96);
    let popup_height = area.height.saturating_sub(4).min(26);
    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);

    let cfg = &app.autopilot_config;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Rgb(16, 22, 28)))
        .title(Span::styled(
            " 🤖 Smart Battery Saver — Configurable Rules & App Whitelist ",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(11),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(inner);

    let cfg_path_line = Line::from(vec![
        Span::styled(" Config: ", Style::default().fg(Color::Gray)),
        Span::styled(
            config_file_path().display().to_string(),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    f.render_widget(Paragraph::new(cfg_path_line), chunks[0]);

    let rules = [
        (
            "Smart Saver Master Switch",
            if cfg.enabled { "ENABLED".to_string() } else { "DISABLED".to_string() },
            "Enable/disable automatic power rules in background",
        ),
        (
            "Auto-Switch Power Profile",
            if cfg.auto_switch_power_profile { "ON".to_string() } else { "OFF".to_string() },
            "Switch to power-saver on low battery / balanced on AC",
        ),
        (
            "Auto-Freeze Idle Apps",
            if cfg.auto_freeze_enabled { "ON".to_string() } else { "OFF".to_string() },
            "Send SIGSTOP to unfocused background hogs",
        ),
        (
            "Idle Freeze Timeout",
            format!("{} min", cfg.idle_freeze_mins),
            "Minutes unfocused before auto-freezing an app",
        ),
        (
            "Idle Power Threshold",
            format!("{:.1} W", cfg.idle_power_threshold_w),
            "Minimum background power draw to trigger auto-freeze",
        ),
        (
            "Low Battery Threshold",
            format!("{:.0}%", cfg.low_battery_pct),
            "Trigger power-saver profile & dim keyboard",
        ),
        (
            "Critical Battery Threshold",
            format!("{:.0}%", cfg.critical_battery_pct),
            "Aggressive freeze threshold on critical charge",
        ),
        (
            "Dim Keyboard on Low Battery",
            if cfg.dim_kbd_on_low_battery { "ON".to_string() } else { "OFF".to_string() },
            "Turn off keyboard backlight below Low Battery %",
        ),
        (
            "s2idle Lid-Close Saver",
            if cfg.s2idle_sleep_saver { "ON".to_string() } else { "OFF".to_string() },
            "Block Bluetooth & enforce power-saver on lid close",
        ),
    ];

    let rule_items: Vec<ListItem> = rules
        .iter()
        .enumerate()
        .map(|(i, (label, val, desc))| {
            let is_sel = i == sel_idx;
            let bg = if is_sel { Color::Rgb(32, 48, 56) } else { Color::Reset };
            let prefix = if is_sel { " ▶ " } else { "   " };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{prefix}{:<28}", label),
                    Style::default()
                        .fg(if is_sel { Color::LightYellow } else { Color::White })
                        .bg(bg)
                        .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                ),
                Span::styled(
                    format!("[{:^10}]", val),
                    Style::default()
                        .fg(Color::LightGreen)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {desc}"),
                    Style::default().fg(Color::Gray).bg(bg),
                ),
            ]))
        })
        .collect();
    f.render_widget(List::new(rule_items), chunks[1]);

    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(chunks[2]);

    let whitelist_text = if cfg.whitelist.is_empty() {
        vec![ListItem::new(Span::styled(
            " (none — press 'w' on any app to protect it)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        cfg.whitelist
            .iter()
            .map(|app_name| {
                ListItem::new(Span::styled(
                    format!(" 🛡 {app_name}"),
                    Style::default().fg(Color::LightGreen),
                ))
            })
            .collect()
    };
    f.render_widget(
        List::new(whitelist_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " 🛡 Protected Whitelist ",
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
                )),
        ),
        bottom_cols[0],
    );

    let logs = &app.autopilot_logs;
    let log_items: Vec<ListItem> = if logs.is_empty() {
        vec![ListItem::new(Span::styled(
            " No Smart Saver actions recorded yet.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        logs.iter()
            .take(8)
            .map(|entry| {
                ListItem::new(Span::styled(
                    format!(" • {entry}"),
                    Style::default().fg(Color::Gray),
                ))
            })
            .collect()
    };
    f.render_widget(
        List::new(log_items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " 📜 Recent Smart Saver Log ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
        ),
        bottom_cols[1],
    );

    let help_line = Line::from(vec![
        Span::styled(" ↑/↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Select Rule   ", Style::default().fg(Color::Gray)),
        Span::styled("←/→/Space", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
        Span::styled(": Adjust / Toggle (auto-saved)   ", Style::default().fg(Color::Gray)),
        Span::styled("Esc/A", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(": Close", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(help_line), chunks[3]);
}

impl AppUi {
    fn handle_selected_app_action(&mut self, action: char) -> Result<()> {
        let Some(idx) = self.table_state.selected() else {
            return Ok(());
        };
        let Some(sel) = self.apps.get(idx).cloned() else {
            return Ok(());
        };
        match action {
            'z' => match set_app_frozen(&sel.app_name, !sel.is_frozen) {
                Ok(msg) => {
                    self.refresh_data(true)?;
                    self.status_message = Some((msg, Instant::now()));
                }
                Err(e) => {
                    self.status_message = Some((format!("Freeze error: {e}"), Instant::now()));
                }
            },
            'x' => match stop_or_kill_app(&sel.app_name) {
                Ok(msg) => {
                    self.refresh_data(true)?;
                    self.status_message = Some((msg, Instant::now()));
                }
                Err(e) => {
                    self.status_message = Some((format!("Stop error: {e}"), Instant::now()));
                }
            },
            'w' => {
                if let Ok(added) = toggle_app_whitelist(&sel.app_name) {
                    if added && sel.is_frozen {
                        let _ = set_app_frozen(&sel.app_name, false);
                    }
                    self.refresh_data(true)?;
                    let msg = if added {
                        format!(
                            "🛡 Whitelisted {} (protected from Smart Saver)",
                            sel.app_name
                        )
                    } else {
                        format!("Removed {} from Smart Saver whitelist", sel.app_name)
                    };
                    self.status_message = Some((msg, Instant::now()));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_filter_matches_expected_groups() {
        assert!(CategoryFilter::All.matches("Browser"));
        assert!(CategoryFilter::All.matches("System"));
        assert!(CategoryFilter::UserAppsOnly.matches("Browser"));
        assert!(CategoryFilter::UserAppsOnly.matches("Development"));
        assert!(!CategoryFilter::UserAppsOnly.matches("System"));
        assert!(CategoryFilter::SystemAndDesktop.matches("System"));
        assert!(CategoryFilter::SystemAndDesktop.matches("Desktop UI"));
        assert!(!CategoryFilter::SystemAndDesktop.matches("Browser"));
    }
}
