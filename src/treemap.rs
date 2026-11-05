//! Live treemap of app groups, or of one group's processes when drilled in.
//!
//! Layout is the squarified algorithm (Bruls, Huizing & van Wijk) run over
//! tiles in a *stable* order: each tile keeps its previous position in the
//! sequence and only overtakes its predecessor when it is clearly bigger, so
//! apps stay roughly where they were. Tiles too small to read are lumped into
//! one "Other" tile.
//!
//! Tiles animate from where they were drawn to their new target over `ANIM`,
//! but only when something visibly moves: a tile enters or leaves, or a
//! corner moves by `MIN_MOVE` px or more. Smaller moves and color changes
//! snap, so an idle system's usual jitter costs no extra frames. Repaints are
//! requested only while a transition runs; at rest the map costs nothing
//! until the next snapshot or input.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Layout, PointerButton, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, Vec2,
};
use eframe::epaint::text::{Galley, LayoutJob, TextFormat, TextWrapping};
use eframe::epaint::{mutex::Mutex, TextureAtlas};
use serde::{Deserialize, Serialize};

use crate::format;
use crate::model::ProcessInfo;
use crate::view::{Group, Metrics, ProcKey, RowId};

const ANIM: Duration = Duration::from_millis(300);
/// Tiles that would get less area than this (px²) are lumped into "Other".
const MIN_TILE_AREA: f32 = 900.0;
const MAX_TILES: usize = 120;
/// A tile already on the map stays out of "Other" until it drops below this
/// fraction of `MIN_TILE_AREA`, so tiles near the cut don't flicker.
const KEEP_FACTOR: f64 = 0.5;
/// In the stable order, a tile moves ahead of the one before it only when it
/// is this many times bigger.
const REORDER_RATIO: f64 = 1.5;
/// Corners moving less than this (px) snap instead of animating. Every
/// transition repaints for `ANIM`, so animating sub-pixel jitter would keep
/// an idle window repainting after nearly every snapshot.
const MIN_MOVE: f32 = 3.0;
/// The default color scale, cool to hot.
pub const DEFAULT_GRADIENT: [[u8; 3]; 6] = [
    [49, 84, 140],
    [38, 128, 150],
    [66, 150, 98],
    [196, 176, 58],
    [224, 122, 44],
    [208, 52, 44],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    Memory,
    Cpu,
    Disk,
    Gpu,
    GpuMemory,
}

impl Metric {
    pub const ALL: [Metric; 5] = [Metric::Memory, Metric::Cpu, Metric::Disk, Metric::Gpu, Metric::GpuMemory];

    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => "Memory",
            Self::Cpu => "CPU",
            Self::Disk => "Disk I/O",
            Self::Gpu => "GPU",
            Self::GpuMemory => "GPU memory",
        }
    }

    fn value(self, s: &Stats) -> f64 {
        match self {
            Self::Memory => s.working_set as f64,
            Self::Cpu => f64::from(s.cpu_percent),
            Self::Disk => (s.disk_read_per_sec + s.disk_write_per_sec) as f64,
            Self::Gpu => f64::from(s.gpu_percent),
            Self::GpuMemory => s.gpu_dedicated as f64,
        }
    }

    /// The value as shown inside a tile.
    fn format(self, s: &Stats) -> String {
        match self {
            Self::Memory => format::mem(s.working_set),
            Self::Cpu => format!("CPU {}%", format::cpu(s.cpu_percent)),
            Self::Disk => format!("Disk {}", format::rate(s.disk_read_per_sec + s.disk_write_per_sec)),
            Self::Gpu => format!("GPU {}%", format::cpu(s.gpu_percent)),
            Self::GpuMemory => format!("GPU {}", format::mem(s.gpu_dedicated)),
        }
    }

    /// 0 = cool, 1 = hot, on a fixed log scale so colors don't shift when
    /// the busiest app changes. Ends of the scale are `legend()`.
    fn heat(self, s: &Stats) -> f32 {
        let h = match self {
            Self::Cpu => percent_heat(s.cpu_percent),
            Self::Gpu => percent_heat(s.gpu_percent),
            Self::Memory => mem_heat(s.working_set),
            Self::GpuMemory => mem_heat(s.gpu_dedicated),
            Self::Disk => match self.value(s) {
                b if b <= 0.0 => 0.0,
                b => (b.log10() - 3.0) / 5.0,
            },
        };
        h.clamp(0.0, 1.0) as f32
    }

    /// Labels of the low and high (and beyond) ends of the color scale.
    fn legend(self) -> (&'static str, &'static str) {
        match self {
            Self::Cpu | Self::Gpu => ("0%", "50%+"),
            Self::Memory | Self::GpuMemory => ("10 MB", "4 GB+"),
            Self::Disk => ("1 KB/s", "100 MB/s+"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    cpu_percent: f32,
    working_set: u64,
    private_bytes: u64,
    disk_read_per_sec: u64,
    disk_write_per_sec: u64,
    gpu_percent: f32,
    gpu_dedicated: u64,
    gpu_shared: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.cpu_percent += o.cpu_percent;
        self.working_set += o.working_set;
        self.private_bytes += o.private_bytes;
        self.disk_read_per_sec += o.disk_read_per_sec;
        self.disk_write_per_sec += o.disk_write_per_sec;
        self.gpu_percent += o.gpu_percent;
        self.gpu_dedicated += o.gpu_dedicated;
        self.gpu_shared += o.gpu_shared;
    }
}

impl From<&Metrics<'_>> for Stats {
    fn from(m: &Metrics) -> Self {
        Self {
            cpu_percent: m.cpu_percent,
            working_set: m.working_set,
            private_bytes: m.private_bytes,
            disk_read_per_sec: m.disk_read_per_sec,
            disk_write_per_sec: m.disk_write_per_sec,
            gpu_percent: m.gpu_percent,
            gpu_dedicated: m.gpu_dedicated,
            gpu_shared: m.gpu_shared,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TileKey {
    Group(Arc<str>),
    Proc(ProcKey),
    Other,
}

struct Tile {
    key: TileKey,
    name: Arc<str>,
    /// Path, PID, or what "Other" contains; shown in the tooltip.
    detail: String,
    /// Processes this tile stands for.
    members: Vec<ProcKey>,
    stats: Stats,
    /// The label: name, size value, color value. Set by `rebuild`.
    lines: Vec<String>,
    labels: Option<Labels>,
}

/// A tile's label lines laid out for one width and color. Reused across
/// frames while those and egui's font atlas stay the same: egui replaces the
/// atlas when the scale changes or it fills up, which moves every glyph.
struct Labels {
    width: f32,
    color: Color32,
    /// Keeps the address from being reused while it is compared.
    atlas: Weak<Mutex<TextureAtlas>>,
    galleys: Vec<Arc<Galley>>,
}

impl Tile {
    fn row_id(&self) -> Option<RowId> {
        match &self.key {
            TileKey::Group(key) => Some(RowId::Group(key.clone())),
            TileKey::Proc(key) => Some(RowId::Proc(*key)),
            TileKey::Other => None,
        }
    }

    /// The selection is this tile, or a process inside it.
    fn is_selected(&self, selected: &RowId) -> bool {
        match (&self.key, selected) {
            (TileKey::Group(a), RowId::Group(b)) => a == b,
            (_, RowId::Proc(p)) => self.members.contains(p),
            _ => false,
        }
    }

    fn other(lumped: &[Tile], drilled: bool) -> Tile {
        let mut stats = Stats::default();
        let mut members = Vec::new();
        for t in lumped {
            stats.add(&t.stats);
            members.extend_from_slice(&t.members);
        }
        let noun = if drilled { "processes" } else { "apps" };
        Tile {
            key: TileKey::Other,
            name: format!("Other ({})", lumped.len()).into(),
            detail: format!("{} smaller {noun}", lumped.len()),
            members,
            stats,
            lines: Vec::new(),
            labels: None,
        }
    }
}

/// One drawn rectangle, moving from `from` to `to` during the transition.
/// Coordinates are relative to the map's top-left corner.
struct Sprite {
    key: TileKey,
    /// Index into `Treemap::tiles`; `None` for a tile that is shrinking away.
    tile: Option<usize>,
    from: Rect,
    to: Rect,
    from_heat: f32,
    to_heat: f32,
}

impl Sprite {
    fn at(&self, e: f32) -> (Rect, f32) {
        let rect = Rect::from_min_max(self.from.min.lerp(self.to.min, e), self.from.max.lerp(self.to.max, e));
        (rect, self.from_heat + (self.to_heat - self.from_heat) * e)
    }

    /// Whether this sprite is worth a transition: it is leaving, or a corner
    /// moves visibly. A color change alone snaps (it still blends while
    /// something else animates).
    fn moves(&self) -> bool {
        self.tile.is_none()
            || (self.from.min - self.to.min).abs().max_elem() >= MIN_MOVE
            || (self.from.max - self.to.max).abs().max_elem() >= MIN_MOVE
    }
}

/// Animates entering or leaving a group: tiles start scaled into (In) or out
/// of (Out) the group's tile.
enum Zoom {
    /// The group tile's rectangle when it was double-clicked.
    In(Rect),
    /// The group being left; its tile is found in the new layout.
    Out(Arc<str>),
}

/// Everything the layout depends on; it is recomputed when this changes.
#[derive(PartialEq)]
struct BuildKey {
    data_gen: u64,
    size_metric: Metric,
    color_metric: Metric,
    drill: Option<Arc<str>>,
    size: Vec2,
}

pub struct Treemap {
    size_metric: Metric,
    color_metric: Metric,
    /// Group key of the app being shown as its processes.
    drill: Option<Arc<str>>,
    zoom: Option<Zoom>,
    groups: Vec<Group>,
    data_gen: u64,
    tiles: Vec<Tile>,
    sprites: Vec<Sprite>,
    /// Layout order of the last build, for stability.
    order: Vec<TileKey>,
    /// Start of the running transition; `None` at rest.
    anim_start: Option<Instant>,
    built: Option<BuildKey>,
    /// Color scale stops, cool to hot.
    gradient: Vec<Color32>,
    /// Selected tiles' rectangles, gathered while drawing (reused).
    outlined: Vec<Rect>,
}

impl Default for Treemap {
    fn default() -> Self {
        Self {
            size_metric: Metric::Memory,
            color_metric: Metric::Cpu,
            drill: None,
            zoom: None,
            groups: Vec::new(),
            data_gen: 0,
            tiles: Vec::new(),
            sprites: Vec::new(),
            order: Vec::new(),
            anim_start: None,
            built: None,
            gradient: to_colors(&DEFAULT_GRADIENT),
            outlined: Vec::new(),
        }
    }
}

impl Treemap {
    /// New groups for the current snapshot; the `ui` call that follows must
    /// pass that same snapshot's processes.
    pub fn set_groups(&mut self, groups: Vec<Group>) {
        self.groups = groups;
        self.data_gen += 1;
    }

    pub fn set_metrics(&mut self, size: Metric, color: Metric) {
        self.size_metric = size;
        self.color_metric = color;
    }

    pub fn set_gradient(&mut self, stops: &[[u8; 3]]) {
        self.gradient = to_colors(stops);
    }

    /// Draws the map. Returns the row the user clicked, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui, procs: &[ProcessInfo], selected: Option<&RowId>) -> Option<RowId> {
        profile_scope!("treemap");
        self.header(ui, procs);
        let size = ui.available_size().max(vec2(1.0, 1.0));
        let (response, painter) = ui.allocate_painter(size, Sense::click());
        let area = response.rect;
        let painter = painter.with_clip_rect(area);
        self.rebuild(area.size(), procs);

        let t = self.progress();
        if t < 1.0 {
            ui.ctx().request_repaint();
        } else if self.anim_start.take().is_some() {
            self.sprites.retain(|s| s.tile.is_some());
        }
        let e = ease(t);

        painter.rect_filled(area, 0.0, ui.visuals().extreme_bg_color);
        if self.tiles.is_empty() {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                format!("No {} usage to show", self.size_metric.label()),
                FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
        }

        let origin = area.min.to_vec2();
        let pointer = response.hover_pos();
        let atlas = ui.fonts(|f| f.texture_atlas());
        let mut hovered: Option<(usize, Rect)> = None;
        self.outlined.clear();
        for sprite in &self.sprites {
            let (rect, heat) = sprite.at(e);
            let rect = rect.translate(origin);
            if rect.width() < 1.0 || rect.height() < 1.0 {
                continue;
            }
            let mut fill = heat_color(heat, &self.gradient);
            match sprite.tile {
                None => fill = fill.gamma_multiply(1.0 - e),
                Some(i) => {
                    if pointer.is_some_and(|p| rect.contains(p)) {
                        hovered = Some((i, rect));
                    }
                    if selected.is_some_and(|s| self.tiles[i].is_selected(s)) {
                        self.outlined.push(rect);
                    }
                }
            }
            // Shrinking by half a pixel leaves a 1 px gap between tiles.
            painter.rect_filled(rect.shrink(0.5), 2.0, fill);
            if let Some(i) = sprite.tile {
                draw_labels(&painter, &mut self.tiles[i], rect, fill, &atlas);
            }
        }
        for &rect in &self.outlined {
            painter.rect_stroke(rect.shrink(0.5), 2.0, Stroke::new(1.0_f32, Color32::BLACK), StrokeKind::Inside);
            painter.rect_stroke(rect.shrink(1.5), 2.0, Stroke::new(2.0_f32, Color32::WHITE), StrokeKind::Inside);
        }

        let back = response.clicked_by(PointerButton::Extra1)
            || (response.hovered()
                && ui.memory(|m| m.focused().is_none())
                && ui.input(|i| i.key_pressed(egui::Key::Backspace)));
        if back && self.drill.is_some() {
            self.drill_out();
            ui.ctx().request_repaint();
        }

        let (i, rect) = hovered?;
        painter.rect_stroke(
            rect.shrink(0.5),
            2.0,
            Stroke::new(1.5_f32, Color32::from_white_alpha(200)),
            StrokeKind::Inside,
        );
        let mut picked = None;
        if response.clicked() {
            picked = self.tiles[i].row_id();
        }
        if let (true, TileKey::Group(key)) = (response.double_clicked(), &self.tiles[i].key) {
            self.drill = Some(key.clone());
            self.zoom = Some(Zoom::In(rect.translate(-origin)));
            ui.ctx().request_repaint();
        }
        let tile = &self.tiles[i];
        response.on_hover_ui_at_pointer(|ui| tooltip(ui, tile));
        picked
    }

    fn drill_out(&mut self) {
        if let Some(key) = self.drill.take() {
            self.zoom = Some(Zoom::Out(key));
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, procs: &[ProcessInfo]) {
        ui.horizontal(|ui| {
            match self.drill.as_ref().and_then(|key| self.groups.iter().find(|g| g.key == *key)) {
                None => {
                    ui.strong("All apps");
                }
                Some(group) => {
                    let name = &procs[group.members[0]].name;
                    let count = group.members.len();
                    let noun = if count == 1 { "process" } else { "processes" };
                    let back = ui
                        .link("All apps")
                        .on_hover_text("Back (Backspace or mouse back button)");
                    ui.label("›");
                    ui.strong(format!("{name} ({count} {noun})"));
                    if back.clicked() {
                        self.drill_out();
                    }
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                legend(ui, self.color_metric, &self.gradient);
                metric_combo(ui, "treemap_color", &mut self.color_metric);
                ui.label("Color:");
                ui.separator();
                metric_combo(ui, "treemap_size", &mut self.size_metric);
                ui.label("Size:");
            });
        });
    }

    /// Transition progress, 0..=1 (1 at rest).
    fn progress(&self) -> f32 {
        self.anim_start.map_or(1.0, |start| {
            (start.elapsed().as_secs_f32() / ANIM.as_secs_f32()).min(1.0)
        })
    }

    /// Recomputes tiles and layout if the data, metrics, drill level or size
    /// changed, and starts a transition from what is currently drawn.
    fn rebuild(&mut self, size: Vec2, procs: &[ProcessInfo]) {
        // The drilled-into app exited or was filtered out.
        if let Some(drill) = &self.drill {
            if !self.groups.iter().any(|g| g.key == *drill) {
                self.drill = None;
                self.zoom = None;
            }
        }
        let key = BuildKey {
            data_gen: self.data_gen,
            size_metric: self.size_metric,
            color_metric: self.color_metric,
            drill: self.drill.clone(),
            size,
        };
        if self.built.as_ref() == Some(&key) {
            return;
        }
        profile_scope!("treemap rebuild");
        let prev = self.built.replace(key);
        let drill_changed = prev.as_ref().is_some_and(|p| p.drill != self.drill);
        // A fresh view gets a fresh order; a resize or the first build snaps.
        let fresh = drill_changed || prev.as_ref().is_some_and(|p| p.size_metric != self.size_metric);
        let snap = prev.as_ref().is_none_or(|p| p.size != size);

        let e = ease(self.progress());
        let prev_order = if fresh { &[][..] } else { &self.order[..] };
        let mut tiles = arrange(
            self.collect_tiles(procs),
            self.size_metric,
            prev_order,
            size.x * size.y,
            self.drill.is_some(),
        );
        for tile in &mut tiles {
            tile.lines = self.label_lines(tile);
        }
        let values: Vec<f32> = tiles.iter().map(|t| self.size_metric.value(&t.stats) as f32).collect();
        let bounds = Rect::from_min_size(Pos2::ZERO, size);
        let targets = squarify(&values, bounds);

        let current: HashMap<&TileKey, (Rect, f32)> = if drill_changed {
            HashMap::new()
        } else {
            self.sprites
                .iter()
                .filter(|s| s.tile.is_some())
                .map(|s| (&s.key, s.at(e)))
                .collect()
        };
        let zoom_frame = match self.zoom.take() {
            Some(Zoom::In(parent)) => Some((bounds, parent)),
            Some(Zoom::Out(group)) => tiles
                .iter()
                .position(|t| t.key == TileKey::Group(group.clone()))
                .map(|j| (targets[j], bounds)),
            None => None,
        };

        let mut sprites = Vec::with_capacity(tiles.len());
        // Tiles that left the map shrink away underneath the others.
        if !snap && !drill_changed {
            let alive: HashSet<&TileKey> = tiles.iter().map(|t| &t.key).collect();
            for s in self.sprites.iter().filter(|s| s.tile.is_some() && !alive.contains(&s.key)) {
                let (rect, heat) = s.at(e);
                sprites.push(Sprite {
                    key: s.key.clone(),
                    tile: None,
                    from: rect,
                    to: Rect::from_center_size(rect.center(), Vec2::ZERO),
                    from_heat: heat,
                    to_heat: heat,
                });
            }
        }
        for (i, (tile, &to)) in tiles.iter().zip(&targets).enumerate() {
            let to_heat = self.color_metric.heat(&tile.stats);
            let (from, from_heat) = if snap {
                (to, to_heat)
            } else if let Some(&now) = current.get(&tile.key) {
                now
            } else if let Some(from) = zoom_frame.and_then(|(src, dst)| map_rect(to, src, dst)) {
                (from, to_heat)
            } else {
                (Rect::from_center_size(to.center(), Vec2::ZERO), to_heat)
            };
            sprites.push(Sprite {
                key: tile.key.clone(),
                tile: Some(i),
                from,
                to,
                from_heat,
                to_heat,
            });
        }
        drop(current);

        // At rest, sprites are drawn at `to`: small changes snap.
        self.anim_start = sprites.iter().any(Sprite::moves).then(Instant::now);
        self.order = tiles.iter().map(|t| t.key.clone()).collect();
        self.tiles = tiles;
        self.sprites = sprites;
    }

    fn collect_tiles(&self, procs: &[ProcessInfo]) -> Vec<Tile> {
        match &self.drill {
            Some(key) => self
                .groups
                .iter()
                .find(|g| g.key == *key)
                .into_iter()
                .flat_map(|g| &g.members)
                .map(|&i| {
                    let p = &procs[i];
                    Tile {
                        key: TileKey::Proc(ProcKey::of(p)),
                        name: Arc::clone(&p.name),
                        detail: format!("PID {}", p.pid),
                        members: vec![ProcKey::of(p)],
                        stats: Stats::from(&Metrics::of(p)),
                        lines: Vec::new(),
                        labels: None,
                    }
                })
                .collect(),
            None => self
                .groups
                .iter()
                .map(|g| {
                    let m = g.metrics(procs);
                    Tile {
                        key: TileKey::Group(Arc::clone(&g.key)),
                        name: Arc::clone(&procs[g.members[0]].name),
                        detail: m.exe.map(|e| e.display().to_string()).unwrap_or_default(),
                        members: g.members.iter().map(|&i| ProcKey::of(&procs[i])).collect(),
                        stats: Stats::from(&m),
                        lines: Vec::new(),
                        labels: None,
                    }
                })
                .collect(),
        }
    }

    /// Name, then the size value, then the color value.
    fn label_lines(&self, tile: &Tile) -> Vec<String> {
        let mut lines = vec![tile.name.to_string(), self.size_metric.format(&tile.stats)];
        if self.color_metric != self.size_metric {
            lines.push(self.color_metric.format(&tile.stats));
        }
        lines
    }
}

/// Draws as many of the tile's label lines as fit, laying them out again
/// only when the width, color or font atlas changed.
fn draw_labels(painter: &egui::Painter, tile: &mut Tile, rect: Rect, fill: Color32, atlas: &Arc<Mutex<TextureAtlas>>) {
    let inner = rect.shrink2(vec2(5.0, 3.0));
    if inner.width() < 28.0 || inner.height() < 13.0 {
        return;
    }
    let color = if luminance(fill) > 0.55 {
        Color32::from_black_alpha(230)
    } else {
        Color32::WHITE
    };
    let fresh = tile.labels.as_ref().is_some_and(|l| {
        l.width == inner.width() && l.color == color && Weak::as_ptr(&l.atlas) == Arc::as_ptr(atlas)
    });
    if !fresh {
        let galleys = tile
            .lines
            .iter()
            .enumerate()
            .map(|(k, text)| {
                let font = FontId::proportional(if k == 0 { 13.0 } else { 11.0 });
                let mut job = LayoutJob::single_section(text.clone(), TextFormat::simple(font, color));
                job.wrap = TextWrapping {
                    max_width: inner.width(),
                    max_rows: 1,
                    break_anywhere: true,
                    overflow_character: Some('…'),
                };
                painter.layout_job(job)
            })
            .collect();
        tile.labels = Some(Labels {
            width: inner.width(),
            color,
            atlas: Arc::downgrade(atlas),
            galleys,
        });
    }
    let Some(labels) = &tile.labels else {
        return;
    };
    let mut y = inner.min.y;
    for galley in &labels.galleys {
        if y + galley.size().y > inner.max.y + 1.0 {
            break;
        }
        painter.galley(pos2(inner.min.x, y), Arc::clone(galley), color);
        y += galley.size().y;
    }
}

/// Picks the tiles to show and their layout order, lumping the ones too
/// small to read into a trailing "Other" tile. Tiles with no size are left
/// out, or counted in "Other".
fn arrange(tiles: Vec<Tile>, metric: Metric, prev: &[TileKey], area: f32, drilled: bool) -> Vec<Tile> {
    let total: f64 = tiles.iter().map(|t| metric.value(&t.stats)).sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let min_frac = f64::from(MIN_TILE_AREA / area.max(1.0));
    let rank: HashMap<&TileKey, usize> = prev.iter().enumerate().map(|(i, k)| (k, i)).collect();

    let mut by_size: Vec<Tile> = tiles;
    by_size.sort_by(|a, b| metric.value(&b.stats).total_cmp(&metric.value(&a.stats)));
    let (mut kept, mut lumped) = (Vec::new(), Vec::new());
    for tile in by_size {
        let frac = metric.value(&tile.stats) / total;
        let needed = if rank.contains_key(&tile.key) {
            min_frac * KEEP_FACTOR
        } else {
            min_frac
        };
        if frac > 0.0 && frac >= needed && kept.len() < MAX_TILES {
            kept.push(tile);
        } else {
            lumped.push(tile);
        }
    }
    // "Other" around a single tile would only hide its name.
    if lumped.len() == 1 && metric.value(&lumped[0].stats) > 0.0 {
        kept.append(&mut lumped);
    }

    stable_order(&mut kept, |t| metric.value(&t.stats), |t| rank.get(&t.key).copied());
    if !lumped.is_empty() {
        let other = Tile::other(&lumped, drilled);
        if metric.value(&other.stats) > 0.0 {
            kept.push(other);
        }
    }
    kept
}

/// Orders items for layout: by their previous position so the map doesn't
/// reshuffle on every sample, new items after them biggest first. Then an
/// item moves ahead of its predecessor only if it is `REORDER_RATIO` times
/// bigger, so the order stays roughly biggest-first without jitter.
fn stable_order<T>(items: &mut [T], value: impl Fn(&T) -> f64, prev_rank: impl Fn(&T) -> Option<usize>) {
    items.sort_by(|a, b| {
        let (ra, rb) = (prev_rank(a).unwrap_or(usize::MAX), prev_rank(b).unwrap_or(usize::MAX));
        ra.cmp(&rb).then_with(|| value(b).total_cmp(&value(a)))
    });
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 && value(&items[j]) > value(&items[j - 1]) * REORDER_RATIO {
            items.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// Squarified treemap layout (Bruls, Huizing & van Wijk, 2000). Returns one
/// rectangle per value, in input order, tiling `bounds` with areas
/// proportional to the values. Items are placed in the order given (the
/// caller decides it); non-positive values get an empty rectangle.
pub fn squarify(values: &[f32], bounds: Rect) -> Vec<Rect> {
    let empty = Rect::from_center_size(bounds.center(), Vec2::ZERO);
    let mut out = vec![empty; values.len()];
    let total: f64 = values.iter().filter(|&&v| v > 0.0).map(|&v| f64::from(v)).sum();
    if total <= 0.0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return out;
    }
    let scale = f64::from(bounds.width()) * f64::from(bounds.height()) / total;
    let items: Vec<(usize, f64)> = values
        .iter()
        .enumerate()
        .filter(|(_, &v)| v > 0.0)
        .map(|(i, &v)| (i, f64::from(v) * scale))
        .collect();

    let (mut x0, mut y0) = (f64::from(bounds.min.x), f64::from(bounds.min.y));
    let (x1, y1) = (f64::from(bounds.max.x), f64::from(bounds.max.y));
    let mut start = 0;
    while start < items.len() {
        let (w, h) = (x1 - x0, y1 - y0);
        let side = w.min(h);
        if side <= 1e-9 {
            break;
        }
        // Grow the row while that improves its worst aspect ratio.
        let first = items[start].1;
        let (mut sum, mut lo, mut hi) = (first, first, first);
        let mut worst = worst_ratio(sum, lo, hi, side);
        let mut end = start + 1;
        while end < items.len() {
            let a = items[end].1;
            let next = worst_ratio(sum + a, lo.min(a), hi.max(a), side);
            if next > worst {
                break;
            }
            (sum, lo, hi, worst) = (sum + a, lo.min(a), hi.max(a), next);
            end += 1;
        }

        // The row is a strip along the shorter side; the last row takes
        // whatever is left so rounding never leaves a gap.
        let row = &items[start..end];
        let last = end == items.len();
        if w >= h {
            let thick = if last { w } else { (sum / h).min(w) };
            let mut y = y0;
            for (k, &(i, a)) in row.iter().enumerate() {
                let len = if k + 1 == row.len() { y1 - y } else { a / thick };
                out[i] = rect(x0, y, x0 + thick, y + len);
                y += len;
            }
            x0 += thick;
        } else {
            let thick = if last { h } else { (sum / w).min(h) };
            let mut x = x0;
            for (k, &(i, a)) in row.iter().enumerate() {
                let len = if k + 1 == row.len() { x1 - x } else { a / thick };
                out[i] = rect(x, y0, x + len, y0 + thick);
                x += len;
            }
            y0 += thick;
        }
        start = end;
    }
    out
}

/// Worst aspect ratio of a row with the given total, smallest and largest
/// area, laid along a side of length `side`.
fn worst_ratio(sum: f64, lo: f64, hi: f64, side: f64) -> f64 {
    let (s2, w2) = (sum * sum, side * side);
    (w2 * hi / s2).max(s2 / (w2 * lo))
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
    Rect::from_min_max(pos2(x0 as f32, y0 as f32), pos2(x1 as f32, y1 as f32))
}

/// Maps `r` from the frame `src` into the frame `dst`.
fn map_rect(r: Rect, src: Rect, dst: Rect) -> Option<Rect> {
    if src.width() <= 0.0 || src.height() <= 0.0 {
        return None;
    }
    let scale = dst.size() / src.size();
    let map = |p: Pos2| dst.min + (p - src.min) * scale;
    Some(Rect::from_min_max(map(r.min), map(r.max)))
}

fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Cool-to-hot gradient: blue, teal, green, yellow, orange, red.
fn to_colors(stops: &[[u8; 3]]) -> Vec<Color32> {
    stops.iter().map(|&[r, g, b]| Color32::from_rgb(r, g, b)).collect()
}

/// `heat` (0..=1) on the gradient through `stops`.
fn heat_color(heat: f32, stops: &[Color32]) -> Color32 {
    match stops {
        [] => Color32::GRAY,
        [only] => *only,
        _ => {
            let x = heat.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
            let i = (x as usize).min(stops.len() - 2);
            stops[i].lerp_to_gamma(stops[i + 1], x - i as f32)
        }
    }
}

/// Log scale from 0% to 50%.
fn percent_heat(percent: f32) -> f64 {
    (1.0 + f64::from(percent)).ln() / 51f64.ln()
}

/// Log scale from 10 MB to 4 GB.
fn mem_heat(bytes: u64) -> f64 {
    let mb = (bytes as f64 / (1024.0 * 1024.0)).max(1.0);
    (mb.log2() - 10f64.log2()) / (4096f64.log2() - 10f64.log2())
}

fn luminance(c: Color32) -> f32 {
    (0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b())) / 255.0
}

pub fn metric_combo(ui: &mut egui::Ui, id: &str, value: &mut Metric) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for m in Metric::ALL {
                ui.selectable_value(value, m, m.label());
            }
        });
}

/// The color scale, low end on the left (added right to left).
fn legend(ui: &mut egui::Ui, metric: Metric, stops: &[Color32]) {
    let (lo, hi) = metric.legend();
    ui.label(RichText::new(hi).small());
    gradient_bar(ui, stops, vec2(80.0, 10.0)).on_hover_ui(|ui| {
        ui.label(format!("Color: {} (log scale)", metric.label()));
    });
    ui.label(RichText::new(lo).small());
}

/// Paints the color scale through `stops`, cool end on the left.
pub fn gradient_preview(ui: &mut egui::Ui, stops: &[[u8; 3]], size: Vec2) -> egui::Response {
    gradient_bar(ui, &to_colors(stops), size)
}

fn gradient_bar(ui: &mut egui::Ui, stops: &[Color32], size: Vec2) -> egui::Response {
    const STEPS: usize = 32;
    let (bar, response) = ui.allocate_exact_size(size, Sense::hover());
    let step = bar.width() / STEPS as f32;
    for k in 0..STEPS {
        let x = bar.min.x + k as f32 * step;
        let segment = Rect::from_min_max(pos2(x, bar.min.y), pos2(x + step + 0.5, bar.max.y));
        ui.painter()
            .rect_filled(segment, 0.0, heat_color((k as f32 + 0.5) / STEPS as f32, stops));
    }
    response
}

fn tooltip(ui: &mut egui::Ui, tile: &Tile) {
    ui.strong(&*tile.name);
    if !tile.detail.is_empty() {
        ui.label(RichText::new(&tile.detail).weak());
    }
    let s = &tile.stats;
    egui::Grid::new("treemap_tooltip").num_columns(2).show(ui, |ui| {
        let mut row = |label: &str, value: String| {
            ui.label(label);
            ui.label(value);
            ui.end_row();
        };
        if tile.members.len() > 1 {
            row("Processes", tile.members.len().to_string());
        }
        row("CPU", format!("{}%", format::cpu(s.cpu_percent)));
        row("Working set", format::mem(s.working_set));
        row("Private", format::mem(s.private_bytes));
        row("Disk read", format::rate(s.disk_read_per_sec));
        row("Disk write", format::rate(s.disk_write_per_sec));
        row("GPU", format!("{}%", format::cpu(s.gpu_percent)));
        if s.gpu_dedicated > 0 || s.gpu_shared > 0 {
            row("GPU dedicated", format::mem(s.gpu_dedicated));
            row("GPU shared", format::mem(s.gpu_shared));
        }
    });
    if matches!(tile.key, TileKey::Group(_)) {
        ui.label(RichText::new("Double-click to show its processes").weak().small());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(r: Rect) -> f32 {
        r.width() * r.height()
    }

    #[test]
    fn squarify_tiles_bounds_proportionally() {
        let values = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let bounds = Rect::from_min_size(pos2(10.0, 20.0), vec2(600.0, 400.0));
        let rects = squarify(&values, bounds);
        let scale = area(bounds) / values.iter().sum::<f32>();
        for (r, v) in rects.iter().zip(values) {
            assert!(bounds.expand(1e-3).contains_rect(*r), "{r:?} outside");
            assert!((area(*r) - v * scale).abs() < 1.0, "{r:?} should have area {}", v * scale);
            assert!(r.aspect_ratio().max(1.0 / r.aspect_ratio()) < 3.0, "{r:?} too thin");
        }
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let overlap = a.intersect(*b);
                assert!(overlap.width().max(0.0) * overlap.height().max(0.0) < 1e-2, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn squarify_skips_zero_values() {
        let rects = squarify(&[0.0, 5.0, 0.0], Rect::from_min_size(Pos2::ZERO, vec2(100.0, 50.0)));
        assert_eq!(area(rects[0]), 0.0);
        assert_eq!(area(rects[2]), 0.0);
        assert!((area(rects[1]) - 5000.0).abs() < 1e-2);
        assert!(squarify(&[0.0], Rect::from_min_size(Pos2::ZERO, vec2(10.0, 10.0)))[0].area() == 0.0);
    }

    #[test]
    fn only_visible_moves_animate() {
        let r = Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 50.0));
        let sprite = |to: Rect, tile| Sprite {
            key: TileKey::Other,
            tile,
            from: r,
            to,
            from_heat: 0.1,
            to_heat: 0.9,
        };
        // A color change alone, or jitter under MIN_MOVE, snaps.
        assert!(!sprite(r, Some(0)).moves());
        assert!(!sprite(r.translate(vec2(2.0, -1.0)), Some(0)).moves());
        assert!(sprite(r.translate(vec2(MIN_MOVE, 0.0)), Some(0)).moves());
        assert!(sprite(r.expand(MIN_MOVE), Some(0)).moves());
        // A tile that left the map always shrinks away.
        assert!(sprite(r, None).moves());
    }

    #[test]
    fn stable_order_resists_small_changes() {
        // Previously a, b, c. b grew a little: stays put. c grew a lot: moves to the front.
        let mut items = vec![("c", 30.0), ("b", 12.0), ("a", 10.0), ("new", 50.0)];
        let prev = ["a", "b", "c"];
        stable_order(&mut items, |t| t.1, |t| prev.iter().position(|p| *p == t.0));
        let names: Vec<&str> = items.iter().map(|t| t.0).collect();
        assert_eq!(names, ["new", "c", "a", "b"]);
    }

    fn tile(name: &str, mem: u64) -> Tile {
        Tile {
            key: TileKey::Group(name.into()),
            name: name.into(),
            detail: String::new(),
            members: Vec::new(),
            stats: Stats {
                working_set: mem,
                ..Default::default()
            },
            lines: Vec::new(),
            labels: None,
        }
    }

    #[test]
    fn small_tiles_are_lumped_into_other() {
        let mut tiles = vec![tile("big", 1000), tile("mid", 500)];
        tiles.extend((0..50).map(|i| tile(&format!("tiny{i}"), 1)));
        // 10_000 px² for ~1550 units: a tiny tile would get ~6 px².
        let out = arrange(tiles, Metric::Memory, &[], 10_000.0, false);
        let names: Vec<&str> = out.iter().map(|t| &*t.name).collect();
        assert_eq!(names, ["big", "mid", "Other (50)"]);
        assert_eq!(out[2].stats.working_set, 50);
    }

    #[test]
    fn heat_is_clamped_and_monotonic() {
        let cpu = |c: f32| Metric::Cpu.heat(&Stats { cpu_percent: c, ..Default::default() });
        assert_eq!(cpu(0.0), 0.0);
        assert!(cpu(1.0) < cpu(10.0));
        assert_eq!(cpu(100.0), 1.0);
        assert_eq!(Metric::Disk.heat(&Stats::default()), 0.0);
    }
}
