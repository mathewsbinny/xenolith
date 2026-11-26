//! `xenolith` (Xenolith): desktop GUI for Xbox 360 -> PC asset inspection and batch conversion, built on
//! `eframe` / `egui` / `egui_dock` and the Tool Suite Design Language (`design/tokens.json`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]
#![allow(clippy::too_many_lines, reason = "egui panel layouts read top to bottom")]

mod app;
mod cli;
mod pipeline;
mod task;
mod theme;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(&args));
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Xenolith — Xbox 360 to PC Porting Toolkit")
            .with_inner_size([1360.0, 860.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Xenolith",
        options,
        Box::new(|_cc| Ok(Box::new(app::StudioApp::default()))),
    )
}
