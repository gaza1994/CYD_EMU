//! cyd-lab: a desktop workbench for emulated ESP32 "Cheap Yellow Display" boards.
//!
//! Module map:
//! * `profile`  – board variant library (TOML, built-in + user dir)
//! * `device`   – on-disk device store (device.toml + flash.bin per board)
//! * `engine`   – the engine boundary: `Backend` trait, `ProcessBackend`, command line
//! * `weblink`  – live screen/touch link to esp32sim's `--web` WebSocket
//! * `running`  – running.json, the powered-on devices for the Arduino discovery
//! * `setup`    – first run: bundled engine, mask ROM download (pinned SHA-256), portable mode
//! * `firmware` – where a dropped .bin goes (merged / app / bootloader heuristics)
//! * `arduino_setup` – "Set up Arduino IDE": the board into the sketchbook's hardware/ folder
//! * `app`      – the egui user interface

// No console window behind the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod catalog;
mod catalog_assets;
mod arduino_setup;
mod combridge;
mod device;
mod engine;
mod firmware;
mod hotspot;
mod improv;
mod network;
mod profile;
mod rfc2217;
mod running;
mod setup;
mod webinstall;
mod weblink;

fn main() -> eframe::Result {
    let paths = match app::Paths::discover() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cyd-lab: cannot set up data directory: {e:#}");
            std::process::exit(1);
        }
    };
    let options = |renderer: eframe::Renderer| eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cyd-lab — emulated Cheap Yellow Display boards")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([800.0, 500.0]),
        renderer,
        ..Default::default()
    };
    let run = |renderer, paths: app::Paths| eframe::run_native("cyd-lab", options(renderer), Box::new(move |cc| Ok(Box::new(app::CydLabApp::new(cc, paths)))));
    #[cfg(windows)]
    {
        // DirectX 12 (or WARP) first; plain OpenGL if that cannot start.
        let first = run(eframe::Renderer::Wgpu, paths);
        if let Err(e) = &first {
            eprintln!("cyd-lab: wgpu renderer failed ({e}); retrying with OpenGL");
            if let Ok(p) = app::Paths::discover() {
                return run(eframe::Renderer::Glow, p);
            }
        }
        first
    }
    #[cfg(not(windows))]
    run(eframe::Renderer::Glow, paths)
}
