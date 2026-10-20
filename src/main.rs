// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod format;
mod model;
mod sampler;
mod source;
mod treemap;
mod view;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Fast Task Manager")
            .with_inner_size([1200.0, 720.0])
            .with_min_inner_size([600.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Fast Task Manager",
        options,
        Box::new(|cc| Ok(Box::new(app::TaskManagerApp::new(cc)))),
    )
}
