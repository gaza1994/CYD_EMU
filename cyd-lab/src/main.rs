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
mod consolelog;
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
    let options = |renderer: eframe::Renderer, paths: &app::Paths| eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cyd-lab — emulated Cheap Yellow Display boards")
            // Fits a 1080p screen with a taskbar (screen, checklist and console side by side);
            // smaller screens are fitted on the first frame. The last size and position are
            // remembered in window.ron in the data folder.
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0]),
        // Centred the first time only (eframe's centring would override a remembered position).
        centered: !paths.root.join("window.ron").exists(),
        persistence_path: Some(paths.root.join("window.ron")),
        renderer,
        ..Default::default()
    };
    let run = |renderer, paths: app::Paths| eframe::run_native("cyd-lab", options(renderer, &paths), Box::new(move |cc| Ok(Box::new(app::CydLabApp::new(cc, paths)))));
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
