//! History graphs: small live plots of the system totals in the top bar,
//! full-size ones in the Performance pane, a CPU sparkline per app row, and
//! the larger plots in the detail panel.
//!
//! The x axis is seconds relative to the newest sample, not to the wall
//! clock, so a graph only changes when a snapshot arrives and never needs a
//! repaint of its own. Hovering snaps to the nearest sample.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Mesh, Rect, Sense, Shape, Stroke, Vec2};
use egui_plot::{Line, Plot, PlotBounds, PlotPoint, PlotPoints, Points, VLine};

use crate::format;
use crate::history::{GroupHistory, History, SystemPoint, WINDOW};
use crate::model::SystemTotals;

pub const CPU_COLOR: Color32 = Color32::from_rgb(80, 160, 255);
pub const MEM_COLOR: Color32 = Color32::from_rgb(180, 120, 255);
pub const DISK_COLOR: Color32 = Color32::from_rgb(90, 200, 120);

const SMALL: Vec2 = vec2(200.0, 44.0);
const MIB: f64 = 1024.0 * 1024.0;

fn window() -> f64 {
    WINDOW.as_secs_f64()
}

/// Draws `points` (x ≤ 0, oldest first) on a fixed `[-WINDOW, 0] × [0, y_max]`
/// plot. Returns the plot response and the index of the hovered point.
fn plot(
    ui: &mut egui::Ui,
    id: &str,
    points: &[PlotPoint],
    y_max: f64,
    color: Color32,
    size: Vec2,
    axes: Option<fn(f64) -> String>,
) -> (egui::Response, Option<usize>) {
    let mut plot = Plot::new(id)
        .width(size.x)
        .height(size.y)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_x(false)
        .show_y(false)
        .cursor_color(Color32::TRANSPARENT)
        .set_margin_fraction(Vec2::ZERO);
    plot = match axes {
        Some(y_fmt) => plot
            .x_axis_formatter(|mark, _| ago(-mark.value))
            .y_axis_formatter(move |mark, _| y_fmt(mark.value))
            .y_axis_min_width(44.0),
        None => plot.show_axes(false).show_grid(false),
    };

    let response = plot.show(ui, |plot_ui| {
        plot_ui.set_plot_bounds(PlotBounds::from_min_max([-window(), 0.0], [0.0, y_max]));
        plot_ui.line(Line::new("", PlotPoints::Borrowed(points)).color(color).fill(0.0_f32).width(1.5_f32));
        let hovered = plot_ui.pointer_coordinate().and_then(|p| nearest(points, p.x))?;
        let at = points[hovered];
        plot_ui.vline(VLine::new("", at.x).color(ui_weak(plot_ui.ctx())).width(1.0_f32));
        plot_ui.points(Points::new("", [at.x, at.y]).color(color).radius(3.0_f32));
        Some(hovered)
    });
    (response.response, response.inner)
}

fn ui_weak(ctx: &egui::Context) -> Color32 {
    ctx.style().visuals.weak_text_color()
}

/// Index of the point closest to `x`; `points` are sorted by x.
fn nearest(points: &[PlotPoint], x: f64) -> Option<usize> {
    let i = points.partition_point(|p| p.x < x);
    match (i.checked_sub(1), (i < points.len()).then_some(i)) {
        (Some(b), Some(a)) => Some(if x - points[b].x <= points[a].x - x { b } else { a }),
        (b, a) => b.or(a),
    }
}

/// "now", "42 s ago", "3:05 ago".
fn ago(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0 => "now".to_owned(),
        1..=59 => format!("{s} s ago"),
        _ => format!("{}:{:02} ago", s / 60, s % 60),
    }
}

/// Paints `text` in the top-left corner of a small plot.
fn caption(ui: &egui::Ui, rect: Rect, text: String) {
    ui.painter().text(
        rect.left_top() + vec2(4.0, 2.0),
        Align2::LEFT_TOP,
        text,
        FontId::proportional(12.0),
        ui.visuals().strong_text_color(),
    );
}

/// Points of one system series, relative to the newest sample.
fn system_series(h: &History, value: impl Fn(&SystemPoint) -> f64) -> Vec<PlotPoint> {
    let now = h.now();
    h.system().iter().map(|p| PlotPoint::new(p.t - now, value(p))).collect()
}

/// The three small graphs of the top bar: total CPU, RAM and disk I/O.
pub fn totals(ui: &mut egui::Ui, h: &History, total_memory: u64) {
    profile_scope!("totals graphs");
    let Some(last) = h.system().back() else {
        return;
    };
    let now = h.now();

    // CPU: hovering shows the busiest apps at that moment.
    let points = system_series(h, |p| f64::from(p.cpu_percent));
    let (response, hovered) = plot(ui, "total_cpu", &points, 100.0, CPU_COLOR, SMALL, None);
    caption(ui, response.rect, format!("CPU {:.1}%", last.cpu_percent));
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_ui_at_pointer(|ui| top_apps_tooltip(ui, p, now));
    }

    let points = system_series(h, |p| p.used_memory as f64);
    let (response, hovered) = plot(ui, "total_ram", &points, total_memory.max(1) as f64, MEM_COLOR, SMALL, None);
    caption(
        ui,
        response.rect,
        format!("RAM {} / {}", format::bytes(last.used_memory), format::bytes(total_memory)),
    );
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_text_at_pointer(format!("{}\n{}", ago(now - p.t), format::bytes(p.used_memory)));
    }

    let io = |p: &SystemPoint| (p.disk_read_per_sec + p.disk_write_per_sec) as f64;
    let points = system_series(h, io);
    // Autoscaled, with a floor so an idle disk doesn't look busy.
    let peak = points.iter().map(|p| p.y).fold(0.0, f64::max);
    let (response, hovered) = plot(ui, "total_disk", &points, (peak * 1.1).max(MIB), DISK_COLOR, SMALL, None);
    caption(
        ui,
        response.rect,
        format!(
            "Disk R {} · W {}",
            format::rate(last.disk_read_per_sec),
            format::rate(last.disk_write_per_sec)
        ),
    );
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_text_at_pointer(format!(
            "{}\nRead {}\nWrite {}",
            ago(now - p.t),
            format::rate(p.disk_read_per_sec),
            format::rate(p.disk_write_per_sec)
        ));
    }
}

/// The Performance pane: CPU, RAM and disk I/O graphs sharing the height,
/// then the load of each logical core.
pub fn system(ui: &mut egui::Ui, h: &History, sys: &SystemTotals) {
    let Some(last) = h.system().back() else {
        ui.weak("No samples yet");
        return;
    };
    let now = h.now();
    let width = ui.available_width();
    const CORE_WIDTH: f32 = 70.0;
    let spacing = ui.spacing().item_spacing;
    let per_row = ((width + 4.0) / (CORE_WIDTH + 4.0)).floor().max(1.0) as usize;
    let core_rows = sys.per_core_percent.len().div_ceil(per_row) as f32;
    let cores_height = core_rows * (ui.spacing().interact_size.y + spacing.y);
    let label_height = ui.text_style_height(&egui::TextStyle::Body) + spacing.y;
    let free = ui.available_height() - cores_height - 3.0 * (label_height + spacing.y) - 8.0;
    let size = vec2(width, (free / 3.0).max(50.0));

    ui.strong(format!("CPU {:.1}%", last.cpu_percent));
    let points = system_series(h, |p| f64::from(p.cpu_percent));
    let (response, hovered) = plot(ui, "perf_cpu", &points, 100.0, CPU_COLOR, size, Some(|v| format!("{v:.0}%")));
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_ui_at_pointer(|ui| top_apps_tooltip(ui, p, now));
    }

    ui.strong(format!(
        "Memory {} / {}",
        format::bytes(last.used_memory),
        format::bytes(sys.total_memory)
    ));
    let points = system_series(h, |p| p.used_memory as f64);
    let y_max = sys.total_memory.max(1) as f64;
    let (response, hovered) = plot(ui, "perf_ram", &points, y_max, MEM_COLOR, size, Some(|v| format::bytes(v as u64)));
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_text_at_pointer(format!("{}
{}", ago(now - p.t), format::bytes(p.used_memory)));
    }

    ui.strong(format!(
        "Disk read {} · write {}",
        format::rate(last.disk_read_per_sec),
        format::rate(last.disk_write_per_sec)
    ));
    let points = system_series(h, |p| (p.disk_read_per_sec + p.disk_write_per_sec) as f64);
    let peak = points.iter().map(|p| p.y).fold(0.0, f64::max);
    let y_fmt = |v: f64| format::rate(v.max(0.0) as u64);
    let (response, hovered) = plot(ui, "perf_disk", &points, (peak * 1.1).max(MIB), DISK_COLOR, size, Some(y_fmt));
    if let Some(p) = hovered.and_then(|i| h.system().get(i)) {
        response.on_hover_text_at_pointer(format!(
            "{}
Read {}
Write {}",
            ago(now - p.t),
            format::rate(p.disk_read_per_sec),
            format::rate(p.disk_write_per_sec)
        ));
    }

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, pct) in sys.per_core_percent.iter().enumerate() {
            ui.add(
                egui::ProgressBar::new(pct / 100.0)
                    .desired_width(CORE_WIDTH)
                    .text(format!("{i}: {pct:.0}%")),
            );
        }
    });
}

fn top_apps_tooltip(ui: &mut egui::Ui, p: &SystemPoint, now: f64) {
    ui.label(format!("{} · CPU {:.1}%", ago(now - p.t), p.cpu_percent));
    if p.top.is_empty() {
        ui.weak("No app was using CPU");
        return;
    }
    ui.separator();
    egui::Grid::new("top_apps").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
        for app in &p.top {
            ui.label(&*app.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{}%", format::cpu(app.cpu_percent)));
            });
            ui.end_row();
        }
    });
}

/// A tiny CPU graph of one app, filling `size`. Each pixel column shows the
/// peak of the samples that fall into it, so short spikes stay visible.
pub fn sparkline(ui: &mut egui::Ui, h: &History, g: &GroupHistory, size: Vec2) {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    profile_scope!("sparkline");
    let now = h.now();
    let columns = rect.width().floor().max(1.0) as usize;
    let column = |t: f64| {
        let age = ((now - t) / window()).clamp(0.0, 1.0);
        ((1.0 - age) * (columns - 1) as f64).round() as usize
    };
    let (mut peak, mut sum, mut count) = (0.0f32, 0.0f32, 0u32);
    for (_, p) in h.group_series(g) {
        peak = peak.max(p.cpu_percent);
        sum += p.cpu_percent;
        count += 1;
    }
    // Scale to the app's own peak, but never so far that noise looks busy.
    let y_max = peak.max(5.0);

    // Points are oldest first, so columns only grow: each column's peak is
    // complete when the next column starts. Buffers are sized up front.
    let points = (count as usize).min(columns);
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(2 * points);
    mesh.reserve_triangles(2 * points);
    let mut line = Vec::with_capacity(points);
    let mut current: Option<(usize, f32)> = None;
    for (t, p) in h.group_series(g) {
        let col = column(t);
        current = match current {
            Some((c, v)) if c == col => Some((c, v.max(p.cpu_percent))),
            Some((c, v)) => {
                sparkline_point(&mut mesh, &mut line, rect, c, v / y_max);
                Some((col, p.cpu_percent))
            }
            None => Some((col, p.cpu_percent)),
        };
    }
    if let Some((c, v)) = current {
        sparkline_point(&mut mesh, &mut line, rect, c, v / y_max);
    }
    let painter = ui.painter_at(rect);
    painter.add(Shape::mesh(mesh));
    painter.add(Shape::line(line, Stroke::new(1.0_f32, CPU_COLOR)));

    response.on_hover_ui(|ui| {
        let avg = if count > 0 { sum / count as f32 } else { 0.0 };
        ui.label(format!(
            "CPU over the last {} min\npeak {}%, average {}%",
            WINDOW.as_secs() / 60,
            format::cpu(peak),
            format::cpu(avg)
        ));
    });
}

/// Adds pixel column `col` at height `frac` (0..=1) of `rect` to the
/// sparkline's filled area and outline.
fn sparkline_point(mesh: &mut Mesh, line: &mut Vec<egui::Pos2>, rect: Rect, col: usize, frac: f32) {
    let fill = CPU_COLOR.gamma_multiply(0.35);
    let x = rect.left() + col as f32 + 0.5;
    let y = rect.bottom() - frac * rect.height();
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(pos2(x, rect.bottom()), fill);
    mesh.colored_vertex(pos2(x, y), fill);
    if !line.is_empty() {
        mesh.add_triangle(base - 2, base - 1, base);
        mesh.add_triangle(base - 1, base, base + 1);
    }
    line.push(pos2(x, y));
}

/// Larger CPU and memory graphs of one app, for the detail panel.
pub fn group_plots(ui: &mut egui::Ui, h: &History, g: &GroupHistory) {
    profile_scope!("detail graphs");
    let now = h.now();
    let width = ui.available_width();
    let size = vec2(width, 130.0);

    let points: Vec<PlotPoint> = h
        .group_series(g)
        .map(|(t, p)| PlotPoint::new(t - now, f64::from(p.cpu_percent)))
        .collect();
    let peak = points.iter().map(|p| p.y).fold(0.0, f64::max);
    ui.strong("CPU");
    let (response, hovered) = plot(
        ui,
        "detail_cpu",
        &points,
        (peak * 1.15).max(5.0),
        CPU_COLOR,
        size,
        Some(|v| format!("{v:.0}%")),
    );
    if let Some(p) = hovered.map(|i| points[i]) {
        response.on_hover_text_at_pointer(format!("{}\n{}%", ago(-p.x), format::cpu(p.y as f32)));
    }

    ui.add_space(6.0);
    let points: Vec<PlotPoint> = h
        .group_series(g)
        .map(|(t, p)| PlotPoint::new(t - now, p.working_set() as f64 / MIB))
        .collect();
    let peak = points.iter().map(|p| p.y).fold(0.0, f64::max);
    ui.strong("Memory (working set)");
    let (response, hovered) = plot(
        ui,
        "detail_mem",
        &points,
        (peak * 1.15).max(10.0),
        MEM_COLOR,
        size,
        Some(|v| format!("{v:.0} MB")),
    );
    if let Some(p) = hovered.map(|i| points[i]) {
        response.on_hover_text_at_pointer(format!("{}\n{:.1} MB", ago(-p.x), p.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_point() {
        let pts: Vec<PlotPoint> = [-4.0, -2.0, 0.0].iter().map(|&x| PlotPoint::new(x, 0.0)).collect();
        assert_eq!(nearest(&pts, -10.0), Some(0));
        assert_eq!(nearest(&pts, -2.9), Some(1));
        assert_eq!(nearest(&pts, -0.9), Some(2));
        assert_eq!(nearest(&pts, 5.0), Some(2));
        assert_eq!(nearest(&[], 0.0), None);
    }

    #[test]
    fn ago_format() {
        assert_eq!(ago(0.2), "now");
        assert_eq!(ago(42.0), "42 s ago");
        assert_eq!(ago(185.0), "3:05 ago");
    }
}
