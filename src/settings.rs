//! User settings and the Settings window: refresh interval, appearance,
//! table columns and treemap defaults. Saved with the dock layout through
//! eframe's storage (see `app::Persisted`); the app applies changes to
//! egui, the sampler and the window when they differ from what it last
//! applied, so nothing here runs per frame.

use eframe::egui::{self, vec2, Color32, FontId, Rect};
use serde::{Deserialize, Serialize};

use crate::treemap::{self, Metric, DEFAULT_GRADIENT};
use crate::view::SortColumn;

pub const INTERVALS_MS: [u64; 5] = [250, 500, 1000, 2000, 5000];
const UI_SCALES: [f32; 8] = [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
const DEFAULT_ACCENT: [u8; 3] = [0, 92, 128];
const MAX_STOPS: usize = 8;

const GRADIENTS: [(&str, &[[u8; 3]]); 4] = [
    ("Default", &DEFAULT_GRADIENT),
    ("Viridis", &[[68, 1, 84], [59, 82, 139], [33, 145, 140], [94, 201, 98], [253, 231, 37]]),
    ("Fire", &[[40, 40, 56], [128, 32, 40], [208, 72, 32], [240, 160, 48], [250, 225, 120]]),
    ("Gray", &[[56, 60, 68], [200, 204, 212]]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    System,
    Dark,
    Light,
}

/// A process table column. All but the CPU sparkline sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TableColumn {
    Name,
    Pid,
    ParentPid,
    Cpu,
    /// CPU sparkline; only shown in Apps mode.
    CpuHistory,
    Gpu,
    WorkingSet,
    Private,
    GpuMemory,
    DiskRead,
    DiskWrite,
    Started,
    Path,
}

impl TableColumn {
    pub const ALL: [TableColumn; 13] = [
        Self::Name,
        Self::Pid,
        Self::ParentPid,
        Self::Cpu,
        Self::CpuHistory,
        Self::Gpu,
        Self::WorkingSet,
        Self::Private,
        Self::GpuMemory,
        Self::DiskRead,
        Self::DiskWrite,
        Self::Started,
        Self::Path,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Pid => "PID",
            Self::ParentPid => "PPID",
            Self::Cpu => "CPU %",
            Self::CpuHistory => "CPU history",
            Self::Gpu => "GPU %",
            Self::GpuMemory => "GPU memory",
            Self::WorkingSet => "Working set",
            Self::Private => "Private",
            Self::DiskRead => "Disk read",
            Self::DiskWrite => "Disk write",
            Self::Started => "Running for",
            Self::Path => "Path",
        }
    }

    pub fn sort(self) -> Option<SortColumn> {
        Some(match self {
            Self::Name => SortColumn::Name,
            Self::Pid => SortColumn::Pid,
            Self::ParentPid => SortColumn::ParentPid,
            Self::Cpu => SortColumn::Cpu,
            Self::CpuHistory => return None,
            Self::Gpu => SortColumn::Gpu,
            Self::GpuMemory => SortColumn::GpuMemory,
            Self::WorkingSet => SortColumn::WorkingSet,
            Self::Private => SortColumn::Private,
            Self::DiskRead => SortColumn::DiskRead,
            Self::DiskWrite => SortColumn::DiskWrite,
            Self::Started => SortColumn::Started,
            Self::Path => SortColumn::Path,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSetting {
    pub column: TableColumn,
    pub visible: bool,
}

/// Everything that goes into the egui style.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    /// Selection, links and progress bars.
    pub accent: [u8; 3],
    /// Text size relative to egui's defaults.
    pub font_scale: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            accent: DEFAULT_ACCENT,
            font_scale: 1.0,
        }
    }
}

impl Appearance {
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_theme(match self.theme {
            Theme::System => egui::ThemePreference::System,
            Theme::Dark => egui::ThemePreference::Dark,
            Theme::Light => egui::ThemePreference::Light,
        });
        let [r, g, b] = self.accent;
        let accent = Color32::from_rgb(r, g, b);
        let on_accent = if luminance(accent) > 0.55 {
            Color32::from_gray(20)
        } else {
            Color32::from_gray(235)
        };
        let text_styles = egui::Style::default().text_styles;
        ctx.all_styles_mut(|style| {
            let mut visuals = if style.visuals.dark_mode {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            visuals.selection.bg_fill = accent;
            visuals.selection.stroke.color = on_accent;
            visuals.hyperlink_color = accent;
            style.visuals = visuals;
            style.text_styles = text_styles
                .iter()
                .map(|(name, font)| (name.clone(), FontId::new(font.size * self.font_scale, font.family.clone())))
                .collect();
        });
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub interval_ms: u64,
    pub appearance: Appearance,
    /// egui zoom factor; also changed with Ctrl +/-.
    pub ui_scale: f32,
    /// Treemap color scale stops, cool to hot.
    pub gradient: Vec<[u8; 3]>,
    /// All table columns in display order.
    pub columns: Vec<ColumnSetting>,
    /// Treemap metrics at startup.
    pub treemap_size: Metric,
    pub treemap_color: Metric,
    /// System graphs and process count above the dock.
    pub totals_bar: bool,
    pub always_on_top: bool,
    /// Global show/hide hotkey (`hotkey::LABEL`).
    pub hotkey: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            appearance: Appearance::default(),
            ui_scale: 1.0,
            gradient: DEFAULT_GRADIENT.to_vec(),
            columns: TableColumn::ALL
                .iter()
                .map(|&column| ColumnSetting { column, visible: true })
                .collect(),
            treemap_size: Metric::Memory,
            treemap_color: Metric::Cpu,
            totals_bar: true,
            always_on_top: false,
            hotkey: true,
        }
    }
}

impl Settings {
    /// Repairs values loaded from an older or hand-edited save.
    pub fn normalize(&mut self) {
        if !INTERVALS_MS.contains(&self.interval_ms) {
            self.interval_ms = 1000;
        }
        self.ui_scale = self.ui_scale.clamp(0.5, 3.0);
        self.appearance.font_scale = self.appearance.font_scale.clamp(0.7, 2.0);
        if !(2..=MAX_STOPS).contains(&self.gradient.len()) {
            self.gradient = DEFAULT_GRADIENT.to_vec();
        }
        // Each column exactly once; columns added in a later version show up.
        let mut seen = Vec::new();
        self.columns.retain(|c| {
            let new = !seen.contains(&c.column);
            seen.push(c.column);
            new
        });
        for (i, &column) in TableColumn::ALL.iter().enumerate() {
            if !seen.contains(&column) {
                // Right after the nearest earlier column that is present.
                let at = TableColumn::ALL[..i]
                    .iter()
                    .rev()
                    .find_map(|prev| self.columns.iter().position(|c| c.column == *prev))
                    .map_or(0, |p| p + 1);
                self.columns.insert(at, ColumnSetting { column, visible: true });
            }
        }
        for c in &mut self.columns {
            c.visible |= c.column == TableColumn::Name;
        }
    }

    pub fn visible_columns(&self) -> impl Iterator<Item = TableColumn> + '_ {
        self.columns.iter().filter(|c| c.visible).map(|c| c.column)
    }
}

pub fn interval_label(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{} s", ms / 1000)
    }
}

pub fn interval_combo(ui: &mut egui::Ui, id: &str, interval_ms: &mut u64) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(interval_label(*interval_ms))
        .show_ui(ui, |ui| {
            for ms in INTERVALS_MS {
                ui.selectable_value(interval_ms, ms, interval_label(ms));
            }
        });
}

fn luminance(c: Color32) -> f32 {
    (0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b())) / 255.0
}

/// The Settings window, kept inside `bounds` (below the menu bar, so the
/// menus stay usable in a small window). Edits `s` in place.
pub fn window(ctx: &egui::Context, bounds: Rect, open: &mut bool, s: &mut Settings, hotkey_label: &str) {
    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .constrain_to(bounds)
        .default_width(360.0_f32.min(bounds.width() - 24.0))
        .default_height(520.0_f32.min(bounds.height() - 24.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                general(ui, s, hotkey_label);
                appearance(ui, s);
                treemap_settings(ui, s);
                columns(ui, s);
                ui.separator();
                if ui.button("Restore defaults").clicked() {
                    *s = Settings::default();
                }
            });
        });
}

fn general(ui: &mut egui::Ui, s: &mut Settings, hotkey_label: &str) {
    egui::CollapsingHeader::new("General").default_open(true).show(ui, |ui| {
        egui::Grid::new("settings_general").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Refresh interval");
            interval_combo(ui, "settings_interval", &mut s.interval_ms);
            ui.end_row();
        });
        ui.checkbox(&mut s.totals_bar, "Show totals bar");
        ui.checkbox(&mut s.always_on_top, "Always on top");
        ui.checkbox(&mut s.hotkey, format!("Show/hide with {hotkey_label} (global hotkey)"));
    });
}

fn appearance(ui: &mut egui::Ui, s: &mut Settings) {
    egui::CollapsingHeader::new("Appearance").default_open(true).show(ui, |ui| {
        let a = &mut s.appearance;
        egui::Grid::new("settings_appearance").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Theme");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut a.theme, Theme::System, "System");
                ui.selectable_value(&mut a.theme, Theme::Dark, "Dark");
                ui.selectable_value(&mut a.theme, Theme::Light, "Light");
            });
            ui.end_row();

            ui.label("Accent color");
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut a.accent);
                if a.accent != DEFAULT_ACCENT && ui.small_button("Reset").clicked() {
                    a.accent = DEFAULT_ACCENT;
                }
            });
            ui.end_row();

            ui.label("Text size");
            ui.add(egui::Slider::new(&mut a.font_scale, 0.8..=1.6).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            ui.end_row();

            // A combo, not a slider: zooming while dragging moves the slider.
            ui.label("UI scale");
            egui::ComboBox::from_id_salt("settings_scale")
                .selected_text(format!("{:.0}%", s.ui_scale * 100.0))
                .show_ui(ui, |ui| {
                    for scale in UI_SCALES {
                        ui.selectable_value(&mut s.ui_scale, scale, format!("{:.0}%", scale * 100.0));
                    }
                })
                .response
                .on_hover_text("Also Ctrl + / Ctrl − / Ctrl 0");
            ui.end_row();
        });
    });
}

fn treemap_settings(ui: &mut egui::Ui, s: &mut Settings) {
    egui::CollapsingHeader::new("Treemap").default_open(true).show(ui, |ui| {
        egui::Grid::new("settings_treemap").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Default size");
            treemap::metric_combo(ui, "settings_tm_size", &mut s.treemap_size);
            ui.end_row();
            ui.label("Default color");
            treemap::metric_combo(ui, "settings_tm_color", &mut s.treemap_color);
            ui.end_row();

            ui.label("Gradient");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for stop in &mut s.gradient {
                    ui.color_edit_button_srgb(stop);
                }
                ui.add_space(6.0);
                if s.gradient.len() < MAX_STOPS && ui.small_button("+").on_hover_text("Add a color").clicked() {
                    let last = *s.gradient.last().unwrap_or(&[255, 255, 255]);
                    s.gradient.push(last);
                }
                if s.gradient.len() > 2 && ui.small_button("−").on_hover_text("Remove the last color").clicked() {
                    s.gradient.pop();
                }
            });
            ui.end_row();

            ui.label("");
            treemap::gradient_preview(ui, &s.gradient, vec2(200.0, 12.0));
            ui.end_row();

            ui.label("Presets");
            ui.horizontal_wrapped(|ui| {
                for (name, stops) in GRADIENTS {
                    if ui.selectable_label(s.gradient == stops, name).clicked() {
                        s.gradient = stops.to_vec();
                    }
                }
            });
            ui.end_row();
        });
    });
}

fn columns(ui: &mut egui::Ui, s: &mut Settings) {
    egui::CollapsingHeader::new("Table columns").default_open(true).show(ui, |ui| {
        let n = s.columns.len();
        let mut swap = None;
        for (i, c) in s.columns.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                if ui.add_enabled(i > 0, egui::Button::new("⏶").small()).on_hover_text("Move left").clicked() {
                    swap = Some(i - 1);
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new("⏷").small()).on_hover_text("Move right").clicked() {
                    swap = Some(i);
                }
                let name = c.column == TableColumn::Name;
                let checkbox = ui.add_enabled(!name, egui::Checkbox::new(&mut c.visible, c.column.label()));
                if name {
                    checkbox.on_disabled_hover_text("Always shown");
                } else if c.column == TableColumn::CpuHistory {
                    checkbox.on_hover_text("Shown in Apps mode only");
                }
            });
        }
        if let Some(i) = swap {
            s.columns.swap(i, i + 1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_repairs_columns_and_values() {
        let mut s = Settings {
            interval_ms: 7,
            gradient: vec![[0, 0, 0]],
            columns: vec![
                ColumnSetting { column: TableColumn::Cpu, visible: true },
                ColumnSetting { column: TableColumn::Name, visible: false },
                ColumnSetting { column: TableColumn::Cpu, visible: false },
            ],
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.interval_ms, 1000);
        assert_eq!(s.gradient, DEFAULT_GRADIENT.to_vec());
        let order: Vec<TableColumn> = s.columns.iter().map(|c| c.column).collect();
        assert_eq!(order.len(), TableColumn::ALL.len());
        // Missing columns go after their predecessor in `TableColumn::ALL`.
        assert_eq!(&order[..4], [TableColumn::Cpu, TableColumn::CpuHistory, TableColumn::Gpu, TableColumn::Name]);
        assert_eq!(order[4], TableColumn::Pid);
        assert!(s.columns[3].visible, "Name is always visible");
    }

    /// Settings saved by v0.1.0 (no GPU columns) keep their order and
    /// visibility; the GPU columns appear next to CPU and memory.
    #[test]
    fn normalize_adds_gpu_columns_to_old_settings() {
        use TableColumn::*;
        let old = [Name, Cpu, CpuHistory, WorkingSet, Private, DiskRead, DiskWrite, Pid, ParentPid, Started, Path];
        let mut s = Settings {
            columns: old
                .iter()
                .map(|&column| ColumnSetting { column, visible: column != Pid })
                .collect(),
            ..Settings::default()
        };
        s.normalize();
        let order: Vec<TableColumn> = s.columns.iter().map(|c| c.column).collect();
        assert_eq!(
            order,
            [Name, Cpu, CpuHistory, Gpu, WorkingSet, Private, GpuMemory, DiskRead, DiskWrite, Pid, ParentPid, Started, Path]
        );
        assert!(!s.columns[9].visible, "hidden columns stay hidden");
        assert!(s.columns[3].visible && s.columns[6].visible);
    }
}
