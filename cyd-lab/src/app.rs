//! The egui user interface.
//!
//! Immediate-mode: every frame we poll each device's backend for console lines,
//! exit notifications and screen frames, then draw. Background threads (process
//! readers, WebSocket link) call the `Waker`, which asks egui to repaint, so the
//! UI is idle when nothing happens.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use serde::{Deserialize, Serialize};

use crate::device::{self, parse_offset, Device, DeviceStore};
use crate::engine::{self, Backend, BootMode, ChipStatus, EngineEvent, EngineFeatures, EngineSettings, LaunchSpec, ProcessBackend, RunState, Stream, Waker};
use crate::profile::{Profile, ProfileLibrary};

const CONSOLE_LINES: usize = 5000;

// ---------------------------------------------------------------------------
// Paths and settings

/// Where cyd-lab keeps its data. `CYD_LAB_HOME` overrides the default
/// (`%APPDATA%\cyd-lab` on Windows, `~/.local/share/cyd-lab` on Linux).
/// Shortest touch the panel will see; quicker clicks are stretched to this.
const MIN_TOUCH_HOLD: std::time::Duration = std::time::Duration::from_millis(120);

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub devices: PathBuf,
    pub profiles: PathBuf,
    pub settings: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Paths> {
        let root = match std::env::var_os("CYD_LAB_HOME") {
            Some(p) => PathBuf::from(p),
            None => directories::BaseDirs::new().context("no home directory")?.data_dir().join("cyd-lab"),
        };
        let p = Paths { devices: root.join("devices"), profiles: root.join("profiles"), settings: root.join("settings.toml"), root };
        std::fs::create_dir_all(&p.devices)?;
        std::fs::create_dir_all(&p.profiles)?;
        Ok(p)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub engine: EngineSettings,
}

impl AppSettings {
    fn load(path: &Path) -> (AppSettings, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(t) => match toml::from_str(&t) {
                Ok(s) => (s, None),
                Err(e) => (AppSettings::default(), Some(format!("settings.toml: {e}"))),
            },
            Err(_) => (AppSettings::default(), None),
        }
    }
    fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Is the engine executable there? A bare name is searched on PATH.
fn resolve_exe(p: &Path) -> Option<PathBuf> {
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(p)).find(|c| c.is_file())
}

/// Can we listen on 127.0.0.1:port right now?
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)

/// The URL tools use to reach a device's `--serial-tcp` port (pyserial / esptool syntax).
pub fn rfc2217_url(port: u16) -> String {
    format!("rfc2217://127.0.0.1:{port}")
}

/// Map a point in the (rotated) view, normalised to 0..1, to normalised panel
/// coordinates. `rotation` is how far the view is turned clockwise from the
/// panel's native orientation.
pub fn view_to_panel_norm(rotation: u16, u: f32, v: f32) -> (f32, f32) {
    match rotation {
        90 => (v, 1.0 - u),
        180 => (1.0 - u, 1.0 - v),
        270 => (1.0 - v, u),
        _ => (u, v),
    }
}

/// View size for a panel of `w`x`h` turned by `rotation`.
pub fn rotated_size(rotation: u16, w: usize, h: usize) -> (usize, usize) {
    if rotation == 90 || rotation == 270 { (h, w) } else { (w, h) }
}

/// Mouse position inside the screen rect -> panel pixel (native orientation).
pub fn view_to_panel_px(rotation: u16, panel_w: usize, panel_h: usize, rect: Rect, pos: Pos2) -> Option<(u16, u16)> {
    if !rect.contains(pos) || panel_w == 0 || panel_h == 0 {
        return None;
    }
    let u = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 0.999_99);
    let v = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 0.999_99);
    let (pu, pv) = view_to_panel_norm(rotation, u, v);
    let x = ((pu * panel_w as f32) as usize).min(panel_w - 1);
    let y = ((pv * panel_h as f32) as usize).min(panel_h - 1);
    Some((x as u16, y as u16))
}

/// Remove ANSI escape sequences (colour codes from ESP-IDF logs).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Per-device UI state

struct DeviceView {
    dev: Device,
    backend: Box<dyn Backend>,
    console: VecDeque<(Stream, String)>,
    autoscroll: bool,
    /// BOOT checkbox (latched). The momentary BOOT button ORs into this.
    boot_latched: bool,
    /// GPIO0 level last sent over the live link (BOOT pressed).
    boot_pin_low: bool,
    screen: Option<TextureHandle>,
    screen_size: (usize, usize),
    screen_from_png: bool,
    png_checked: Option<Instant>,
    png_mtime: Option<SystemTime>,
    link_up: bool,
    link_note: String,
    touch_down: Option<(u16, u16)>,
    /// When the current press started (to enforce a minimum hold).
    touch_since: Option<std::time::Instant>,
    /// A release held back until the press has lasted `MIN_TOUCH_HOLD`.
    touch_release_due: Option<(u16, u16, std::time::Instant)>,
    touch_note: String,
    serial_input: String,
    rename_buf: String,
    import_offset: String,
    import_path: String,
}

impl DeviceView {
    fn new(dev: Device, waker: Waker) -> DeviceView {
        DeviceView {
            rename_buf: dev.meta.name.clone(),
            dev,
            backend: Box::new(ProcessBackend::new(waker)),
            console: VecDeque::new(),
            autoscroll: true,
            boot_latched: false,
            boot_pin_low: false,
            screen: None,
            screen_size: (0, 0),
            screen_from_png: false,
            png_checked: None,
            png_mtime: None,
            link_up: false,
            link_note: String::new(),
            touch_down: None,
            touch_since: None,
            touch_release_due: None,
            touch_note: String::new(),
            serial_input: String::new(),
            import_offset: "0x10000".into(),
            import_path: String::new(),
        }
    }

    fn log(&mut self, stream: Stream, line: String) {
        if self.console.len() >= CONSOLE_LINES {
            self.console.pop_front();
        }
        self.console.push_back((stream, strip_ansi(&line)));
    }

    fn lab(&mut self, msg: impl Into<String>) {
        self.log(Stream::Lab, format!("[cyd-lab] {}", msg.into()));
    }
}

// ---------------------------------------------------------------------------
// The app

struct NewDeviceDialog {
    name: String,
    profile: String,
}

pub struct CydLabApp {
    paths: Paths,
    settings: AppSettings,
    settings_draft: Option<AppSettings>,
    profiles: ProfileLibrary,
    store: DeviceStore,
    devices: Vec<DeviceView>,
    selected: Option<usize>,
    new_device: Option<NewDeviceDialog>,
    confirm_delete: Option<String>,
    status: String,
    waker: Waker,
}

impl CydLabApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths) -> CydLabApp {
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = Arc::new(move || ctx.request_repaint());
        let (settings, settings_err) = AppSettings::load(&paths.settings);
        let profiles = ProfileLibrary::load(Some(&paths.profiles));
        let store = DeviceStore::open(&paths.devices).expect("device store");
        let mut status = String::new();
        let mut devices = Vec::new();
        match store.list() {
            Ok((list, errors)) => {
                devices = list.into_iter().map(|d| DeviceView::new(d, waker.clone())).collect();
                if !errors.is_empty() {
                    status = format!("Skipped unreadable devices: {}", errors.join("; "));
                }
            }
            Err(e) => status = format!("Could not list devices: {e:#}"),
        }
        if let Some(e) = settings_err {
            status = e;
        }
        if !profiles.errors.is_empty() {
            status = format!("Profile errors: {}", profiles.errors.join("; "));
        }
        let selected = if devices.is_empty() { None } else { Some(0) };
        CydLabApp { paths, settings, settings_draft: None, profiles, store, devices, selected, new_device: None, confirm_delete: None, status, waker }
    }

    fn profile_for(&self, i: usize) -> Option<Profile> {
        self.profiles.get(&self.devices[i].dev.meta.profile).cloned()
    }

    /// What the configured engine build supports (cached per exe path/size/mtime).
    fn engine_features(&self) -> EngineFeatures {
        resolve_exe(&self.settings.engine.engine_exe).map(|p| EngineFeatures::probe(&p)).unwrap_or_default()
    }

    /// The TCP port for this device's `--serial-tcp`: its configured port if free,
    /// otherwise the first free one not configured on another device.
    fn serial_tcp_port_for(&self, i: usize) -> Option<u16> {
        let others: Vec<u16> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).filter_map(|(_, d)| d.dev.meta.serial_tcp_port).collect();
        let wanted = self.devices[i].dev.meta.serial_tcp_port;
        if let Some(p) = wanted.filter(|p| port_is_free(*p)) {
            return Some(p);
        }
        let mut used = others;
        if let Some(p) = wanted {
            used.push(p);
        }
        (device::FIRST_SERIAL_TCP_PORT..u16::MAX).find(|p| !used.contains(p) && port_is_free(*p))
    }

    fn launch_spec(&self, i: usize, boot: BootMode) -> Result<LaunchSpec> {
        let profile = self.profile_for(i).with_context(|| format!("unknown profile {:?}", self.devices[i].dev.meta.profile))?;
        let features = self.engine_features();
        let serial = if features.serial_tcp { self.serial_tcp_port_for(i) } else { None };
        let port = if self.settings.engine.live_link {
            Some(engine::reserve_port(self.settings.engine.web_port_base).context("no free port for --web")?)
        } else {
            None
        };
        #[allow(unused_mut)]
        let mut spec = LaunchSpec::build(&profile, &self.devices[i].dev, &self.settings.engine, &features, boot, port, serial);
        #[cfg(windows)]
        if let Some(com) = self.devices[i].dev.meta.com_port.as_ref().filter(|_| self.settings.engine.com_bridge) {
            match serial {
                Some(p) => match crate::combridge::plan_for(com, p) {
                    Ok(relay) => spec.relay = Some(relay),
                    Err(why) => spec.relay_note = Some(format!("COM port not bridged: {why}")),
                },
                None => spec.relay_note = Some(format!("{com}: not bridged, the engine has no --serial-tcp")),
            }
        }
        Ok(spec)
    }

    fn power_on(&mut self, i: usize, boot_held: bool) {
        let boot = BootMode::from_boot_held(boot_held);
        let r = self.launch_spec(i, boot).and_then(|spec| {
            let port = spec.web_port;
            let r = self.devices[i].backend.start(&spec);
            if r.is_err() {
                if let Some(p) = port {
                    engine::release_port(p);
                }
            }
            r
        });
        let d = &mut self.devices[i];
        d.link_up = false;
        d.link_note = if self.settings.engine.live_link { "connecting to engine --web…".into() } else { "live link disabled in Settings".into() };
        if let Err(e) = r {
            d.lab(format!("power on failed: {e:#}"));
            self.status = format!("{}: {e:#}", d.dev.meta.name);
        }
    }

    fn press_en(&mut self, i: usize, boot_held: bool) {
        let boot = BootMode::from_boot_held(boot_held);
        let state = self.devices[i].backend.state();
        // The chip already runs in that mode (as the ROM last reported it, so also after
        // esptool): the engine's own in-process reset, which keeps the strap.
        if state == RunState::Running(boot) && self.devices[i].backend.soft_reset().is_ok() {
            return;
        }
        // Strap changed: still in-process, by pulsing DTR/RTS on the engine's serial port.
        if matches!(state, RunState::Running(_)) {
            match self.devices[i].backend.strap_reset(boot) {
                Ok(()) => return,
                Err(e) if self.devices[i].backend.capabilities().serial_tcp.is_some() => self.devices[i].lab(format!("in-process strap reset failed ({e}); power-cycling instead")),
                Err(_) => {}
            }
        }
        // No live link / no serial port: power-cycle so the new BOOT strap is sampled.
        // With --flash-file this waits for the engine to save flash.bin first.
        let r = self.launch_spec(i, boot).and_then(|spec| {
            let port = spec.web_port;
            let r = self.devices[i].backend.restart(spec);
            if r.is_err() {
                if let Some(p) = port {
                    engine::release_port(p);
                }
            }
            r
        });
        let d = &mut self.devices[i];
        d.link_up = false;
        if let Err(e) = r {
            d.lab(format!("reset failed: {e:#}"));
        }
    }

    fn poll_backends(&mut self, ctx: &egui::Context) {
        let mut any_running = false;
        for d in &mut self.devices {
            for ev in d.backend.poll() {
                match ev {
                    EngineEvent::Console(s, l) => d.log(s, l),
                    EngineEvent::Exited(_) => {
                        d.link_up = false;
                        d.touch_down = None;
                        d.touch_since = None;
                        d.touch_release_due = None;
                    }
                    EngineEvent::LinkUp => {
                        d.link_up = true;
                        d.link_note = "live link connected".into();
                    }
                    EngineEvent::LinkDown(why) => {
                        d.link_up = false;
                        if matches!(d.backend.state(), RunState::Running(_)) {
                            d.link_note = why.clone();
                            d.lab(format!("live link: {why}"));
                        } else {
                            // The engine closing its socket on the way out is expected.
                            d.link_note = "live link closed (powered off)".into();
                        }
                    }
                }
            }
            if let Some(f) = d.backend.take_frame() {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                match &mut d.screen {
                    Some(t) if d.screen_size == (f.width, f.height) && !d.screen_from_png => t.set(img, TextureOptions::NEAREST),
                    _ => d.screen = Some(ctx.load_texture(format!("screen-{}", d.dev.id), img, TextureOptions::NEAREST)),
                }
                d.screen_size = (f.width, f.height);
                d.screen_from_png = false;
            }
            // Fallback: the engine's --tft-png snapshot (written at the end of a run).
            if !d.link_up && d.png_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                d.png_checked = Some(Instant::now());
                let path = d.dev.screen_path();
                if let Ok(m) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                    if Some(m) != d.png_mtime && (d.screen.is_none() || d.screen_from_png) {
                        d.png_mtime = Some(m);
                        if let Ok(img) = image::open(&path) {
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            d.screen = Some(ctx.load_texture(format!("png-{}", d.dev.id), ci, TextureOptions::LINEAR));
                            d.screen_size = (size[0], size[1]);
                            d.screen_from_png = true;
                        }
                    }
                }
            }
            any_running |= matches!(d.backend.state(), RunState::Running(_) | RunState::Stopping);
        }
        if any_running {
            // Notice process exits even when the engine is silent.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Graceful stop of every engine, in parallel: ask all, then wait (shared grace period).
    fn stop_all(&mut self) {
        for d in &mut self.devices {
            d.backend.request_stop();
        }
        for d in &mut self.devices {
            d.backend.stop();
        }
    }

    // --- dialogs ----------------------------------------------------------

    fn new_device_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.new_device.as_mut() else { return };
        let mut open = true;
        let mut create = false;
        egui::Window::new("New device").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.label("A new device is a factory-fresh board: blank flash (all 0xFF), its own folder, COM port name and MAC.");
            ui.add_space(6.0);
            egui::Grid::new("newdev").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                let r = ui.text_edit_singleline(&mut dlg.name);
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                ui.end_row();
                ui.label("Variant");
                egui::ComboBox::from_id_salt("profile").selected_text(self.profiles.get(&dlg.profile).map(|p| p.name.as_str()).unwrap_or("?")).show_ui(ui, |ui| {
                    for p in self.profiles.profiles.values() {
                        ui.selectable_value(&mut dlg.profile, p.id.clone(), &p.name);
                    }
                });
                ui.end_row();
            });
            if let Some(p) = self.profiles.get(&dlg.profile) {
                ui.add_space(4.0);
                ui.label(RichText::new(&p.description).small());
                ui.label(RichText::new(format!("{} · {} MB flash · engine board `{}`", p.chip, p.flash.size_mb, p.engine.board)).small().weak());
            }
            ui.add_space(6.0);
            if ui.button("Create").clicked() {
                create = true;
            }
        });
        if create {
            let (name, pid) = (dlg.name.clone(), dlg.profile.clone());
            match self.profiles.get(&pid).cloned().context("pick a variant").and_then(|p| self.store.create(&name, &p)) {
                Ok(dev) => {
                    self.status = format!("Created {} ({}), flash.bin blank at {}", dev.meta.name, dev.id, dev.flash_path().display());
                    self.devices.push(DeviceView::new(dev, self.waker.clone()));
                    self.selected = Some(self.devices.len() - 1);
                    self.new_device = None;
                }
                Err(e) => self.status = format!("Create failed: {e:#}"),
            }
        } else if !open {
            self.new_device = None;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(draft) = self.settings_draft.as_mut() else { return };
        let mut open = true;
        let mut save = false;
        let mut reload_profiles = false;
        egui::Window::new("Settings").collapsible(false).default_width(520.0).open(&mut open).show(ctx, |ui| {
            let e = &mut draft.engine;
            ui.heading("Engine");
            egui::Grid::new("settings").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("esp32sim executable");
                let mut s = e.engine_exe.to_string_lossy().into_owned();
                if ui.add(egui::TextEdit::singleline(&mut s).desired_width(300.0)).changed() {
                    e.engine_exe = PathBuf::from(&s);
                }
                if ui.button("Browse…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().pick_file() {
                        e.engine_exe = f;
                    }
                }
                ui.end_row();
                ui.label("");
                match resolve_exe(&e.engine_exe) {
                    Some(p) => {
                        let f = EngineFeatures::probe(&p);
                        let yn = |b: bool| if b { "yes" } else { "no" };
                        ui.label(RichText::new(format!("found: {}\n--flash-file: {}   --serial-tcp: {}   --web-dir: {}", p.display(), yn(f.flash_file), yn(f.serial_tcp), yn(f.web_dir))).small().color(Color32::LIGHT_GREEN))
                    }
                    None => ui.label(RichText::new("not found (give a full path, or put it on PATH)").small().color(Color32::LIGHT_RED)),
                };
                ui.end_row();

                ui.label("ROM directory");
                let mut s = e.rom_dir.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut s).desired_width(300.0).hint_text("ESP32SIM_ROM_DIR / engine default")).changed() {
                    e.rom_dir = if s.trim().is_empty() { None } else { Some(PathBuf::from(s.trim())) };
                }
                if ui.button("Browse…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().pick_folder() {
                        e.rom_dir = Some(f);
                    }
                }
                ui.end_row();
                ui.label("");
                if let Some(dir) = &e.rom_dir {
                    let rom = dir.join("esp32_rev300_rom.elf");
                    if rom.is_file() {
                        ui.label(RichText::new("esp32_rev300_rom.elf found").small().color(Color32::LIGHT_GREEN));
                    } else {
                        ui.label(RichText::new("esp32_rev300_rom.elf not in this folder").small().color(Color32::LIGHT_RED));
                    }
                } else {
                    ui.label(RichText::new("unset: the engine uses $ESP32SIM_ROM_DIR or ~/.espressif").small().weak());
                }
                ui.end_row();

                ui.label("Live link (--web)");
                ui.checkbox(&mut e.live_link, "live screen + touch over the engine's WebSocket");
                ui.end_row();
                ui.label("First --web port");
                ui.add(egui::DragValue::new(&mut e.web_port_base).range(1024..=65000));
                ui.end_row();
                ui.label("Power-off grace");
                ui.add(egui::DragValue::new(&mut e.stop_grace_secs).range(1..=120).suffix(" s")).on_hover_text("How long to wait for the engine to write flash.bin before killing it");
                ui.end_row();
                ui.label("COM ports (optional)");
                ui.checkbox(&mut e.com_bridge, "Windows only: also expose devices as COMxx via com0com + hub4com (off by default; not needed: tools take the rfc2217:// URL)");
                ui.end_row();
                ui.label("Real time");
                ui.checkbox(&mut e.realtime, "pace emulation to the wall clock (--realtime)");
                ui.end_row();
                ui.label("PNG snapshot");
                ui.checkbox(&mut e.tft_png, "save the display at the end of a run (--tft-png)");
                ui.end_row();
                ui.label("Stop after");
                let mut limited = e.max_seconds.is_some();
                ui.horizontal(|ui| {
                    ui.checkbox(&mut limited, "");
                    let mut secs = e.max_seconds.unwrap_or(30);
                    ui.add_enabled(limited, egui::DragValue::new(&mut secs).range(1..=86400).suffix(" emulated s"));
                    e.max_seconds = limited.then_some(secs);
                });
                ui.end_row();
                ui.label("Extra arguments");
                let mut s = e.extra_args.join(" ");
                if ui.add(egui::TextEdit::singleline(&mut s).desired_width(300.0).hint_text("e.g. --no-jit")).changed() {
                    e.extra_args = s.split_whitespace().map(str::to_string).collect();
                }
                ui.end_row();
            });
            ui.separator();
            ui.heading("Data");
            ui.label(format!("Data folder: {}", self.paths.root.display()));
            ui.label(format!("Devices: {}", self.paths.devices.display()));
            ui.horizontal(|ui| {
                ui.label(format!("User profiles: {}", self.paths.profiles.display()));
                if ui.small_button("Reload profiles").clicked() {
                    reload_profiles = true;
                }
            });
            ui.label(format!("{} profiles loaded: {}", self.profiles.profiles.len(), self.profiles.profiles.keys().cloned().collect::<Vec<_>>().join(", ")));
            for err in &self.profiles.errors {
                ui.colored_label(Color32::LIGHT_RED, err);
            }
            ui.separator();
            ui.label(RichText::new("Changes apply the next time a device is powered on or reset.").small().weak());
            if ui.button("Save").clicked() {
                save = true;
            }
        });
        if reload_profiles {
            self.profiles = ProfileLibrary::load(Some(&self.paths.profiles));
        }
        if save {
            let draft = self.settings_draft.take().unwrap();
            match draft.save(&self.paths.settings) {
                Ok(()) => self.status = format!("Settings saved to {}", self.paths.settings.display()),
                Err(e) => self.status = format!("Saving settings failed: {e:#}"),
            }
            self.settings = draft;
        } else if !open {
            self.settings_draft = None;
        }
    }

    fn delete_modal(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let Some(i) = self.devices.iter().position(|d| d.dev.id == id) else {
            self.confirm_delete = None;
            return;
        };
        let mut decision = None;
        egui::Modal::new(egui::Id::unique("confirm-delete")).show(ctx, |ui| {
            ui.heading("Delete device?");
            ui.label(format!("\"{}\" and its folder will be removed:\n{}\nThis deletes its flash image and cannot be undone.", self.devices[i].dev.meta.name, self.devices[i].dev.dir.display()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Delete").color(Color32::LIGHT_RED)).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                let mut d = self.devices.remove(i);
                d.backend.stop();
                match self.store.delete(&d.dev) {
                    Ok(()) => self.status = format!("Deleted {}", d.dev.meta.name),
                    Err(e) => self.status = format!("Delete failed: {e:#}"),
                }
                self.selected = if self.devices.is_empty() { None } else { Some(i.min(self.devices.len() - 1)) };
                self.confirm_delete = None;
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    // --- panels -----------------------------------------------------------

    fn device_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Devices");
            if ui.button("+ New").on_hover_text("Create an emulated board").clicked() {
                self.open_new_device();
            }
        });
        ui.separator();
        if self.devices.is_empty() {
            ui.label(RichText::new("No devices yet. Click + New to unbox one.").weak());
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, d) in self.devices.iter().enumerate() {
                let (color, state) = status_of(d.backend.state(), d.backend.chip_status());
                let selected = self.selected == Some(i);
                let resp = ui
                    .horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 5.0, color);
                        let label = ui.selectable_label(selected, RichText::new(&d.dev.meta.name).strong());
                        ui.label(RichText::new(state).small().weak());
                        label
                    })
                    .inner;
                if resp.clicked() {
                    self.selected = Some(i);
                }
            }
        });
    }

    fn open_new_device(&mut self) {
        let n = self.devices.len() + 1;
        let profile = if self.profiles.get("esp32-2432s028r").is_some() { "esp32-2432s028r".to_string() } else { self.profiles.profiles.keys().next().cloned().unwrap_or_default() };
        self.new_device = Some(NewDeviceDialog { name: format!("CYD {n}"), profile });
    }

    fn device_panel(&mut self, ui: &mut egui::Ui, i: usize) {
        let profile = self.profile_for(i);
        let state = self.devices[i].backend.state();
        let running = matches!(state, RunState::Running(_));
        let alive = running || state == RunState::Stopping;
        let features = self.engine_features();

        // --- header
        ui.horizontal(|ui| {
            let d = &mut self.devices[i];
            let (color, s) = status_of(state, d.backend.chip_status());
            let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 7.0, color);
            ui.heading(&d.dev.meta.name);
            ui.label(RichText::new(s).weak());
        });
        let d = &self.devices[i];
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| format!("unknown profile {}", d.dev.meta.profile))).strong());
            ui.label("·");
            ui.label(format!("id {}", d.dev.id));
            ui.label("·");
            ui.label(format!("MAC {}", d.dev.meta.mac.as_deref().unwrap_or("-")));
        });
        let caps = d.backend.capabilities();
        ui.horizontal_wrapped(|ui| {
            let chip = |ui: &mut egui::Ui, on: bool, text: &str, tip: &str| {
                let c = if on { Color32::from_rgb(90, 190, 110) } else { Color32::from_gray(120) };
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                if on {
                    ui.painter().circle_filled(rect.center(), 3.5, c);
                } else {
                    ui.painter().circle_stroke(rect.center(), 3.0, egui::Stroke::new(1.0, c));
                }
                ui.label(RichText::new(text).small().color(c)).on_hover_text(tip);
            };
            ui.label(RichText::new("Engine:").small().weak());
            chip(ui, caps.live_screen, "live screen", "TFT frames over the engine's --web WebSocket");
            chip(ui, caps.touch, "touch", "Mouse clicks on the screen are sent as panel touches");
            chip(ui, caps.png_snapshot, "PNG snapshot", "--tft-png: display saved when a run ends");
            let (wb, wb_tip) = if alive {
                (caps.flash_writeback, if caps.flash_writeback { "This run uses --flash-file: firmware writes are saved to flash.bin when you power off" } else { "This run uses --flash-image: firmware writes are lost at power-off" })
            } else {
                (features.flash_file, if features.flash_file { "Engine supports --flash-file: firmware writes persist across power cycles" } else { "This engine build has no --flash-file: firmware writes are lost at power-off" })
            };
            chip(ui, wb, "flash write-back", wb_tip);
            let (sb, sb_tip) = if alive {
                (caps.serial_tcp.is_some(), "UART0 served as RFC 2217 over TCP (--serial-tcp)")
            } else {
                (features.serial_tcp, if features.serial_tcp { "Engine supports --serial-tcp (RFC 2217, DTR/RTS auto-reset)" } else { "This engine build has no --serial-tcp yet" })
            };
            chip(ui, sb, "serial bridge", sb_tip);
        });
        self.serial_bridge_ui(ui, i, alive, &features);
        ui.separator();

        // --- controls
        let mut power_toggle = false;
        let mut en = false;
        let mut boot_button_down = false;
        ui.horizontal(|ui| {
            let d = &mut self.devices[i];
            let power_text = if state == RunState::Stopping { "Saving flash…" } else if running { "Power off" } else { "Power on" };
            let power_btn = egui::Button::new(RichText::new(power_text).strong()).fill(if running { Color32::from_rgb(120, 40, 40) } else { Color32::from_rgb(40, 100, 50) });
            if ui.add_enabled(state != RunState::Stopping, power_btn).clicked() {
                power_toggle = true;
            }
            if ui.add_enabled(running, egui::Button::new("EN (reset)")).on_hover_text("Pulse EN: reset the chip in-process. BOOT is sampled on release. Same mode as now: the engine's reset. Changed BOOT: DTR/RTS on the engine's serial port (this disconnects a tool connected to it). Without --serial-tcp a changed BOOT power-cycles the engine.").clicked() {
                en = true;
            }
            ui.separator();
            let boot = ui.add(egui::Button::new("BOOT").sense(Sense::click_and_drag())).on_hover_text("GPIO0 low while held (firmware sees the button). Hold BOOT and click EN, or tick \"BOOT held\" and press EN / Power, for download mode.");
            boot_button_down = boot.is_pointer_button_down_on();
            ui.checkbox(&mut d.boot_latched, "BOOT held (download mode, strap 0x0f)");
        });
        let boot_held = self.devices[i].boot_latched || boot_button_down;
        // BOOT is also a live button (GPIO0) for the firmware, like on the board.
        {
            let d = &mut self.devices[i];
            if !running || !d.link_up {
                d.boot_pin_low = false;
            } else if boot_held != d.boot_pin_low && d.backend.set_boot_pin(boot_held).is_ok() {
                d.boot_pin_low = boot_held;
            }
        }
        if power_toggle {
            if running {
                self.devices[i].backend.request_stop();
            } else {
                self.power_on(i, boot_held);
            }
        }
        if en {
            self.press_en(i, boot_held);
        }

        // --- flash tools
        let mut erase = false;
        let mut import = false;
        let mut delete = false;
        let mut rename = false;
        ui.horizontal_wrapped(|ui| {
            let d = &mut self.devices[i];
            let size = d.dev.flash_size().map(|s| format!("{} MB", s / (1024 * 1024))).unwrap_or_else(|_| "missing".into());
            ui.label(format!("Flash ({size}):"));
            ui.add_enabled_ui(!alive, |ui| {
            if ui.button("Erase flash").on_hover_text("Reset flash.bin to all 0xFF").on_disabled_hover_text("Power off first: the running engine owns flash.bin and rewrites it at power-off").clicked() {
                erase = true;
            }
            ui.separator();
            ui.label("Import .bin at");
            ui.add(egui::TextEdit::singleline(&mut d.import_offset).desired_width(70.0));
            ui.add(egui::TextEdit::singleline(&mut d.import_path).desired_width(220.0).hint_text("firmware.bin (merged @0x0, app @0x10000)"));
            if ui.button("…").clicked() {
                if let Some(f) = rfd::FileDialog::new().add_filter("firmware", &["bin"]).pick_file() {
                    d.import_path = f.to_string_lossy().into_owned();
                }
            }
            if ui.button("Import").on_disabled_hover_text("Power off first: the running engine owns flash.bin and rewrites it at power-off").clicked() {
                import = true;
            }
            });
        });
        ui.horizontal(|ui| {
            let d = &mut self.devices[i];
            ui.add(egui::TextEdit::singleline(&mut d.rename_buf).desired_width(160.0));
            if ui.button("Rename").clicked() {
                rename = true;
            }
            ui.separator();
            if ui.button(RichText::new("Delete device…").color(Color32::LIGHT_RED)).clicked() {
                delete = true;
            }
        });
        if erase {
            let r = self.store.erase_flash(&self.devices[i].dev);
            let d = &mut self.devices[i];
            match r {
                Ok(()) => d.lab("flash erased to 0xFF"),
                Err(e) => d.lab(format!("erase failed: {e:#}")),
            }
        }
        if import && self.devices[i].import_path.trim().is_empty() {
            self.devices[i].lab("import: choose a .bin first (type a path or use …)");
        } else if import {
            let d = &self.devices[i];
            let r = parse_offset(&d.import_offset).and_then(|off| self.store.import_firmware_file(&d.dev, off, Path::new(d.import_path.trim())).map(|_| off));
            let d = &mut self.devices[i];
            match r {
                Ok(off) => {
                    let msg = format!("imported {} at 0x{off:x}", d.import_path.trim());
                    d.lab(msg);
                }
                Err(e) => d.lab(format!("import failed: {e:#}")),
            }
        }
        if rename {
            let d = &mut self.devices[i];
            let name = d.rename_buf.clone();
            if let Err(e) = self.store.rename(&mut d.dev, &name) {
                self.status = format!("Rename failed: {e:#}");
            }
        }
        if delete {
            self.confirm_delete = Some(self.devices[i].dev.id.clone());
        }
        ui.separator();

        // --- screen + console
        let avail = ui.available_size();
        ui.horizontal_top(|ui| {
            ui.set_min_height(avail.y);
            ui.vertical(|ui| self.screen_ui(ui, i, profile.as_ref()));
            ui.separator();
            ui.vertical(|ui| self.console_ui(ui, i));
        });
    }

    /// Per-device serial settings: TCP port for `--serial-tcp`, the planned COM name,
    /// and the rfc2217:// URL with a copy button.
    fn serial_bridge_ui(&mut self, ui: &mut egui::Ui, i: usize, alive: bool, features: &EngineFeatures) {
        let live_port = self.devices[i].backend.capabilities().serial_tcp.filter(|_| alive);
        let others: Vec<u16> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).filter_map(|(_, d)| d.dev.meta.serial_tcp_port).collect();
        let mut save = false;
        let d = &mut self.devices[i];
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Serial:").small().weak());
            ui.add_enabled_ui(!alive, |ui| {
                ui.label("TCP port");
                let mut port = d.dev.meta.serial_tcp_port.unwrap_or(device::FIRST_SERIAL_TCP_PORT);
                if ui.add(egui::DragValue::new(&mut port).range(1024..=65535)).on_hover_text("--serial-tcp PORT (RFC 2217). If it is taken at power-on, the next free port is used for that run.").changed() {
                    d.dev.meta.serial_tcp_port = Some(port);
                    save = true;
                }
                if ui.button("Auto").on_hover_text("Pick a free port not used by another device").clicked() {
                    let p = (device::FIRST_SERIAL_TCP_PORT..u16::MAX).find(|p| !others.contains(p) && port_is_free(*p));
                    d.dev.meta.serial_tcp_port = p;
                    save = true;
                }
                let com_bridge = self.settings.engine.com_bridge;
                if com_bridge {
                ui.label("COM name");
                let mut com = d.dev.meta.com_port.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut com).desired_width(60.0)).on_hover_text("Windows only: expose the board as this COM port (com0com pair COMn <-> CNCBn plus a hub4com relay started at power-on). Empty = no COM port. While the relay is connected, the chip's serial output goes to the COM port instead of this console.").changed() {
                    d.dev.meta.com_port = if com.trim().is_empty() { None } else { Some(com.trim().to_uppercase()) };
                    save = true;
                }
                }
            });
            let port = live_port.or(d.dev.meta.serial_tcp_port);
            if let Some(p) = port {
                let url = rfc2217_url(p);
                let color = if live_port.is_some() { Color32::LIGHT_GREEN } else { Color32::GRAY };
                ui.label(RichText::new(&url).monospace().color(color)).on_hover_text(format!(
                    "esptool --chip esp32 --port {url} --baud 921600 write-flash 0x1000 firmware.bin\nPlatformIO: upload_port = {url}\nThe engine maps DTR/RTS through the auto-reset circuit, so esptool resets into the bootloader and back by itself.\nOne client at a time: a new connection (also cyd-lab's EN with a changed BOOT) replaces the old one."
                ));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(url);
                }
            }
            let note = match (alive, live_port.is_some(), features.serial_tcp) {
                (true, true, _) => "live".to_string(),
                (true, false, true) => "not enabled for this run (no free port?)".to_string(),
                (_, _, false) => "engine has no --serial-tcp yet: URL is reserved, not served".to_string(),
                (false, _, true) => "served at power-on".to_string(),
            };
            ui.label(RichText::new(note).small().weak());
            if let Some(com) = d.dev.meta.com_port.clone().filter(|_| self.settings.engine.com_bridge) {
                com_port_ui(ui, d, &com);
            }
        });
        if save {
            if let Err(e) = d.dev.save() {
                d.lab(format!("could not save device settings: {e:#}"));
            }
        }
    }

    fn screen_ui(&mut self, ui: &mut egui::Ui, i: usize, profile: Option<&Profile>) {
        let running = matches!(self.devices[i].backend.state(), RunState::Running(_));
        let live_link = self.settings.engine.live_link;
        let d = &mut self.devices[i];
        let (pw, ph) = profile.and_then(|p| p.panel_size()).map(|(w, h)| (w as usize, h as usize)).unwrap_or((240, 320));
        let (panel_w, panel_h) = if d.screen.is_some() && !d.screen_from_png { d.screen_size } else { (pw, ph) };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Screen").strong());
            let mut rot = d.dev.meta.view_rotation;
            egui::ComboBox::from_id_salt(("rot", &d.dev.id)).width(60.0).selected_text(format!("{rot}°")).show_ui(ui, |ui| {
                for r in [0u16, 90, 180, 270] {
                    ui.selectable_value(&mut rot, r, format!("{r}°"));
                }
            });
            if rot != d.dev.meta.view_rotation {
                d.dev.meta.view_rotation = rot;
                let _ = self.store.update_meta(&d.dev);
            }
        });
        let rot = d.dev.meta.view_rotation;
        let (vw, vh) = rotated_size(rot, panel_w, panel_h);
        let scale = ((ui.available_height() - 90.0) / vh as f32).clamp(0.5, 2.0);
        let scale = (scale * 4.0).floor() / 4.0;
        let size = Vec2::new(vw as f32 * scale, vh as f32 * scale);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(12));

        match &d.screen {
            Some(tex) if !d.screen_from_png => {
                let mut mesh = egui::Mesh::with_texture(tex.id());
                let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
                let uvs = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
                for (pos, (u, v)) in corners.iter().zip(uvs) {
                    let (pu, pv) = view_to_panel_norm(rot, u, v);
                    mesh.vertices.push(egui::epaint::Vertex { pos: *pos, uv: Pos2::new(pu, pv), color: Color32::WHITE });
                }
                mesh.add_triangle(0, 1, 2);
                mesh.add_triangle(0, 2, 3);
                painter.add(egui::Shape::mesh(mesh));
            }
            Some(tex) => {
                // PNG snapshot: shown as written by the engine, fitted to the area.
                let (w, h) = d.screen_size;
                let s = (rect.width() / w as f32).min(rect.height() / h as f32);
                let r = Rect::from_center_size(rect.center(), Vec2::new(w as f32 * s, h as f32 * s));
                painter.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                let msg = if !running {
                    format!("{panel_w}×{panel_h} panel\n\npowered off")
                } else if !live_link {
                    format!("{panel_w}×{panel_h} panel\n\nlive link disabled\n(Settings > Live link)")
                } else {
                    format!("{panel_w}×{panel_h} panel\n\nwaiting for the first frame\nfrom the engine…")
                };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(14.0), Color32::from_gray(150));
            }
        }
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::from_gray(90)), egui::StrokeKind::Outside);

        // --- touch: mouse on the screen = finger on the panel
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        // The engine samples the panel at its own pace, so a press+release inside one
        // frame (a fast click, or xdotool) would be missed: hold every touch >= MIN_TOUCH_HOLD.
        let now = std::time::Instant::now();
        if resp.is_pointer_button_down_on() {
            if let Some((x, y)) = pointer.and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p)) {
                if d.touch_down != Some((x, y)) {
                    d.touch_release_due = None;
                    if d.touch_down.is_none() {
                        d.touch_since = Some(now);
                    }
                    d.touch_down = Some((x, y));
                    d.touch_note = match d.backend.touch(x, y, true) {
                        Ok(()) => format!("touch ({x}, {y})"),
                        Err(e) => format!("touch ({x}, {y}) not delivered: {e}"),
                    };
                }
            }
        } else if let Some((x, y)) = d.touch_down.take() {
            let since = d.touch_since.take().unwrap_or(now);
            d.touch_release_due = Some((x, y, since + MIN_TOUCH_HOLD));
        } else if resp.clicked() && d.touch_release_due.is_none() {
            // Down and up landed in the same frame: synthesise the press now.
            if let Some((x, y)) = pointer.and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p)) {
                d.touch_note = match d.backend.touch(x, y, true) {
                    Ok(()) => format!("touch ({x}, {y})"),
                    Err(e) => format!("touch ({x}, {y}) not delivered: {e}"),
                };
                d.touch_release_due = Some((x, y, now + MIN_TOUCH_HOLD));
            }
        }
        if let Some((x, y, due)) = d.touch_release_due {
            if now >= due {
                d.touch_release_due = None;
                d.touch_note = match d.backend.touch(x, y, false) {
                    Ok(()) => format!("release ({x}, {y})"),
                    Err(e) => format!("release ({x}, {y}) not delivered: {e}"),
                };
            } else {
                ui.ctx().request_repaint_after(due - now);
            }
        }
        if let Some((x, y)) = resp.hover_pos().and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p)) {
            resp.on_hover_text_at_pointer(format!("panel ({x}, {y})"));
        }

        let source = if d.screen.is_some() && !d.screen_from_png {
            "live (engine --web)"
        } else if d.screen_from_png {
            "last --tft-png snapshot"
        } else {
            "placeholder"
        };
        ui.label(RichText::new(format!("{source} · {}", if d.link_up { "link up" } else { d.link_note.as_str() })).small().weak());
        if !d.touch_note.is_empty() {
            ui.label(RichText::new(&d.touch_note).small().weak());
        }
    }

    fn console_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let d = &mut self.devices[i];
        let mut send = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Console (UART0)").strong());
            ui.checkbox(&mut d.autoscroll, "autoscroll");
            if ui.small_button("Clear").clicked() {
                d.console.clear();
            }
            if ui.small_button("Copy").clicked() {
                let text = d.console.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
                ui.ctx().copy_text(text);
            }
            if let Some(cmd) = d.backend.last_command() {
                ui.small_button("Command line").on_hover_text(cmd.clone()).clicked().then(|| ui.ctx().copy_text(cmd));
            }
        });
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        // Reserve room for the serial input row below the log so it never falls off the window.
        let input_h = ui.spacing().interact_size.y + 2.0 * ui.spacing().item_spacing.y + 8.0;
        let log_h = (ui.available_height() - input_h - 8.0).max(80.0);
        egui::Frame::new().fill(Color32::from_gray(16)).inner_margin(4.0).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            egui::ScrollArea::both()
                .id_salt(("console", &d.dev.id))
                .auto_shrink(false)
                .max_height(log_h)
                .min_scrolled_height(log_h)
                .stick_to_bottom(d.autoscroll)
                .show_rows(ui, row_h, d.console.len(), |ui, range| {
                    for (s, line) in d.console.range(range) {
                        let color = match s {
                            Stream::Stdout => Color32::from_gray(220),
                            Stream::Stderr => Color32::from_rgb(230, 170, 90),
                            Stream::Lab => Color32::from_rgb(120, 170, 240),
                        };
                        ui.label(RichText::new(line).monospace().color(color));
                    }
                });
        });
        ui.horizontal(|ui| {
            let r = ui.add_enabled(d.link_up, egui::TextEdit::singleline(&mut d.serial_input).desired_width(ui.available_width() - 60.0).hint_text("type a line for UART0 RX (needs live link)"));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                send = true;
            }
            if ui.add_enabled(d.link_up, egui::Button::new("Send")).clicked() {
                send = true;
            }
        });
        if send && !d.serial_input.is_empty() {
            let line = std::mem::take(&mut d.serial_input);
            if let Err(e) = d.backend.serial_line(&line) {
                d.lab(format!("serial: {e}"));
            }
        }
    }
}

/// The COM-port line of the serial row. Windows: what com0com/hub4com detection found,
/// and a button to create the pair (UAC prompt). Untested on a real Windows box so far.
#[cfg(windows)]
fn com_port_ui(ui: &mut egui::Ui, d: &mut DeviceView, com: &str) {
    use crate::combridge;
    let found = combridge::detect_in(&combridge::search_dirs());
    let (text, color, tip) = match (&found.setupc, &found.hub4com) {
        (None, _) => (format!("· {com}: com0com not installed"), Color32::from_rgb(230, 180, 60), combridge::plan(com, 0, &found, None).unwrap_err()),
        (Some(_), None) => (format!("· {com}: hub4com.exe missing"), Color32::from_rgb(230, 180, 60), combridge::plan(com, 0, &found, None).unwrap_err()),
        (Some(_), Some(_)) => (format!("· {com} via com0com (untested)"), Color32::LIGHT_GREEN, format!("At power-on cyd-lab checks the pair with setupc list and starts hub4com relaying {} to the serial port. Watch the console for its status.", combridge::peer(com))),
    };
    ui.label(RichText::new(text).small().color(color)).on_hover_text(tip);
    if let Some(setupc) = found.setupc.clone() {
        if ui.small_button(format!("Create {com} pair")).on_hover_text(format!("Runs as administrator (UAC prompt): {}", combridge::setupc_command(com))).clicked() {
            match combridge::create_pair_elevated(&setupc, com) {
                Ok(()) => d.lab(format!("created com0com pair {com} <-> {}", combridge::peer(com))),
                Err(e) => d.lab(format!("could not create the {com} pair: {e}")),
            }
        }
    }
}

#[cfg(not(windows))]
fn com_port_ui(ui: &mut egui::Ui, _d: &mut DeviceView, com: &str) {
    ui.label(RichText::new(format!("· {com}: COM ports are Windows-only (com0com)")).small().weak())
        .on_hover_text("On Linux/macOS use the rfc2217:// URL directly, or a pty: socat pty,link=/tmp/ttyCYD,raw tcp:127.0.0.1:PORT (no DTR/RTS).");
}

fn status_of(s: RunState, chip: ChipStatus) -> (Color32, &'static str) {
    match s {
        RunState::Running(_) if chip.halted => (Color32::from_rgb(220, 70, 60), "halted: waiting for EN"),
        RunState::Running(BootMode::Download) if chip.by_serial => (Color32::from_rgb(80, 150, 240), "download mode (serial)"),
        RunState::Stopped => (Color32::from_gray(110), "stopped"),
        RunState::Running(BootMode::Normal) => (Color32::from_rgb(70, 200, 90), "running"),
        RunState::Running(BootMode::Download) => (Color32::from_rgb(240, 190, 40), "download mode"),
        RunState::Stopping => (Color32::from_rgb(230, 140, 50), "saving flash…"),
        RunState::Exited(Some(0)) => (Color32::from_gray(110), "exited"),
        RunState::Exited(_) => (Color32::from_rgb(220, 70, 60), "crashed / exited"),
    }
}

impl eframe::App for CydLabApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_backends(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("cyd-lab").strong().size(16.0));
                ui.label(RichText::new(env!("CARGO_PKG_VERSION")).weak());
                ui.separator();
                if ui.button("+ New device").clicked() {
                    self.open_new_device();
                }
                if ui.button("Settings").clicked() {
                    self.settings_draft = Some(self.settings.clone());
                }
                ui.separator();
                let running = self.devices.iter().filter(|d| matches!(d.backend.state(), RunState::Running(_))).count();
                ui.label(format!("{} devices, {running} running", self.devices.len()));
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(&self.status).small());
                }
            });
        });
        egui::Panel::left("devices").default_size(230.0).show(ui, |ui| self.device_list(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| match self.selected {
            Some(i) if i < self.devices.len() => self.device_panel(ui, i),
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("No device selected");
                    ui.label("Create one with + New device. Each device is a blank board with its own flash image, console and screen.");
                });
            }
        });

        self.new_device_window(&ctx);
        self.settings_window(&ctx);
        self.delete_modal(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
    }
}

impl Drop for CydLabApp {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_bridge_helpers() {
        assert_eq!(rfc2217_url(22170), "rfc2217://127.0.0.1:22170");
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = l.local_addr().unwrap().port();
        assert!(!port_is_free(taken));
        drop(l);
        assert!(port_is_free(taken));
    }

    #[test]
    fn rotation_mapping_round_trip_corners() {
        // 240x320 panel; view rotated 90° is 320 wide, 240 tall.
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 240.0));
        // top-left of a 90° view is the panel's bottom-left
        assert_eq!(view_to_panel_px(90, 240, 320, rect, Pos2::new(0.0, 0.0)), Some((0, 319)));
        // top-right of the view is the panel's origin
        assert_eq!(view_to_panel_px(90, 240, 320, rect, Pos2::new(319.9, 0.0)), Some((0, 0)));
        let rect0 = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(480.0, 640.0)); // 2x scale
        assert_eq!(view_to_panel_px(0, 240, 320, rect0, Pos2::new(10.0, 10.0)), Some((0, 0)));
        assert_eq!(view_to_panel_px(0, 240, 320, rect0, Pos2::new(20.0, 30.0)), Some((5, 10)));
        assert_eq!(view_to_panel_px(0, 240, 320, rect0, Pos2::new(489.9, 649.9)), Some((239, 319)));
        assert_eq!(view_to_panel_px(180, 240, 320, rect0, Pos2::new(10.0, 10.0)), Some((239, 319)));
        assert_eq!(view_to_panel_px(0, 240, 320, rect0, Pos2::new(5.0, 5.0)), None, "outside");
        let rect270 = Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 240.0));
        assert_eq!(view_to_panel_px(270, 240, 320, rect270, Pos2::new(0.0, 0.0)), Some((239, 0)));
        assert_eq!(rotated_size(90, 240, 320), (320, 240));
        assert_eq!(rotated_size(180, 240, 320), (240, 320));
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\u{1b}[0;32mI (31) boot: ok\u{1b}[0m"), "I (31) boot: ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn settings_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        let mut s = AppSettings::default();
        s.engine.rom_dir = Some("/roms".into());
        s.engine.max_seconds = Some(10);
        s.save(&path).unwrap();
        let (loaded, err) = AppSettings::load(&path);
        assert!(err.is_none());
        assert_eq!(loaded, s);
        // partial file falls back to defaults for missing keys
        std::fs::write(&path, "[engine]\nrealtime = false\n").unwrap();
        let (partial, _) = AppSettings::load(&path);
        assert!(!partial.engine.realtime);
        assert!(partial.engine.live_link);
    }
}
