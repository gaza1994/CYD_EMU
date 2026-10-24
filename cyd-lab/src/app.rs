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
        let root = crate::running::data_dir().context("no home directory")?;
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
    /// Wi-Fi details given to new devices ("Use these Wi-Fi details for new devices").
    /// Plain text in settings.toml.
    pub wifi: crate::network::WifiDetails,
    /// Optional online refresh of the firmware catalogue (off by default).
    pub catalog: crate::catalog::CatalogSettings,
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
/// The PC's LAN address, refreshed every 20 s.
fn cached_lan_ip() -> Option<std::net::IpAddr> {
    static CACHE: std::sync::Mutex<Option<(Instant, Option<std::net::IpAddr>)>> = std::sync::Mutex::new(None);
    let mut c = CACHE.lock().unwrap();
    match *c {
        Some((t, ip)) if t.elapsed() < Duration::from_secs(20) => ip,
        _ => {
            let ip = crate::network::lan_ip();
            *c = Some((Instant::now(), ip));
            ip
        }
    }
}

/// Network part of the "Use with…" menu: what is reachable over the emulated Wi-Fi and how
/// common tools use it. Returns a preset to add when one of its buttons is clicked.
fn network_help_ui(ui: &mut egui::Ui, ip: &str, n: &crate::network::DeviceNetwork, mdns: bool) -> Option<usize> {
    use crate::network::PRESETS;
    let mut add = None;
    ui.label(RichText::new(format!("Over Wi-Fi (this PC: {ip})")).strong());
    if !n.wifi {
        ui.label("Wi-Fi is off for this device (Network section).");
        return None;
    }
    let host = if n.lan { ip.to_string() } else { "127.0.0.1".to_string() };
    if n.forwards.is_empty() {
        ui.label(RichText::new("No ports published yet. The firmware can reach the internet, but nothing can reach it. Add a preset:").small());
    }
    for f in n.active_forwards() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{host}:{}", f.host)).monospace());
            ui.label(RichText::new(format!("-> firmware port {} ({})", f.guest, if f.label.is_empty() { "TCP" } else { &f.label })).small());
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(format!("{host}:{}", f.host));
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        for (k, p) in PRESETS.iter().enumerate() {
            if ui.small_button(format!("+ {}", p.name)).on_hover_text(p.tip).clicked() {
                add = Some(k);
            }
        }
    });
    if !n.lan {
        ui.label(RichText::new("\"Reach from my network\" is off: only programs on this PC can connect (127.0.0.1).").small().color(Color32::from_rgb(230, 160, 90)));
    }
    if let Some(api) = n.to_guest(6053) {
        ui.separator();
        ui.label(RichText::new("Home Assistant (ESPHome firmware)").strong());
        if mdns && n.lan {
            ui.label(RichText::new("It should show up under Settings > Devices & services > Discovered. If not:").small());
        }
        ui.label(RichText::new(format!("Settings > Devices & services > Add integration > ESPHome. Host {host}, port {}. Then the API encryption key from your YAML, if it has one.", api.host)).small());
    }
    if let Some(ota) = n.forwards.iter().find(|f| f.same_port) {
        ui.separator();
        ui.label(RichText::new("OTA uploads").strong());
        let line = if ota.label.contains("ESPHome") {
            format!("ESPHome Builder / esphome upload --device {host}")
        } else {
            format!("Arduino IDE network port or PlatformIO upload_port = {host} (espota, port {})", ota.host)
        };
        ui.label(RichText::new(line).small());
        if ota.host != 3232 {
            ui.label(RichText::new(format!("The firmware must listen on port {p}: ESPHome ota: port: {p}; Arduino ArduinoOTA.setPort({p}).", p = ota.host)).small().weak());
        }
    }
    if let Some(web) = n.to_guest(80) {
        ui.separator();
        ui.label(RichText::new(format!("Web page: http://{host}:{}/", web.host)).small());
    }
    ui.separator();
    ui.label(RichText::new("Windows: the first time a device with published ports powers on, Windows Firewall asks about esp32sim. Tick Private networks and click Allow, or other machines can't connect.").small().weak());
    add
}

fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Can a TCP port be listened on at `addr` (0.0.0.0 or 127.0.0.1)? The listener is closed
/// again straight away, before the engine binds it.
fn bind_is_free(addr: &str, port: u16) -> bool {
    let free = |a: &str| std::net::TcpListener::bind((a, port)).is_ok();
    // A port taken on one address of the PC also blocks 0.0.0.0 on Windows; check both.
    free(addr) && (addr != "0.0.0.0" || free("127.0.0.1"))
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)

/// One-liners for the "Use with…" popover: (tool, what to do, the line to copy).
pub fn use_with_lines(url: &str, name: &str) -> Vec<(&'static str, String, String)> {
    vec![
        ("esptool", "flash a merged/factory image".into(), format!("esptool --port {url} --baud 921600 write-flash 0x0 firmware.factory.bin")),
        ("esptool", "or an app image (Arduino *.ino.bin, PlatformIO firmware.bin)".into(), format!("esptool --port {url} --baud 921600 write-flash 0x10000 firmware.bin")),
        ("PlatformIO", "platformio.ini, then Upload / Monitor as usual".into(), format!("upload_port = {url}\nmonitor_port = {url}")),
        ("Arduino IDE 2", format!("after \"Set up Arduino IDE\": Tools > Port > \"{name} (cyd-lab)\""), format!("arduino-cli compile -u -b {} -p {url} MySketch", crate::arduino_setup::FQBN)),
        ("ESPHome", "first install: drop the factory .bin onto this window; later updates go over Wi-Fi with the ESPHome ports (Network)".into(), "esphome compile cyd.yaml   →   .esphome/build/<name>/.pioenvs/<name>/firmware.factory.bin".into()),
        ("Serial terminal", "pyserial's miniterm (or any rfc2217 client)".into(), format!("python -m serial.tools.miniterm {url} 115200")),
    ]
}

fn use_with_ui(ui: &mut egui::Ui, url: &str, name: &str) {
    ui.set_max_width(620.0);
    ui.label(RichText::new("This device's serial port is a network URL: no COM driver needed.").weak());
    ui.label(RichText::new("DTR/RTS auto-reset works, so tools reset into the bootloader by themselves. One client at a time.").weak().small());
    for (tool, what, line) in use_with_lines(url, name) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new(tool).strong());
            ui.label(RichText::new(what).small());
        });
        ui.horizontal(|ui| {
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(line.clone());
            }
            ui.label(RichText::new(&line).monospace());
        });
    }
}

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
    /// "Install from web installer": manifest URL or local manifest.json.
    web_manifest: String,
    /// Erase first when the manifest leaves it to the user (new_install_prompt_erase).
    web_erase: bool,
    /// A download in progress (manifest + parts), run on a thread.
    web_job: Option<std::sync::mpsc::Receiver<WebMsg>>,
    /// Log prefix for the job above ("web install" or "catalogue").
    job_label: &'static str,
    /// The "Install firmware" gallery is open, and shows broken entries too.
    gallery_open: bool,
    gallery_broken: bool,
    /// Firmware waiting to be written: the device is powered off first if needed.
    pending_flash: Option<PendingFlash>,
    /// A dropped/picked .bin whose offset the user must confirm.
    drop_ask: Option<DropAsk>,
    /// No bootloader in flash.bin (0xE9 at 0x1000), refreshed after writes and power-off.
    flash_blank: bool,
    /// Wi-Fi join attempts seen in the console, for the "firmware is looking for" banner.
    wifi_watch: crate::network::WifiWatch,
    /// The access point of the current run (what the banner compares against).
    wifi_ap: crate::network::WifiDetails,
    /// Show the Wi-Fi password in the Network section.
    show_password: bool,
}

/// Firmware queued for flash.bin. Applied once the engine is stopped (it owns the file
/// while running and rewrites it at power-off), then the device is powered on.
struct PendingFlash {
    erase: bool,
    parts: Vec<(u64, Vec<u8>, String)>,
    what: String,
    power_on_after: bool,
    stop_sent: bool,
}

struct DropAsk {
    name: String,
    data: Vec<u8>,
    kind: DropKind,
}

enum DropKind {
    /// Unknown file: the user confirms the offset.
    Offset { offset: String, why: String },
    /// App-only image, the device has a partition table.
    IntoPartition { offset: u64, otadata: Option<(u64, u64)> },
    /// App-only image on a blank board.
    OnBlank { install_anyway: bool },
    /// Several files dropped at once.
    Multi { rows: Vec<MultiRow> },
}

struct MultiRow {
    name: String,
    data: Vec<u8>,
    part: crate::firmware::Part,
    offset: String,
}

/// Does flash.bin hold a bootloader? (Reads 0x1001 bytes.)
fn flash_is_blank(path: &Path) -> bool {
    use std::io::Read;
    let mut b = vec![0u8; 0x1001];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => b[0x1000] != crate::firmware::IMAGE_MAGIC && b[0] != crate::firmware::IMAGE_MAGIC,
        Err(_) => true,
    }
}

/// The first 0x9000 bytes of flash.bin (bootloader + partition table) for the heuristics.
fn flash_head(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut b = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(0x9000).read_to_end(&mut b);
    }
    b
}

enum WebMsg {
    Line(String),
    Done(Result<crate::webinstall::Prepared, String>),
}

impl DeviceView {
    fn new(dev: Device, waker: Waker) -> DeviceView {
        let flash_blank = flash_is_blank(&dev.flash_path());
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
            web_manifest: String::new(),
            web_erase: true,
            job_label: "web install",
            gallery_open: false,
            gallery_broken: false,
            web_job: None,
            pending_flash: None,
            drop_ask: None,
            flash_blank,
            wifi_watch: Default::default(),
            wifi_ap: Default::default(),
            show_password: false,
        }
    }

    /// Start downloading a web installer's firmware on a thread.
    fn start_web_install(&mut self, waker: Waker) {
        let src = self.web_manifest.trim().to_string();
        let erase = self.web_erase;
        let size = match self.dev.flash_size() {
            Ok(s) => s,
            Err(e) => return self.lab(format!("web install: {e:#}")),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.web_job = Some(rx);
        self.job_label = "web install";
        std::thread::spawn(move || {
            let w = waker.clone();
            let t = tx.clone();
            let r = crate::webinstall::prepare(&src, erase, size, &mut |l| {
                let _ = t.send(WebMsg::Line(l));
                w();
            });
            let _ = tx.send(WebMsg::Done(r.map_err(|e| format!("{e:#}"))));
            waker();
        });
    }

    /// Start a one-click catalogue install: download, verify the pinned SHA-256s, then write
    /// like a web install (erase first, power on after).
    fn start_catalog_install(&mut self, e: crate::catalog::Entry, cache: PathBuf, waker: Waker) {
        let size = match self.dev.flash_size() {
            Ok(s) => s,
            Err(err) => return self.lab(format!("catalogue: {err:#}")),
        };
        self.lab(format!("catalogue: installing {} {}", e.name, e.version));
        let (tx, rx) = std::sync::mpsc::channel();
        self.web_job = Some(rx);
        self.job_label = "catalogue";
        std::thread::spawn(move || {
            let w = waker.clone();
            let t = tx.clone();
            let r = crate::catalog::prepare(&e, size, Some(cache), &mut |l| {
                let _ = t.send(WebMsg::Line(l));
                w();
            });
            let _ = tx.send(WebMsg::Done(r.map_err(|e| format!("{e:#}"))));
            waker();
        });
    }

    /// Queue downloaded parts for flash.bin (written once the device is off, then it boots).
    fn finish_web_install(&mut self, p: crate::webinstall::Prepared) {
        if p.erase {
            let l = if p.prompt_erase { "will erase flash first (\"erase first\" ticked)" } else { "will erase flash first (a new install always erases)" };
            self.lab(format!("{}: {l}", self.job_label));
        } else {
            self.lab("web install: not erasing (manifest leaves it to you, \"erase first\" unticked)");
        }
        let what = format!("{} {}", if p.name.is_empty() { "firmware" } else { &p.name }, p.version);
        if p.improv {
            self.lab("web install: this firmware offers Improv Wi-Fi setup over serial; cyd-lab doesn't do that step, use the firmware's own setup (e.g. its access point or web UI).");
        }
        self.queue_flash(p.erase, p.parts, what.trim().to_string(), true);
    }

    fn queue_flash(&mut self, erase: bool, parts: Vec<(u64, Vec<u8>, String)>, what: String, power_on_after: bool) {
        if matches!(self.backend.state(), RunState::Running(_) | RunState::Stopping) {
            self.lab(format!("{what}: powering off to write flash.bin (the engine saves it first)…"));
        }
        self.pending_flash = Some(PendingFlash { erase, parts, what, power_on_after, stop_sent: false });
    }

    /// Write the pending firmware if the engine is stopped. Returns true when the device
    /// should now be powered on.
    fn apply_pending(&mut self, store: &DeviceStore) -> bool {
        let state = self.backend.state();
        let Some(p) = self.pending_flash.as_mut() else { return false };
        match state {
            RunState::Running(_) => {
                if !p.stop_sent {
                    p.stop_sent = true;
                    p.power_on_after = true;
                    self.backend.request_stop();
                }
                return false;
            }
            RunState::Stopping => return false,
            RunState::Stopped | RunState::Exited(_) => {}
        }
        let p = self.pending_flash.take().unwrap();
        if p.erase {
            if let Err(e) = store.erase_flash(&self.dev) {
                self.lab(format!("{}: erase failed: {e:#}", p.what));
                return false;
            }
        }
        let mut bytes = 0;
        for (off, data, src) in &p.parts {
            if let Err(e) = store.import_firmware(&self.dev, *off, data) {
                self.lab(format!("{}: writing {src} at 0x{off:x}: {e:#}", p.what));
                self.flash_blank = flash_is_blank(&self.dev.flash_path());
                return false;
            }
            bytes += data.len();
        }
        let at: Vec<String> = p.parts.iter().map(|(o, _, _)| format!("0x{o:x}")).collect();
        self.lab(match (p.erase, p.parts.is_empty()) {
            (true, true) => "flash erased to 0xFF".to_string(),
            (e, _) => format!("{}: wrote {bytes} bytes at {}{}", p.what, at.join(", "), if e { " (after erasing)" } else { "" }),
        });
        self.flash_blank = flash_is_blank(&self.dev.flash_path());
        p.power_on_after
    }

    /// Several files dropped at once: classify each and confirm the offsets.
    fn flash_files(&mut self, paths: &[PathBuf]) {
        let mut files = Vec::new();
        for p in paths {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match std::fs::read(p) {
                Ok(d) => files.push((name, d)),
                Err(e) => return self.lab(format!("{name}: {e}")),
            }
        }
        let plan = crate::firmware::plan_files(&files, &flash_head(&self.dev.flash_path()));
        let rows = files
            .into_iter()
            .zip(plan)
            .map(|((name, data), (off, part))| MultiRow { name, data, part, offset: off.map(|o| format!("0x{o:x}")).unwrap_or_default() })
            .collect::<Vec<_>>();
        let name = format!("{} files", rows.len());
        self.drop_ask = Some(DropAsk { name, data: Vec::new(), kind: DropKind::Multi { rows } });
    }

    /// A .bin (or manifest.json) dropped onto the window or picked with "Flash a .bin…".
    fn flash_file(&mut self, path: &Path, waker: Waker) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.to_ascii_lowercase().ends_with(".json") {
            self.web_manifest = path.to_string_lossy().into_owned();
            return self.start_web_install(waker);
        }
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => return self.lab(format!("{name}: {e}")),
        };
        match crate::firmware::place(&data, &flash_head(&self.dev.flash_path())) {
            crate::firmware::Placement::At(off, kind) => {
                self.lab(format!("{name}: {kind} at 0x{off:x}"));
                self.queue_flash(false, vec![(off, data, name.clone())], name, true);
            }
            crate::firmware::Placement::Ask(off, why) => self.drop_ask = Some(DropAsk { name, data, kind: DropKind::Offset { offset: format!("0x{off:x}"), why } }),
            crate::firmware::Placement::AppIntoPartition { offset, otadata } => self.drop_ask = Some(DropAsk { name, data, kind: DropKind::IntoPartition { offset, otadata } }),
            crate::firmware::Placement::AppOnBlank { install_anyway } => {
                self.lab(format!("{name}: app-only image and the board is blank (no bootloader or partition table), not written. Use the merged/factory image (ESPHome: Manual download > Factory format; Arduino: <sketch>.ino.merged.bin)."));
                self.drop_ask = Some(DropAsk { name, data, kind: DropKind::OnBlank { install_anyway } })
            }
        }
    }

    fn log(&mut self, stream: Stream, line: String) {
        if self.console.len() >= CONSOLE_LINES {
            self.console.pop_front();
        }
        let line = strip_ansi(&line);
        if stream != Stream::Lab {
            let before = self.wifi_watch.banner.clone();
            self.wifi_watch.feed(&line, &self.wifi_ap);
            if self.wifi_watch.banner != before {
                if let Some(b) = self.wifi_watch.banner.clone() {
                    self.console.push_back((Stream::Lab, format!("[cyd-lab] {b}")));
                }
            }
        }
        self.console.push_back((stream, line));
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
    /// What `running.json` last said (rewritten only when this changes).
    running_written: Option<Vec<crate::running::RunningDevice>>,
    /// The mask ROM is available (settings' ROM dir or the downloaded copy).
    rom_ready: bool,
    rom_job: Option<std::sync::mpsc::Receiver<crate::setup::RomMsg>>,
    rom_progress: (u64, Option<u64>),
    rom_error: Option<String>,
    arduino_report: Option<crate::arduino_setup::Report>,
    /// The firmware catalogue (bundled, or a refreshed copy) and its thumbnails.
    catalog: crate::catalog::Catalog,
    catalog_note: String,
    catalog_job: Option<std::sync::mpsc::Receiver<Result<crate::catalog::Catalog, String>>>,
    thumbs: std::collections::HashMap<String, TextureHandle>,
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
        let (catalog, catalog_note) = crate::catalog::load(&paths.root, &settings.catalog);
        let catalog_note = catalog_note.unwrap_or_default();
        let mut app = CydLabApp {
            paths,
            settings,
            settings_draft: None,
            profiles,
            store,
            devices,
            selected,
            new_device: None,
            confirm_delete: None,
            status,
            waker,
            running_written: None,
            rom_ready: false,
            rom_job: None,
            rom_progress: (0, None),
            rom_error: None,
            arduino_report: None,
            catalog,
            catalog_note,
            catalog_job: None,
            thumbs: Default::default(),
        };
        app.rom_ready = app.check_rom();
        if !app.rom_ready {
            app.start_rom_download();
        }
        app
    }

    /// Where the downloaded mask ROMs live.
    fn rom_cache(&self) -> PathBuf {
        self.paths.root.join("roms")
    }

    fn check_rom(&self) -> bool {
        match self.settings.engine.rom_dir.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) => crate::setup::ROMS.iter().all(|(n, _)| d.join(n).is_file()),
            None => crate::setup::roms_ok(&self.rom_cache()),
        }
    }

    fn start_rom_download(&mut self) {
        let dir = self.rom_cache();
        let waker = self.waker.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.rom_job = Some(rx);
        self.rom_error = None;
        self.rom_progress = (0, None);
        std::thread::spawn(move || {
            let t = tx.clone();
            let w = waker.clone();
            let mut last = 0u64;
            let r = crate::setup::fetch_roms(&dir, &mut |n, total| {
                if n - last >= 256 * 1024 || Some(n) == total {
                    last = n;
                    let _ = t.send(crate::setup::RomMsg::Progress(n, total));
                    w();
                }
            });
            let _ = tx.send(crate::setup::RomMsg::Done(r.map_err(|e| format!("{e:#}"))));
            waker();
        });
    }

    fn poll_rom(&mut self) {
        let Some(rx) = &self.rom_job else { return };
        for m in rx.try_iter().collect::<Vec<_>>() {
            match m {
                crate::setup::RomMsg::Progress(n, t) => self.rom_progress = (n, t),
                crate::setup::RomMsg::Done(r) => {
                    self.rom_job = None;
                    match r {
                        Ok(()) => self.rom_ready = self.check_rom(),
                        Err(e) => self.rom_error = Some(e),
                    }
                }
            }
        }
    }

    /// Settings with the release defaults filled in: the engine next to cyd-lab when the
    /// configured one is the plain default name and not on PATH, and the downloaded ROMs.
    fn effective_engine(&self) -> EngineSettings {
        let mut e = self.settings.engine.clone();
        if e.engine_exe.components().count() <= 1 {
            if let Some(b) = crate::setup::bundled_engine() {
                e.engine_exe = b;
            }
        }
        if e.rom_dir.as_ref().is_none_or(|d| d.as_os_str().is_empty()) && crate::setup::roms_ok(&self.rom_cache()) {
            e.rom_dir = Some(self.rom_cache());
        }
        e
    }

    fn setup_arduino(&self) -> crate::arduino_setup::Report {
        use crate::arduino_setup::*;
        let fail = |m: String| Report { ok: false, lines: vec![m] };
        let Some(dirs) = ArduinoDirs::discover() else { return fail("No home folder found.".into()) };
        let Some(tools) = crate::setup::exe_dir().and_then(|d| find_tools(&d)) else {
            return fail("cyd-lab-discovery / cyd-lab-monitor are missing: they belong in the arduino\\tools folder next to cyd-lab (re-extract the zip).".into());
        };
        let running = self.paths.root.join(crate::running::FILE_NAME);
        install(&dirs, &tools, &running).unwrap_or_else(|e| fail(format!("Setup failed: {e:#}")))
    }

    fn arduino_window(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.arduino_report else { return };
        let mut open = true;
        let mut close = false;
        egui::Window::new(if r.ok { "Arduino IDE: ready" } else { "Arduino IDE: one more step" }).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            for l in &r.lines {
                if l.starts_with("  ") || l.starts_with("arduino-cli") {
                    ui.label(RichText::new(l).monospace());
                } else {
                    ui.label(l);
                }
            }
            ui.add_space(6.0);
            if ui.button("OK").clicked() {
                close = true;
            }
        });
        if !open || close {
            self.arduino_report = None;
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let text = match self.selected.and_then(|i| self.devices.get(i)) {
                Some(d) => format!("Drop to flash {}", d.dev.meta.name),
                None => "Create a device first (+ New device)".to_string(),
            };
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(28.0), Color32::WHITE);
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        let Some(path) = dropped.first() else { return };
        match self.selected.filter(|i| *i < self.devices.len()) {
            Some(i) if self.devices[i].pending_flash.is_none() && self.devices[i].web_job.is_none() => {
                if dropped.len() > 1 {
                    self.devices[i].flash_files(&dropped);
                } else {
                    let w = self.waker.clone();
                    self.devices[i].flash_file(path, w);
                }
            }
            Some(_) => self.status = "Busy writing firmware; drop again when it is done.".into(),
            None => self.status = "Create a device first (+ New device), then drop the .bin onto it.".into(),
        }
    }

    fn drop_ask_window(&mut self, ctx: &egui::Context) {
        let Some(i) = self.devices.iter().position(|d| d.drop_ask.is_some()) else { return };
        let mut go = false;
        let mut cancel = false;
        let mut to_offset = false;
        let d = &mut self.devices[i];
        let ask = d.drop_ask.as_mut().unwrap();
        let name = ask.name.clone();
        egui::Modal::new(egui::Id::unique("drop_ask")).show(ctx, |ui| {
            ui.set_max_width(540.0);
            match &mut ask.kind {
                DropKind::Offset { offset, why } => {
                    ui.heading(format!("Where should {name} go?"));
                    ui.label(why.as_str());
                    ui.horizontal(|ui| {
                        ui.label("Write at offset");
                        ui.add(egui::TextEdit::singleline(offset).desired_width(90.0));
                        ui.label(RichText::new("0x0 merged/factory · 0x1000 bootloader · 0x8000 partition table · 0xe000 otadata · 0x10000 app").small().weak());
                    });
                }
                DropKind::Multi { rows } => {
                    ui.heading(format!("Write {} files?", rows.len()));
                    ui.label("Each file goes to the usual offset for its type (app and otadata where the partition table puts them). Check, change if needed, then Write.");
                    egui::Grid::new("multi_drop").num_columns(3).striped(true).show(ui, |ui| {
                        for r in rows.iter_mut() {
                            ui.label(RichText::new(&r.name).monospace());
                            ui.label(RichText::new(r.part.label()).small());
                            ui.add(egui::TextEdit::singleline(&mut r.offset).desired_width(90.0).hint_text("0x…"));
                            ui.end_row();
                        }
                    });
                    ui.label(RichText::new("Leave an offset empty to skip that file.").small().weak());
                }
                DropKind::IntoPartition { offset, .. } => {
                    ui.heading("App-only image");
                    ui.label(format!("{name} is an app-only image: the program without a bootloader or partition table. This board already has both, so it can go into the first app partition at 0x{offset:x}, and the boot selection (otadata) is reset so that partition boots, like an OTA update. Saved settings (NVS) stay."));
                    ui.label(RichText::new("For a clean start, use the merged/factory image instead (it goes to 0x0, exactly what a real CYD gets over USB).").small().weak());
                }
                DropKind::OnBlank { install_anyway } => {
                    ui.heading("App-only image on a blank board");
                    ui.label(format!("{name} holds only the app. A blank board also needs a bootloader (0x1000) and a partition table (0x8000), so the app alone can't boot. Tools send this file to boards that are already set up (OTA updates, uploads)."));
                    ui.add_space(4.0);
                    ui.label(RichText::new("Use the merged/factory image instead: it is the like-for-like choice, the same bytes a real CYD gets over USB.").strong());
                    for (tool, hint) in crate::firmware::TOOL_HINTS {
                        let mut job = egui::text::LayoutJob::default();
                        let small = egui::TextStyle::Small.resolve(ui.style());
                        let fmt = |c: Color32| egui::TextFormat { font_id: small.clone(), color: c, ..Default::default() };
                        job.append(&format!("{tool}: "), 0.0, fmt(ui.visuals().strong_text_color()));
                        job.append(hint, 0.0, fmt(ui.visuals().text_color()));
                        job.wrap.max_width = ui.available_width();
                        ui.label(job);
                    }
                    ui.label(RichText::new("You can also drop a build's bootloader, partition table and app together.").small().weak());
                    ui.add_space(4.0);
                    if *install_anyway {
                        ui.label(RichText::new("Install anyway adds a generic ESP-IDF bootloader and the Arduino-ESP32 default 4 MB partition table (app at 0x10000, 1.25 MB slots). This may differ from the layout the firmware was built for, e.g. a custom partition table or flash settings.").small().weak());
                    } else {
                        ui.label(RichText::new("It is too big for the default layout's 1.25 MB app slot, so only the merged/factory image will work.").small().weak());
                    }
                }
            }
            ui.horizontal(|ui| match &ask.kind {
                DropKind::OnBlank { install_anyway } => {
                    if ui.button(RichText::new("OK, I'll get the merged/factory image").strong()).clicked() {
                        cancel = true;
                    }
                    if ui.add_enabled(*install_anyway, egui::Button::new("Install anyway")).clicked() {
                        go = true;
                    }
                    if ui.button("Write at offset…").clicked() {
                        to_offset = true;
                    }
                }
                k => {
                    let label = match k {
                        DropKind::IntoPartition { .. } => "Write to app partition",
                        _ => "Write",
                    };
                    if ui.button(RichText::new(label).strong()).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if matches!(k, DropKind::IntoPartition { .. }) && ui.button("Write at offset…").clicked() {
                        to_offset = true;
                    }
                }
            });
        });
        if to_offset {
            ask.kind = DropKind::Offset { offset: "0x10000".into(), why: "Advanced: write the file as is at an offset you choose.".into() };
            return;
        }
        if go {
            let parts: Result<Vec<(u64, Vec<u8>, String)>> = match &mut ask.kind {
                DropKind::Offset { offset, .. } => parse_offset(offset).map(|off| vec![(off, std::mem::take(&mut ask.data), name.clone())]),
                DropKind::Multi { rows } => rows
                    .iter_mut()
                    .filter(|r| !r.offset.trim().is_empty())
                    .map(|r| parse_offset(&r.offset).map(|o| (o, std::mem::take(&mut r.data), r.name.clone())).with_context(|| r.name.clone()))
                    .collect(),
                DropKind::IntoPartition { offset, otadata } => {
                    let mut v = Vec::new();
                    if let Some((o, len)) = otadata.filter(|(_, len)| *len <= 0x10000) {
                        v.push((o, vec![0xFF; len as usize], "otadata (reset: boot the first app)".to_string()));
                    }
                    v.push((*offset, std::mem::take(&mut ask.data), name.clone()));
                    Ok(v)
                }
                DropKind::OnBlank { .. } => Ok(crate::firmware::stock_layout(&std::mem::take(&mut ask.data))),
            };
            match parts {
                Ok(parts) if parts.is_empty() => self.status = "nothing to write: every offset is empty".into(),
                Ok(parts) => {
                    d.drop_ask = None;
                    d.queue_flash(false, parts, name, true);
                }
                Err(e) => self.status = format!("offset: {e:#}"),
            }
        } else if cancel {
            d.drop_ask = None;
        }
    }

    fn first_run_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("Welcome to cyd-lab");
            ui.add_space(8.0);
            ui.label("One-time setup: cyd-lab needs Espressif's ESP32 mask ROM to boot the emulated chip.");
            ui.label(RichText::new(format!("esp-rom-elfs {} from github.com/espressif (Apache-2.0), checked by SHA-256, kept in {}", crate::setup::ROM_RELEASE, self.rom_cache().display())).small().weak());
            ui.add_space(14.0);
            if self.rom_job.is_some() {
                let (n, t) = self.rom_progress;
                let frac = t.map(|t| n as f32 / t.max(1) as f32);
                let text = match t {
                    _ if n == 0 => "Connecting to github.com…".to_string(),
                    Some(t) => format!("Downloading… {:.1} / {:.1} MB", n as f64 / 1e6, t as f64 / 1e6),
                    None => format!("Downloading… {:.1} MB", n as f64 / 1e6),
                };
                ui.add(egui::ProgressBar::new(frac.unwrap_or(0.0)).desired_width(360.0).text(text).animate(frac.is_none()));
            } else if let Some(e) = &self.rom_error {
                ui.label(RichText::new("The download did not work:").color(Color32::LIGHT_RED).strong());
                ui.label(RichText::new(e).color(Color32::LIGHT_RED));
                ui.label(RichText::new("Check the internet connection (or a proxy/firewall blocking github.com) and retry.").small());
                ui.add_space(6.0);
                if ui.button(RichText::new("Retry").strong().size(16.0)).clicked() {
                    self.start_rom_download();
                }
            }
            ui.add_space(20.0);
            let engine = self.effective_engine().engine_exe;
            match resolve_exe(&engine) {
                Some(p) => ui.label(RichText::new(format!("Engine: {}", p.display())).small().weak()),
                None => ui.label(RichText::new(format!("Engine not found: put {} next to cyd-lab (it ships in the zip), or set it in Settings.", crate::setup::engine_file_name())).color(Color32::LIGHT_RED)),
            };
            ui.add_space(6.0);
            if ui.small_button("I already have the ROM ELFs: choose their folder…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let mut s = self.settings.clone();
                    s.engine.rom_dir = Some(dir);
                    self.settings = s;
                    if let Err(e) = self.settings.save(&self.paths.settings) {
                        self.status = format!("could not save settings: {e:#}");
                    }
                    self.rom_ready = self.check_rom();
                    if !self.rom_ready {
                        self.rom_error = Some(format!("{} is not in that folder", crate::setup::ROMS[0].0));
                    }
                }
            }
        });
    }

    fn profile_for(&self, i: usize) -> Option<Profile> {
        self.profiles.get(&self.devices[i].dev.meta.profile).cloned()
    }

    /// What the configured engine build supports (cached per exe path/size/mtime).
    fn engine_features(&self) -> EngineFeatures {
        resolve_exe(&self.effective_engine().engine_exe).map(|p| EngineFeatures::probe(&p)).unwrap_or_default()
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
        let mut spec = LaunchSpec::build(&profile, &self.devices[i].dev, &self.effective_engine(), &features, boot, port, serial);
        let (net_args, notes) = crate::network::engine_args(&self.devices[i].dev.meta.network, &self.settings.wifi, features.net(), &bind_is_free);
        spec.args.extend(net_args);
        spec.notes.extend(notes);
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
        d.wifi_ap = d.dev.meta.network.effective(&self.settings.wifi);
        d.wifi_watch = Default::default();
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
            if let Some(rx) = &d.web_job {
                let msgs: Vec<WebMsg> = rx.try_iter().collect();
                for m in msgs {
                    match m {
                        WebMsg::Line(l) => d.lab(format!("{}: {l}", d.job_label)),
                        WebMsg::Done(r) => {
                            d.web_job = None;
                            match r {
                                Ok(p) => d.finish_web_install(p),
                                Err(e) => d.lab(format!("{} failed: {e}", d.job_label)),
                            }
                        }
                    }
                }
            }
            for ev in d.backend.poll() {
                match ev {
                    EngineEvent::Console(s, l) => d.log(s, l),
                    EngineEvent::Exited(_) => {
                        d.flash_blank = flash_is_blank(&d.dev.flash_path());
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
        self.publish_running();
        if any_running {
            // Notice process exits even when the engine is silent.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Keep `running.json` (read by the Arduino discovery) in step with the powered-on devices.
    fn publish_running(&mut self) {
        let now: Vec<crate::running::RunningDevice> = self
            .devices
            .iter()
            .filter(|d| matches!(d.backend.state(), RunState::Running(_)))
            .filter_map(|d| {
                let port = d.backend.capabilities().serial_tcp?;
                Some(crate::running::RunningDevice { id: d.dev.id.clone(), name: d.dev.meta.name.clone(), profile: d.dev.meta.profile.clone(), serial_port: port })
            })
            .collect();
        if self.running_written.as_ref() == Some(&now) {
            return;
        }
        let file = crate::running::RunningFile { version: 1, pid: std::process::id(), devices: now.clone() };
        match file.write(&self.paths.root.join(crate::running::FILE_NAME)) {
            Ok(()) => self.running_written = Some(now),
            Err(e) => {
                self.status = format!("could not write running.json: {e}");
                self.running_written = Some(now);
            }
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
        let _ = std::fs::remove_file(self.paths.root.join(crate::running::FILE_NAME));
        self.running_written = None;
    }

    // --- dialogs ----------------------------------------------------------

    fn new_device_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.new_device.as_mut() else { return };
        let mut open = true;
        let mut create = false;
        egui::Window::new("New device").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.label("A new device is a factory-fresh board: blank flash (all 0xFF), its own folder, network serial port and MAC.");
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
            match self.profiles.get(&pid).cloned().context("pick a variant").and_then(|p| self.store.create_with(&name, &p, &self.settings.wifi)) {
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

    fn thumb(&mut self, ctx: &egui::Context, id: &str, png: &[u8]) -> Option<TextureHandle> {
        if let Some(t) = self.thumbs.get(id) {
            return Some(t.clone());
        }
        let img = image::load_from_memory(png).ok()?.to_rgba8();
        let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        let t = ctx.load_texture(format!("thumb-{id}"), ci, TextureOptions::LINEAR);
        self.thumbs.insert(id.to_string(), t.clone());
        Some(t)
    }

    /// "Install firmware": the catalogue entries for the selected device's board.
    fn gallery_window(&mut self, ctx: &egui::Context) {
        let Some(i) = self.selected.filter(|&i| i < self.devices.len() && self.devices[i].gallery_open) else { return };
        if let Some(rx) = &self.catalog_job {
            if let Ok(r) = rx.try_recv() {
                self.catalog_job = None;
                match r {
                    Ok(c) => {
                        self.catalog_note = format!("catalogue refreshed: {} entries, updated {}", c.entries.len(), c.updated);
                        self.catalog = c;
                    }
                    Err(e) => self.catalog_note = format!("catalogue refresh failed (keeping the current one): {e}"),
                }
            }
        }
        let board = self.devices[i].dev.meta.profile.clone();
        let board_name = self.profiles.get(&board).map(|p| p.name.clone()).unwrap_or_else(|| board.clone());
        let cat_profile = self.catalog.profile_for(&board).cloned();
        let show_broken = self.devices[i].gallery_broken;
        let entries: Vec<crate::catalog::Entry> = cat_profile.as_ref().map(|p| self.catalog.entries_for(&p.id, show_broken).into_iter().cloned().collect()).unwrap_or_default();
        let hidden = cat_profile.as_ref().map(|p| self.catalog.hidden_broken(&p.id)).unwrap_or(0);
        let mut thumbs = Vec::new();
        for e in &entries {
            thumbs.push(e.thumbnail().and_then(|png| self.thumb(ctx, &e.id, png)));
        }
        let busy = self.devices[i].pending_flash.is_some() || self.devices[i].web_job.is_some();
        let running = matches!(self.devices[i].backend.state(), RunState::Running(_) | RunState::Stopping);
        let mut open = true;
        let mut install: Option<crate::catalog::Entry> = None;
        let mut use_web: Option<String> = None;
        let mut saved: Option<String> = None;
        let mut refresh = false;
        let dev_name = self.devices[i].dev.meta.name.clone();
        let refresh_on = self.settings.catalog.refresh && !self.settings.catalog.url.trim().is_empty();
        let refreshing = self.catalog_job.is_some();
        let (updated, note) = (self.catalog.updated.clone(), self.catalog_note.clone());
        egui::Window::new(format!("Install firmware on {dev_name}"))
            .id(egui::Id::unique(("gallery", i)))
            .collapsible(false)
            .default_size([600.0, 620.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Board: {board_name}"));
                    ui.label(RichText::new(format!("· catalogue {updated}")).weak());
                    if refresh_on {
                        if ui.add_enabled(!refreshing, egui::Button::new("Check for updates").small()).clicked() {
                            refresh = true;
                        }
                        if refreshing {
                            ui.spinner();
                        }
                    }
                });
                if !note.is_empty() {
                    ui.label(RichText::new(&note).small().weak());
                }
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Installing… (progress in the console)");
                    });
                } else if running {
                    ui.label(RichText::new("The device is on: installing powers it off, writes the flash and powers it on again.").small().weak());
                }
                ui.separator();
                let Some(cp) = &cat_profile else {
                    ui.label(format!("The catalogue has no firmware for this board ({board}). Use \"Install from web\" with the firmware's manifest URL, or drop your own .bin files on the window."));
                    return;
                };
                egui::ScrollArea::vertical().max_height(470.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (e, thumb) in entries.iter().zip(&thumbs) {
                        ui.horizontal(|ui| {
                            let size = Vec2::new(60.0, 80.0);
                            match thumb {
                                Some(t) => {
                                    let sz = t.size_vec2();
                                    let scale = (size.x / sz.x).min(size.y / sz.y);
                                    ui.add(egui::Image::new((t.id(), sz * scale))).on_hover_ui(|ui| {
                                        ui.add(egui::Image::new((t.id(), sz)));
                                        ui.label("In the emulator");
                                    });
                                }
                                None => {
                                    let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                                    ui.painter().rect_filled(r, 3.0, Color32::from_gray(20));
                                    let txt = if e.is_template() { "config" } else { "no screen\npicture" };
                                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, txt, egui::FontId::proportional(10.0), Color32::GRAY);
                                }
                            }
                            ui.vertical(|ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(&e.name).strong());
                                    if !e.version.is_empty() {
                                        ui.label(RichText::new(&e.version).weak());
                                    }
                                    let col = match e.emulator_status {
                                        crate::catalog::Status::Works => Color32::from_rgb(60, 160, 80),
                                        crate::catalog::Status::Partial => Color32::from_rgb(200, 140, 30),
                                        crate::catalog::Status::Broken => Color32::from_rgb(190, 60, 60),
                                        crate::catalog::Status::Untested => Color32::from_gray(110),
                                    };
                                    let badge = format!(" {} ", e.emulator_status.label());
                                    ui.label(RichText::new(badge).small().color(Color32::WHITE).background_color(col))
                                        .on_hover_text(format!("{}\n\n{}", e.emulator_status.help(), e.emulator_notes));
                                    for need in e.needs() {
                                        ui.label(RichText::new(format!(" needs {need} ")).small().color(Color32::BLACK).background_color(Color32::from_rgb(230, 200, 120)))
                                            .on_hover_text("The emulator doesn't model this; those features won't work here.");
                                    }
                                });
                                ui.label(&e.description);
                                if !e.emulator_notes.is_empty() {
                                    ui.label(RichText::new(format!("In the emulator: {}", e.emulator_notes)).small().weak());
                                }
                                ui.horizontal_wrapped(|ui| {
                                    if e.is_template() {
                                        if let Ok(crate::catalog::Spec::Template { files }) = e.spec() {
                                            for f in files {
                                                let short = f.rsplit('/').next().unwrap_or(&f).to_string();
                                                let text = crate::catalog::Entry::template_text(&f).unwrap_or("");
                                                if ui.button(format!("Copy {short}")).on_hover_text("Copy the file to the clipboard").clicked() {
                                                    ui.ctx().copy_text(text.to_string());
                                                    saved = Some(format!("{short} copied to the clipboard"));
                                                }
                                                if ui.small_button("Save…").clicked() {
                                                    if let Some(path) = rfd::FileDialog::new().set_file_name(&short).save_file() {
                                                        saved = Some(match std::fs::write(&path, text) {
                                                            Ok(()) => format!("saved {}", path.display()),
                                                            Err(err) => format!("saving {}: {err}", path.display()),
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        let tip = if e.pinned() { "Download, check the SHA-256 the catalogue lists, erase the flash and write it. The device boots it straight away." } else { "Download from the web installer, erase the flash and write it (this entry has no pinned SHA-256)." };
                                        if ui.add_enabled(!busy, egui::Button::new(RichText::new("Install").strong())).on_hover_text(tip).clicked() {
                                            install = Some(e.clone());
                                        }
                                        if let Some(u) = e.manifest_url() {
                                            if ui.small_button("Use manifest…").on_hover_text(format!("Put {u} into \"Install from web\" (skips the catalogue's SHA-256 check)")).clicked() {
                                                use_web = Some(u);
                                            }
                                        }
                                    }
                                    if !e.homepage.is_empty() {
                                        ui.hyperlink_to(RichText::new("homepage").small(), &e.homepage);
                                    }
                                    if !e.licence.is_empty() {
                                        ui.label(RichText::new(&e.licence).small().weak());
                                    }
                                });
                                if !e.notes.trim().is_empty() {
                                    egui::CollapsingHeader::new(RichText::new("Notes").small()).id_salt(("gnotes", &e.id)).show(ui, |ui| {
                                        ui.label(RichText::new(e.notes.trim()).small());
                                    });
                                }
                            });
                        });
                        ui.separator();
                    }
                    let unsup: Vec<_> = self.catalog.unsupported.iter().collect();
                    if !unsup.is_empty() {
                        egui::CollapsingHeader::new(RichText::new(format!("Not in the list ({})", unsup.len())).small()).id_salt("gunsup").show(ui, |ui| {
                            for u in unsup {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(format!("{}: {}", u.name, u.reason)).small());
                                    if !u.homepage.is_empty() {
                                        ui.hyperlink_to(RichText::new("link").small(), &u.homepage);
                                    }
                                });
                            }
                        });
                    }
                });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    let d = &mut self.devices[i];
                    let label = if hidden > 0 { format!("Show entries that don't boot in the emulator ({hidden})") } else { "Show entries that don't boot in the emulator".into() };
                    ui.checkbox(&mut d.gallery_broken, label);
                });
                ui.label(RichText::new(format!("Something else? Paste its web flasher's manifest URL into \"Install from web\", or drop your own .bin files on the window. ({} profile: {})", cp.display, cp.name)).small().weak());
                if let Some(m) = &saved {
                    ui.label(RichText::new(m).small());
                }
            });
        if let Some(m) = saved {
            self.devices[i].lab(format!("catalogue: {m}"));
        }
        if refresh {
            let url = self.settings.catalog.url.trim().to_string();
            let root = self.paths.root.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let w = self.waker.clone();
            self.catalog_job = Some(rx);
            std::thread::spawn(move || {
                let _ = tx.send(crate::catalog::refresh(&url, &root).map_err(|e| format!("{e:#}")));
                w();
            });
        }
        if let Some(u) = use_web {
            self.devices[i].web_manifest = u;
            self.devices[i].gallery_open = false;
            return;
        }
        if let Some(e) = install {
            let cache = self.paths.root.join("cache").join("firmware");
            let w = self.waker.clone();
            self.devices[i].start_catalog_install(e, cache, w);
        }
        if !open {
            self.devices[i].gallery_open = false;
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
            ui.heading("Firmware catalogue");
            let c = &mut draft.catalog;
            ui.checkbox(&mut c.refresh, "Check a URL for catalogue updates (off: use the catalogue built into cyd-lab)");
            ui.add_enabled(c.refresh, egui::TextEdit::singleline(&mut c.url).desired_width(420.0).hint_text("https://raw.githubusercontent.com/<user>/cyd-lab/main/catalog/cyd-firmware.toml"));
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
            if draft.catalog != self.settings.catalog {
                let (c, note) = crate::catalog::load(&self.paths.root, &draft.catalog);
                self.catalog = c;
                self.catalog_note = note.unwrap_or_default();
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
        self.network_ui(ui, i, alive, &features);
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

        // --- firmware
        let mut erase = false;
        let mut import = false;
        let mut delete = false;
        let mut rename = false;
        let mut pick_bin = false;
        let mut web_install = false;
        let busy = self.devices[i].pending_flash.is_some() || self.devices[i].web_job.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Firmware:").strong());
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button(RichText::new("Install firmware…").strong()).on_hover_text("Pick a ready-made firmware for this board (openHASP, NerdMiner, Bruce, …): cyd-lab downloads it, checks its SHA-256 and writes it in one click.").clicked() {
                    self.devices[i].gallery_open = true;
                }
                if ui.button(RichText::new("Flash a .bin…").strong()).on_hover_text("Pick a firmware .bin. Merged/factory images go to 0x0, app images into the app partition; you are asked only when it is unclear. A running device is powered off, written and powered on again.").clicked() {
                    pick_bin = true;
                }
                if ui.button("Erase flash").on_hover_text("Reset flash.bin to all 0xFF (a running device is powered off first)").clicked() {
                    erase = true;
                }
            });
            if busy {
                ui.spinner();
            }
            ui.label(RichText::new("…or drop a .bin / manifest.json onto this window").weak());
        });
        ui.horizontal_wrapped(|ui| {
            let d = &mut self.devices[i];
            ui.label(RichText::new("Install from web:").strong()).on_hover_text("ESP Web Tools manifest: the JSON behind an \"Install\" / \"Connect\" button on a web flasher page (openHASP, WLED, ESPHome projects, ...). cyd-lab downloads the ESP32 build's parts and writes them into flash.bin, as the browser would over Web Serial. On the flasher page, look for manifest.json in the page source or the browser's network tab.");
            ui.add_enabled_ui(!busy, |ui| {
                ui.add(egui::TextEdit::singleline(&mut d.web_manifest).desired_width(330.0).hint_text("paste the web flasher's manifest URL: https://…/manifest.json"));
                if ui.button("…").on_hover_text("Pick a local manifest.json (parts are read next to it)").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("manifest", &["json"]).pick_file() {
                        d.web_manifest = f.to_string_lossy().into_owned();
                    }
                }
                ui.checkbox(&mut d.web_erase, "erase first").on_hover_text("Used when the manifest leaves erasing to the user (new_install_prompt_erase). Otherwise a new install always erases, as ESP Web Tools does.");
                if ui.add_enabled(!d.web_manifest.trim().is_empty(), egui::Button::new("Install")).clicked() {
                    web_install = true;
                }
            });
        });
        egui::CollapsingHeader::new(RichText::new("More: write at an offset, rename, delete").small()).id_salt(("more", i)).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let d = &mut self.devices[i];
                let size = d.dev.flash_size().map(|s| format!("{} MB", s / (1024 * 1024))).unwrap_or_else(|_| "missing".into());
                ui.label(format!("Flash ({size}): write .bin at"));
                ui.add(egui::TextEdit::singleline(&mut d.import_offset).desired_width(70.0));
                ui.add(egui::TextEdit::singleline(&mut d.import_path).desired_width(220.0).hint_text("firmware.bin (merged @0x0, app @0x10000)"));
                if ui.button("…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("firmware", &["bin"]).pick_file() {
                        d.import_path = f.to_string_lossy().into_owned();
                    }
                }
                if ui.add_enabled(!busy, egui::Button::new("Write")).clicked() {
                    import = true;
                }
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
        });
        if pick_bin {
            if let Some(f) = rfd::FileDialog::new().add_filter("firmware", &["bin", "json"]).pick_file() {
                let w = self.waker.clone();
                self.devices[i].flash_file(&f, w);
            }
        }
        if web_install {
            let w = self.waker.clone();
            self.devices[i].start_web_install(w);
        }
        if erase {
            let was_on = alive;
            self.devices[i].queue_flash(true, Vec::new(), "erase".into(), was_on);
        }
        if import && self.devices[i].import_path.trim().is_empty() {
            self.devices[i].lab("write: choose a .bin first (type a path or use …)");
        } else if import {
            let d = &mut self.devices[i];
            let path = PathBuf::from(d.import_path.trim());
            match parse_offset(&d.import_offset).and_then(|off| Ok((off, std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?))) {
                Ok((off, data)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    d.queue_flash(false, vec![(off, data, name.clone())], name, true);
                }
                Err(e) => d.lab(format!("write failed: {e:#}")),
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
    /// Network: the access point the chip joins and the ports published on this PC.
    fn network_ui(&mut self, ui: &mut egui::Ui, i: usize, alive: bool, features: &EngineFeatures) {
        let ip = cached_lan_ip();
        let ip_text = ip.map(|i| i.to_string()).unwrap_or_else(|| "this PC's IP".into());
        let defaults = self.settings.wifi.clone();
        let others: Vec<crate::network::DeviceNetwork> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, d)| d.dev.meta.network.clone()).collect();
        let mut save = false;
        let mut make_default = false;
        let mut add_preset = None;
        let d = &mut self.devices[i];
        let net = d.dev.meta.network.clone();
        let eff = net.effective(&defaults);
        let ports: Vec<String> = net.active_forwards().iter().map(|f| format!("{}->{}", f.host, f.guest)).collect();
        let summary = if !net.wifi {
            "Network: Wi-Fi off".to_string()
        } else {
            format!(
                "Network: Wi-Fi \"{}\" · {}",
                eff.ssid,
                if ports.is_empty() { "no ports published".to_string() } else { format!("{} {}", if net.lan { ip_text.as_str() } else { "127.0.0.1" }, ports.join(", ")) }
            )
        };
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), ui.make_persistent_id(("net", i)), false);
        state
            .show_header(ui, |ui| {
                ui.label(RichText::new(summary).small());
                ui.menu_button(RichText::new("Use with…").small(), |ui| {
                    ui.set_max_width(460.0);
                    if let Some(k) = network_help_ui(ui, &ip_text, &d.dev.meta.network, features.mdns_relay) {
                        add_preset = Some(k);
                    }
                });
            })
            .body(|ui| {
                let n = &mut d.dev.meta.network;
                ui.add_enabled_ui(!alive, |ui| {
                    if alive {
                        ui.label(RichText::new("Power off to change these; they apply at the next power-on.").small().weak());
                    }
                    save |= ui.checkbox(&mut n.wifi, "Wi-Fi: offer the chip an access point").on_hover_text("The emulator runs a virtual access point; the firmware joins it as it would join your router. Nothing changes on your real Wi-Fi.").changed();
                    egui::Grid::new(("netgrid", i)).num_columns(2).show(ui, |ui| {
                        ui.label("Network name");
                        save |= ui.add(egui::TextEdit::singleline(&mut n.ssid).desired_width(200.0).hint_text(if defaults.ssid.is_empty() { crate::network::FALLBACK_SSID } else { defaults.ssid.as_str() })).changed();
                        ui.end_row();
                        ui.label("Password");
                        ui.horizontal(|ui| {
                            save |= ui.add(egui::TextEdit::singleline(&mut n.password).password(!d.show_password).desired_width(200.0).hint_text("empty = open network")).changed();
                            ui.checkbox(&mut d.show_password, "show");
                        });
                        ui.end_row();
                    });
                    ui.label(RichText::new("Use the network name and password built into the firmware (the ones it uses at home). Then the same firmware works unchanged.").small().weak());
                    if let Err(e) = crate::network::validate_wifi(&n.effective(&defaults)) {
                        ui.label(RichText::new(format!("Can't use this: {e}.")).small().color(Color32::from_rgb(230, 120, 90)));
                    }
                    ui.horizontal(|ui| {
                        let mine = crate::network::WifiDetails { ssid: n.ssid.clone(), password: n.password.clone() };
                        if mine == defaults && !mine.ssid.is_empty() {
                            ui.label(RichText::new("New devices get these Wi-Fi details").small().weak());
                        } else if ui.add_enabled(!mine.ssid.is_empty(), egui::Button::new("Use these Wi-Fi details for new devices").small()).on_hover_text("Saved in settings.toml, so you only type them once").clicked() {
                            make_default = true;
                        }
                    });
                    ui.separator();
                    save |= ui.checkbox(&mut n.lan, "Reach from my network (other machines, e.g. Home Assistant)").on_hover_text("On: published ports listen on all of this PC's addresses (0.0.0.0). Off: only on this PC (127.0.0.1).").changed();
                    ui.label(RichText::new("Published ports").small().strong());
                    if !n.forwards.is_empty() {
                        egui::Grid::new(("fwd", i)).num_columns(4).show(ui, |ui| {
                            ui.label("");
                            ui.label(RichText::new("this PC's port").small().weak());
                            ui.label("");
                            ui.label(RichText::new("firmware's port").small().weak());
                            ui.end_row();
                            let mut remove = None;
                            for (k, f) in n.forwards.iter_mut().enumerate() {
                                save |= ui.add(egui::TextEdit::singleline(&mut f.label).desired_width(120.0).hint_text("label")).changed();
                                let hc = ui.add(egui::DragValue::new(&mut f.host).range(1..=65535)).changed();
                                if hc && f.same_port {
                                    f.guest = f.host;
                                }
                                save |= hc;
                                ui.label("->");
                                ui.horizontal(|ui| {
                                    save |= ui.add(egui::DragValue::new(&mut f.guest).range(1..=65535)).changed();
                                    if ui.small_button("Remove").clicked() {
                                        remove = Some(k);
                                    }
                                });
                                ui.end_row();
                            }
                            if let Some(k) = remove {
                                n.forwards.remove(k);
                                save = true;
                            }
                        });
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (k, p) in crate::network::PRESETS.iter().enumerate() {
                            if ui.small_button(format!("+ {}", p.name)).on_hover_text(p.tip).clicked() {
                                add_preset = Some(k);
                            }
                        }
                        if ui.small_button("+ Other port").clicked() {
                            let host = (8000..u16::MAX).find(|p| !n.host_ports().contains(p)).unwrap_or(8000);
                            n.forwards.push(crate::network::Forward { label: String::new(), host, guest: host, same_port: false });
                            save = true;
                        }
                    });
                    if n.forwards.iter().any(|f| f.same_port && f.host != 3232) {
                        ui.label(RichText::new("OTA tools connect to the port the firmware listens on, so set it to the same number in the firmware (ESPHome: ota: port: N; Arduino: ArduinoOTA.setPort(N)).").small().weak());
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("This PC: {}", ip.map(|i| i.to_string()).unwrap_or_else(|| "no network address found".into()))).small());
                    let mdns = if features.mdns_relay { "mDNS: relayed, so name.local and discovery work." } else { "mDNS: not available with this engine build; connect by IP." };
                    ui.label(RichText::new(mdns).small().weak());
                });
            });
        if let Some(k) = add_preset {
            d.dev.meta.network.add_preset(&crate::network::PRESETS[k], &others);
            save = true;
            if alive {
                d.lab(format!("{} ports added: they apply at the next power-on.", crate::network::PRESETS[k].name));
            }
        }
        if make_default {
            let n = &d.dev.meta.network;
            self.settings.wifi = crate::network::WifiDetails { ssid: n.ssid.clone(), password: n.password.clone() };
            if let Err(e) = self.settings.save(&self.paths.settings) {
                self.status = format!("could not save settings: {e:#}");
            }
        }
        let d = &mut self.devices[i];
        if save {
            if let Err(e) = d.dev.save() {
                d.lab(format!("could not save device settings: {e:#}"));
            }
        }
    }

    fn serial_bridge_ui(&mut self, ui: &mut egui::Ui, i: usize, alive: bool, features: &EngineFeatures) {
        let live_port = self.devices[i].backend.capabilities().serial_tcp.filter(|_| alive);
        let others: Vec<u16> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).filter_map(|(_, d)| d.dev.meta.serial_tcp_port).collect();
        let mut save = false;
        let net_ip = cached_lan_ip().map(|i| i.to_string()).unwrap_or_else(|| "this PC's IP".into());
        let net_others: Vec<crate::network::DeviceNetwork> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, d)| d.dev.meta.network.clone()).collect();
        let mut net_preset = None;
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
                if ui.small_button("Copy").on_hover_text("Copy the rfc2217:// URL").clicked() {
                    ui.ctx().copy_text(url.clone());
                }
                let name = d.dev.meta.name.clone();
                ui.menu_button("Use with…", |ui| {
                    use_with_ui(ui, &url, &name);
                    ui.separator();
                    if let Some(k) = network_help_ui(ui, &net_ip, &d.dev.meta.network, features.mdns_relay) {
                        net_preset = Some(k);
                    }
                });
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
        if let Some(k) = net_preset {
            d.dev.meta.network.add_preset(&crate::network::PRESETS[k], &net_others);
            save = true;
            if alive {
                d.lab(format!("{} ports added: they apply at the next power-on.", crate::network::PRESETS[k].name));
            }
        }
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
            }            if d.flash_blank && d.pending_flash.is_none() {
                ui.label(RichText::new("Blank flash: press Install firmware..., or drop a .bin here").color(Color32::from_rgb(240, 190, 40)));
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
        let wifi_defaults = self.settings.wifi.clone();
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
                ui.small_button("Command line").on_hover_text(format!("{}\n\nClick to copy (includes the Wi-Fi password).", crate::engine::mask_secrets(&cmd))).clicked().then(|| ui.ctx().copy_text(cmd));
            }
        });
        let mut use_ssid = None;
        // Wi-Fi settings changed since power-on: the banner describes the old access point.
        let now = d.dev.meta.network.effective(&wifi_defaults);
        let pending = d.wifi_watch.banner.is_some() && (now != d.wifi_ap || !d.dev.meta.network.wifi);
        if pending {
            egui::Frame::new().fill(Color32::from_rgb(40, 70, 100)).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new(format!("Wi-Fi changed to \"{}\". Power off and on to apply it.", now.ssid)).color(Color32::WHITE));
            });
        } else if let Some(b) = d.wifi_watch.banner.clone() {
            egui::Frame::new().fill(Color32::from_rgb(90, 60, 10)).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(b).color(Color32::WHITE));
                    if let Some(w) = d.wifi_watch.wanted.clone().filter(|w| *w != d.dev.meta.network.ssid) {
                        if ui.button(format!("Use \"{w}\" for this device")).on_hover_text("Sets this device's Wi-Fi network name. Check the password in Network, then power-cycle.").clicked() {
                            use_ssid = Some(w);
                        }
                    }
                });
            });
        }
        if let Some(w) = use_ssid {
            d.dev.meta.network.ssid = w;
            d.dev.meta.network.wifi = true;
            if let Err(e) = d.dev.save() {
                d.lab(format!("could not save device settings: {e:#}"));
            }
            d.lab("Wi-Fi network name updated. Check the password under Network, then Power off and on.");
        }
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
        self.poll_rom();
        for i in 0..self.devices.len() {
            if self.devices[i].apply_pending(&self.store) {
                self.power_on(i, false);
            }
        }
        self.handle_drops(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("cyd-lab").strong().size(16.0));
                ui.label(RichText::new(env!("CARGO_PKG_VERSION")).weak());
                ui.separator();
                if ui.button("+ New device").clicked() {
                    self.open_new_device();
                }
                if ui.button("Set up Arduino IDE").on_hover_text("Add the \"CYD (cyd-lab, emulated)\" board and its network ports to Arduino IDE 2: no Boards Manager URL, no COM driver. Needs the esp32 core installed in the IDE.").clicked() {
                    self.arduino_report = Some(self.setup_arduino());
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
            _ if !self.rom_ready => self.first_run_ui(ui),
            Some(i) if i < self.devices.len() => self.device_panel(ui, i),
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("No device yet");
                    ui.label("Each device is a blank board with its own flash, console and screen.");
                    ui.add_space(8.0);
                    if ui.button(RichText::new("+ New device").size(18.0).strong()).clicked() {
                        self.open_new_device();
                    }
                });
            }
        });

        self.new_device_window(&ctx);
        self.drop_ask_window(&ctx);
        self.gallery_window(&ctx);
        self.arduino_window(&ctx);
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
