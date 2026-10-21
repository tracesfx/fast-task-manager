//! The UI: a menu bar and the system totals (with history graphs) on top,
//! then a dock of panes (see `layout::Pane`) the user can rearrange, toasts
//! in the bottom-right corner. Settings and the dock layout are saved
//! between launches.
//!
//! The UI never polls. It repaints only when egui receives input, when the
//! sampler publishes a new snapshot, when an action finishes, when a toast
//! is due to expire, or while a treemap transition is running.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_dock::{DockArea, DockState, TabViewer};
use egui_extras::{Column, TableBuilder, TableRow};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};

use crate::actions::{self, Outcome, Priority, ProcTarget, Request};
use crate::format;
use crate::graphs;
use crate::history::History;
use crate::hotkey::{self, Hotkey};
use crate::layout::{self, Pane, Preset};
use crate::model::Snapshot;
use crate::sampler::SamplerHandle;
use crate::settings::{self, Appearance, Settings, TableColumn};
use crate::source::NtSource;
use crate::treemap::{Metric, Treemap};
use crate::view::{Metrics, ProcKey, Row, RowId, RowKind, Sort, SortColumn, View, ViewMode, ViewState};

const INDENT: f32 = 16.0;
const SPARKLINE_WIDTH: f32 = 90.0;
const TOAST_INFO: Duration = Duration::from_secs(4);
const TOAST_ERROR: Duration = Duration::from_secs(8);
const DEFAULT_SIZE: egui::Vec2 = egui::vec2(1200.0, 720.0);
const COMPACT_SIZE: egui::Vec2 = egui::vec2(320.0, 300.0);

/// Storage key of the dock layout. It is saved apart from `Persisted` so a
/// layout that no longer loads (say, after an egui_dock update) only
/// resets the layout, not the settings.
const DOCK_KEY: &str = "dock";

/// Settings saved between launches (eframe storage, under `APP_KEY`).
/// Fields missing from an older save take their defaults; an unreadable
/// save is ignored.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    settings: Settings,
    mode: ViewMode,
    sort: Sort,
    /// Window size to return to when leaving the compact layout; set while
    /// it is active.
    compact_restore: Option<[f32; 2]>,
}

impl Default for Persisted {
    fn default() -> Self {
        let state = ViewState::default();
        Self {
            settings: Settings::default(),
            mode: state.mode,
            sort: state.sort,
            compact_restore: None,
        }
    }
}

/// Settings as last pushed to egui, the sampler, the window and the
/// treemap; `None` until the first frame applies them.
#[derive(Default)]
struct Applied {
    interval_ms: Option<u64>,
    appearance: Option<Appearance>,
    zoom: Option<f32>,
    on_top: Option<bool>,
    treemap: Option<(Metric, Metric)>,
    gradient: Option<Vec<[u8; 3]>>,
}

/// Something the user did this frame, applied after the table is drawn.
enum UiAction {
    Sort(SortColumn),
    Toggle(RowId),
    Select(RowId),
    /// Selected in the treemap: also scroll the table to it.
    Reveal(RowId),
    /// Open the detail panel for this app group.
    Details(Arc<str>),
    CloseDetails,
    Run(Request),
}

struct Toast {
    message: String,
    is_error: bool,
    expires: Instant,
}

pub struct TaskManagerApp {
    sampler: SamplerHandle,
    snapshot: Arc<Snapshot>,
    history: Arc<Mutex<History>>,
    state: ViewState,
    view: View,
    /// Snapshot seq the view was built from.
    view_seq: u64,
    view_dirty: bool,
    selected: Option<RowId>,
    /// Scroll the table to the selection on the next frame.
    scroll_to_selected: bool,
    treemap: Treemap,
    /// Group key of the app shown in the detail pane.
    detail: Option<Arc<str>>,
    dock: DockState<Pane>,
    settings: Settings,
    applied: Applied,
    show_settings: bool,
    compact_restore: Option<[f32; 2]>,
    /// The main window, for the hotkey.
    hwnd: Option<isize>,
    hotkey: Option<Hotkey>,
    toasts: Vec<Toast>,
    outcome_tx: Sender<Outcome>,
    outcome_rx: Receiver<Outcome>,
    ctx: egui::Context,
}

impl TaskManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let saved: Persisted = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let mut settings = saved.settings;
        settings.normalize();
        let dock = cc
            .storage
            .and_then(|s| eframe::get_value(s, DOCK_KEY))
            .unwrap_or_else(|| Preset::Default.dock());

        let ctx = cc.egui_ctx.clone();
        let sampler = spawn_sampler(Duration::from_millis(settings.interval_ms), ctx.clone());
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let mut state = ViewState::default();
        state.mode = saved.mode;
        state.sort = saved.sort;
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
            _ => None,
        };
        Self {
            snapshot: sampler.latest(),
            history: sampler.history(),
            sampler,
            state,
            view: View::default(),
            view_seq: u64::MAX,
            view_dirty: true,
            selected: None,
            scroll_to_selected: false,
            treemap: Treemap::default(),
            detail: None,
            dock,
            applied: Applied {
                interval_ms: Some(settings.interval_ms),
                ..Applied::default()
            },
            settings,
            show_settings: false,
            compact_restore: saved.compact_restore,
            hwnd,
            hotkey: None,
            toasts: Vec::new(),
            outcome_tx,
            outcome_rx,
            ctx,
        }
    }

    fn run(&self, request: Request) {
        let tx = self.outcome_tx.clone();
        let ctx = self.ctx.clone();
        let refresher = self.sampler.refresher();
        actions::spawn(request, move |outcome| {
            if let Some(outcome) = outcome {
                let _ = tx.send(outcome);
            }
            refresher.refresh_now();
            ctx.request_repaint();
        });
    }

    fn toast(&mut self, message: String, is_error: bool) {
        let lifetime = if is_error { TOAST_ERROR } else { TOAST_INFO };
        self.toasts.push(Toast {
            message,
            is_error,
            expires: Instant::now() + lifetime,
        });
    }

    fn apply(&mut self, action: UiAction) {
        match action {
            UiAction::Sort(column) => {
                let sort = &mut self.state.sort;
                if sort.column == column {
                    sort.descending = !sort.descending;
                } else {
                    *sort = Sort {
                        column,
                        descending: column.default_descending(),
                    };
                }
                self.view_dirty = true;
            }
            UiAction::Toggle(id) => {
                self.state.toggle(&id);
                self.selected = Some(id);
                self.view_dirty = true;
            }
            // Clicking an app (not one of its processes) shows its details,
            // if the details pane is open.
            UiAction::Select(id) => {
                self.show_details_of(&id);
                self.selected = Some(id);
            }
            UiAction::Reveal(id) => {
                self.show_details_of(&id);
                self.selected = Some(id);
                self.scroll_to_selected = true;
            }
            UiAction::Details(key) => {
                self.detail = Some(key);
                layout::open(&mut self.dock, Pane::Details);
            }
            UiAction::CloseDetails => self.detail = None,
            UiAction::Run(request) => self.run(request),
        }
        // Show the result now rather than on the next snapshot.
        self.ctx.request_repaint();
    }

    fn show_details_of(&mut self, id: &RowId) {
        if let RowId::Group(key) = id {
            self.detail = Some(key.clone());
            layout::focus(&mut self.dock, Pane::Details);
        }
    }

    /// Rebuilds the view and the treemap's groups if the snapshot or the
    /// view settings changed.
    fn refresh_view(&mut self) {
        if self.view_dirty || self.view_seq != self.snapshot.seq {
            profile_scope!("view rebuild");
            self.view = self.state.build(&self.snapshot);
            // Always, so the treemap's process indices match the snapshot.
            self.treemap.set_groups(self.state.app_groups(&self.snapshot));
            self.view_seq = self.snapshot.seq;
            self.view_dirty = false;
        }
    }

    /// Switches to `preset`'s dock layout. The compact one also shrinks the
    /// window and keeps it on top; leaving it restores the window size.
    fn apply_preset(&mut self, preset: Preset, ctx: &egui::Context) {
        self.dock = preset.dock();
        let compact = preset == Preset::Compact;
        if compact && self.compact_restore.is_none() {
            let size = ctx
                .input(|i| i.viewport().inner_rect)
                .map_or(DEFAULT_SIZE, |r| r.size());
            self.compact_restore = Some(size.into());
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(COMPACT_SIZE));
        } else if !compact {
            if let Some(size) = self.compact_restore.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.into()));
            }
        }
        self.settings.totals_bar = !compact;
        self.settings.always_on_top = compact;
    }

    /// Pushes settings that changed since the last call to egui, the
    /// sampler, the window, the treemap and the hotkey. Returns whether
    /// anything was applied that needs another frame to show.
    fn apply_settings(&mut self, ctx: &egui::Context) -> bool {
        let s = &mut self.settings;
        let a = &mut self.applied;
        let mut changed = false;

        if a.interval_ms != Some(s.interval_ms) {
            self.sampler.set_interval(Duration::from_millis(s.interval_ms));
            a.interval_ms = Some(s.interval_ms);
        }
        if a.appearance.as_ref() != Some(&s.appearance) {
            s.appearance.apply(ctx);
            a.appearance = Some(s.appearance.clone());
            changed = true;
        }
        // A changed setting wins; otherwise pick up Ctrl +/- zooming, which
        // takes effect at the start of the next frame.
        let zoom = ctx.zoom_factor();
        if a.zoom != Some(s.ui_scale) {
            ctx.set_zoom_factor(s.ui_scale);
            a.zoom = Some(s.ui_scale);
            changed = true;
        } else if zoom != s.ui_scale {
            s.ui_scale = zoom;
            a.zoom = Some(zoom);
        }
        if a.on_top != Some(s.always_on_top) {
            let level = if s.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            a.on_top = Some(s.always_on_top);
        }
        // Defaults: applied at startup and when edited in Settings; the
        // treemap's own controls change only the current view.
        let metrics = (s.treemap_size, s.treemap_color);
        if a.treemap != Some(metrics) {
            self.treemap.set_metrics(metrics.0, metrics.1);
            a.treemap = Some(metrics);
            changed = true;
        }
        if a.gradient.as_ref() != Some(&s.gradient) {
            self.treemap.set_gradient(&s.gradient);
            a.gradient = Some(s.gradient.clone());
            changed = true;
        }

        if s.hotkey != self.hotkey.is_some() {
            if s.hotkey {
                let result = self
                    .hwnd
                    .ok_or_else(|| "No window handle for the global hotkey".to_owned())
                    .and_then(Hotkey::register);
                match result {
                    Ok(hotkey) => self.hotkey = Some(hotkey),
                    Err(message) => {
                        s.hotkey = false;
                        self.toast(message, true);
                        changed = true;
                    }
                }
            } else {
                self.hotkey = None;
            }
        }
        changed
    }

    /// Processes the selected row stands for, for the Delete key.
    fn selected_targets(&self) -> Vec<ProcTarget> {
        let procs = &self.snapshot.processes;
        match &self.selected {
            Some(RowId::Group(key)) => match self.view.find_group(key) {
                Some(g) => g.members.iter().map(|&i| ProcTarget::of(&procs[i])).collect(),
                // Picked in the treemap while the table isn't in Apps mode.
                None => procs
                    .iter()
                    .filter(|p| p.group_key == *key)
                    .map(ProcTarget::of)
                    .collect(),
            },
            Some(RowId::Proc(key)) => procs
                .iter()
                .filter(|p| ProcKey::of(p) == *key)
                .map(ProcTarget::of)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Whether `row` (with identity `id`) shows as selected. Besides an exact
    /// match, a group picked in the treemap highlights its processes in Tree
    /// and Processes modes, and a process highlights its collapsed group.
    fn row_selected(&self, row: &Row, id: &RowId) -> bool {
        let Some(selected) = &self.selected else {
            return false;
        };
        if selected == id {
            return true;
        }
        let procs = &self.snapshot.processes;
        match (selected, row.kind) {
            (RowId::Group(key), RowKind::Process(i)) => {
                self.state.mode != ViewMode::Apps && procs[i].group_key == *key
            }
            (RowId::Proc(key), RowKind::Group(g)) => {
                row.expander != Some(true)
                    && self.view.groups[g].members.iter().any(|&i| ProcKey::of(&procs[i]) == *key)
            }
            _ => false,
        }
    }

    fn selected_row_index(&self) -> Option<usize> {
        let procs = &self.snapshot.processes;
        self.view
            .rows
            .iter()
            .position(|row| self.row_selected(row, &self.view.row_id(row, procs)))
    }

    fn totals_panel(&mut self, ui: &mut egui::Ui, history: &History) {
        let snap = &self.snapshot;
        let sys = &snap.system;

        ui.horizontal(|ui| {
            graphs::totals(ui, history, sys.total_memory);
            ui.separator();
            ui.label(format!("{} processes", snap.processes.len()));
            ui.separator();
            ui.label(format!(
                "sample took {:.1} ms",
                snap.sample_cost.as_secs_f64() * 1000.0
            ));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                settings::interval_combo(ui, "interval", &mut self.settings.interval_ms);
                ui.label("Refresh:");
            });
        });
    }

    /// The menu bar. Returns a layout preset the user picked.
    fn menu_bar(&mut self, ui: &mut egui::Ui) -> Option<Preset> {
        let mut preset = None;
        egui::menu::bar(ui, |ui| {
            ui.menu_button("View", |ui| {
                for pane in Pane::ALL {
                    let mut open = self.dock.find_tab(&pane).is_some();
                    if ui.checkbox(&mut open, pane.title()).changed() {
                        if open {
                            layout::open(&mut self.dock, pane);
                        } else {
                            layout::close(&mut self.dock, pane);
                        }
                    }
                }
                ui.separator();
                ui.checkbox(&mut self.settings.totals_bar, "Totals bar");
                ui.checkbox(&mut self.settings.always_on_top, "Always on top");
                if let (Some(_), Some(hwnd)) = (&self.hotkey, self.hwnd) {
                    ui.separator();
                    if ui.button(format!("Hide window ({})", hotkey::LABEL)).clicked() {
                        ui.close_menu();
                        hotkey::hide(hwnd);
                    }
                }
            });
            ui.menu_button("Layout", |ui| {
                for p in Preset::ALL {
                    if ui.button(p.label()).on_hover_text(p.description()).clicked() {
                        preset = Some(p);
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Reset layout").clicked() {
                    preset = Some(Preset::Default);
                    ui.close_menu();
                }
            });
            if ui.button("Settings…").clicked() {
                self.show_settings = !self.show_settings;
            }
        });
        preset
    }

    /// Totals and the busiest apps, for the compact layout.
    fn mini_view(&self, ui: &mut egui::Ui, history: &History) {
        let sys = &self.snapshot.system;
        ui.add(egui::ProgressBar::new(sys.cpu_percent / 100.0).text(format!("CPU {:.1}%", sys.cpu_percent)));
        ui.add(
            egui::ProgressBar::new(sys.used_memory as f32 / sys.total_memory.max(1) as f32).text(format!(
                "RAM {} / {}",
                format::bytes(sys.used_memory),
                format::bytes(sys.total_memory)
            )),
        );
        let Some(last) = history.system().back() else {
            return;
        };
        ui.label(format!(
            "Disk R {} · W {}",
            format::rate(last.disk_read_per_sec),
            format::rate(last.disk_write_per_sec)
        ));
        ui.separator();
        if last.top.is_empty() {
            ui.weak("No app is using CPU");
            return;
        }
        ui.strong("Top apps by CPU");
        // Bars are relative to the busiest app; the text has the values.
        let busiest = last.top.iter().map(|a| a.cpu_percent).fold(0.0, f32::max);
        for app in &last.top {
            ui.add(
                egui::ProgressBar::new(app.cpu_percent / busiest.max(0.1))
                    .text(format!("{}  {}%", app.name, format::cpu(app.cpu_percent))),
            );
        }
    }

    fn view_controls(&mut self, ui: &mut egui::Ui) {
        let focus_filter = ui.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::F,
            ))
        });

        ui.horizontal(|ui| {
            for (mode, label) in [
                (ViewMode::Apps, "Apps"),
                (ViewMode::Tree, "Tree"),
                (ViewMode::Processes, "Processes"),
            ] {
                if ui
                    .selectable_label(self.state.mode == mode, label)
                    .clicked()
                    && self.state.mode != mode
                {
                    self.state.mode = mode;
                    self.view_dirty = true;
                }
            }
            ui.separator();

            let filter = ui.add(
                egui::TextEdit::singleline(&mut self.state.filter)
                    .hint_text("Filter by name, path or PID  (Ctrl+F)")
                    .desired_width(300.0),
            );
            if focus_filter {
                filter.request_focus();
            }
            if filter.changed() {
                self.view_dirty = true;
            }
            let escape = filter.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape));
            if !self.state.filter.is_empty() && (escape || ui.small_button("✖").clicked()) {
                self.state.filter.clear();
                self.view_dirty = true;
            }
        });
    }

    fn process_table(&self, ui: &mut egui::Ui, scroll_to: Option<usize>, history: &History) -> Option<UiAction> {
        profile_scope!("table");
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let mut action = None;
        let sort = self.state.sort;
        // Only app rows have a CPU history. On the stack: drawn every frame.
        let mut column_buf = [TableColumn::Name; TableColumn::ALL.len()];
        let mut count = 0;
        for c in self.settings.visible_columns() {
            if c != TableColumn::CpuHistory || self.state.mode == ViewMode::Apps {
                column_buf[count] = c;
                count += 1;
            }
        }
        let columns = &column_buf[..count];

        // Column widths are remembered per column set.
        let mut builder = TableBuilder::new(ui).id_salt(("table", columns));
        if let Some(row) = scroll_to {
            builder = builder.scroll_to_row(row, Some(egui::Align::Center));
        }
        builder = builder
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for (i, &c) in columns.iter().enumerate() {
            builder = builder.column(column_spec(c, i + 1 == columns.len()));
        }
        builder
            .header(row_height + 2.0, |mut header| {
                for &c in columns {
                    header.col(|ui| {
                        let Some(column) = c.sort() else {
                            ui.strong(c.label());
                            return;
                        };
                        let arrow = match (sort.column == column, sort.descending) {
                            (true, true) => " ⏷",
                            (true, false) => " ⏶",
                            _ => "",
                        };
                        let button = egui::Button::new(
                            egui::RichText::new(format!("{}{arrow}", c.label())).strong(),
                        )
                        .frame(false);
                        if ui.add(button).clicked() {
                            action = Some(UiAction::Sort(column));
                        }
                    });
                }
            })
            .body(|body| {
                // Virtualized: only the rows in view are laid out.
                body.rows(row_height, self.view.rows.len(), |mut table_row| {
                    let row = self.view.rows[table_row.index()];
                    self.table_row(&mut table_row, &row, columns, history, &mut action);
                });
            });

        action
    }

    fn table_row(
        &self,
        table_row: &mut TableRow,
        row: &Row,
        columns: &[TableColumn],
        history: &History,
        action: &mut Option<UiAction>,
    ) {
        profile_scope!("table row");
        let procs = &self.snapshot.processes;
        let id = self.view.row_id(row, procs);
        table_row.set_selected(self.row_selected(row, &id));

        // A multi-process group shows sums and no single PID.
        let (m, count) = match row.kind {
            RowKind::Group(g) => {
                let group = &self.view.groups[g];
                if group.members.len() == 1 {
                    (Metrics::of(&procs[group.members[0]]), 1)
                } else {
                    (group.metrics(procs), group.members.len())
                }
            }
            RowKind::Process(i) => (Metrics::of(&procs[i]), 1),
        };
        let is_aggregate = count > 1;

        for &column in columns {
            table_row.col(|ui| match column {
                TableColumn::Name => {
                    ui.add_space(f32::from(row.depth) * INDENT);
                    match row.expander {
                        Some(expanded) => {
                            let icon = if expanded { "⏷" } else { "⏵" };
                            if ui.add(egui::Button::new(icon).frame(false)).clicked() {
                                *action = Some(UiAction::Toggle(id.clone()));
                            }
                        }
                        None => ui.add_space(INDENT),
                    }
                    let label = if is_aggregate {
                        format!("{} ({count})", m.name)
                    } else {
                        m.name.to_owned()
                    };
                    let mut text = egui::RichText::new(label);
                    if row.context_only {
                        text = text.weak();
                    }
                    ui.label(text);
                }
                TableColumn::Pid => {
                    if !is_aggregate {
                        right(ui, m.pid.to_string());
                    }
                }
                TableColumn::ParentPid => {
                    if let (false, Some(ppid)) = (is_aggregate, m.parent_pid) {
                        right(ui, ppid.to_string());
                    }
                }
                TableColumn::Cpu => right(ui, format::cpu(m.cpu_percent)),
                TableColumn::CpuHistory => {
                    if let RowKind::Group(g) = row.kind {
                        if let Some(h) = history.group(&self.view.groups[g].key) {
                            let size = egui::vec2(ui.available_width(), ui.available_height() - 4.0);
                            graphs::sparkline(ui, history, h, size);
                        }
                    }
                }
                TableColumn::WorkingSet => right(ui, format::mem(m.working_set)),
                TableColumn::Private => right(ui, format::mem(m.private_bytes)),
                TableColumn::DiskRead => right(ui, format::rate(m.disk_read_per_sec)),
                TableColumn::DiskWrite => right(ui, format::rate(m.disk_write_per_sec)),
                TableColumn::Started => {
                    if m.start_time != 0 {
                        right(
                            ui,
                            format::elapsed(self.snapshot.unix_time.saturating_sub(m.start_time)),
                        );
                    }
                }
                TableColumn::Path => {
                    if let Some(exe) = m.exe {
                        ui.label(exe.display().to_string());
                    }
                }
            });
        }

        let response = table_row.response();
        if action.is_none() {
            if response.double_clicked() && row.expander.is_some() {
                *action = Some(UiAction::Toggle(id.clone()));
            } else if response.clicked() || response.secondary_clicked() {
                *action = Some(UiAction::Select(id.clone()));
            }
        }
        response.context_menu(|ui| self.context_menu(ui, row, m.exe, action));
    }

    fn context_menu(
        &self,
        ui: &mut egui::Ui,
        row: &Row,
        exe: Option<&std::path::Path>,
        action: &mut Option<UiAction>,
    ) {
        let procs = &self.snapshot.processes;
        let targets: Vec<ProcTarget> = match row.kind {
            RowKind::Group(g) => self.view.groups[g]
                .members
                .iter()
                .map(|&i| ProcTarget::of(&procs[i]))
                .collect(),
            RowKind::Process(i) => vec![ProcTarget::of(&procs[i])],
        };
        if ui.button("App details").clicked() {
            let key = match row.kind {
                RowKind::Group(g) => Arc::clone(&self.view.groups[g].key),
                RowKind::Process(i) => Arc::clone(&procs[i].group_key),
            };
            *action = Some(UiAction::Details(key));
            ui.close_menu();
        }
        ui.separator();

        let mut run = |ui: &mut egui::Ui, request: Request| {
            *action = Some(UiAction::Run(request));
            ui.close_menu();
        };

        if targets.len() > 1 {
            if ui
                .button(format!("End group ({} processes)", targets.len()))
                .clicked()
            {
                run(ui, Request::End(targets.clone()));
            }
        } else if ui.button("End process").clicked() {
            run(ui, Request::End(targets.clone()));
        }

        // Tree mode: a process with children can be ended with its subtree.
        if let RowKind::Process(i) = row.kind {
            let subtree = self.view.subtree(i);
            if subtree.len() > 1
                && ui
                    .button(format!("End process tree ({} processes)", subtree.len()))
                    .clicked()
            {
                let tree = subtree.iter().map(|&j| ProcTarget::of(&procs[j])).collect();
                run(ui, Request::End(tree));
            }
        }

        ui.menu_button("Set priority", |ui| {
            for priority in Priority::ALL {
                if ui.button(priority.label()).clicked() {
                    run(ui, Request::SetPriority(targets.clone(), priority));
                }
            }
        });

        ui.separator();
        let open = ui.add_enabled(exe.is_some(), egui::Button::new("Open file location"));
        if let (true, Some(exe)) = (open.clicked(), exe) {
            run(ui, Request::OpenLocation(exe.to_path_buf()));
        }
    }

    /// History graphs and the process list of the app with group `key`.
    fn detail_panel(&self, ui: &mut egui::Ui, key: &str, history: &History) -> Option<UiAction> {
        profile_scope!("details pane");
        let procs = &self.snapshot.processes;
        let mut members: Vec<usize> = (0..procs.len())
            .filter(|&i| &*procs[i].group_key == key)
            .collect();
        members.sort_by(|&a, &b| {
            procs[b]
                .cpu_percent
                .total_cmp(&procs[a].cpu_percent)
                .then(procs[a].pid.cmp(&procs[b].pid))
        });
        let group_history = history.group(key);
        let mut action = None;

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let name = members
                .first()
                .map(|&i| &*procs[i].name)
                .or(group_history.map(|g| &*g.name))
                .unwrap_or(key);
            ui.heading(name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✖").on_hover_text("Close (Esc)").clicked() {
                    action = Some(UiAction::CloseDetails);
                }
            });
        });
        match members.first().and_then(|&i| procs[i].exe.as_ref()) {
            Some(exe) => {
                ui.add(egui::Label::new(egui::RichText::new(exe.display().to_string()).weak()).truncate());
            }
            None if members.is_empty() => {
                ui.colored_label(ui.visuals().warn_fg_color, "Not running");
            }
            None => {}
        }
        ui.separator();

        match group_history {
            Some(g) => graphs::group_plots(ui, history, g),
            None => {
                ui.weak("No history yet");
            }
        }

        ui.add_space(6.0);
        let noun = if members.len() == 1 { "process" } else { "processes" };
        ui.strong(format!("{} {noun}", members.len()));
        egui::Grid::new("detail_procs")
            .num_columns(4)
            .striped(true)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for label in ["Name", "PID", "CPU %", "Memory"] {
                    ui.strong(label);
                }
                ui.end_row();
                for &i in &members {
                    let p = &procs[i];
                    let id = RowId::Proc(ProcKey::of(p));
                    let selected = self.selected.as_ref() == Some(&id);
                    if ui.selectable_label(selected, &*p.name).clicked() {
                        action = Some(UiAction::Reveal(id));
                    }
                    ui.label(p.pid.to_string());
                    ui.label(format::cpu(p.cpu_percent));
                    ui.label(format::mem(p.working_set));
                    ui.end_row();
                }
            });
        action
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.expires > now);
        let Some(next_expiry) = self.toasts.iter().map(|t| t.expires).min() else {
            return;
        };

        let mut dismissed = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (i, toast) in self.toasts.iter().enumerate() {
                    let frame = egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(420.0);
                        let color = if toast.is_error {
                            ui.visuals().error_fg_color
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(egui::RichText::new(&toast.message).color(color));
                    });
                    let click = frame.response.interact(egui::Sense::click());
                    if click.on_hover_text("Click to dismiss").clicked() {
                        dismissed = Some(i);
                    }
                }
            });
        if let Some(i) = dismissed {
            self.toasts.remove(i);
        }

        // Wake up once to remove the next toast; no continuous repainting.
        ctx.request_repaint_after(next_expiry.saturating_duration_since(now));
    }
}

/// Draws the panes of the dock. Actions are applied after the dock is
/// drawn, once it is back in the app.
struct Panes<'a> {
    app: &'a mut TaskManagerApp,
    history: &'a History,
    scroll_to: Option<usize>,
    actions: Vec<UiAction>,
}

impl TabViewer for Panes<'_> {
    type Tab = Pane;

    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        pane.title().into()
    }

    fn id(&mut self, pane: &mut Pane) -> egui::Id {
        egui::Id::new(("pane", *pane))
    }

    /// The table, treemap and graphs fill their pane; the others scroll.
    fn scroll_bars(&self, pane: &Pane) -> [bool; 2] {
        match pane {
            Pane::Details | Pane::Mini => [false, true],
            Pane::Processes | Pane::Treemap | Pane::Performance => [false, false],
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, pane: &mut Pane) {
        let app = &mut *self.app;
        let action = match pane {
            Pane::Processes => {
                app.view_controls(ui);
                ui.add_space(2.0);
                // Apply filter/mode edits this frame.
                app.refresh_view();
                app.process_table(ui, self.scroll_to, self.history)
            }
            Pane::Treemap => app
                .treemap
                .ui(ui, &app.snapshot.processes, app.selected.as_ref())
                .map(UiAction::Reveal),
            Pane::Performance => {
                profile_scope!("performance pane");
                graphs::system(ui, self.history, &app.snapshot.system);
                None
            }
            Pane::Details => match app.detail.clone() {
                Some(key) => app.detail_panel(ui, &key, self.history),
                None => {
                    ui.add_space(8.0);
                    ui.weak("Click an app in the table or the treemap to see its history and processes.");
                    None
                }
            },
            Pane::Mini => {
                app.mini_view(ui, self.history);
                None
            }
        };
        self.actions.extend(action);
    }
}

impl eframe::App for TaskManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        profile_scope!("update");
        self.snapshot = self.sampler.latest();
        while let Ok(outcome) = self.outcome_rx.try_recv() {
            self.toast(outcome.message, outcome.is_error);
        }

        // Held for this frame only; the sampler waits on it just to append.
        let history = Arc::clone(&self.history);
        let history = history.lock().unwrap();

        let menu = egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ui));
        let preset = menu.inner;
        let below_menu = ctx.screen_rect().with_min_y(menu.response.rect.max.y);
        if self.settings.totals_bar {
            egui::TopBottomPanel::top("totals").show(ctx, |ui| {
                profile_scope!("totals bar");
                ui.add_space(4.0);
                self.totals_panel(ui, &history);
                ui.add_space(4.0);
            });
        }

        self.refresh_view();

        // Forget an app that exited and whose history is gone.
        let detail_gone = self.detail.as_ref().is_some_and(|key| {
            history.group(key).is_none() && !self.snapshot.processes.iter().any(|p| p.group_key == *key)
        });
        if detail_gone {
            self.detail = None;
        }

        let scroll_to = std::mem::take(&mut self.scroll_to_selected)
            .then(|| self.selected_row_index())
            .flatten();
        let mut dock = std::mem::replace(&mut self.dock, DockState::new(Vec::new()));
        let mut panes = Panes {
            app: self,
            history: &history,
            scroll_to,
            actions: Vec::new(),
        };
        {
            profile_scope!("dock");
            DockArea::new(&mut dock)
                .show_add_buttons(false)
                .show(ctx, &mut panes);
        }
        let actions = panes.actions;
        self.dock = dock;
        for action in actions {
            self.apply(action);
        }
        drop(history);

        settings::window(ctx, below_menu, &mut self.show_settings, &mut self.settings, hotkey::LABEL);
        if let Some(preset) = preset {
            self.apply_preset(preset, ctx);
        }

        let unfocused = ctx.memory(|m| m.focused().is_none());
        if unfocused && self.detail.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.apply(UiAction::CloseDetails);
        }

        // Delete ends the selection, unless a text field has focus.
        if unfocused && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            let targets = self.selected_targets();
            if !targets.is_empty() {
                self.run(Request::End(targets));
            }
        }

        if self.apply_settings(ctx) {
            ctx.request_repaint();
        }
        self.show_toasts(ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let persisted = Persisted {
            settings: self.settings.clone(),
            mode: self.state.mode,
            sort: self.state.sort,
            compact_restore: self.compact_restore,
        };
        eframe::set_value(storage, eframe::APP_KEY, &persisted);
        eframe::set_value(storage, DOCK_KEY, &self.dock);
    }
}

/// Starts sampling with the NT backend, or with sysinfo when built with
/// feature `sysinfo` and run with `FTM_SOURCE=sysinfo` (for comparisons).
fn spawn_sampler(interval: Duration, ctx: egui::Context) -> SamplerHandle {
    let notify = move || ctx.request_repaint();
    #[cfg(feature = "sysinfo")]
    if std::env::var_os("FTM_SOURCE").is_some_and(|v| v == "sysinfo") {
        return SamplerHandle::spawn(crate::source::SysinfoSource::new(), interval, notify);
    }
    SamplerHandle::spawn(NtSource::new(), interval, notify)
}

/// Width rules of a table column; the last one takes the remaining width.
fn column_spec(column: TableColumn, last: bool) -> Column {
    let (initial, min) = match column {
        TableColumn::Name => (260.0, 80.0),
        TableColumn::Pid | TableColumn::ParentPid | TableColumn::Cpu => (60.0, 40.0),
        TableColumn::CpuHistory => (SPARKLINE_WIDTH, 40.0),
        TableColumn::Path => (300.0, 80.0),
        _ => (90.0, 50.0),
    };
    let spec = if last { Column::remainder() } else { Column::initial(initial) };
    spec.at_least(min).clip(true)
}

/// Right-aligned text filling the cell. Painted directly: a right-to-left
/// child layout would add a `Ui` (and its allocations) to every cell of every
/// visible row, every frame.
fn right(ui: &mut egui::Ui, text: String) {
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let rect = egui::Align2::RIGHT_CENTER.align_size_within_rect(galley.size(), ui.max_rect());
    let response = ui.allocate_rect(rect, egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text()));
    ui.painter().galley(rect.min, galley, ui.visuals().text_color());
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct MemoryStorage(HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn persisted_state_round_trips() {
        let mut saved = Persisted {
            mode: ViewMode::Tree,
            compact_restore: Some([800.0, 600.0]),
            ..Persisted::default()
        };
        saved.settings.columns.swap(0, 3);
        saved.settings.columns[1].visible = false;
        saved.settings.appearance.theme = settings::Theme::Light;

        let dock = Preset::Visual.dock();

        let mut storage = MemoryStorage::default();
        eframe::set_value(&mut storage, eframe::APP_KEY, &saved);
        eframe::set_value(&mut storage, DOCK_KEY, &dock);
        let loaded: Persisted = eframe::get_value(&storage, eframe::APP_KEY).unwrap();
        assert_eq!(loaded.settings, saved.settings);
        assert_eq!(loaded.mode, ViewMode::Tree);
        assert_eq!(loaded.compact_restore, saved.compact_restore);
        let loaded_dock: DockState<Pane> = eframe::get_value(&storage, DOCK_KEY).unwrap();
        let tabs = |d: &DockState<Pane>| d.iter_all_tabs().map(|((s, n), &t)| (s, n, t)).collect::<Vec<_>>();
        assert_eq!(tabs(&loaded_dock), tabs(&dock));
    }

    #[test]
    fn partial_save_takes_defaults() {
        let mut storage = MemoryStorage::default();
        storage.0.insert(eframe::APP_KEY.to_owned(), "(mode: Processes)".to_owned());
        let loaded: Persisted = eframe::get_value(&storage, eframe::APP_KEY).unwrap();
        assert_eq!(loaded.mode, ViewMode::Processes);
        assert_eq!(loaded.settings, Settings::default());
    }
}
