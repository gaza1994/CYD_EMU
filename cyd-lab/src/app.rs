//! The egui user interface.
//!
//! Immediate-mode: every frame we poll each device's backend for console lines,
//! exit notifications and screen frames, then draw. Background threads (process
//! readers, WebSocket link) call the `Waker`, which asks egui to repaint, so the
//! UI is idle when nothing happens.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// Text size and one-time notes.
    pub ui: UiSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Text size on top of the system's display scale (Settings, or Ctrl +/-, Ctrl 0).
    pub text_scale: f32,
    /// The Windows Firewall note after publishing ports was dismissed.
    pub firewall_note_seen: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings { text_scale: 1.0, firewall_note_seen: false }
    }
}

/// Bigger, roomier text than egui's defaults (body 13 pt), and lighter grey labels so they
/// stay readable on the dark theme. Sizes are in points: the system display scale and the
/// Text size setting multiply them.
pub fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, TextStyle};
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(21.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(9.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.interact_size.y = 24.0;
        if s.visuals.dark_mode {
            s.visuals.weak_text_color = Some(Color32::from_gray(165));
            s.visuals.override_text_color = None;
        }
    });
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
    if mdns && n.lan {
        ui.label(RichText::new("Discovery: esp32sim answers mDNS (UDP 5353) on this PC's network for the device, so Home Assistant finds it and name.local resolves. The same firewall rule covers it; a firewall that blocks UDP 5353 for esp32sim stops discovery (adding by IP and port still works).").small().weak());
    }
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

/// The device hotspot driven live over the engine's control channel (`ap_client` /
/// `ap_status`). Newer engines push `ap_status` on every change; older ones only answer,
/// so the panel polls them while it is open.
/// Where an install came from: what "try again" offers when it fails.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum InstallSource {
    #[default]
    Catalog,
    Web,
    File,
}

/// An install's progress and result, shown in the device panel (the "Install firmware…"
/// window closes as soon as Install is clicked).
#[derive(Default)]
struct InstallUi {
    what: String,
    stage: String,
    step: u8,
    since: Option<Instant>,
    finished: Option<Instant>,
    outcome: Option<Result<String, String>>,
    source: InstallSource,
}

impl InstallUi {
    const STEPS: f32 = 5.0;
    fn begin(&mut self, what: impl Into<String>, source: InstallSource) {
        *self = InstallUi { what: what.into(), stage: "starting".into(), step: 0, since: Some(Instant::now()), source, ..Default::default() };
    }
    fn active(&self) -> bool {
        self.since.is_some() && self.outcome.is_none()
    }
    /// A progress line from the download/verify thread.
    fn line(&mut self, l: &str) {
        let step = if l.contains("SHA-256 OK") || l.contains("verified copy") || l.contains("not pinned") {
            3
        } else if l.starts_with("downloading") {
            2
        } else if l.starts_with("fetching") {
            1
        } else {
            self.step
        };
        self.step = self.step.max(step);
        self.stage = l.to_string();
    }
    fn writing(&mut self, powering_off: bool) {
        self.step = 4;
        self.stage = if powering_off { "powering off to write the flash".into() } else { "writing the flash".into() };
    }
    fn done(&mut self, msg: String) {
        self.step = 5;
        self.outcome = Some(Ok(msg));
        self.finished = Some(Instant::now());
    }
    fn fail(&mut self, err: String) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
        self.outcome = Some(Err(err));
        self.finished = Some(Instant::now());
    }
    fn fraction(&self) -> f32 {
        (self.step as f32 + 0.5).min(Self::STEPS) / Self::STEPS
    }
    /// What the panel shows: the running install, a failure until dismissed, a success for a minute.
    fn visible(&self) -> bool {
        match &self.outcome {
            None => self.since.is_some(),
            Some(Err(_)) => true,
            Some(Ok(_)) => self.finished.is_some_and(|t| t.elapsed() < Duration::from_secs(60)),
        }
    }
}

#[derive(Default)]
struct ApUi {
    /// The newest `ap_status`.
    status: Option<crate::hotspot::ApStatus>,
    /// Requests sent that have not been answered yet; an `ap_status` beyond those was pushed.
    pending: u32,
    /// The engine pushes `ap_status` (no polling needed).
    pushes: bool,
    /// The phone was up on the hotspot being joined this run.
    was_up: bool,
    /// After that, the hotspot went away (the firmware left setup mode).
    gone: bool,
    /// The firmware said it is on Wi-Fi after the phone was up on its hotspot.
    station_up: bool,
    /// Set once the device joined its Wi-Fi after leaving setup mode: the panel's note.
    left: Option<String>,
    /// The panel was drawn open this frame (poll only then).
    panel_open: bool,
    next_poll: Option<Instant>,
    /// A scanning phone was made this run (so the panel can list hotspots).
    scanning: bool,
    /// Joining (ssid, page ip); open the page once the phone is up on it.
    joining: Option<(String, String)>,
    open_when_up: bool,
    /// This run's forward to the page, and the guest address it points at.
    bound: Option<(std::net::SocketAddr, String)>,
    /// The Host-rewriting proxy the browser uses (kept across runs, so its URL stays).
    proxy: Option<crate::hotspot::HostProxy>,
    error: Option<String>,
}

impl ApUi {
    /// A new run: the engine's phone and forwards are gone.
    fn new_run(&mut self) {
        self.status = None;
        self.next_poll = None;
        self.left = None;
        self.pending = 0;
        self.was_up = false;
        self.gone = false;
        self.station_up = false;
        self.scanning = false;
        self.bound = None;
        self.error = None;
        if self.joining.is_none() {
            self.open_when_up = false;
        }
        if let Some(p) = &self.proxy {
            p.set_target(None, "");
        }
    }

    fn proxy(&mut self) -> Option<&crate::hotspot::HostProxy> {
        if self.proxy.is_none() {
            self.proxy = crate::hotspot::HostProxy::start().ok();
        }
        self.proxy.as_ref()
    }

    /// Send a control request that the engine answers with an `ap_status`.
    fn ask(&mut self, backend: &mut dyn Backend, msg: String) -> Result<(), String> {
        backend.control(msg).map_err(|e| e.to_string())?;
        self.pending += 1;
        Ok(())
    }

    /// Poll for status only with engines that don't push it.
    fn wants_poll(&self) -> bool {
        !self.pushes
    }

    fn on_status(&mut self, st: crate::hotspot::ApStatus) {
        if self.pending > 0 {
            self.pending -= 1;
        } else {
            self.pushes = true;
        }
        if let Some((ssid, _)) = self.joining.clone() {
            if st.up_on(&ssid) {
                self.was_up = true;
                self.gone = false;
            } else if self.was_up && st.phone.as_ref().is_some_and(|p| p.state != "up" || !st.sees(&ssid)) {
                self.gone = true;
            }
        }
        self.error = st.error.clone();
        if st.error.is_some() {
            self.open_when_up = false;
            return;
        }
        if let Some(addr) = st.bound_addr() {
            let guest = self.joining.as_ref().map(|j| j.1.clone()).unwrap_or_else(|| crate::hotspot::GUEST_AP_IP.to_string());
            if let Some(p) = self.proxy() {
                p.set_target(Some(addr), &guest);
            }
            self.bound = Some((addr, guest));
        }
        if st.phone.is_some() {
            self.scanning = true;
        }
        self.status = Some(st);
    }

    /// The firmware left its hotspot and joined `station` (its Wi-Fi): stop joining and
    /// close the proxy (its listener stops; open page connections already got EOF).
    fn left_setup(&mut self, station: &str) {
        self.left = Some(station.to_string());
        self.joining = None;
        self.open_when_up = false;
        self.was_up = false;
        self.gone = false;
        self.station_up = false;
        self.bound = None;
        if let Some(p) = self.proxy.take() {
            p.set_target(None, "");
        }
    }

    /// The URL to open now, once: joined the network and the forward is in place.
    fn take_open_url(&mut self) -> Option<String> {
        let (ssid, _) = self.joining.as_ref()?;
        if !self.open_when_up || self.bound.is_none() || !self.status.as_ref().is_some_and(|s| s.up_on(ssid)) {
            return None;
        }
        self.open_when_up = false;
        self.proxy.as_ref().map(|p| p.url())
    }
}

struct DeviceView {
    dev: Device,
    backend: Box<dyn Backend>,
    console: VecDeque<(Stream, String)>,
    autoscroll: bool,
    /// BOOT checkbox (latched). The momentary BOOT button ORs into this.
    boot_latched: bool,
    /// GPIO0 level last sent over the live link (BOOT pressed).
    boot_pin_low: bool,
    /// The panel's last frame from the live link. Cleared at power-on (a real panel starts
    /// dark) and kept untouched while the link reconnects.
    screen: Option<TextureHandle>,
    screen_size: (usize, usize),
    /// The live link dropped while running and is reconnecting.
    link_lost: bool,
    /// The pointer's panel pixel over the screen (shown in the strip under it).
    hover_px: Option<(u16, u16)>,
    /// Main-panel zoom: 0 = largest whole number that fits.
    main_zoom: u8,
    /// The pop-out window: position to open at and size to ask for (fixed while open, so
    /// egui only sends the OS a command when they change).
    pop_pos0: Option<egui::Pos2>,
    pop_size: Vec2,
    pop_moved: Option<Instant>,
    link_up: bool,
    link_note: String,
    touch_down: Option<(u16, u16)>,
    /// When the current press started (to enforce a minimum hold).
    touch_since: Option<std::time::Instant>,
    /// A release held back until the press has lasted `MIN_TOUCH_HOLD`.
    touch_release_due: Option<(u16, u16, std::time::Instant)>,
    touch_note: String,
    serial_input: String,
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
    /// Improv Wi-Fi over serial: detection, the last session and its result.
    improv: ImprovUi,
    /// Access point names the firmware announced in this run (its setup hotspot).
    hotspot_ssids: Vec<String>,
    /// "Device hotspot": join this network at the next power-on (engines with --ap-client).
    hotspot: Option<crate::hotspot::Plan>,
    /// Open the hotspot's setup page in the browser once the restarted run is up.
    hotspot_open_at: Option<Instant>,
    /// The "Device hotspot" form: network name, password, page address.
    hotspot_form: (String, String, String),
    /// The live device-hotspot session over the control channel.
    ap: ApUi,
    install: InstallUi,
    /// Live clients on forwarded ports (engines that send `fwd` events).
    fwd: crate::hotspot::FwdClients,
    /// After installing a firmware with a first-setup guide (the Home Assistant entry):
    /// its id, shown as a step card until dismissed.
    /// The setup checklist after installing a catalogue firmware that needs setting up.
    /// The last ESPHome API client the firmware logged as connected.
    api_client: Option<String>,
}

use crate::device::Guide;

/// Room kept under the gallery's list for its footer (checkbox and hint lines).
const GALLERY_FOOTER: f32 = 84.0;

/// Largest whole-number zoom (screen pixels per panel pixel) at which a `view`-sized
/// picture fits in `avail` points; at least 1 (then it scrolls off rather than shrink).
fn fit_zoom(avail: Vec2, view: (usize, usize), ppp: f32) -> u32 {
    let zx = avail.x * ppp / view.0.max(1) as f32;
    let zy = avail.y * ppp / view.1.max(1) as f32;
    (zx.min(zy).floor().max(1.0) as u32).min(8)
}

fn rotation_label(r: u16) -> &'static str {
    match r {
        90 => "turned right",
        180 => "upside down",
        270 => "turned left",
        _ => "upright",
    }
}

/// Fit / 1x / 2x / 3x (0 = fit).
fn zoom_combo(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, zoom: &mut u8) {
    let label = |z: u8| if z == 0 { "Fit".to_string() } else { format!("{z}x") };
    egui::ComboBox::from_id_salt(id).width(56.0).selected_text(label(*zoom)).show_ui(ui, |ui| {
        for z in [0u8, 1, 2, 3, 4] {
            ui.selectable_value(zoom, z, label(z));
        }
    })
    .response
    .on_hover_text("Whole-number zoom, so every panel pixel is the same size. 1x = one screen pixel per panel pixel.");
}

impl DeviceView {
    /// The panel's size: the live frame's, else the profile's.
    fn panel_dims(&self, profile: Option<&Profile>) -> (usize, usize) {
        if self.screen.is_some() {
            return self.screen_size;
        }
        profile.and_then(|p| p.panel_size()).map(|(w, h)| (w as usize, h as usize)).unwrap_or((240, 320))
    }

    /// What the screen shows right now, in words (for the strip and the pop-out title), and
    /// a colour when it is not simply live.
    fn screen_status(&self, powered: bool, running: bool, live_link: bool) -> (String, Option<Color32>) {
        let orange = Color32::from_rgb(240, 170, 60);
        if !powered {
            ("Powered off".into(), None)
        } else if !running {
            ("Powering off…".into(), None)
        } else if !live_link {
            ("No live screen: Live link is off in Settings".into(), Some(orange))
        } else if self.link_up && self.screen.is_some() {
            ("Live".into(), None)
        } else if self.link_up {
            ("Waiting for the first frame".into(), None)
        } else if self.link_lost {
            ("Link lost, reconnecting…".into(), Some(orange))
        } else {
            ("Connecting to the engine…".into(), None)
        }
    }

    /// The emulated panel, pixel for pixel at a whole-number zoom (in physical screen
    /// pixels), and the mouse as a finger. Nothing is ever drawn over it: a real panel shows
    /// only what the firmware draws, so state goes in the strip below (`screen_strip`).
    fn screen_view(&mut self, ui: &mut egui::Ui, panel: (usize, usize), zoom: u32, powered: bool) {
        let (panel_w, panel_h) = panel;
        let rot = self.dev.meta.view_rotation;
        let (vw, vh) = rotated_size(rot, panel_w, panel_h);
        let ppp = ui.ctx().pixels_per_point();
        let size = Vec2::new(vw as f32, vh as f32) * zoom.max(1) as f32 / ppp;
        let (alloc, resp) = ui.allocate_exact_size(size + Vec2::splat(2.0), Sense::click_and_drag());
        // On the physical pixel grid, so each panel pixel is exactly zoom×zoom pixels.
        let min = alloc.min + Vec2::splat(1.0);
        let rect = Rect::from_min_size(egui::pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp), size);
        let painter = ui.painter();
        // Bezel outside the panel, then the panel: dark until the firmware draws.
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::from_gray(90)), egui::StrokeKind::Outside);
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        if let (true, Some(tex)) = (powered, &self.screen) {
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

        // --- touch: mouse on the screen = finger on the panel
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        // Losing focus mid-press (Alt+Tab, another window popping up) never delivers the
        // button-up: let go then, rather than leave the finger on the panel.
        let focused = ui.input(|i| i.viewport().focused != Some(false));
        let now = Instant::now();
        if resp.is_pointer_button_down_on() && focused {
            if let Some((x, y)) = pointer.and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p)) {
                if self.touch_down != Some((x, y)) {
                    self.touch_release_due = None;
                    if self.touch_down.is_none() {
                        self.touch_since = Some(now);
                    }
                    self.touch_down = Some((x, y));
                    self.touch_note = match self.backend.touch(x, y, true) {
                        Ok(()) => format!("touch ({x}, {y})"),
                        Err(e) => format!("touch ({x}, {y}) not delivered: {e}"),
                    };
                }
            }
        } else if let Some((x, y)) = self.touch_down.take() {
            // The engine samples the panel at its own pace, so a press+release inside one
            // frame would be missed: every touch is held >= MIN_TOUCH_HOLD (see poll_backends).
            let since = self.touch_since.take().unwrap_or(now);
            self.touch_release_due = Some((x, y, if focused { since + MIN_TOUCH_HOLD } else { now }));
        } else if resp.clicked() && self.touch_release_due.is_none() {
            // Down and up landed in the same frame: synthesise the press now.
            if let Some((x, y)) = pointer.and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p)) {
                self.touch_note = match self.backend.touch(x, y, true) {
                    Ok(()) => format!("touch ({x}, {y})"),
                    Err(e) => format!("touch ({x}, {y}) not delivered: {e}"),
                };
                self.touch_release_due = Some((x, y, now + MIN_TOUCH_HOLD));
            }
        }
        self.hover_px = resp.hover_pos().and_then(|p| view_to_panel_px(rot, panel_w, panel_h, rect, p));
    }

    /// Release a touch whose minimum hold is over (for every device, each frame).
    fn release_due_touch(&mut self, ctx: &egui::Context) {
        if let Some((x, y, due)) = self.touch_release_due {
            let now = Instant::now();
            if now >= due {
                self.touch_release_due = None;
                self.touch_note = match self.backend.touch(x, y, false) {
                    Ok(()) => format!("release ({x}, {y})"),
                    Err(e) => format!("release ({x}, {y}) not delivered: {e}"),
                };
            } else {
                ctx.request_repaint_after(due - now);
            }
        }
    }

    /// The line under the screen: what it shows (live / off / link lost), zoom, touch, and
    /// the panel pixel under the mouse.
    fn screen_strip(&self, ui: &mut egui::Ui, powered: bool, running: bool, live_link: bool, zoom: u32) {
        let (status, colour) = self.screen_status(powered, running, live_link);
        ui.horizontal_wrapped(|ui| {
            let mut t = RichText::new(&status);
            if let Some(c) = colour {
                t = t.color(c).strong();
            }
            let r = ui.label(t);
            if self.link_lost && powered {
                r.on_hover_text(format!("{}\nThe screen keeps the last frame it got until the link is back.", self.link_note));
            }
            if self.link_lost && powered && self.screen.is_some() {
                ui.label(RichText::new("(last frame kept)").small().weak());
            }
            if zoom > 0 {
                ui.label(RichText::new(format!("· {zoom}x")).small().weak());
            }
            if let Some((x, y)) = self.hover_px {
                ui.label(RichText::new(format!("· panel ({x}, {y})")).small().weak());
            }
        });
        if !self.touch_note.is_empty() && powered {
            ui.label(RichText::new(&self.touch_note).small().weak());
        }
    }
}

const INTERNAL_IP_TIP: &str = "10.0.2.15 is the device's internal emulator address: only the emulator can reach it. Home Assistant and your browser use this PC's address and the published ports instead.";

#[derive(Clone, Copy)]
enum DevAction {
    Rename,
    Duplicate,
    Delete,
}

/// Rename / Duplicate / Delete (device header and the device list's right-click menu).
fn device_menu(ui: &mut egui::Ui) -> Option<DevAction> {
    let mut a = None;
    if ui.button("Rename…").clicked() {
        a = Some(DevAction::Rename);
    }
    if ui.button("Duplicate").clicked() {
        a = Some(DevAction::Duplicate);
    }
    ui.separator();
    if ui.button(RichText::new("Delete…").color(Color32::from_rgb(255, 140, 130))).clicked() {
        a = Some(DevAction::Delete);
    }
    if a.is_some() {
        ui.close();
    }
    a
}

/// "Add to Home Assistant": the host and port to type into the ESPHome integration, big,
/// each with a Copy button.
fn ha_box(ui: &mut egui::Ui, api_port: Option<u16>) {
    let host = cached_lan_ip().map(|ip| ip.to_string());
    egui::Frame::new().fill(Color32::from_rgb(16, 28, 40)).stroke(Stroke::new(1.0, Color32::from_rgb(70, 110, 150))).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
        ui.label(RichText::new("In Home Assistant: Settings > Devices & services > Add integration > ESPHome, then enter:").small());
        egui::Grid::new("ha-box").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: Option<String>| {
                ui.label(k);
                match v {
                    Some(v) => {
                        ui.label(RichText::new(&v).monospace().size(20.0).strong().color(Color32::WHITE));
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(v);
                        }
                    }
                    None => {
                        ui.label(RichText::new("—").size(20.0));
                        ui.label("");
                    }
                }
                ui.end_row();
            };
            row(ui, "Host", host.clone());
            row(ui, "Port", api_port.map(|p| p.to_string()));
        });
        if host.is_none() {
            ui.label(RichText::new("No network address found for this PC: is it connected to your network?").small().color(Color32::from_rgb(240, 170, 60)));
        }
        if api_port.is_none() {
            ui.label(RichText::new("Publish the ESPHome ports first (above).").small().weak());
        }
        ui.label(RichText::new("The device itself shows 10.0.2.15: that is its internal emulator address, not reachable from Home Assistant.").small().weak()).on_hover_text(INTERNAL_IP_TIP);
    });
}

/// Improv Wi-Fi state for one device (see `crate::improv`).
#[derive(Default)]
struct ImprovUi {
    rx: Option<std::sync::mpsc::Receiver<crate::improv::Event>>,
    /// "probe" or "provision" while a session runs.
    busy: Option<&'static str>,
    state: Option<crate::improv::State>,
    error: Option<u8>,
    /// Firmware name, version, chip, device name.
    info: Vec<String>,
    url: Option<String>,
    outcome: Option<Result<String, String>>,
    /// Passive clues from the firmware's own log (nothing is ever sent unasked): it has
    /// Improv over serial / it is waiting for a Wi-Fi network.
    capable: bool,
    waiting_log: bool,
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
            dev,
            backend: Box::new(ProcessBackend::new(waker)),
            console: VecDeque::new(),
            autoscroll: true,
            boot_latched: false,
            boot_pin_low: false,
            screen: None,
            screen_size: (0, 0),
            link_lost: false,
            hover_px: None,
            main_zoom: 0,
            pop_pos0: None,
            pop_size: Vec2::ZERO,
            pop_moved: None,
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
            improv: ImprovUi::default(),
            hotspot_ssids: Vec::new(),
            hotspot: None,
            hotspot_open_at: None,
            hotspot_form: (String::new(), String::new(), crate::hotspot::GUEST_AP_IP.to_string()),
            ap: ApUi::default(),
            install: InstallUi::default(),
            fwd: Default::default(),
            api_client: None,
        }
    }

    /// Start downloading a web installer's firmware on a thread.
    fn start_web_install(&mut self, waker: Waker) {
        let src = self.web_manifest.trim().to_string();
        let erase = self.web_erase;
        let short = src.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(&src).to_string();
        self.install.begin(format!("web install ({short})"), InstallSource::Web);
        let size = match self.dev.flash_size() {
            Ok(s) => s,
            Err(e) => {
                self.install.fail(format!("{e:#}"));
                return self.lab(format!("web install: {e:#}"));
            }
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
        self.install.begin(format!("{} {}", e.name, e.version).trim().to_string(), InstallSource::Catalog);
        let size = match self.dev.flash_size() {
            Ok(s) => s,
            Err(err) => {
                self.install.fail(format!("{err:#}"));
                return self.lab(format!("catalogue: {err:#}"));
            }
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
            self.lab(format!("{}: this firmware offers Improv Wi-Fi setup over serial: once it has booted, use \"Send Wi-Fi details\" (Wi-Fi setup row).", self.job_label));
        }
        self.queue_flash(p.erase, p.parts, what.trim().to_string(), true);
    }

    /// Start an Improv session on the device's serial port.
    fn start_improv(&mut self, job: crate::improv::Job) {
        let kind = match job {
            crate::improv::Job::Probe => "probe",
            crate::improv::Job::Provision { .. } => "provision",
        };
        match self.backend.improv(job) {
            Ok(rx) => {
                self.improv.rx = Some(rx);
                self.improv.busy = Some(kind);
                if kind == "provision" {
                    self.improv.outcome = None;
                    self.improv.error = None;
                }
            }
            Err(e) if kind == "provision" => self.improv.outcome = Some(Err(e)),
            Err(_) => {}
        }
    }

    fn poll_improv(&mut self) {
        use crate::improv::Event;
        if let Some(rx) = &self.improv.rx {
            let evs: Vec<Event> = rx.try_iter().collect();
            for ev in evs {
                match ev {
                    Event::State(s) => self.improv.state = Some(s),
                    Event::Error(e) => self.improv.error = Some(e),
                    Event::Info(v) => self.improv.info = v,
                    Event::Url(u) => self.improv.url = Some(u),
                    Event::Done(r) => {
                        let kind = self.improv.busy.take();
                        self.improv.rx = None;
                        match (kind, r) {
                            (Some("provision"), Ok(m)) => {
                                self.lab(format!("Wi-Fi setup (Improv): {m}"));
                                self.improv.outcome = Some(Ok(m));
                            }
                            (Some("provision"), Err(e)) => {
                                self.lab(format!("Wi-Fi setup (Improv) failed: {e}"));
                                self.improv.outcome = Some(Err(e));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn queue_flash(&mut self, erase: bool, parts: Vec<(u64, Vec<u8>, String)>, what: String, power_on_after: bool) {
        let on = matches!(self.backend.state(), RunState::Running(_) | RunState::Stopping);
        if !self.install.active() {
            let label = if parts.is_empty() && erase { "Erase flash".to_string() } else { what.clone() };
            self.install.begin(label, InstallSource::File);
        }
        self.install.writing(on);
        if on {
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
                self.install.fail(format!("erasing the flash: {e:#}"));
                return false;
            }
        }
        let mut bytes = 0;
        for (off, data, src) in &p.parts {
            if let Err(e) = store.import_firmware(&self.dev, *off, data) {
                self.lab(format!("{}: writing {src} at 0x{off:x}: {e:#}", p.what));
                self.install.fail(format!("writing {src} at 0x{off:x}: {e:#}"));
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
        let msg = match (p.erase, p.parts.is_empty(), p.power_on_after) {
            (true, true, _) => "Flash erased".to_string(),
            (_, _, true) => format!("Installed {} ({bytes} bytes written); powering on", self.install.what),
            (_, _, false) => format!("Installed {} ({bytes} bytes written)", self.install.what),
        };
        self.install.done(msg);
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
            if let Some(c) = crate::network::api_client_connected(&line) {
                self.api_client = Some(c);
            }
            if let Some(st) = crate::improv::sniff_console(&line) {
                self.improv.state = Some(st);
                self.improv.capable = true;
            }
            match crate::improv::sniff_setup(&line) {
                Some(crate::improv::Clue::Improv) => self.improv.capable = true,
                Some(crate::improv::Clue::Waiting) => self.improv.waiting_log = true,
                None => {}
            }
            if let Some(ssid) = crate::hotspot::sniff_ssid(&line) {
                crate::hotspot::remember(&mut self.hotspot_ssids, ssid);
            }
            let before = self.wifi_watch.banner.clone();
            if self.wifi_watch.feed(&line, &self.wifi_ap) && self.ap.was_up {
                // Joined its Wi-Fi after its hotspot page was used (see the hotspot panel).
                self.ap.station_up = true;
            }
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
    /// Rename dialog: device id and the name being typed.
    rename: Option<(String, String)>,
    /// The zoom factor last applied/saved (Ctrl +/-/0 change egui's directly).
    text_scale: f32,
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
        apply_style(&cc.egui_ctx);
        let text_scale = if (0.5..=3.0).contains(&settings.ui.text_scale) { settings.ui.text_scale } else { 1.0 };
        cc.egui_ctx.set_zoom_factor(text_scale);
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
            rename: None,
            text_scale,
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
        app.add_engine_boards();
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
    /// Offer every board the engine has built in (`--list-boards`) alongside the profiles.
    fn add_engine_boards(&mut self) {
        if let Some(exe) = resolve_exe(&self.effective_engine().engine_exe) {
            self.profiles.add_engine_boards(&crate::engine::list_boards(&exe));
        }
    }

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
        if let Some(plan) = self.devices[i].hotspot.as_ref().filter(|_| features.ap_client) {
            spec.args.extend(plan.args());
            spec.notes.push(format!("joining the firmware's hotspot \"{}\"; its page at {}", plan.ssid, plan.url()));
        }
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
        let hotspot_ok = self.engine_features().ap_client;
        let d = &mut self.devices[i];
        d.improv = ImprovUi::default();
        d.hotspot_ssids.clear();
        d.ap.new_run();
        d.fwd = Default::default();
        // No automatic Improv probe: a real board on a PC gets no bytes unless a tool or the
        // user sends them. "Waiting for setup" is read from its log and its hotspot instead.
        d.hotspot_open_at = (r.is_ok() && hotspot_ok && d.hotspot.is_some()).then(|| Instant::now() + Duration::from_secs(10));
        d.wifi_ap = d.dev.meta.network.effective(&self.settings.wifi);
        d.wifi_watch = Default::default();
        d.link_up = false;
        d.link_lost = false;
        // A real panel starts dark: never show the previous run's last frame.
        d.screen = None;
        d.api_client = None;
        d.touch_down = None;
        d.touch_since = None;
        d.touch_release_due = None;
        d.touch_note.clear();
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
        d.link_lost = false;
        d.screen = None; // power-cycled: the panel lost power too
        d.touch_down = None;
        d.touch_release_due = None;
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
                        WebMsg::Line(l) => {
                            d.install.line(&l);
                            d.lab(format!("{}: {l}", d.job_label));
                        }
                        WebMsg::Done(r) => {
                            d.web_job = None;
                            match r {
                                Ok(p) => d.finish_web_install(p),
                                Err(e) => {
                                    d.lab(format!("{} failed: {e}", d.job_label));
                                    d.install.fail(e);
                                }
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
                        d.link_lost = false;
                        d.touch_down = None;
                        d.touch_since = None;
                        d.touch_release_due = None;
                    }
                    EngineEvent::Control(text) => {
                        if let Some(st) = crate::hotspot::ApStatus::parse(&text) {
                            d.ap.on_status(st);
                        } else if let Some(ev) = crate::hotspot::FwdEvent::parse(&text) {
                            d.fwd.apply(&ev);
                        }
                    }
                    EngineEvent::LinkUp => {
                        d.link_up = true;
                        // A new session: its hello re-sends the open clients and the status.
                        d.fwd = Default::default();
                        d.ap.pending = 0;
                        d.link_lost = false;
                        d.link_note = "live link connected".into();
                    }
                    EngineEvent::LinkDown(why) => {
                        d.link_up = false;
                        // Never leave the finger down: the release is queued and sent as
                        // soon as the link is back (the link also re-sends it itself).
                        let held = d.touch_down.take().or(d.touch_release_due.take().map(|(x, y, _)| (x, y)));
                        if let Some((x, y)) = held {
                            let _ = d.backend.touch(x, y, false);
                            d.touch_since = None;
                            d.touch_note = format!("released ({x}, {y}): link dropped");
                        }
                        if matches!(d.backend.state(), RunState::Running(_)) {
                            d.link_lost = true;
                            d.link_note = why.clone();
                            d.lab(format!("live link: {why}"));
                        } else {
                            // The engine closing its socket on the way out is expected.
                            d.link_note = "live link closed (powered off)".into();
                        }
                    }
                }
            }
            d.poll_improv();
            if d.hotspot_open_at.is_some_and(|t| Instant::now() >= t) {
                d.hotspot_open_at = None;
                // The command-line plan's forward, behind the Host-rewriting proxy.
                if let Some(plan) = d.hotspot.clone() {
                    let target = std::net::SocketAddr::from(([127, 0, 0, 1], plan.host_port));
                    let url = d.ap.proxy().map(|p| {
                        p.set_target(Some(target), &plan.guest_ip);
                        p.url()
                    });
                    let url = url.unwrap_or_else(|| plan.url());
                    d.lab(format!("device hotspot: opening {url}"));
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
            // Live hotspot: newer engines push ap_status on change; older ones are polled
            // while the panel is open (faster while joining).
            if d.ap.panel_open && d.link_up && d.ap.wants_poll() {
                let now = Instant::now();
                if d.ap.next_poll.is_none_or(|t| now >= t) {
                    let _ = d.ap.ask(d.backend.as_mut(), crate::hotspot::STATUS_MESSAGE.to_string());
                    d.ap.next_poll = Some(now + if d.ap.open_when_up { Duration::from_millis(700) } else { Duration::from_secs(2) });
                }
                ctx.request_repaint_after(Duration::from_millis(500));
            }
            // The firmware saved the Wi-Fi from its page, left setup mode and joined.
            if d.ap.gone && d.ap.station_up && d.ap.joining.is_some() {
                let station = d.wifi_ap.ssid.clone();
                d.ap.left_setup(&station);
                d.lab(format!("device hotspot: the device left setup mode and joined \"{station}\"; closed the setup-page proxy"));
            }
            if let Some(url) = d.ap.take_open_url() {
                d.lab(format!("device hotspot: joined; opening {url} (the page at {} on the device's hotspot)", d.ap.bound.as_ref().map(|b| b.1.clone()).unwrap_or_default()));
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
            if d.improv.busy.is_some() || d.hotspot_open_at.is_some() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            d.release_due_touch(ctx);
            if let Some(f) = d.backend.take_frame() {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                match &mut d.screen {
                    Some(t) if d.screen_size == (f.width, f.height) => t.set(img, TextureOptions::NEAREST),
                    _ => d.screen = Some(ctx.load_texture(format!("screen-{}", d.dev.id), img, TextureOptions::NEAREST)),
                }
                d.screen_size = (f.width, f.height);
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
                egui::ComboBox::from_id_salt("profile").height(600.0).selected_text(self.profiles.get(&dlg.profile).map(|p| p.name.as_str()).unwrap_or("?")).show_ui(ui, |ui| {
                    // Yellow boards first (smallest screen first), then the rest.
                    let mut list: Vec<&Profile> = self.profiles.profiles.values().collect();
                    list.sort_by_key(|p| (!p.engine.board.starts_with("cyd-"), p.id.clone()));
                    for p in list {
                        ui.selectable_value(&mut dlg.profile, p.id.clone(), &p.name).on_hover_text(&p.description);
                    }
                });
                ui.end_row();
            });
            if let Some(p) = self.profiles.get(&dlg.profile) {
                ui.add_space(4.0);
                ui.label(RichText::new(&p.description).small());
                let board = p.engine.board_config.clone().unwrap_or_else(|| p.engine.board.clone());
                let screen = p.panel_size().map(|(w, h)| format!(" · {w}x{h} screen")).unwrap_or_default();
                ui.label(RichText::new(format!("{} · {} MB flash{screen} · engine board `{board}`", p.chip, p.flash.size_mb)).small().weak());
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
        // (heading before this entry, entry), in gallery order.
        let grouped: Vec<(Option<&'static str>, crate::catalog::Entry)> = cat_profile
            .as_ref()
            .map(|p| self.catalog.grouped(&p.id, show_broken).into_iter().flat_map(|(h, v)| v.into_iter().enumerate().map(move |(k, e)| ((k == 0).then_some(h), e.for_profile(&p.id)))).collect())
            .unwrap_or_default();
        let headings: Vec<Option<&'static str>> = grouped.iter().map(|(h, _)| *h).collect();
        let entries: Vec<crate::catalog::Entry> = grouped.into_iter().map(|(_, e)| e).collect();
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
        // Tall by default: most of the window, so the list shows as many entries as fit.
        let tall = (ctx.content_rect().height() - 60.0).max(360.0);
        egui::Window::new(format!("Install firmware on {dev_name}"))
            .id(egui::Id::unique(("gallery", i)))
            .collapsible(false)
            .resizable(true)
            .default_size([600.0, tall])
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
                // The list takes whatever height the window has, less the footer below it.
                let list_h = (ui.available_height() - GALLERY_FOOTER).max(160.0);
                egui::ScrollArea::vertical().max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    for ((e, thumb), heading) in entries.iter().zip(&thumbs).zip(&headings) {
                        if let Some(h) = heading {
                            ui.add_space(4.0);
                            ui.label(RichText::new(*h).heading().strong());
                            ui.add_space(2.0);
                        }
                        ui.horizontal(|ui| {
                            let size = Vec2::new(60.0, 80.0);
                            match thumb {
                                Some(t) => {
                                    let sz = t.size_vec2();
                                    let scale = (size.x / sz.x).min(size.y / sz.y);
                                    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
                                    egui::Image::new((t.id(), sz * scale)).paint_at(ui, Rect::from_center_size(r.center(), sz * scale));
                                    resp.on_hover_ui(|ui| {
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
                                    if e.is_custom() {
                                        ui.label(RichText::new(" custom build ").small().color(Color32::BLACK).background_color(Color32::from_rgb(240, 150, 60)))
                                            .on_hover_text("Not the firmware project's official release: built by cyd-lab (or a third party). A real board from the shop would not have it; prefer the official entries.");
                                    }
                                    for need in e.needs() {
                                        ui.label(RichText::new(format!(" needs {need} ")).small().color(Color32::BLACK).background_color(Color32::from_rgb(230, 200, 120)))
                                            .on_hover_text("The emulator doesn't model this; those features won't work here.");
                                    }
                                });
                                ui.label(&e.description);
                                if e.borrowed_for(&cp.id) {
                                    ui.label(RichText::new("Built for the micro-USB 2432S028R. It runs on this board too, but its picture may be turned, mirrored or cut off on this panel.").small().color(Color32::from_rgb(220, 170, 80)));
                                }
                                if let Some(f) = e.first_setup.as_ref().map(|f| f.summary()).filter(|t| !t.is_empty()) {
                                    ui.label(RichText::new(format!("First Wi-Fi setup: {f}")).small());
                                }
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
                                        if let Some(text) = crate::catalog::Entry::template_text(&e.config) {
                                            if ui.button("Copy config").on_hover_text("Copy the ESPHome config this image was built from, to paste as a new device in Home Assistant's ESPHome add-on").clicked() {
                                                ui.ctx().copy_text(text.to_string());
                                                saved = Some(format!("{} config copied to the clipboard", e.name));
                                            }
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
            let others: Vec<_> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, o)| o.dev.meta.network.clone()).collect();
            let d = &mut self.devices[i];
            let improv = e.first_setup.as_ref().is_some_and(|f| f.improv);
            d.dev.meta.guide = (e.home_assistant || improv || !e.steps.is_empty() || e.view_rotation.is_some()).then(|| Guide {
                name: e.name.clone(),
                improv,
                home_assistant: e.home_assistant,
                config: e.config.clone(),
                rotation: e.view_rotation,
                notes: e.steps.clone(),
                ..Default::default()
            });
            // A fresh checklist must not inherit the old firmware's Wi-Fi / Home Assistant
            // state: it would be ticked again before the new firmware even runs.
            d.wifi_watch = Default::default();
            d.improv = ImprovUi::default();
            d.api_client = None;
            d.fwd = Default::default();
            let _ = self.store.update_meta(&d.dev);
            if e.home_assistant {
                // Home Assistant has to reach the device: publish the ESPHome API and OTA
                // ports on the network (free PC ports picked), so "Add to Home Assistant" works.
                let n = &mut d.dev.meta.network;
                let added = n.add_preset_free(&crate::network::PRESETS[0], &others, &|p| bind_is_free("0.0.0.0", p));
                if added > 0 || !n.lan || !n.wifi {
                    n.wifi = true;
                    n.lan = true;
                    let _ = self.store.update_meta(&d.dev);
                    let ports: Vec<String> = d.dev.meta.network.forwards.iter().map(|f| format!("{} {}", f.label, f.host)).collect();
                    d.lab(format!("published for Home Assistant on the network: {}", ports.join(", ")));
                }
            }
            // The view's rotation is how the board is held: only suggested (setup checklist).
            if let Some(f) = &e.first_setup {
                if !f.hotspot.is_empty() {
                    d.hotspot_form = (String::new(), f.password.clone(), f.page_ip().unwrap_or_else(|| crate::hotspot::GUEST_AP_IP.to_string()));
                }
            }
            d.start_catalog_install(e, cache, w);
            // Progress and the result show in the device panel; the window has done its job.
            d.gallery_open = false;
            return;
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
            ui.heading("Appearance");
            ui.horizontal(|ui| {
                ui.label("Text size");
                let r = ui.add(egui::Slider::new(&mut draft.ui.text_scale, 0.8..=1.8).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                // Applied when let go: resizing everything mid-drag moves the slider away.
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    ui.ctx().set_zoom_factor(draft.ui.text_scale);
                }
                if ui.button("Reset").clicked() {
                    ui.ctx().set_zoom_factor(1.0);
                }
                ui.label(RichText::new("also Ctrl + / Ctrl − / Ctrl 0 · on top of the system's display scale").small().weak());
            });
            ui.separator();
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
            self.add_engine_boards();
        }
        if save {
            let mut draft = self.settings_draft.take().unwrap();
            match draft.save(&self.paths.settings) {
                Ok(()) => self.status = format!("Settings saved to {}", self.paths.settings.display()),
                Err(e) => self.status = format!("Saving settings failed: {e:#}"),
            }
            if draft.catalog != self.settings.catalog {
                let (c, note) = crate::catalog::load(&self.paths.root, &draft.catalog);
                self.catalog = c;
                self.catalog_note = note.unwrap_or_default();
            }
            draft.ui = self.settings.ui.clone();
            self.settings = draft;
            self.profiles = ProfileLibrary::load(Some(&self.paths.profiles));
            self.add_engine_boards();
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
        let running = !matches!(self.devices[i].backend.state(), RunState::Stopped);
        egui::Modal::new(egui::Id::unique("confirm-delete")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(format!("Delete \"{}\"?", self.devices[i].dev.meta.name));
            ui.add_space(4.0);
            ui.label("This removes its flash, settings and folder. It cannot be undone.");
            ui.label(RichText::new(self.devices[i].dev.dir.display().to_string()).monospace().weak());
            if running {
                ui.label(RichText::new("It is running: it is powered off first.").color(Color32::from_rgb(240, 170, 60)));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let del = egui::Button::new(RichText::new("Delete device").strong().color(Color32::WHITE)).fill(Color32::from_rgb(170, 40, 40));
                if ui.add(del).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                let mut d = self.devices.remove(i);
                d.web_job = None;
                d.pending_flash = None;
                match power_off_and_delete(&self.store, d.backend.as_mut(), &mut d.ap.proxy, &d.dev) {
                    Ok(()) => {
                        self.status = format!("Deleted {}", d.dev.meta.name);
                        self.selected = if self.devices.is_empty() { None } else { Some(i.min(self.devices.len() - 1)) };
                    }
                    Err(e) => {
                        // Keep it in the list (powered off) so the user sees what is left.
                        self.status = format!("Delete failed (the device is powered off; close anything using its folder and try again): {e:#}");
                        self.devices.insert(i, d);
                        self.selected = Some(i);
                    }
                }
                self.confirm_delete = None;
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    fn rename_modal(&mut self, ctx: &egui::Context) {
        let Some((id, mut buf)) = self.rename.take() else { return };
        let Some(i) = self.devices.iter().position(|d| d.dev.id == id) else { return };
        let mut decision = None;
        egui::Modal::new(egui::Id::unique("rename-device")).show(ctx, |ui| {
            ui.heading("Rename device");
            let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(280.0));
            r.request_focus();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.label(RichText::new("Only the name changes: its folder, flash and ports stay.").small().weak());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!buf.trim().is_empty(), egui::Button::new("Rename")).clicked() || (enter && !buf.trim().is_empty()) {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                let d = &mut self.devices[i];
                if let Err(e) = self.store.rename(&mut d.dev, buf.trim()) {
                    self.status = format!("Rename failed: {e:#}");
                }
            }
            Some(false) => {}
            None => self.rename = Some((id, buf)),
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
        let mut action = None;
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
                resp.context_menu(|ui| {
                    if let Some(a) = device_menu(ui) {
                        action = Some((i, a));
                    }
                });
            }
        });
        if let Some((i, a)) = action {
            self.device_action(i, a);
        }
    }

    fn device_action(&mut self, i: usize, a: DevAction) {
        match a {
            DevAction::Rename => self.rename = Some((self.devices[i].dev.id.clone(), self.devices[i].dev.meta.name.clone())),
            DevAction::Delete => self.confirm_delete = Some(self.devices[i].dev.id.clone()),
            DevAction::Duplicate => {
                let src = self.devices[i].dev.clone();
                let running = !matches!(self.devices[i].backend.state(), RunState::Stopped);
                let name = format!("{} copy", src.meta.name);
                match self.store.duplicate(&src, &name) {
                    Ok(dev) => {
                        let mut view = DeviceView::new(dev, self.waker.clone());
                        view.lab(format!("duplicated from \"{}\" (flash, settings; new MAC and ports){}", src.meta.name, if running { ": the flash as last saved, since that device is running" } else { "" }));
                        self.status = format!("Duplicated {} as {}", src.meta.name, view.dev.meta.name);
                        self.devices.push(view);
                        self.selected = Some(self.devices.len() - 1);
                    }
                    Err(e) => self.status = format!("Duplicate failed: {e:#}"),
                }
            }
        }
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
        let mut action = None;
        ui.horizontal(|ui| {
            let d = &mut self.devices[i];
            let (color, s) = status_of(state, d.backend.chip_status());
            let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 7.0, color);
            ui.heading(&d.dev.meta.name);
            ui.label(RichText::new(s).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let del = egui::Button::new(RichText::new("Delete…").color(Color32::from_rgb(255, 140, 130)));
                if ui.add(del).on_hover_text("Delete this device: its flash, settings and folder (asks first)").clicked() {
                    action = Some(DevAction::Delete);
                }
                if ui.button("Duplicate").on_hover_text("A copy with the same flash and settings, a new MAC address and its own ports").clicked() {
                    action = Some(DevAction::Duplicate);
                }
                if ui.button("Rename…").clicked() {
                    action = Some(DevAction::Rename);
                }
            });
        });
        if let Some(a) = action {
            self.device_action(i, a);
            return;
        }
        let d = &self.devices[i];
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| format!("unknown profile {}", d.dev.meta.profile))).strong());
            ui.label("·");
            ui.label(format!("id {}", d.dev.id));
            ui.label("·");
            ui.label(format!("MAC {}", d.dev.meta.mac.as_deref().unwrap_or("-")));
        });
        let caps = d.backend.capabilities();
        let live_link = self.settings.engine.live_link;
        // What this engine gives the device (only what it has: nothing about what it lacks).
        ui.horizontal_wrapped(|ui| {
            let chip = |ui: &mut egui::Ui, on: bool, text: &str, tip: &str| {
                if on {
                    let c = Color32::from_rgb(110, 200, 130);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, c);
                    ui.label(RichText::new(text).small().color(c)).on_hover_text(tip);
                }
            };
            chip(ui, caps.live_screen || (!alive && live_link), "live screen", "TFT frames over the engine's --web WebSocket");
            chip(ui, caps.touch, "touch", "Mouse clicks on the screen are sent as panel touches");
            if alive {
                chip(ui, caps.flash_writeback, "flash saved", "This run uses --flash-file: firmware writes are saved to flash.bin when you power off");
                chip(ui, caps.serial_tcp.is_some(), "serial bridge", "UART0 served as RFC 2217 over TCP (--serial-tcp)");
            } else {
                chip(ui, features.flash_file, "flash saved", "Firmware writes persist across power cycles (--flash-file)");
                chip(ui, features.serial_tcp, "serial bridge", "UART0 can be served as RFC 2217 (DTR/RTS auto-reset) for esptool and other tools");
            }
        });
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
            ui.label(RichText::new("or drop a .bin on the window").weak());
        });
        let mut reopen = false;
        let mut retry_web = false;
        if self.devices[i].install.visible() {
            let st = &mut self.devices[i].install;
            let mut dismiss = false;
            ui.horizontal_wrapped(|ui| match &st.outcome {
                None => {
                    ui.spinner();
                    ui.label(RichText::new(format!("Installing {}", st.what)).strong());
                    ui.add(egui::ProgressBar::new(st.fraction()).desired_width(140.0).desired_height(10.0));
                    let secs = st.since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                    ui.label(RichText::new(format!("{}  ·  {secs} s", st.stage)).small().weak());
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                Some(Ok(m)) => {
                    ui.label(RichText::new(format!("✔ {m}")).color(Color32::from_rgb(120, 220, 140)));
                    if ui.small_button("×").on_hover_text("Hide").clicked() {
                        dismiss = true;
                    }
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("✖ {} failed: {e}", st.what)).color(Color32::from_rgb(240, 110, 100)));
                    match st.source {
                        InstallSource::Catalog => {
                            if ui.small_button("Reopen Install firmware…").clicked() {
                                reopen = true;
                                dismiss = true;
                            }
                        }
                        InstallSource::Web => {
                            if ui.small_button("Try again").on_hover_text("Install from the same manifest again (edit it under \"More: install from a web flasher\")").clicked() {
                                retry_web = true;
                            }
                            if ui.small_button("Install firmware…").clicked() {
                                reopen = true;
                                dismiss = true;
                            }
                        }
                        InstallSource::File => {
                            ui.label(RichText::new("pick the file again with Flash a .bin…, or").small().weak());
                            if ui.small_button("Install firmware…").clicked() {
                                reopen = true;
                                dismiss = true;
                            }
                        }
                    }
                    if ui.small_button("×").on_hover_text("Hide").clicked() {
                        dismiss = true;
                    }
                }
            });
            if dismiss {
                *st = InstallUi::default();
            }
        }
        if reopen {
            self.devices[i].gallery_open = true;
        }
        if retry_web && !busy {
            let w = self.waker.clone();
            self.devices[i].start_web_install(w);
        }
        egui::CollapsingHeader::new("More: install from a web flasher, write at an offset").id_salt(("more", i)).show(ui, |ui| {
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
        self.serial_bridge_ui(ui, i, alive, &features);
        self.network_ui(ui, i, alive, &features);
        self.wifi_setup_ui(ui, i, alive, &features);
        ui.separator();

        // --- screen | setup checklist + console. Helpers never go on the screen itself.
        let avail = ui.available_size();
        ui.horizontal_top(|ui| {
            ui.set_min_height(avail.y);
            ui.vertical(|ui| self.screen_ui(ui, i, profile.as_ref()));
            ui.separator();
            ui.vertical(|ui| {
                self.guide_ui(ui, i, alive);
                self.console_ui(ui, i);
            });
        });
    }

    /// The setup checklist after a catalogue install: compact, dismissable, and each step
    /// ticks itself from what the device actually did.
    fn guide_ui(&mut self, ui: &mut egui::Ui, i: usize, alive: bool) {
        let Some(mut g) = self.devices[i].dev.meta.guide.clone() else {
            return;
        };
        let before = g.clone();
        let firewall_seen = self.settings.ui.firewall_note_seen;
        let mut dismiss_firewall = false;
        let mut publish = false;
        let mut send_wifi = false;
        let mut rotate = None;
        let wifi_on = self.devices[i].dev.meta.network.wifi;
        let d = &mut self.devices[i];
        let n = &d.dev.meta.network;
        let api = n.forwards.iter().find(|f| f.guest == 6053).map(|f| f.host);
        let ota = n.forwards.iter().find(|f| f.label.contains("OTA")).map(|f| f.host);
        let published = api.is_some() && n.wifi && n.lan;
        let firmware_done = !d.flash_blank && d.pending_flash.is_none() && d.web_job.is_none();
        let installing = d.pending_flash.is_some() || d.web_job.is_some();
        // Remembered in device.toml, so a finished step stays ticked after a restart.
        g.wifi_connected |= d.wifi_watch.connected;
        g.wifi_sent |= matches!(d.improv.outcome, Some(Ok(_)));
        let wifi_connected = g.wifi_connected;
        let wifi_done = g.wifi_sent || wifi_connected;
        let ha_client = d.api_client.clone();
        g.ha_seen |= ha_client.as_deref().is_some_and(|c| c.contains("Home Assistant"));
        // The engine's port events: a client the firmware accepted on the published API port.
        let (api_clients, api_peers) = api.map(|p| d.fwd.on_host_port(p)).map(|(n, p)| (n, p.to_vec())).unwrap_or_default();
        g.ha_seen |= alive && api_clients > 0;
        let ha_done = g.ha_manual || g.ha_seen;
        let mut close = false;
        let green = Color32::from_rgb(110, 200, 130);
        let tick = |ui: &mut egui::Ui, done: bool, text: &str| {
            let (sym, c) = if done { ("✔", green) } else { ("○", Color32::from_gray(170)) };
            ui.label(RichText::new(sym).color(c).strong());
            ui.label(if done { RichText::new(text).color(Color32::from_gray(200)) } else { RichText::new(text).strong() });
        };
        egui::Frame::new().fill(Color32::from_rgb(24, 40, 56)).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Set up {}", g.name)).strong().color(Color32::WHITE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = ui.small_button("×").on_hover_text("Hide this checklist").clicked();
                });
            });
            ui.horizontal(|ui| {
                tick(ui, firmware_done, "Firmware installed");
                if installing {
                    ui.spinner();
                }
            });
            if g.improv {
                ui.horizontal_wrapped(|ui| {
                    tick(ui, wifi_done, if wifi_connected { "Wi-Fi connected" } else { "Wi-Fi sent" });
                    if !wifi_done {
                        let can = alive && wifi_on && d.improv.busy.is_none() && d.backend.capabilities().serial_tcp.is_some();
                        if ui.add_enabled(can, egui::Button::new("Send Wi-Fi details")).on_disabled_hover_text("Power the device on first; Wi-Fi must be on under Network").clicked() {
                            send_wifi = true;
                        }
                        if d.improv.busy == Some("provision") {
                            ui.spinner();
                        }
                    }
                });
            }
            if g.home_assistant {
                ui.horizontal_wrapped(|ui| {
                    tick(ui, published, "Ports published");
                    match (published, api) {
                        (true, Some(p)) => {
                            ui.label(RichText::new(format!("API {p}{}", ota.map(|o| format!(", OTA {o}")).unwrap_or_default())).small().weak());
                        }
                        _ => {
                            if ui.button("Publish ESPHome ports").on_hover_text("ESPHome API (6053) and OTA (3232) on this PC's network, on free ports").clicked() {
                                publish = true;
                            }
                        }
                    }
                });
                if published && cfg!(windows) && !firewall_seen {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Windows Firewall asks about esp32sim the first time: allow it on Private networks so Home Assistant can connect and discover it (TCP for the ports, UDP 5353 for mDNS).").small().color(Color32::from_rgb(240, 200, 120)));
                        if ui.small_button("Got it").clicked() {
                            dismiss_firewall = true;
                        }
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    tick(ui, ha_done, if g.ha_seen { "Home Assistant connected" } else { "Add to Home Assistant" });
                    if let Some(c) = &ha_client {
                        ui.label(RichText::new(format!("{c} connected")).small().color(green));
                    } else if api_clients > 0 {
                        ui.label(RichText::new(format!("API client{} connected", if api_clients == 1 { "" } else { "s" })).small().color(green)).on_hover_text(api_peers.join("\n"));
                    }
                });
                if !ha_done {
                    ha_box(ui, api);
                    ui.checkbox(&mut g.ha_manual, "I've added it");
                }
            }
            if let Some(text) = (!g.config.is_empty()).then(|| crate::catalog::Entry::template_text(&g.config)).flatten() {
                ui.horizontal_wrapped(|ui| {
                    tick(ui, g.config_copied, "Customise");
                    if ui.button("Copy config").on_hover_text("The ESPHome config this firmware was built from: paste it as a new device in the ESPHome add-on, change it, and install over the air to the OTA port above.").clicked() {
                        ui.ctx().copy_text(text.to_string());
                        g.config_copied = true;
                    }
                });
            }
            if let Some(r) = g.rotation.filter(|r| *r != d.dev.meta.view_rotation) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("This firmware draws for the board held sideways.").small().weak());
                    if ui.small_button(format!("Hold it {}", rotation_label(r).replace("turned", "turned"))).on_hover_text("Turns the view only (Screen > Held)").clicked() {
                        rotate = Some(r);
                    }
                });
            }
            for note in &g.notes {
                ui.label(RichText::new(format!("• {note}")).small().weak());
            }
        });
        ui.add_space(4.0);
        let after = (!close).then_some(g);
        if after != Some(before) {
            d.dev.meta.guide = after;
            let _ = self.store.update_meta(&d.dev);
        }
        if send_wifi {
            let wifi = d.dev.meta.network.effective(&self.settings.wifi);
            d.start_improv(crate::improv::Job::Provision { ssid: wifi.ssid, password: wifi.password });
        }
        if let Some(r) = rotate {
            d.dev.meta.view_rotation = r;
            d.pop_size = Vec2::ZERO;
            let _ = self.store.update_meta(&d.dev);
        }
        if publish {
            let others: Vec<_> = self.devices.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, o)| o.dev.meta.network.clone()).collect();
            let d = &mut self.devices[i];
            let n = &mut d.dev.meta.network;
            n.add_preset_free(&crate::network::PRESETS[0], &others, &|p| bind_is_free("0.0.0.0", p));
            n.wifi = true;
            n.lan = true;
            let _ = self.store.update_meta(&d.dev);
            if alive {
                d.lab("ESPHome ports published: they apply at the next power-on.");
            }
        }
        if dismiss_firewall {
            self.settings.ui.firewall_note_seen = true;
            let _ = self.settings.save(&self.paths.settings);
        }
    }

    /// Wi-Fi setup for firmware that asks for it: Improv over serial ("Send Wi-Fi details")
    /// and the firmware's own setup hotspot ("Device hotspot").
    fn wifi_setup_ui(&mut self, ui: &mut egui::Ui, i: usize, alive: bool, features: &EngineFeatures) {
        use crate::improv::State;
        let wifi = self.devices[i].dev.meta.network.effective(&self.settings.wifi);
        let wifi_on = self.devices[i].dev.meta.network.wifi;
        let mut job = None;
        let mut restart = false;
        let d = &mut self.devices[i];
        let serial = d.backend.capabilities().serial_tcp.filter(|_| alive);
        // Passive: the firmware said so (Improv state it printed, or a setup line in its log), or
        // it put up an access point (named in the log, or seen by the engine's phone).
        let seen_ap = d.ap.status.as_ref().and_then(|s| s.phone.as_ref()).and_then(|p| p.hotspots.first()).map(|h| h.ssid.clone()).or_else(|| d.hotspot_ssids.last().cloned());
        let connected = d.wifi_watch.connected || matches!(d.improv.outcome, Some(Ok(_)));
        let improv_ready = d.improv.state == Some(State::Ready);
        let waiting = alive && !connected && d.improv.busy != Some("provision") && (improv_ready || d.improv.waiting_log || seen_ap.is_some());
        let send_tip = if !wifi_on {
            "Wi-Fi is off for this device (Network section)".to_string()
        } else {
            format!("Sends this device's Wi-Fi network (\"{}\", set under Network) to the firmware over its USB serial port, the way ESPHome Web or WLED's installer do it (Improv Wi-Fi). Takes up to a minute.", wifi.ssid)
        };
        let send_enabled = serial.is_some() && wifi_on && d.improv.busy.is_none();
        let provision = crate::improv::Job::Provision { ssid: wifi.ssid.clone(), password: wifi.password.clone() };
        if waiting {
            egui::Frame::new().fill(Color32::from_rgb(90, 60, 10)).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    let lead = match &seen_ap {
                        Some(ap) => format!("This firmware is waiting for Wi-Fi setup (it opened the hotspot \"{ap}\"):"),
                        None => "This firmware is waiting for Wi-Fi setup:".to_string(),
                    };
                    ui.label(RichText::new(lead).color(Color32::WHITE));
                    let improv_tip = if improv_ready || d.improv.capable { send_tip.clone() } else { format!("{send_tip}\n\nIts log doesn't say it supports Improv: if it doesn't, it ignores this (or prints an error) and you set it up through its hotspot instead.") };
                    if ui.add_enabled(send_enabled, egui::Button::new("Send Wi-Fi details")).on_hover_text(improv_tip).clicked() {
                        job = Some(provision.clone());
                    }
                    if seen_ap.is_some() {
                        ui.label(RichText::new("or join it under Device hotspot below").small().color(Color32::from_gray(220)));
                    }
                });
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Wi-Fi setup:").small());
            if !waiting && ui.add_enabled(send_enabled, egui::Button::new(RichText::new("Send Wi-Fi details").small())).on_hover_text(&send_tip).on_disabled_hover_text(if serial.is_none() { "Power the device on first (needs the engine's serial port)" } else { send_tip.as_str() }).clicked() {
                job = Some(provision.clone());
            }
            if ui
                .add_enabled(serial.is_some() && d.improv.busy.is_none(), egui::Button::new(RichText::new("Check").small()))
                .on_hover_text("Asks the firmware over serial whether it supports Wi-Fi setup (Improv) and what state it is in. Sends bytes to the board (firmware without Improv may print an error). Briefly takes the serial port: a connected tool is disconnected.")
                .clicked()
            {
                job = Some(crate::improv::Job::Probe);
            }
            let status = match (d.improv.busy, d.improv.state) {
                (Some("provision"), Some(s)) => format!("sending… firmware: {}", s.label()),
                (Some("provision"), None) => "sending…".into(),
                (Some(_), _) => "asking the firmware…".into(),
                (None, Some(s)) => format!("firmware: {}", s.label()),
                (None, None) if alive && d.improv.capable => "its log says it supports Improv (nothing sent yet)".into(),
                (None, None) if alive && connected => "on Wi-Fi".into(),
                (None, None) if alive => "nothing is sent until you ask (Check sends an Improv request)".into(),
                (None, None) => "power on first".into(),
            };
            if d.improv.busy.is_some() {
                ui.spinner();
            }
            ui.label(RichText::new(status).small().weak());
        });
        if !d.improv.info.is_empty() {
            ui.label(RichText::new(format!("  {}", d.improv.info.join(" · "))).small().weak());
        }
        match d.improv.outcome.clone() {
            Some(Ok(m)) => {
                let forwards: Vec<(u16, u16)> = d.dev.meta.network.active_forwards().iter().map(|f| (f.host, f.guest)).collect();
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("  Done: {m}")).small().color(Color32::LIGHT_GREEN));
                    if d.improv.url.is_none() {
                        if let Some((hp, _)) = forwards.iter().find(|(_, g)| *g == 6053) {
                            let host = cached_lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "this PC's IP".into());
                            ui.label(RichText::new(format!("Home Assistant: add the ESPHome integration with host {host}, port {hp}")).small());
                        }
                    }
                    if let Some(url) = d.improv.url.clone() {
                        match crate::improv::local_url(&url, &forwards) {
                            Some(local) => {
                                ui.hyperlink_to(RichText::new(format!("open {local}")).small(), local).on_hover_text(format!("The firmware reported {url} (its address on the emulated network), published on this PC."));
                            }
                            None => {
                                ui.label(RichText::new(format!("(it reported {url}, its address inside the emulator: publish port {} under Network to open it here)", url_port(&url))).small().weak()).on_hover_text(INTERNAL_IP_TIP);
                            }
                        }
                    }
                });
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("  Failed: {e}")).small().color(Color32::from_rgb(240, 120, 100)));
            }
            None => {}
        }
        if let Some(err) = d.improv.error.filter(|_| d.improv.outcome.is_none()) {
            ui.label(RichText::new(format!("  firmware error: {}", crate::improv::error_label(err))).small().weak());
        }

        // Device hotspot: only with an engine that can join it (--ap-client/--ap-fwd). With the
        // live link it is joined at run time (control channel); otherwise at the next power-on.
        if features.ap_client {
            let catalog = &self.catalog;
            let live = d.link_up && alive;
            let hs_id = ui.make_persistent_id(("hotspot", i));
            let hs_state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), hs_id, false);
            let mut hs_header_clicked = false;
            d.ap.panel_open = hs_state.is_open() && live;
            // Seen on the air (the engine's phone) first, then names the firmware printed.
            let mut seen: Vec<(String, Option<bool>)> = d.ap.status.as_ref().and_then(|s| s.phone.as_ref()).map(|p| p.hotspots.iter().map(|h| (h.ssid.clone(), Some(h.secure))).collect()).unwrap_or_default();
            for n in &d.hotspot_ssids {
                if !seen.iter().any(|(s, _)| s == n) {
                    seen.push((n.clone(), None));
                }
            }
            if hs_state.is_open() && live && !d.ap.scanning {
                // A phone that only listens, so the panel can list the hotspots on the air.
                if d.ap.ask(d.backend.as_mut(), crate::hotspot::client_message(None, "", None)).is_ok() {
                    d.ap.scanning = true;
                }
            }
            let mut lab_msg = None;
            hs_state
                .show_header(ui, |ui| {
                    let progress = d.ap.joining.as_ref().and(d.ap.status.as_ref()).and_then(|s| s.phone.as_ref()).map(|p| p.progress());
                    let t = match (&d.hotspot, progress, seen.len()) {
                        _ if d.ap.left.is_some() => format!("Device hotspot: device left setup mode and joined {}", d.ap.left.as_deref().unwrap_or_default()),
                        _ if d.ap.gone && d.ap.joining.is_some() => format!("Device hotspot: device is leaving setup mode (joining {})", d.wifi_ap.ssid),
                        (_, Some(p), _) => format!("Device hotspot: {p}"),
                        (Some(p), None, _) => format!("Device hotspot: joined \"{}\" at power-on", p.ssid),
                        (_, None, 0) => "Device hotspot".to_string(),
                        (_, None, n) => format!("Device hotspot: {n} network{} seen", if n == 1 { "" } else { "s" }),
                    };
                    hs_header_clicked = header_text(ui, RichText::new(t).small());
                })
                .body(|ui| {
                    ui.label(RichText::new("Many firmwares start their own Wi-Fi network for first setup (openHASP HASP-xxxxxx, NerdMinerAP, Tasmota, WLED-AP, ESPHome's fallback hotspot) with a setup page at 192.168.4.1. cyd-lab joins it like a phone would and opens that page in your browser (on this PC only).").small().weak());
                    let (ssid, pw, ip) = &mut d.hotspot_form;
                    if !seen.is_empty() {
                        egui::Grid::new(("hs-seen", i)).num_columns(3).show(ui, |ui| {
                            for (name, secure) in &seen {
                                ui.label(RichText::new(name).monospace());
                                ui.label(RichText::new(match secure { Some(true) => "password", Some(false) => "open", None => "from the log" }).small().weak());
                                if ui.small_button("Use").clicked() {
                                    *ssid = name.clone();
                                    match catalog.hotspot_setup(name) {
                                        Some(f) => {
                                            *pw = f.password.clone();
                                            *ip = f.page_ip().unwrap_or_else(|| crate::hotspot::GUEST_AP_IP.to_string());
                                        }
                                        None if *secure == Some(false) => pw.clear(),
                                        None => {}
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    } else if live {
                        ui.label(RichText::new("Listening for hotspots… (the firmware starts one when it has no Wi-Fi saved)").small().weak());
                    }
                    let known = catalog.hotspot_setup(ssid.trim());
                    egui::Grid::new(("hs-grid", i)).num_columns(2).show(ui, |ui| {
                        ui.label(RichText::new("Network").small());
                        ui.add(egui::TextEdit::singleline(ssid).desired_width(170.0).hint_text("e.g. HASP-1A2B3C"));
                        ui.end_row();
                        ui.label(RichText::new("Password").small());
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(pw).password(true).desired_width(170.0).hint_text("blank if open"));
                            if let Some(f) = known.filter(|f| !f.password.is_empty() && *pw != f.password) {
                                if ui.small_button("Use catalogue password").on_hover_text(format!("The firmware's default hotspot password ({})", f.hotspot)).clicked() {
                                    *pw = f.password.clone();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label(RichText::new("Page at").small());
                        ui.add(egui::TextEdit::singleline(ip).desired_width(110.0)).on_hover_text("192.168.4.1 for most firmwares, 4.3.2.1 for WLED");
                        ui.end_row();
                    });
                    let secure = seen.iter().find(|(n, _)| n == ssid.trim()).and_then(|(_, s)| *s);
                    if secure == Some(true) && pw.is_empty() {
                        ui.label(RichText::new("This network needs a password (see the firmware's docs or its screen).").small().color(Color32::from_rgb(240, 200, 120)));
                    }
                    if !pw.is_empty() && pw.len() < 8 {
                        ui.label(RichText::new("Wi-Fi passwords are 8 to 63 characters.").small().color(Color32::from_rgb(240, 200, 120)));
                    }
                    ui.horizontal(|ui| {
                        let ok = !ssid.trim().is_empty() && (pw.is_empty() || (8..=63).contains(&pw.len()));
                        let tip = if live { "Joins that network now and opens its setup page in your browser (served on 127.0.0.1 only)." } else { "Restarts the device joined to that network and opens its setup page in your browser (served on 127.0.0.1 only)." };
                        if ui.add_enabled(ok, egui::Button::new("Connect & open setup page")).on_hover_text(tip).clicked() {
                            let (name, page) = (ssid.trim().to_string(), if ip.trim().is_empty() { crate::hotspot::GUEST_AP_IP.to_string() } else { ip.trim().to_string() });
                            if live {
                                // One forward per run (per page address); the proxy follows it.
                                let fwd = if d.ap.bound.as_ref().is_some_and(|b| b.1 == page) { None } else { Some((page.as_str(), 80)) };
                                d.ap.joining = Some((name.clone(), page.clone()));
                                d.ap.open_when_up = true;
                                d.ap.error = None;
                                d.ap.left = None;
                                d.ap.was_up = false;
                                d.ap.gone = false;
                                d.ap.station_up = false;
                                if let Some((addr, _)) = d.ap.bound.clone() {
                                    if let Some(p) = d.ap.proxy() {
                                        p.set_target(Some(addr), &page);
                                    }
                                }
                                d.ap.next_poll = None;
                                if let Err(e) = d.ap.ask(d.backend.as_mut(), crate::hotspot::client_message(Some(&name), pw, fwd)) {
                                    d.ap.error = Some(e);
                                }
                                lab_msg = Some(format!("device hotspot: joining \"{name}\""));
                            } else {
                                let host_port = d.hotspot.as_ref().map(|p| p.host_port).filter(|&p| bind_is_free("127.0.0.1", p)).or_else(free_local_port).unwrap_or(18480);
                                d.hotspot = Some(crate::hotspot::Plan { ssid: name, password: pw.clone(), host_port, guest_ip: page, guest_port: 80 });
                                restart = true;
                            }
                        }
                        if let Some(url) = d.ap.proxy.as_ref().filter(|p| p.target().is_some()).map(|p| p.url()) {
                            if ui.small_button("Open page").on_hover_text(url.clone()).clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                            }
                        }
                        if (d.hotspot.is_some() || d.ap.joining.is_some()) && ui.button("Leave hotspot").on_hover_text("Stop using the device's hotspot (back to the normal Wi-Fi at the next power-on)").clicked() {
                            d.hotspot = None;
                            d.hotspot_open_at = None;
                            d.ap.joining = None;
                            d.ap.open_when_up = false;
                        }
                    });
                    if let Some(e) = &d.ap.error {
                        ui.label(RichText::new(format!("Engine: {e}")).small().color(Color32::from_rgb(240, 120, 100)));
                    }
                    if let Some(station) = &d.ap.left {
                        ui.label(RichText::new(format!("✔ Device left setup mode and joined {station}")).color(Color32::from_rgb(120, 220, 140)));
                        ui.label(RichText::new("Its setup page is closed (the hotspot is gone). Reach the device on its Wi-Fi now, e.g. through the ports under Network.").small().weak());
                    } else if d.ap.gone && d.ap.joining.is_some() {
                        ui.label(RichText::new(format!("The hotspot is gone: the device is leaving setup mode (waiting for it to join {})…", d.wifi_ap.ssid)).small().weak());
                    }
                    if let Some(st) = d.ap.status.as_ref().and_then(|s| s.phone.as_ref()).filter(|_| d.ap.joining.is_some()) {
                        ui.label(RichText::new(st.progress()).small().weak());
                    }
                    if !live {
                        ui.label(RichText::new("The live link is off, so joining restarts the device.").small().weak());
                    }
                });
            toggle_if(ui, hs_id, hs_header_clicked);
            if let Some(m) = lab_msg {
                d.lab(m);
            }
        }
        if let Some(j) = job {
            d.start_improv(j);
        }
        if restart {
            self.restart_device(i);
        }
    }

    /// Power-cycle (or power on) with a fresh command line, e.g. after joining a hotspot.
    fn restart_device(&mut self, i: usize) {
        if !matches!(self.devices[i].backend.state(), RunState::Running(_)) {
            return self.power_on(i, false);
        }
        let r = self.launch_spec(i, BootMode::Normal).and_then(|spec| {
            let port = spec.web_port;
            let r = self.devices[i].backend.restart(spec);
            if r.is_err() {
                if let Some(p) = port {
                    engine::release_port(p);
                }
            }
            r
        });
        let open = self.devices[i].hotspot.is_some() && self.engine_features().ap_client;
        let d = &mut self.devices[i];
        d.link_up = false;
        d.improv = ImprovUi::default();
        d.hotspot_ssids.clear();
        d.ap.new_run();
        d.fwd = Default::default();
        d.hotspot_open_at = open.then(|| Instant::now() + Duration::from_secs(10));
        if let Err(e) = r {
            d.lab(format!("restart failed: {e:#}"));
        }
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
        let live_fwd = alive && d.fwd.reported;
        let ports: Vec<String> = net
            .active_forwards()
            .iter()
            .map(|f| {
                let n = d.fwd.on_host_port(f.host).0;
                if live_fwd && n > 0 { format!("{}->{} ({n} connected)", f.host, f.guest) } else { format!("{}->{}", f.host, f.guest) }
            })
            .collect();
        let summary = if !net.wifi {
            "Network: Wi-Fi off".to_string()
        } else {
            format!(
                "Network: Wi-Fi \"{}\" · {}",
                eff.ssid,
                if ports.is_empty() { "no ports published".to_string() } else { format!("{} {}", if net.lan { ip_text.as_str() } else { "127.0.0.1" }, ports.join(", ")) }
            )
        };
        let net_id = ui.make_persistent_id(("net", i));
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), net_id, false);
        let mut net_header_clicked = false;
        state
            .show_header(ui, |ui| {
                net_header_clicked = header_text(ui, RichText::new(summary).small());
                ui.menu_button(RichText::new("Use with…").small(), |ui| {
                    ui.set_max_width(460.0);
                    if let Some(k) = network_help_ui(ui, &ip_text, &d.dev.meta.network, features.mdns_relay) {
                        add_preset = Some(k);
                    }
                });
            })
            .body(|ui| {
                // Live clients per published port (engines that report them), outside the
                // greyed-out settings below.
                if live_fwd && !d.dev.meta.network.active_forwards().is_empty() {
                    ui.label(RichText::new("Connected clients").small().strong());
                    egui::Grid::new(("fwd-live", i)).num_columns(3).show(ui, |ui| {
                        for f in d.dev.meta.network.active_forwards() {
                            let (n, peers) = d.fwd.on_host_port(f.host);
                            ui.label(RichText::new(format!("{} -> {}", f.host, f.guest)).monospace().small());
                            ui.label(RichText::new(if f.label.is_empty() { "TCP" } else { f.label.as_str() }).small().weak());
                            let c = if n > 0 { Color32::from_rgb(120, 220, 140) } else { Color32::from_gray(150) };
                            ui.label(RichText::new(format!("{n} connected")).small().color(c)).on_hover_text(if peers.is_empty() { "Nobody is connected to this port".to_string() } else { peers.join("\n") });
                            ui.end_row();
                        }
                    });
                }
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
                    if features.mdns_relay && d.dev.meta.network.lan && d.dev.meta.network.wifi {
                        ui.label(RichText::new("mDNS: relayed on this PC's network (UDP 5353), so name.local and Home Assistant discovery work; allow esp32sim in the firewall.").small().weak());
                    }
                });
            });
        toggle_if(ui, net_id, net_header_clicked);
        if let Some(k) = add_preset {
            let addr = if d.dev.meta.network.lan { "0.0.0.0" } else { "127.0.0.1" };
            d.dev.meta.network.add_preset_free(&crate::network::PRESETS[k], &others, &|p| bind_is_free(addr, p));
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
        // Without the engine's serial bridge there is nothing to set up here.
        if live_port.is_none() && !features.serial_tcp {
            return;
        }
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
                (true, false, false) | (false, _, false) => "not served by this run".to_string(),
                (false, _, true) => "served at power-on".to_string(),
            };
            ui.label(RichText::new(note).small().weak());
            if let Some(com) = d.dev.meta.com_port.clone().filter(|_| self.settings.engine.com_bridge) {
                com_port_ui(ui, d, &com);
            }
        });
        if let Some(k) = net_preset {
            let addr = if d.dev.meta.network.lan { "0.0.0.0" } else { "127.0.0.1" };
            d.dev.meta.network.add_preset_free(&crate::network::PRESETS[k], &net_others, &|p| bind_is_free(addr, p));
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
        let state = self.devices[i].backend.state();
        let running = matches!(state, RunState::Running(_));
        let powered = matches!(state, RunState::Running(_) | RunState::Stopping);
        let live_link = self.settings.engine.live_link;
        let d = &mut self.devices[i];
        let panel = d.panel_dims(profile);
        let mut pop = false;
        let mut dock = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Screen").strong());
            ui.label("Held:");
            let mut rot = d.dev.meta.view_rotation;
            egui::ComboBox::from_id_salt(("rot", &d.dev.id))
                .width(110.0)
                .selected_text(rotation_label(rot))
                .show_ui(ui, |ui| {
                    for r in [0u16, 90, 180, 270] {
                        ui.selectable_value(&mut rot, r, rotation_label(r));
                    }
                })
                .response
                .on_hover_text("How you hold the board. This turns the view (and touch with it); the firmware's picture is unchanged.");
            if rot != d.dev.meta.view_rotation {
                d.dev.meta.view_rotation = rot;
                d.pop_size = Vec2::ZERO;
                let _ = self.store.update_meta(&d.dev);
            }
            if d.dev.meta.screen_window.popped {
                if ui.button("Dock").on_hover_text("Bring the screen back into this window").clicked() {
                    dock = true;
                }
            } else {
                ui.label("Zoom:");
                zoom_combo(ui, ("mzoom", &d.dev.id), &mut d.main_zoom);
                if ui.button("Pop out").on_hover_text("Show the screen in its own window you can move about").clicked() {
                    pop = true;
                }
            }
        });
        if d.flash_blank && d.pending_flash.is_none() {
            ui.label(RichText::new("Blank flash: press Install firmware…, or drop a .bin on the window").color(Color32::from_rgb(240, 190, 40)));
        }
        if d.dev.meta.screen_window.popped {
            ui.label(RichText::new("The screen is in its own window.").weak());
            d.screen_strip(ui, powered, running, live_link, 0);
        } else {
            let (vw, vh) = rotated_size(d.dev.meta.view_rotation, panel.0, panel.1);
            let ppp = ui.ctx().pixels_per_point();
            let strip = ui.text_style_height(&egui::TextStyle::Small) * 2.0 + 16.0;
            let avail = Vec2::new(ui.available_width().min(ui.ctx().content_rect().width() * 0.55), ui.available_height() - strip);
            let fit = fit_zoom(avail, (vw, vh), ppp);
            let zoom = if d.main_zoom == 0 { fit } else { d.main_zoom as u32 };
            d.screen_view(ui, panel, zoom, powered);
            d.screen_strip(ui, powered, running, live_link, zoom);
        }
        if pop || dock {
            d.dev.meta.screen_window.popped = pop;
            d.pop_pos0 = None;
            d.pop_size = Vec2::ZERO;
            let _ = self.store.update_meta(&d.dev);
        }
    }

    /// One device's screen in its own OS window (egui viewport). Closing it docks it again.
    fn screen_window(&mut self, ctx: &egui::Context, i: usize, profile: Option<&Profile>) {
        let state = self.devices[i].backend.state();
        let running = matches!(state, RunState::Running(_));
        let powered = matches!(state, RunState::Running(_) | RunState::Stopping);
        let live_link = self.settings.engine.live_link;
        let d = &mut self.devices[i];
        let panel = d.panel_dims(profile);
        let (vw, vh) = rotated_size(d.dev.meta.view_rotation, panel.0, panel.1);
        let ppp = ctx.pixels_per_point();
        let sw = d.dev.meta.screen_window.clone();
        if d.pop_size == Vec2::ZERO {
            let z = if sw.zoom == 0 { 2.0 } else { sw.zoom as f32 };
            let strip = 110.0;
            d.pop_size = Vec2::new((vw as f32 * z / ppp).max(300.0) + 24.0, vh as f32 * z / ppp + strip);
        }
        if d.pop_pos0.is_none() {
            d.pop_pos0 = Some(sw.pos.map(|p| egui::pos2(p[0], p[1])).unwrap_or(egui::pos2(120.0, 120.0)));
        }
        let (status, _) = d.screen_status(powered, running, live_link);
        let title = if status == "Live" { format!("{} – screen", d.dev.meta.name) } else { format!("{} – screen – {status}", d.dev.meta.name) };
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(d.pop_size)
            .with_min_inner_size([200.0, 160.0])
            .with_position(d.pop_pos0.unwrap())
            .with_window_level(if sw.always_on_top { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal });
        let vid = egui::ViewportId::from_hash_of(("screen", &d.dev.id));
        let mut changed = false;
        let mut dock = false;
        ctx.show_viewport_immediate(vid, builder, |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| {
                let sw = &mut d.dev.meta.screen_window;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Zoom:");
                    let before = sw.zoom;
                    zoom_combo(ui, ("pzoom", vid), &mut sw.zoom);
                    if sw.zoom != before {
                        changed = true;
                    }
                    if ui.checkbox(&mut sw.always_on_top, "Always on top").changed() {
                        changed = true;
                    }
                    if ui.button("Dock").on_hover_text("Put the screen back in the main window").clicked() {
                        dock = true;
                    }
                });
                let strip = ui.text_style_height(&egui::TextStyle::Small) * 2.0 + 16.0;
                let avail = ui.available_size() - Vec2::new(0.0, strip);
                let ppp = ui.ctx().pixels_per_point();
                let zoom = if sw.zoom == 0 { fit_zoom(avail, (vw, vh), ppp) } else { sw.zoom as u32 };
                d.screen_view(ui, panel, zoom, powered);
                d.screen_strip(ui, powered, running, live_link, zoom);
            });
            let (close, outer) = ui.input(|i| (i.viewport().close_requested(), i.viewport().outer_rect));
            if close {
                dock = true;
            }
            if let Some(r) = outer {
                let p = [r.min.x, r.min.y];
                if d.dev.meta.screen_window.pos != Some(p) {
                    d.dev.meta.screen_window.pos = Some(p);
                    d.pop_moved = Some(Instant::now());
                }
            }
        });
        if changed {
            d.pop_size = Vec2::ZERO; // re-measure for the new zoom
        }
        if d.pop_moved.is_some_and(|t| t.elapsed() > Duration::from_secs(1)) {
            d.pop_moved = None;
            changed = true;
        } else if d.pop_moved.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if dock {
            d.dev.meta.screen_window.popped = false;
            d.pop_pos0 = None;
            changed = true;
        }
        if changed {
            let _ = self.store.update_meta(&d.dev);
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
                ui.label(RichText::new("cyd-lab").strong().size(20.0));
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
        egui::Panel::left("devices").default_size(240.0).show(ui, |ui| self.device_list(ui));
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
        self.rename_modal(&ctx);

        // Popped-out screens, each in its own OS window.
        for i in 0..self.devices.len() {
            if self.devices[i].dev.meta.screen_window.popped {
                let profile = self.profile_for(i);
                self.screen_window(&ctx, i, profile.as_ref());
            }
        }
        // Text size: Ctrl +/- and Ctrl 0 change the zoom directly; keep it for next time.
        let z = ctx.zoom_factor();
        if (z - self.text_scale).abs() > 0.001 {
            self.text_scale = z;
            self.settings.ui.text_scale = (z * 100.0).round() / 100.0;
            if let Some(d) = &mut self.settings_draft {
                d.ui.text_scale = self.settings.ui.text_scale;
            }
            let _ = self.settings.save(&self.paths.settings);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
    }
}

/// Delete a device that may be running: power it off for good first (graceful quit,
/// killed after [`DELETE_STOP_WAIT`]) and wait until the engine and its COM relay have
/// exited, so flash.bin is closed (Windows refuses to delete an open file) and its
/// published ports are free; close the hotspot page proxy (its port too); then remove
/// the folder, retrying briefly while a file is still locked.
fn power_off_and_delete(store: &DeviceStore, backend: &mut dyn Backend, proxy: &mut Option<crate::hotspot::HostProxy>, dev: &crate::device::Device) -> anyhow::Result<()> {
    backend.stop_within(DELETE_STOP_WAIT);
    *proxy = None;
    store.delete(dev)
}

/// How long a delete lets the engine save and quit before killing it.
const DELETE_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl Drop for CydLabApp {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Header text for a `CollapsingState` header: clickable, like a `CollapsingHeader`'s title
/// (egui only makes the small triangle of a custom header toggle). Returns whether it was clicked.
fn header_text(ui: &mut egui::Ui, text: RichText) -> bool {
    ui.add(egui::Label::new(text).sense(egui::Sense::click())).clicked()
}

/// Toggles the `CollapsingState` stored under `id` when its header text was clicked.
fn toggle_if(ui: &egui::Ui, id: egui::Id, clicked: bool) {
    if clicked {
        let mut s = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        s.toggle(ui);
        s.store(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn install_status_follows_the_pipeline() {
        let mut st = InstallUi::default();
        assert!(!st.visible() && !st.active());
        st.begin("openHASP 0.7.0-rc13", InstallSource::Catalog);
        assert!(st.active() && st.visible());
        let f0 = st.fraction();
        st.line("downloading https://example.invalid/x.bin");
        assert!(st.fraction() > f0);
        st.line("firmware: SHA-256 OK");
        st.line("downloading again"); // never goes backwards
        assert_eq!(st.step, 3);
        st.writing(true);
        assert!(st.stage.contains("powering off"));
        st.done("Installed openHASP".into());
        assert!(!st.active() && st.visible() && st.fraction() == 1.0);
        st.begin("web install (manifest.json)", InstallSource::Web);
        st.fail("HTTP 404".into());
        assert!(matches!(st.outcome, Some(Err(_))) && st.visible() && !st.active());
        let mut never_started = InstallUi::default();
        never_started.fail("flash.bin missing".into());
        assert!(never_started.visible());
    }

    use super::*;

    #[test]
    fn symbol_fonts_are_bundled() {
        // The checklist ticks (✔ ○), ⚠ and friends come from egui's emoji fallback fonts,
        // which egui 0.37 only ships with its `monochrome_emoji_fonts` feature: without it
        // they draw as empty boxes.
        let defs = egui::FontDefinitions::default();
        let prop = &defs.families[&egui::FontFamily::Proportional];
        assert!(prop.iter().any(|f| f == "NotoEmoji-Regular"), "{prop:?}");
        assert!(prop.iter().any(|f| f == "emoji-icon-font"), "{prop:?}");
    }

    #[test]
    fn screen_zoom_is_a_whole_number_that_fits() {
        // 320x240 view in 700x500 points at 100%: 2x (3x would be 960 wide).
        assert_eq!(fit_zoom(Vec2::new(700.0, 500.0), (320, 240), 1.0), 2);
        // At 150% display scale the same area has 1050x750 physical pixels: 3x.
        assert_eq!(fit_zoom(Vec2::new(700.0, 500.0), (320, 240), 1.5), 3);
        // Too small: still 1x (never shrunk below the panel's own pixels).
        assert_eq!(fit_zoom(Vec2::new(100.0, 100.0), (240, 320), 1.0), 1);
    }

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

    #[cfg(target_os = "linux")]
    #[test]
    fn deleting_a_running_device_stops_it_first_and_frees_everything() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let tmp = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(tmp.path().join("devices")).unwrap();
        let profile = ProfileLibrary::load(None).get("esp32-2432s028r").unwrap().clone();
        let dev = store.create("Doomed", &profile).unwrap();
        // A stubborn engine: ignores SIGTERM and stdin EOF, keeps flash.bin open.
        let exe = tmp.path().join("fake-esp32sim");
        std::fs::write(&exe, format!("#!/bin/sh\ntrap '' TERM\nexec 3<'{}'\nwhile :; do sleep 0.05; done\n", dev.flash_path().display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = EngineSettings { engine_exe: exe, stop_grace_secs: 60, ..Default::default() };
        let features = EngineFeatures { found: true, flash_file: true, ..Default::default() };
        let mut spec = LaunchSpec::build(&profile, &dev, &settings, &features, BootMode::Normal, None, None);
        spec.relay = Some(crate::combridge::Relay { program: "sleep".into(), args: vec!["30".into()], label: "test relay".into() });
        let mut b = ProcessBackend::new(std::sync::Arc::new(|| {}));
        b.start(&spec).unwrap();
        std::thread::sleep(Duration::from_millis(200)); // let the trap install
        let pid = b.pid().unwrap();
        assert!(b.relay_running());
        let mut proxy = crate::hotspot::HostProxy::start().ok();
        let proxy_port = proxy.as_ref().unwrap().port;

        let t = Instant::now();
        power_off_and_delete(&store, &mut b, &mut proxy, &dev).unwrap();
        // Killed after the short delete wait, not the 60 s grace.
        assert!(t.elapsed() < DELETE_STOP_WAIT + Duration::from_secs(2), "{:?}", t.elapsed());
        assert_eq!(b.state(), RunState::Stopped);
        assert!(b.pid().is_none() && !b.relay_running());
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "engine still alive");
        assert!(proxy.is_none());
        assert!(std::net::TcpListener::bind(("127.0.0.1", proxy_port)).is_ok(), "hotspot proxy port still held");
        assert!(!dev.dir.exists());
        assert!(store.list().unwrap().0.is_empty());
        // Deleting a stopped device is immediate.
        let dev2 = store.create("Idle", &profile).unwrap();
        power_off_and_delete(&store, &mut b, &mut None, &dev2).unwrap();
        assert!(!dev2.dir.exists());
    }

    #[test]
    fn delete_retries_while_the_folder_is_busy() {
        // Stand-in for a file still locked on Windows: the folder only becomes deletable
        // after a moment; the retry loop waits for it instead of failing at once.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dev");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("flash.bin"), b"x").unwrap();
        assert!(crate::device::remove_dir_retrying(&tmp.path().join("missing"), std::time::Duration::ZERO).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock = || std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            lock();
            if std::fs::remove_file(dir.join("flash.bin")).is_ok() {
                return; // running as root: permissions don't lock anything
            }
            let d2 = dir.clone();
            let unlock = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                std::fs::set_permissions(&d2, std::fs::Permissions::from_mode(0o755)).unwrap();
            });
            crate::device::remove_dir_retrying(&dir, std::time::Duration::from_secs(3)).unwrap();
            unlock.join().unwrap();
            assert!(!dir.exists());
            // Gives up after the window.
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("f"), b"x").unwrap();
            lock();
            assert!(crate::device::remove_dir_retrying(&dir, std::time::Duration::from_millis(300)).is_err());
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// The port in an http URL (80 if none).
fn url_port(url: &str) -> u16 {
    url.split_once("://").map(|(_, r)| r.split('/').next().unwrap_or("")).and_then(|h| h.rsplit_once(':')).and_then(|(_, p)| p.parse().ok()).unwrap_or(80)
}

/// A free TCP port on 127.0.0.1 (for a forwarded setup page).
fn free_local_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|l| l.local_addr().ok()).map(|a| a.port())
}

