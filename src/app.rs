//! The UI: system totals and view controls on top, then the treemap, the
//! process table below, toasts in the bottom-right corner.
//!
//! The UI never polls. It repaints only when egui receives input, when the
//! sampler publishes a new snapshot, when an action finishes, when a toast
//! is due to expire, or while a treemap transition is running.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder, TableRow};

use crate::actions::{self, Outcome, Priority, ProcTarget, Request};
use crate::format;
use crate::model::Snapshot;
use crate::sampler::SamplerHandle;
use crate::source::SysinfoSource;
use crate::treemap::Treemap;
use crate::view::{self, Metrics, ProcKey, Row, RowId, RowKind, Sort, SortColumn, View, ViewMode, ViewState};

const INTERVALS_MS: [u64; 5] = [250, 500, 1000, 2000, 5000];
const INDENT: f32 = 16.0;
const TOAST_INFO: Duration = Duration::from_secs(4);
const TOAST_ERROR: Duration = Duration::from_secs(8);

const COLUMNS: [(SortColumn, &str); 12] = [
    (SortColumn::Name, "Name"),
    (SortColumn::Pid, "PID"),
    (SortColumn::ParentPid, "PPID"),
    (SortColumn::Cpu, "CPU %"),
    (SortColumn::Gpu, "GPU %"),
    (SortColumn::WorkingSet, "Working set"),
    (SortColumn::Private, "Private"),
    (SortColumn::GpuMemory, "GPU memory"),
    (SortColumn::DiskRead, "Disk read"),
    (SortColumn::DiskWrite, "Disk write"),
    (SortColumn::Started, "Running for"),
    (SortColumn::Path, "Path"),
];

/// Something the user did this frame, applied after the table is drawn.
enum UiAction {
    Sort(SortColumn),
    Toggle(RowId),
    Select(RowId),
    /// Selected in the treemap: also scroll the table to it.
    Reveal(RowId),
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
    state: ViewState,
    view: View,
    /// Snapshot seq the view was built from.
    view_seq: u64,
    view_dirty: bool,
    selected: Option<RowId>,
    /// Scroll the table to the selection on the next frame.
    scroll_to_selected: bool,
    treemap: Treemap,
    show_treemap: bool,
    interval_ms: u64,
    toasts: Vec<Toast>,
    outcome_tx: Sender<Outcome>,
    outcome_rx: Receiver<Outcome>,
    ctx: egui::Context,
}

impl TaskManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let interval_ms = 1000;
        let ctx = cc.egui_ctx.clone();
        let repaint_ctx = ctx.clone();
        let sampler = SamplerHandle::spawn(
            SysinfoSource::new(),
            Duration::from_millis(interval_ms),
            move || repaint_ctx.request_repaint(),
        );
        let (outcome_tx, outcome_rx) = mpsc::channel();
        Self {
            snapshot: sampler.latest(),
            sampler,
            state: ViewState::default(),
            view: View::default(),
            view_seq: u64::MAX,
            view_dirty: true,
            selected: None,
            scroll_to_selected: false,
            treemap: Treemap::default(),
            show_treemap: true,
            interval_ms,
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
            UiAction::Select(id) => self.selected = Some(id),
            UiAction::Reveal(id) => {
                self.selected = Some(id);
                self.scroll_to_selected = true;
            }
            UiAction::Run(request) => self.run(request),
        }
        // Show the result now rather than on the next snapshot.
        self.ctx.request_repaint();
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
                    .filter(|p| view::group_key(p) == *key)
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
                self.state.mode != ViewMode::Apps && view::group_key(&procs[i]) == *key
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

    fn totals_panel(&mut self, ui: &mut egui::Ui) {
        let snap = &self.snapshot;
        let sys = &snap.system;

        ui.horizontal(|ui| {
            ui.strong(format!("CPU {:.1}%", sys.cpu_percent));
            ui.separator();
            let mem_frac = if sys.total_memory > 0 {
                sys.used_memory as f32 / sys.total_memory as f32
            } else {
                0.0
            };
            ui.strong(format!(
                "RAM {} / {} ({:.0}%)",
                format::bytes(sys.used_memory),
                format::bytes(sys.total_memory),
                mem_frac * 100.0
            ));
            ui.separator();
            match &sys.gpu {
                Some(gpu) => {
                    ui.strong(format!("GPU {:.1}%", gpu.percent))
                        .on_hover_text("Busiest GPU engine, all adapters");
                    ui.label(format!(
                        "GPU mem {} dedicated / {} shared",
                        format::bytes(gpu.dedicated),
                        format::bytes(gpu.shared)
                    ));
                }
                None => {
                    ui.label("GPU n/a")
                        .on_hover_text("GPU performance counters are unavailable (needs a WDDM 2.0+ driver)");
                }
            }
            ui.separator();
            ui.label(format!("{} processes", snap.processes.len()));
            ui.separator();
            ui.label(format!(
                "sample took {:.1} ms",
                snap.sample_cost.as_secs_f64() * 1000.0
            ));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let before = self.interval_ms;
                egui::ComboBox::from_id_salt("interval")
                    .selected_text(interval_label(self.interval_ms))
                    .show_ui(ui, |ui| {
                        for ms in INTERVALS_MS {
                            ui.selectable_value(&mut self.interval_ms, ms, interval_label(ms));
                        }
                    });
                ui.label("Refresh:");
                if self.interval_ms != before {
                    self.sampler
                        .set_interval(Duration::from_millis(self.interval_ms));
                }
            });
        });

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (i, pct) in sys.per_core_percent.iter().enumerate() {
                ui.add(
                    egui::ProgressBar::new(pct / 100.0)
                        .desired_width(70.0)
                        .text(format!("{i}: {pct:.0}%")),
                );
            }
        });
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
            ui.separator();
            ui.toggle_value(&mut self.show_treemap, "Treemap");
        });
    }

    fn process_table(&self, ui: &mut egui::Ui, scroll_to: Option<usize>) -> Option<UiAction> {
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let mut action = None;
        let sort = self.state.sort;

        let mut builder = TableBuilder::new(ui);
        if let Some(row) = scroll_to {
            builder = builder.scroll_to_row(row, Some(egui::Align::Center));
        }
        builder
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(260.0).at_least(80.0).clip(true))
            .columns(Column::initial(60.0).at_least(40.0), 4)
            .columns(Column::initial(90.0).at_least(50.0), 6)
            .column(Column::remainder().at_least(80.0).clip(true))
            .header(row_height + 2.0, |mut header| {
                for (column, label) in COLUMNS {
                    header.col(|ui| {
                        let arrow = match (sort.column == column, sort.descending) {
                            (true, true) => " ⏷",
                            (true, false) => " ⏶",
                            _ => "",
                        };
                        let button = egui::Button::new(
                            egui::RichText::new(format!("{label}{arrow}")).strong(),
                        )
                        .frame(false);
                        if ui.add(button).clicked() {
                            action = Some(UiAction::Sort(column));
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, self.view.rows.len(), |mut table_row| {
                    let row = self.view.rows[table_row.index()];
                    self.table_row(&mut table_row, &row, &mut action);
                });
            });

        action
    }

    fn table_row(&self, table_row: &mut TableRow, row: &Row, action: &mut Option<UiAction>) {
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
        let has_gpu = self.snapshot.system.gpu.is_some();

        table_row.col(|ui| {
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
        });
        table_row.col(|ui| {
            if !is_aggregate {
                right(ui, m.pid.to_string());
            }
        });
        table_row.col(|ui| {
            if let (false, Some(ppid)) = (is_aggregate, m.parent_pid) {
                right(ui, ppid.to_string());
            }
        });
        table_row.col(|ui| right(ui, format::cpu(m.cpu_percent)));
        table_row.col(|ui| {
            if has_gpu {
                right(ui, format::cpu(m.gpu_percent));
            }
        });
        table_row.col(|ui| right(ui, format::mem(m.working_set)));
        table_row.col(|ui| right(ui, format::mem(m.private_bytes)));
        table_row.col(|ui| {
            if has_gpu {
                let cell = right_label(ui, format::mem(m.gpu_dedicated));
                if m.gpu_shared > 0 {
                    cell.on_hover_text(format!("Shared GPU memory: {}", format::mem(m.gpu_shared)));
                }
            }
        });
        table_row.col(|ui| right(ui, format::rate(m.disk_read_per_sec)));
        table_row.col(|ui| right(ui, format::rate(m.disk_write_per_sec)));
        table_row.col(|ui| {
            if m.start_time != 0 {
                right(
                    ui,
                    format::elapsed(self.snapshot.unix_time.saturating_sub(m.start_time)),
                );
            }
        });
        table_row.col(|ui| {
            if let Some(exe) = m.exe {
                ui.label(exe.display().to_string());
            }
        });

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

impl eframe::App for TaskManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snapshot = self.sampler.latest();
        while let Ok(outcome) = self.outcome_rx.try_recv() {
            let lifetime = if outcome.is_error { TOAST_ERROR } else { TOAST_INFO };
            self.toasts.push(Toast {
                message: outcome.message,
                is_error: outcome.is_error,
                expires: Instant::now() + lifetime,
            });
        }

        egui::TopBottomPanel::top("totals").show(ctx, |ui| {
            ui.add_space(4.0);
            self.totals_panel(ui);
            ui.add_space(2.0);
            self.view_controls(ui);
            ui.add_space(4.0);
        });

        // Rebuild after the controls so filter/mode edits apply this frame.
        if self.view_dirty || self.view_seq != self.snapshot.seq {
            self.view = self.state.build(&self.snapshot);
            // Always, so the treemap's process indices match the snapshot.
            self.treemap.set_groups(self.state.app_groups(&self.snapshot));
            self.view_seq = self.snapshot.seq;
            self.view_dirty = false;
        }

        if self.show_treemap {
            let max_height = (ctx.screen_rect().height() - 250.0).max(150.0);
            let picked = egui::TopBottomPanel::top("treemap")
                .resizable(true)
                .default_height(300.0)
                .height_range(120.0..=max_height)
                .show(ctx, |ui| {
                    self.treemap
                        .ui(ui, &self.snapshot.processes, self.selected.as_ref())
                })
                .inner;
            if let Some(id) = picked {
                self.apply(UiAction::Reveal(id));
            }
        }

        let scroll_to = std::mem::take(&mut self.scroll_to_selected)
            .then(|| self.selected_row_index())
            .flatten();
        let action = egui::CentralPanel::default()
            .show(ctx, |ui| self.process_table(ui, scroll_to))
            .inner;
        if let Some(action) = action {
            self.apply(action);
        }

        // Delete ends the selection, unless a text field has focus.
        if ctx.memory(|m| m.focused().is_none()) && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            let targets = self.selected_targets();
            if !targets.is_empty() {
                self.run(Request::End(targets));
            }
        }

        self.show_toasts(ctx);
    }
}

fn right(ui: &mut egui::Ui, text: String) {
    right_label(ui, text);
}

fn right_label(ui: &mut egui::Ui, text: String) -> egui::Response {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(text))
        .inner
}

fn interval_label(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{} s", ms / 1000)
    }
}
