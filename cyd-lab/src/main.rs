//! cyd-lab: a desktop workbench for emulated ESP32 "Cheap Yellow Display" boards.
//!
//! Module map:
//! * `profile`  – board variant library (TOML, built-in + user dir)
//! * `device`   – on-disk device store (device.toml + flash.bin per board)
//! * `engine`   – the engine boundary: `Backend` trait, `ProcessBackend`, command line
//! * `weblink`  – live screen/touch link to esp32sim's `--web` WebSocket
//! * `app`      – the egui user interface

// No console window behind the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod combridge;
mod device;
mod engine;
mod profile;
mod rfc2217;
mod weblink;

fn main() -> eframe::Result {
    let paths = match app::Paths::discover() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cyd-lab: cannot set up data directory: {e:#}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cyd-lab — emulated Cheap Yellow Display boards")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("cyd-lab", options, Box::new(move |cc| Ok(Box::new(app::CydLabApp::new(cc, paths)))))
}
