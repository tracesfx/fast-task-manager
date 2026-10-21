// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Profiles the rest of the enclosing block with puffin and the summary log
/// (`--features puffin`, see `profiler`); expands to nothing otherwise.
macro_rules! profile_scope {
    ($name:expr) => {
        #[cfg(feature = "puffin")]
        puffin::profile_scope!($name);
        #[cfg(feature = "puffin")]
        let _section = $crate::profiler::Section::new($name);
    };
}

mod actions;
mod app;
mod format;
mod graphs;
mod history;
mod hotkey;
mod layout;
mod model;
#[cfg(feature = "puffin")]
mod profiler;
mod sampler;
mod settings;
mod source;
mod treemap;
mod view;

use eframe::egui;

fn main() -> eframe::Result {
    #[cfg(feature = "puffin")]
    let _profiler = profiler::start();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Fast Task Manager")
            .with_inner_size([1200.0, 720.0])
            // Small enough for the compact layout.
            .with_min_inner_size([280.0, 180.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Fast Task Manager",
        options,
        Box::new(|cc| Ok(Box::new(app::TaskManagerApp::new(cc)))),
    )
}
