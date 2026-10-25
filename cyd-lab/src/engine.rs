//! The engine boundary.
//!
//! Everything cyd-lab knows about esp32sim lives behind the [`Backend`] trait.
//! Today the only implementation is [`ProcessBackend`], which runs the engine
//! as a child process (`esp32sim.exe` / `esp32sim`) per device:
//!
//! * **command line** – built by [`LaunchSpec::build`] from the profile, the
//!   device folder and the settings (pure function, unit tested);
//! * **console** – the engine prints the emulated UART to stdout; two reader
//!   threads (stdout + stderr) push lines into a channel;
//! * **live link** – when `--web PORT` is passed, esp32sim also serves a
//!   WebSocket on that port. [`crate::weblink`] connects to it as a native
//!   client to receive TFT frames and send touch / serial / reset messages;
//! * **lifetime** – power-off is a *graceful* stop when the engine owns a
//!   persistent flash file (`--flash-file`): cyd-lab asks the engine to quit
//!   (WebSocket `quit`, stdin EOF, and SIGTERM on Unix), waits up to the grace
//!   period for it to write `flash.bin`, and only then kills it. `Drop` does
//!   the same, and on Windows every child is put in a Job object with
//!   KILL_ON_JOB_CLOSE so engines die even if cyd-lab crashes;
//! * **features** – [`EngineFeatures::probe`] looks at the engine build
//!   (`--help` text plus the flag strings in the exe) to decide between
//!   `--flash-file` / `--flash-image` and whether `--serial-tcp` exists.
//!
//! A future `LibraryBackend` can link the engine crates directly and implement
//! the same trait; the UI would not change.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::device::Device;
use crate::profile::Profile;
use crate::weblink::{Frame, WebLink};

/// Strap value for UART download mode (GPIO0 low = BOOT button held).
pub const STRAP_DOWNLOAD: u8 = 0x0f;
/// Strap value for normal SPI flash boot (GPIO0 high).
pub const STRAP_SPI_BOOT: u8 = 0x13;

/// Called from background threads when there is something new to show.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    /// Normal boot from SPI flash (BOOT released).
    Normal,
    /// ROM serial download mode (BOOT held while EN released / power applied).
    Download,
}

impl BootMode {
    pub fn from_boot_held(held: bool) -> BootMode {
        if held { BootMode::Download } else { BootMode::Normal }
    }
    pub fn strap(self) -> u8 {
        match self {
            BootMode::Normal => STRAP_SPI_BOOT,
            BootMode::Download => STRAP_DOWNLOAD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Stopped,
    Running(BootMode),
    /// Power-off requested; waiting for the engine to save flash and exit.
    Stopping,
    /// Process ended on its own (crash, bad arguments, --max-seconds reached).
    Exited(Option<i32>),
}

/// What the current backend / engine build can do. The UI greys out what is missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Live TFT frames are arriving (engine `--web` link is connected).
    pub live_screen: bool,
    /// Touch events reach the emulated panel.
    pub touch: bool,
    /// Engine writes the display to a PNG at the end of a run (`--tft-png`).
    pub png_snapshot: bool,
    /// The run owns `flash.bin` (`--flash-file`): firmware writes are saved on a graceful stop.
    pub flash_writeback: bool,
    /// The engine serves UART0 as an RFC 2217 port on this TCP port (`--serial-tcp`).
    pub serial_tcp: Option<u16>,
}

/// What the console says about the chip right now (beyond "the process runs").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChipStatus {
    /// The boot mode the ROM last reported, if any was seen since power-on.
    pub mode: Option<BootMode>,
    /// The last reset came from the serial port's DTR/RTS (esptool or another client).
    pub by_serial: bool,
    /// The chip stopped (e.g. a half-written bootloader) and waits for an EN reset from
    /// the serial client.
    pub halted: bool,
    /// The engine reported writing flash.bin during this stop.
    pub flash_saved: bool,
}

impl ChipStatus {
    /// Update from one console line (stdout or stderr). Pure; unit tested.
    pub fn observe(&mut self, line: &str) {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("[emu] serial: EN reset") {
            self.mode = Some(if rest.contains("download") { BootMode::Download } else { BootMode::Normal });
            self.by_serial = true;
            self.halted = false;
        } else if l.contains("waiting for an EN reset from the client") {
            self.halted = true;
        } else if l.starts_with("[emu] flash: wrote") && l.contains("--flash-file") {
            self.flash_saved = true;
        } else if l.starts_with("rst:") {
            if let Some(m) = rom_boot_mode(l) {
                self.mode = Some(m);
                self.halted = false;
            }
        }
    }
}

/// The boot mode in the ESP32 ROM banner, e.g.
/// `rst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)` or `boot:0xf (UART_BOOT(UART0))`.
pub fn rom_boot_mode(line: &str) -> Option<BootMode> {
    let after = &line[line.find("boot:")? + 5..];
    if after.contains("DOWNLOAD") || after.contains("UART_BOOT") {
        return Some(BootMode::Download);
    }
    if after.contains("FLASH_BOOT") || after.contains("SPI_") {
        return Some(BootMode::Normal);
    }
    let hex: String = after.trim_start_matches("0x").chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    let v = u8::from_str_radix(&hex, 16).ok()?;
    // GPIO0 is strap bit 4 (MTDI.. order: GPIO0 = 0x10): low = download family.
    Some(if v & 0x10 == 0 { BootMode::Download } else { BootMode::Normal })
}

// ---------------------------------------------------------------------------
// What the engine build supports

/// Optional engine features, detected from the engine executable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineFeatures {
    /// The executable was found and answered `--help`.
    pub found: bool,
    /// `--flash-file PATH`: load the whole flash and write it back at exit.
    pub flash_file: bool,
    /// `--serial-tcp PORT`: UART0 as an RFC 2217 server with DTR/RTS auto-reset.
    pub serial_tcp: bool,
    /// `--web-dir DIR`.
    pub web_dir: bool,
    /// `--quit-on-stdin-eof`: save and exit 0 when stdin closes. Engines with it also
    /// honour the WebSocket `{"t":"quit"}` and save on Ctrl-C / console close on Windows.
    pub quit_on_stdin_eof: bool,
    /// `--wifi SPEC` (a virtual access point) with `--net nat`.
    pub wifi: bool,
    /// `--wifi any` / `ssid=*` (match-any) and the `wifi_wanted` event (detected from `--help`).
    pub wifi_any: bool,
    /// `\,` / `\\` escapes in the `--wifi` SPEC (detected from `--help`).
    pub wifi_escapes: bool,
    /// The web channel's `{"t":"wifi"}`: change the access point while running (detected from `--help`).
    pub wifi_live: bool,
    /// `--hostfwd tcp:HOST:PORT-:GUESTPORT`.
    pub hostfwd: bool,
    /// `--mdns-relay`: answer/forward mDNS between the LAN and the NAT'd chip.
    pub mdns_relay: bool,
    /// `--ap-client` + `--ap-fwd`: join the firmware's own setup hotspot and forward its page
    /// (detected from `--help` only).
    pub ap_client: bool,
    /// `--list-boards`: the engine's built-in board profiles as JSON.
    pub list_boards: bool,
    /// `--board-config FILE.toml`: a board described in a TOML file.
    pub board_config: bool,
}

impl EngineFeatures {
    /// The flags cyd-lab cares about.
    pub const FLAGS: [&'static str; 7] = ["--flash-file", "--serial-tcp", "--web-dir", "--quit-on-stdin-eof", "--wifi", "--hostfwd", "--mdns-relay"];

    /// Decide from the `--help` text, falling back to the executable's bytes for a flag the
    /// help does not mention. Current esp32sim lists all of these in `--help`; older builds
    /// left some out, but every flag is a string literal in the argument parser.
    pub fn from_probe(help: &str, binary: &[u8]) -> EngineFeatures {
        let has = |flag: &str| help.contains(flag) || contains_bytes(binary, flag.as_bytes());
        EngineFeatures {
            found: true,
            flash_file: has("--flash-file"),
            serial_tcp: has("--serial-tcp"),
            web_dir: has("--web-dir"),
            quit_on_stdin_eof: has("--quit-on-stdin-eof"),
            wifi: has("--wifi"),
            wifi_any: help.contains("ssid=*"),
            wifi_escapes: help.contains("\\, for a comma"),
            wifi_live: help.contains("{\"t\":\"wifi\"}"),
            hostfwd: has("--hostfwd"),
            mdns_relay: has("--mdns-relay"),
            ap_client: help.contains("--ap-client") && help.contains("--ap-fwd"),
            list_boards: help.contains("--list-boards"),
            board_config: help.contains("--board-config"),
        }
    }

    pub fn net(&self) -> crate::network::NetFeatures {
        crate::network::NetFeatures { wifi: self.wifi, wifi_any: self.wifi && self.wifi_any, wifi_escapes: self.wifi_escapes, hostfwd: self.hostfwd, mdns_relay: self.mdns_relay }
    }

    /// Probe an engine executable (cached by path, size and mtime, so a rebuilt engine is
    /// re-probed). Never panics; a missing exe gives `found: false`.
    pub fn probe(exe: &Path) -> EngineFeatures {
        let Ok(meta) = std::fs::metadata(exe) else { return EngineFeatures::default() };
        let key = (exe.to_path_buf(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        static CACHE: OnceLock<Mutex<Vec<((PathBuf, u64, SystemTime), EngineFeatures)>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
        if let Some((_, f)) = cache.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return *f;
        }
        let help = run_help(exe).unwrap_or_default();
        // Only read the (large) executable when the help text leaves something out.
        let binary = if Self::FLAGS.iter().all(|f| help.contains(f)) { Vec::new() } else { std::fs::read(exe).unwrap_or_default() };
        let mut f = EngineFeatures::from_probe(&help, &binary);
        f.found = !help.is_empty() || !binary.is_empty() || meta.len() > 0;
        cache.lock().unwrap().push((key, f));
        f
    }
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The engine's built-in boards (`--list-boards`), cached like [`EngineFeatures::probe`].
/// Empty for an engine without the flag.
pub fn list_boards(exe: &Path) -> Vec<crate::profile::EngineBoard> {
    if !EngineFeatures::probe(exe).list_boards {
        return Vec::new();
    }
    let Ok(meta) = std::fs::metadata(exe) else { return Vec::new() };
    let key = (exe.to_path_buf(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    type Cache = Mutex<Vec<((PathBuf, u64, SystemTime), Vec<crate::profile::EngineBoard>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Some((_, b)) = cache.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return b.clone();
    }
    let boards = run_args(exe, &["--list-boards"]).map(|out| crate::profile::parse_board_list(&out)).unwrap_or_default();
    cache.lock().unwrap().push((key, boards.clone()));
    boards
}

/// `exe --help`, stdout + stderr, with a 3 s timeout.
fn run_help(exe: &Path) -> Option<String> {
    run_args(exe, &["--help"])
}

/// `exe ARGS`, stdout + stderr, with a 3 s timeout.
fn run_args(exe: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let (mut out, mut err) = (child.stdout.take()?, child.stderr.take()?);
    let t_out = std::thread::spawn(move || { let mut s = String::new(); let _ = out.read_to_string(&mut s); s });
    let t_err = std::thread::spawn(move || { let mut s = String::new(); let _ = err.read_to_string(&mut s); s });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    Some(t_out.join().unwrap_or_default() + &t_err.join().unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// Messages from cyd-lab itself (start, stop, errors).
    Lab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    Console(Stream, String),
    Exited(Option<i32>),
    LinkUp,
    LinkDown(String),
    /// A text message from the engine's control channel (JSON, e.g. `ap_status`).
    Control(String),
}

#[derive(Debug)]
pub enum TouchError {
    /// No live link to the engine (not running, or `--web` disabled / not connected yet).
    NotConnected,
}

impl std::fmt::Display for TouchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TouchError::NotConnected => write!(f, "touch input needs the live link (engine --web), which is not connected"),
        }
    }
}

/// The interface the UI uses to drive one emulated board.
pub trait Backend: Send {
    fn capabilities(&self) -> Capabilities;
    /// Power on with the given launch spec (BOOT strap is part of the spec).
    fn start(&mut self, spec: &LaunchSpec) -> Result<()>;
    /// Power off. Blocks until the engine process has exited (killed after its grace).
    fn stop(&mut self);
    /// Power off for good (device delete, quit): like [`Backend::stop`] but waits at most
    /// `max` for a graceful exit before killing, and returns only once the process and its
    /// helper are gone, so its open files (flash.bin) and published ports are released.
    fn stop_within(&mut self, max: Duration) {
        let _ = max;
        self.stop()
    }
    /// Power off without blocking: ask the engine to save and quit; [`Backend::poll`]
    /// finishes the job (or kills it after the grace period).
    fn request_stop(&mut self) {
        self.stop()
    }
    /// EN with a changed BOOT strap: power-cycle (graceful stop, then start `spec`).
    fn restart(&mut self, spec: LaunchSpec) -> Result<()> {
        self.stop();
        self.start(&spec)
    }
    /// EN without a strap change: the engine's in-process reset (WebSocket `reset`), so
    /// nothing is lost. `Err` when there is no live link.
    fn soft_reset(&mut self) -> std::result::Result<(), TouchError>;
    /// EN with a strap change, in-process: pulse the auto-reset lines on the engine's
    /// RFC 2217 port (EN low, then IO0 low for download). `Err` without `--serial-tcp`.
    fn strap_reset(&mut self, _boot: BootMode) -> std::result::Result<(), String> {
        Err("needs the engine's --serial-tcp".into())
    }
    /// Talk Improv Wi-Fi over UART0 (the `--serial-tcp` port): see [`crate::improv`].
    /// Events arrive on the returned channel; chip output keeps going to the console.
    fn improv(&mut self, _job: crate::improv::Job) -> std::result::Result<std::sync::mpsc::Receiver<crate::improv::Event>, String> {
        Err("needs the engine's --serial-tcp".into())
    }
    /// The BOOT button as a live input: GPIO0 low while held (WebSocket `btn`, pin 0).
    fn set_boot_pin(&mut self, _low: bool) -> std::result::Result<(), TouchError> {
        Err(TouchError::NotConnected)
    }
    /// What the console says about the chip (mode the ROM reported, halted, ...).
    fn chip_status(&self) -> ChipStatus {
        ChipStatus::default()
    }
    fn state(&self) -> RunState;
    /// Drain pending events; also notices a process that exited.
    fn poll(&mut self) -> Vec<EngineEvent>;
    /// Take the newest screen frame, if a new one arrived since the last call.
    fn take_frame(&mut self) -> Option<Frame>;
    /// Touch at panel coordinates (native orientation, pixels). `down=false` releases.
    fn touch(&mut self, x: u16, y: u16, down: bool) -> std::result::Result<(), TouchError>;
    /// Send a line of text into the emulated UART0 RX.
    fn serial_line(&mut self, line: &str) -> std::result::Result<(), TouchError>;
    /// The command line of the current / last run, for display.
    fn last_command(&self) -> Option<String>;
    /// A JSON message on the live link's control channel (e.g. the device hotspot's
    /// `ap_client` / `ap_status`); answers arrive as [`EngineEvent::Control`].
    fn control(&mut self, _json: String) -> std::result::Result<(), TouchError> {
        Err(TouchError::NotConnected)
    }
}

/// Engine-related settings (persisted by the app in `settings.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// Path to esp32sim(.exe). A bare name is looked up on PATH.
    pub engine_exe: PathBuf,
    /// Directory with the mask ROM ELFs (esp32_rev300_rom.elf, ...). Also exported
    /// to the child as ESP32SIM_ROM_DIR. Empty = let the engine use its defaults.
    pub rom_dir: Option<PathBuf>,
    /// Pace emulation to wall-clock time (`--realtime`).
    pub realtime: bool,
    /// Pass `--web PORT` and connect the live link (screen + touch).
    pub live_link: bool,
    /// First port tried for `--web`; each running device gets the next free one.
    pub web_port_base: u16,
    /// Ask the engine to save the display at the end of a run (`--tft-png`).
    pub tft_png: bool,
    /// Seconds to wait for the engine to save flash and exit before killing it.
    pub stop_grace_secs: u32,
    /// Stop after N emulated seconds (`--max-seconds`). None = run until powered off.
    pub max_seconds: Option<u32>,
    /// Extra arguments for every device.
    pub extra_args: Vec<String>,
    /// Static files for the engine's own browser UI (`--web-dir`). cyd-lab only uses the
    /// WebSocket, so by default it points the engine at an empty per-device folder: without
    /// `--web-dir` esp32sim looks for `web/` next to its exe and panics if it is missing.
    pub web_dir: Option<PathBuf>,
    /// Optional extra, Windows only, off by default: expose each device as a real COMxx port
    /// through com0com + hub4com. Not needed for esptool, Arduino, ESPHome or PlatformIO,
    /// which all take the `rfc2217://` URL directly.
    pub com_bridge: bool,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            engine_exe: PathBuf::from(if cfg!(windows) { "esp32sim.exe" } else { "esp32sim" }),
            rom_dir: None,
            realtime: true,
            live_link: true,
            web_port_base: 18320,
            tft_png: true,
            max_seconds: None,
            stop_grace_secs: 6,
            extra_args: Vec::new(),
            web_dir: None,
            com_bridge: false,
        }
    }
}

/// A fully resolved engine invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub boot: BootMode,
    pub web_port: Option<u16>,
    pub flash_image: PathBuf,
    /// Folder passed as `--web-dir`; created before launch if missing.
    pub web_dir: Option<PathBuf>,
    /// `--flash-file`: the engine writes flash.bin back at exit, so stop gracefully.
    pub flash_persist: bool,
    /// `--serial-tcp PORT` passed (engine supports it and the device has a port).
    pub serial_tcp: Option<u16>,
    /// Grace period for a graceful stop.
    pub stop_grace: Duration,
    /// The engine has a portable quit (`--quit-on-stdin-eof`, WebSocket `quit`): it saves
    /// and exits 0 on every OS, so no signal is needed.
    pub portable_quit: bool,
    /// A helper to run alongside (the Windows COM-port relay), started after the engine
    /// and killed with it.
    pub relay: Option<crate::combridge::Relay>,
    /// Shown at power-on, e.g. why the COM relay is not running.
    pub relay_note: Option<String>,
    /// More lines for the console at power-on (network notes).
    pub notes: Vec<String>,
}

impl LaunchSpec {
    /// Map (profile, device, settings, BOOT state) to the esp32sim command line.
    /// See the README "Engine contract" table; keep the two in sync.
    pub fn build(
        profile: &Profile,
        device: &Device,
        settings: &EngineSettings,
        features: &EngineFeatures,
        boot: BootMode,
        web_port: Option<u16>,
        serial_tcp: Option<u16>,
    ) -> LaunchSpec {
        let mut a: Vec<String> = Vec::new();
        let mut push = |k: &str, v: String| {
            a.push(k.to_string());
            a.push(v);
        };
        push("--chip", profile.chip.clone());
        match profile.engine.board_config.as_deref().filter(|c| !c.trim().is_empty()) {
            Some(cfg) => push("--board-config", cfg.to_string()),
            None => push("--board", profile.engine.board.clone()),
        }
        push("--boot", "rom".into());
        if let Some(dir) = settings.rom_dir.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            push("--rom", path_str(&dir.join(&profile.engine.rom)));
        }
        // --flash-file loads flash.bin and writes it back at exit (firmware writes persist);
        // older engines only have --flash-image (read-only: writes are lost at power-off).
        let flash_persist = features.flash_file;
        push(if flash_persist { "--flash-file" } else { "--flash-image" }, path_str(&device.flash_path()));
        push("--flash-mb", profile.flash.size_mb.to_string());
        // The classic ESP32 model rejects --psram-mb entirely, even 0.
        if !profile.is_classic_esp32() && profile.psram_mb > 0 {
            push("--psram-mb", profile.psram_mb.to_string());
        }
        push("--strap", format!("0x{:02x}", boot.strap()));
        if let Some(mac) = device.meta.mac.as_ref().filter(|m| !m.is_empty()) {
            push("--mac", mac.clone());
        }
        if let Some(c) = &profile.engine.console {
            push("--console", c.clone());
        }
        if settings.tft_png && profile.display.is_some() {
            push("--tft-png", path_str(&device.screen_path()));
        }
        if let Some(s) = settings.max_seconds {
            push("--max-seconds", s.to_string());
        }
        let mut web_dir = None;
        if let Some(p) = web_port {
            push("--web", p.to_string());
            let dir = settings.web_dir.clone().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| device.dir.join("web"));
            push("--web-dir", path_str(&dir));
            web_dir = Some(dir);
        }
        let serial_tcp = serial_tcp.filter(|_| features.serial_tcp);
        if let Some(p) = serial_tcp {
            push("--serial-tcp", p.to_string());
        }
        let portable_quit = features.quit_on_stdin_eof;
        if portable_quit {
            a.push("--quit-on-stdin-eof".into());
        }
        a.push("--no-dump".into());
        if settings.realtime {
            a.push("--realtime".into());
        }
        a.extend(profile.engine.extra_args.iter().cloned());
        a.extend(settings.extra_args.iter().cloned());

        let mut env = Vec::new();
        if let Some(dir) = settings.rom_dir.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            env.push(("ESP32SIM_ROM_DIR".to_string(), path_str(dir)));
        }
        LaunchSpec { program: settings.engine_exe.clone(), args: a, env, boot, web_port, flash_image: device.flash_path(), web_dir, flash_persist, serial_tcp, portable_quit, relay: None, relay_note: None, notes: Vec::new(),
            stop_grace: Duration::from_secs(settings.stop_grace_secs.clamp(1, 120) as u64) }
    }

    /// Shell-style rendering for the UI and logs.
    pub fn display(&self) -> String {
        std::iter::once(path_str(&self.program))
            .chain(self.args.iter().cloned())
            .map(|s| if s.contains(' ') { format!("\"{s}\"") } else { s })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `display()` with Wi-Fi passwords replaced by `***`, for the console log.
    pub fn display_masked(&self) -> String {
        mask_secrets(&self.display())
    }
}

/// Hide `psk=...` values (in `--wifi ssid=..,psk=..`) from text shown on screen.
pub fn mask_secrets(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    let mut quoted = false;
    while let Some(i) = rest.find("psk=") {
        quoted ^= rest[..i].matches('"').count() % 2 == 1;
        out.push_str(&rest[..i + 4]);
        out.push_str("***");
        let tail = &rest[i + 4..];
        // A quoted argument (it contains spaces) ends at its closing quote.
        let end = if quoted { tail.find([',', '"']) } else { tail.find([',', ' ']) }.unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Port allocation for --web

fn reserved_ports() -> &'static Mutex<HashSet<u16>> {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    PORTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Pick a free loopback port at or above `base`, reserved until [`release_port`].
pub fn reserve_port(base: u16) -> Option<u16> {
    let mut set = reserved_ports().lock().unwrap();
    (base..base.saturating_add(500)).find(|p| !set.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).inspect(|p| {
        set.insert(*p);
    })
}

pub fn release_port(port: u16) {
    reserved_ports().lock().unwrap().remove(&port);
}

// ---------------------------------------------------------------------------
// ProcessBackend

static GENERATION: AtomicU64 = AtomicU64::new(1);

pub struct ProcessBackend {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    state: RunState,
    tx: Sender<EngineEvent>,
    rx: Receiver<EngineEvent>,
    waker: Waker,
    link: Option<WebLink>,
    frame_slot: Arc<Mutex<Option<Frame>>>,
    last_command: Option<String>,
    web_port: Option<u16>,
    generation: u64,
    /// Current run: launch spec facts needed later.
    boot: BootMode,
    flash_persist: bool,
    serial_tcp: Option<u16>,
    stop_grace: Duration,
    /// Set while Stopping: when to give up and kill.
    stop_deadline: Option<Instant>,
    /// EN with a strap change: start this once the old process is gone.
    pending_restart: Option<LaunchSpec>,
    portable_quit: bool,
    chip: ChipStatus,
    relay: Option<Child>,
    /// Our own DTR/RTS pulse is in flight until then: its EN reset is not "by a tool".
    own_pulse_until: Option<Instant>,
}

impl ProcessBackend {
    pub fn new(waker: Waker) -> ProcessBackend {
        let (tx, rx) = channel();
        ProcessBackend {
            child: None,
            stdin: None,
            state: RunState::Stopped,
            tx,
            rx,
            waker,
            link: None,
            frame_slot: Arc::new(Mutex::new(None)),
            last_command: None,
            web_port: None,
            generation: 0,
            boot: BootMode::Normal,
            flash_persist: false,
            serial_tcp: None,
            stop_grace: Duration::from_secs(6),
            stop_deadline: None,
            pending_restart: None,
            portable_quit: false,
            chip: ChipStatus::default(),
            relay: None,
            own_pulse_until: None,
        }
    }

    fn lab(&self, msg: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Console(Stream::Lab, msg.into()));
        (self.waker)();
    }

    /// Tear down after the child is gone (or killed). `graceful` = it exited by itself
    /// after a stop request.
    fn finish(&mut self, code: Option<i32>, how: Finish) {
        self.child = None;
        self.stop_relay();
        self.stdin = None;
        self.stop_deadline = None;
        if let Some(link) = self.link.take() {
            link.close();
        }
        if let Some(p) = self.web_port.take() {
            release_port(p);
        }
        let code_text = code.map_or("signal".to_string(), |c| format!("code {c}"));
        match how {
            Finish::Graceful => {
                self.state = RunState::Stopped;
                let saved = match (self.flash_persist, self.chip.flash_saved, code) {
                    (false, ..) => "",
                    (true, true, _) => ", flash.bin saved",
                    // exit 0 = the engine's own save-and-quit; 130 = saved after a signal
                    (true, false, Some(0) | Some(130)) => ", flash.bin saved (no confirmation line seen)",
                    (true, false, _) => ", but it did not report saving flash.bin",
                };
                self.lab(format!("[cyd-lab] power off: engine exited ({code_text}){saved}"));
            }
            Finish::Killed { waited } => {
                self.state = RunState::Stopped;
                if self.flash_persist {
                    self.lab(format!(
                        "[cyd-lab] power off: the engine did not stop within {:.0} s and was killed; flash writes since power-on were NOT saved{}",
                        waited.as_secs_f32(),
                        if cfg!(windows) && !self.portable_quit { " (this esp32sim build cannot stop gracefully on Windows; update it)" } else { "" }
                    ));
                } else {
                    self.lab("[cyd-lab] power off");
                }
            }
            Finish::Crashed => {
                self.state = RunState::Exited(code);
                self.lab(format!("[cyd-lab] engine exited ({code_text})"));
            }
        }
        self.flash_persist = false;
        self.serial_tcp = None;
        self.portable_quit = false;
    }

    fn kill_now(&mut self) -> Option<i32> {
        let child = self.child.as_mut()?;
        let _ = child.kill();
        child.wait().ok().and_then(|s| s.code())
    }

    /// Wait for the child to exit until `deadline`, then kill. Blocking.
    fn wait_or_kill(&mut self, deadline: Instant) {
        while let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => return self.finish(status.code(), Finish::Graceful),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    let code = self.kill_now();
                    return self.finish(code, Finish::Killed { waited: self.stop_grace });
                }
            }
        }
    }

    /// Follow the chip through console lines: download mode entered by esptool shows up
    /// in the state (and the status dot) just like one chosen with the BOOT button.
    fn observe(&mut self, events: &[EngineEvent]) {
        for e in events {
            if let EngineEvent::Console(Stream::Stdout | Stream::Stderr, line) = e {
                self.chip.observe(line);
                if self.own_pulse_until.is_some_and(|t| Instant::now() < t) {
                    self.chip.by_serial = false;
                }
            }
        }
        if let (RunState::Running(_), Some(mode)) = (self.state, self.chip.mode) {
            self.state = RunState::Running(mode);
        }
    }

    /// Start the helper next to the engine (Job object on Windows, so it also dies with
    /// cyd-lab). Its output goes to the console prefixed with its name.
    fn start_relay(&mut self, r: &crate::combridge::Relay) {
        let mut cmd = Command::new(&r.program);
        cmd.args(&r.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
        }
        match cmd.spawn() {
            Ok(mut child) => {
                #[cfg(windows)]
                winjob::assign(&child);
                let name = r.program.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "relay".into());
                for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>), child.stderr.take().map(|e| Box::new(e) as Box<dyn Read + Send>)].into_iter().flatten() {
                    spawn_prefixed_reader(out, format!("[{name}] "), self.tx.clone(), self.waker.clone());
                }
                self.lab(format!("[cyd-lab] relay started: {} (pid {})", r.label, child.id()));
                self.relay = Some(child);
            }
            Err(e) => self.lab(format!("[cyd-lab] could not start {}: {e} ({})", r.program.display(), r.label)),
        }
    }

    fn stop_relay(&mut self) {
        if let Some(mut c) = self.relay.take() {
            let _ = c.kill();
            let _ = c.wait();
            self.lab("[cyd-lab] relay stopped".to_string());
        }
    }

    /// The engine's process id while it runs. (Tests.)
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Is the helper process still running? (Tests.)
    #[cfg(test)]
    pub fn relay_running(&mut self) -> bool {
        self.relay.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn start_pending(&mut self) {
        if let Some(spec) = self.pending_restart.take() {
            if let Err(e) = self.start(&spec) {
                if let Some(p) = spec.web_port {
                    release_port(p);
                }
                self.lab(format!("[cyd-lab] restart failed: {e:#}"));
                let _ = self.tx.send(EngineEvent::Exited(None));
            }
        }
    }
}

enum Finish {
    Graceful,
    Killed { waited: Duration },
    Crashed,
}

/// Ask a child process to quit (Unix: SIGTERM, which esp32sim handles by saving
/// `--flash-file` and exiting with 130). Only used for engines without a portable quit.
fn signal_terminate(child: &Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child;
}

fn spawn_reader<R: Read + Send + 'static>(r: R, stream: Stream, tx: Sender<EngineEvent>, waker: Waker) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
                    if tx.send(EngineEvent::Console(stream.clone(), line)).is_err() {
                        break;
                    }
                    waker();
                }
            }
        }
    });
}

/// Like [`spawn_reader`] for a helper process: lines go to the lab stream with a prefix.
fn spawn_prefixed_reader(r: Box<dyn Read + Send>, prefix: String, tx: Sender<EngineEvent>, waker: Waker) {
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if tx.send(EngineEvent::Console(Stream::Lab, format!("{prefix}{}", line.trim_end()))).is_err() {
                break;
            }
            waker();
        }
    });
}

impl Backend for ProcessBackend {
    fn capabilities(&self) -> Capabilities {
        let linked = self.link.as_ref().is_some_and(|l| l.is_connected());
        let alive = self.child.is_some();
        Capabilities {
            live_screen: linked,
            touch: linked,
            png_snapshot: true,
            flash_writeback: alive && self.flash_persist,
            serial_tcp: self.serial_tcp.filter(|_| alive),
        }
    }

    fn start(&mut self, spec: &LaunchSpec) -> Result<()> {
        if self.child.is_some() {
            bail!("already running");
        }
        if !spec.flash_image.exists() {
            bail!("flash image {} is missing", spec.flash_image.display());
        }
        let mut cmd = Command::new(&spec.program);
        // stdin is a pipe we never write: closing it is one of the "please quit" signals.
        cmd.args(&spec.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(target_os = "linux")]
        {
            // If cyd-lab itself dies (crash, kill), the kernel sends the engine SIGTERM so it
            // does not linger as an orphan. (Fires when the spawning thread exits; we spawn
            // from the UI thread.) The engine saves flash.bin on SIGTERM and no longer
            // dies on the closed stderr pipe; with --quit-on-stdin-eof the closed stdin alone
            // already makes it save and exit 0.
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
        }
        if let Some(dir) = &spec.web_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        self.last_command = Some(spec.display());
        self.lab(format!("[cyd-lab] power on ({}): {}", if spec.boot == BootMode::Download { "BOOT held, download mode" } else { "normal boot" }, spec.display_masked()));
        let mut child = cmd.spawn().with_context(|| format!("starting engine {:?} (set the path in Settings)", spec.program))?;
        #[cfg(windows)]
        winjob::assign(&child);
        self.generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, Stream::Stdout, self.tx.clone(), self.waker.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, Stream::Stderr, self.tx.clone(), self.waker.clone());
        }
        self.stdin = child.stdin.take();
        self.child = Some(child);
        if let Some(note) = &spec.relay_note {
            self.lab(format!("[cyd-lab] {note}"));
        }
        for note in &spec.notes {
            self.lab(format!("[cyd-lab] {note}"));
        }
        if let Some(r) = &spec.relay {
            self.start_relay(r);
        }
        self.state = RunState::Running(spec.boot);
        self.boot = spec.boot;
        self.flash_persist = spec.flash_persist;
        self.serial_tcp = spec.serial_tcp;
        self.stop_grace = spec.stop_grace;
        self.portable_quit = spec.portable_quit;
        self.chip = ChipStatus { mode: Some(spec.boot), ..ChipStatus::default() };
        self.web_port = spec.web_port;
        if let Some(port) = spec.web_port {
            *self.frame_slot.lock().unwrap() = None;
            self.link = Some(WebLink::connect(port, self.frame_slot.clone(), self.tx.clone(), self.waker.clone()));
        }
        Ok(())
    }

    fn request_stop(&mut self) {
        if self.child.is_none() || self.stop_deadline.is_some() {
            return;
        }
        if !self.flash_persist {
            // Nothing to save: no reason to wait.
            let code = self.kill_now();
            return self.finish(code, Finish::Killed { waited: Duration::ZERO });
        }
        // The engine's own quit (WebSocket `{"t":"quit"}`, stdin EOF with
        // --quit-on-stdin-eof) saves and exits 0 on every OS. Older engines only save on
        // SIGINT/SIGTERM (Unix), so they get the signal as well.
        self.chip.flash_saved = false;
        if let Some(l) = self.link.as_ref().filter(|l| l.is_connected()) {
            l.send_json(WS_QUIT.to_string());
        }
        self.stdin = None; // EOF
        if !self.portable_quit {
            if let Some(child) = &self.child {
                signal_terminate(child);
            }
        }
        self.state = RunState::Stopping;
        self.stop_deadline = Some(Instant::now() + self.stop_grace);
        self.lab(format!("[cyd-lab] power off: asking the engine to save flash.bin and quit (up to {} s)…", self.stop_grace.as_secs()));
    }

    fn stop(&mut self) {
        self.pending_restart = None;
        self.request_stop();
        if let Some(deadline) = self.stop_deadline {
            self.wait_or_kill(deadline);
        }
        self.state = RunState::Stopped;
    }

    fn stop_within(&mut self, max: Duration) {
        self.pending_restart = None;
        self.request_stop();
        if self.child.is_some() {
            let cap = Instant::now() + max;
            let deadline = self.stop_deadline.map_or(cap, |d| d.min(cap));
            self.wait_or_kill(deadline);
        }
        self.stop_relay();
        self.state = RunState::Stopped;
    }

    fn restart(&mut self, spec: LaunchSpec) -> Result<()> {
        if self.child.is_none() {
            return self.start(&spec);
        }
        self.pending_restart = Some(spec);
        self.request_stop();
        if self.child.is_none() {
            // Killed immediately (no flash to save): start right away.
            self.start_pending();
        }
        Ok(())
    }

    fn soft_reset(&mut self) -> std::result::Result<(), TouchError> {
        match &self.link {
            Some(l) if l.is_connected() && self.child.is_some() => {
                l.send_json(r#"{"t":"reset"}"#.to_string());
                let mode = self.chip.mode.unwrap_or(self.boot);
                self.lab(format!("[cyd-lab] EN: in-process reset ({} kept)", if mode == BootMode::Download { "download mode" } else { "normal boot" }));
                Ok(())
            }
            _ => Err(TouchError::NotConnected),
        }
    }

    fn strap_reset(&mut self, boot: BootMode) -> std::result::Result<(), String> {
        let Some(port) = self.serial_tcp.filter(|_| self.child.is_some()) else {
            return Err("needs the engine's --serial-tcp".into());
        };
        crate::rfc2217::pulse_en(port, boot == BootMode::Download, self.tx.clone(), self.waker.clone()).map_err(|e| format!("serial port {port}: {e}"))?;
        self.boot = boot;
        self.own_pulse_until = Some(Instant::now() + Duration::from_secs(2));
        self.lab(format!(
            "[cyd-lab] EN with BOOT {}: in-process reset through the serial port's DTR/RTS ({}); a tool connected to port {port} is disconnected",
            if boot == BootMode::Download { "held" } else { "released" },
            if boot == BootMode::Download { "download mode" } else { "normal boot" }
        ));
        Ok(())
    }

    fn improv(&mut self, job: crate::improv::Job) -> std::result::Result<std::sync::mpsc::Receiver<crate::improv::Event>, String> {
        let Some(port) = self.serial_tcp.filter(|_| self.child.is_some()) else {
            return Err("needs the engine's --serial-tcp".into());
        };
        let (tx, rx) = std::sync::mpsc::channel();
        crate::improv::start(port, job, self.tx.clone(), self.waker.clone(), tx).map_err(|e| format!("serial port {port}: {e}"))?;
        Ok(rx)
    }

    fn set_boot_pin(&mut self, low: bool) -> std::result::Result<(), TouchError> {
        match &self.link {
            Some(l) if l.is_connected() => {
                l.send_json(format!(r#"{{"t":"btn","pin":0,"v":{}}}"#, low as u8));
                Ok(())
            }
            _ => Err(TouchError::NotConnected),
        }
    }

    fn chip_status(&self) -> ChipStatus {
        self.chip
    }

    fn state(&self) -> RunState {
        self.state
    }

    fn poll(&mut self) -> Vec<EngineEvent> {
        // Take what is queued *before* handling an exit, so the old run's link error
        // (the engine closing its socket on the way out) is not mistaken for the next run's.
        let stopping = self.stop_deadline.is_some();
        let mut events: Vec<EngineEvent> = self.rx.try_iter().filter(|e| !(stopping && matches!(e, EngineEvent::LinkDown(_)))).collect();
        self.observe(&events);
        let mut exited = None;
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some(status.code()),
                _ => {
                    if self.stop_deadline.is_some_and(|d| Instant::now() >= d) {
                        let code = self.kill_now();
                        self.finish(code, Finish::Killed { waited: self.stop_grace });
                        self.start_pending();
                    }
                }
            }
        }
        if let Some(code) = exited {
            let graceful = self.stop_deadline.is_some();
            self.finish(code, if graceful { Finish::Graceful } else { Finish::Crashed });
            if !graceful {
                let _ = self.tx.send(EngineEvent::Exited(code));
            }
            self.start_pending();
        }
        // Late output still in the pipes arrives on later polls.
        let late: Vec<EngineEvent> = self.rx.try_iter().collect();
        self.observe(&late);
        events.extend(late);
        events
    }

    fn take_frame(&mut self) -> Option<Frame> {
        self.frame_slot.lock().unwrap().take()
    }

    fn touch(&mut self, x: u16, y: u16, down: bool) -> std::result::Result<(), TouchError> {
        match &self.link {
            // A release is always queued while the run's link exists, even mid-reconnect:
            // it goes out as soon as the link is back, so the panel never stays pressed.
            Some(l) if l.is_connected() || !down => {
                l.send_json(format!(r#"{{"t":"touch","x":{x},"y":{y},"down":{}}}"#, down as u8));
                Ok(())
            }
            _ => Err(TouchError::NotConnected),
        }
    }

    fn serial_line(&mut self, line: &str) -> std::result::Result<(), TouchError> {
        match &self.link {
            Some(l) if l.is_connected() => {
                l.send_json(format!(r#"{{"t":"serial","src":"uart0","line":{}}}"#, json_string(line)));
                Ok(())
            }
            _ => Err(TouchError::NotConnected),
        }
    }

    fn last_command(&self) -> Option<String> {
        self.last_command.clone()
    }

    fn control(&mut self, json: String) -> std::result::Result<(), TouchError> {
        match &self.link {
            Some(l) if l.is_connected() => {
                l.send_json(json);
                Ok(())
            }
            _ => Err(TouchError::NotConnected),
        }
    }
}

impl Drop for ProcessBackend {
    fn drop(&mut self) {
        self.stop();
        self.stop_relay();
    }
}

/// The engine's quit message: save `--flash-file` and exit 0.
pub const WS_QUIT: &str = r#"{"t":"quit"}"#;

pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Serial / COM-port bridge

#[cfg_attr(not(test), allow(dead_code))] // planned_* / auto_reset: documentation + tests until automated
pub mod bridge {
    //! How a device reaches host serial tools.
    //!
    //! **Step 1 – TCP:** `esp32sim --serial-tcp PORT` serves UART0 as an RFC 2217
    //! server; DTR/RTS changes drive EN / GPIO0 through the classic auto-reset
    //! circuit ([`auto_reset`]). cyd-lab passes the flag when the engine build has
    //! it ([`super::EngineFeatures`]) and shows `rfc2217://127.0.0.1:PORT`, which
    //! esptool, pyserial, PlatformIO and `esphome` accept directly. cyd-lab's own
    //! EN-with-BOOT uses the same lines ([`crate::rfc2217`]).
    //!
    //! **Step 2 – a real COM port on Windows:** see [`crate::combridge`] (com0com
    //! pair + hub4com relay, started with the engine; compile-checked, not yet run).

    /// Host-side modem control lines, `true` = asserted (pin driven low on the USB chip).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ModemLines {
        pub dtr: bool,
        pub rts: bool,
    }

    /// Resulting chip pins, `true` = pulled low.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChipPins {
        pub en_low: bool,
        pub io0_low: bool,
    }

    /// The cross-coupled NPN pair found on ESP32 dev boards (and the CYD):
    /// EN is pulled low only when RTS is asserted and DTR is not; IO0 only when
    /// DTR is asserted and RTS is not. Both asserted = neither pulled (so a
    /// terminal that raises both lines does not hold the chip in reset).
    pub fn auto_reset(l: ModemLines) -> ChipPins {
        ChipPins { en_low: l.rts && !l.dtr, io0_low: l.dtr && !l.rts }
    }
}

// ---------------------------------------------------------------------------
// Windows: kill engines when cyd-lab dies

#[cfg(windows)]
mod winjob {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    fn job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            // The handle is deliberately never closed: the OS closes it when
            // cyd-lab exits, which is what kills the children.
            if ok == 0 { None } else { Some(Job(h)) }
        })
        .as_ref()
    }

    pub fn assign(child: &std::process::Child) {
        if let Some(j) = job() {
            unsafe {
                AssignProcessToJobObject(j.0, child.as_raw_handle() as HANDLE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceMeta;
    use crate::profile::ProfileLibrary;

    fn fixture(profile_id: &str) -> (Profile, Device) {
        let profile = ProfileLibrary::load(None).get(profile_id).unwrap().clone();
        let device = Device {
            id: "dev1".into(),
            dir: PathBuf::from("/data/devices/dev1"),
            meta: DeviceMeta {
                name: "Dev 1".into(),
                profile: profile.id.clone(),
                com_port: Some("COM20".into()),
                mac: Some("24:0A:C4:01:02:03".into()),
                serial_tcp_port: Some(22170),
                notes: String::new(),
                view_rotation: 0,
                created: 0,
                firmware_label: String::new(),
                details_open: false,
                network: Default::default(),
                screen_window: Default::default(),
                guide: None,
                pane_tab: None,
            },
        };
        (profile, device)
    }

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str())
    }

    #[test]
    fn strap_selection() {
        assert_eq!(BootMode::from_boot_held(true).strap(), 0x0f);
        assert_eq!(BootMode::from_boot_held(false).strap(), 0x13);
        let (p, d) = fixture("esp32-2432s028r");
        let s = EngineSettings::default();
        let normal = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Normal, None, None);
        let dl = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Download, None, None);
        assert_eq!(value_of(&normal.args, "--strap"), Some("0x13"));
        assert_eq!(value_of(&dl.args, "--strap"), Some("0x0f"));
    }

    #[test]
    fn cyd_command_line() {
        let (p, d) = fixture("esp32-2432s028r");
        let s = EngineSettings { engine_exe: "C:/emu/esp32sim.exe".into(), rom_dir: Some("/roms".into()), ..Default::default() };
        let spec = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Normal, Some(18320), None);
        let a = &spec.args;
        assert_eq!(spec.program, PathBuf::from("C:/emu/esp32sim.exe"));
        assert_eq!(value_of(a, "--chip"), Some("esp32"));
        assert_eq!(value_of(a, "--board"), Some("cyd-2432s028r"));
        assert_eq!(value_of(a, "--boot"), Some("rom"));
        assert_eq!(PathBuf::from(value_of(a, "--rom").unwrap()), PathBuf::from("/roms").join("esp32_rev300_rom.elf"));
        assert_eq!(PathBuf::from(value_of(a, "--flash-image").unwrap()), d.flash_path());
        assert_eq!(value_of(a, "--flash-mb"), Some("4"));
        assert_eq!(value_of(a, "--mac"), Some("24:0A:C4:01:02:03"));
        assert_eq!(value_of(a, "--console"), Some("uart0"));
        assert_eq!(value_of(a, "--web"), Some("18320"));
        assert_eq!(PathBuf::from(value_of(a, "--web-dir").unwrap()), d.dir.join("web"));
        assert_eq!(PathBuf::from(value_of(a, "--tft-png").unwrap()), d.screen_path());
        assert!(a.contains(&"--no-dump".to_string()));
        assert!(a.contains(&"--realtime".to_string()));
        assert!(!a.contains(&"--psram-mb".to_string()), "classic ESP32 rejects --psram-mb");
        assert!(!a.contains(&"--max-seconds".to_string()), "interactive devices run until powered off");
        assert!(!a.contains(&"--flash-at".to_string()));
        assert_eq!(spec.env, vec![("ESP32SIM_ROM_DIR".to_string(), "/roms".to_string())]);
        // every flag has its value (no flag directly followed by another flag where a value is expected)
        for f in ["--chip", "--board", "--boot", "--rom", "--flash-image", "--flash-mb", "--strap", "--web"] {
            assert!(!value_of(a, f).unwrap().starts_with("--"), "{f}");
        }
    }

    #[test]
    fn optional_flags() {
        let (p, d) = fixture("ttgo-tdisplay");
        let s = EngineSettings {
            rom_dir: None,
            realtime: false,
            tft_png: false,
            max_seconds: Some(5),
            extra_args: vec!["--no-jit".into()],
            ..Default::default()
        };
        let spec = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Normal, None, None);
        let a = &spec.args;
        assert_eq!(value_of(a, "--board"), Some("tdisplay"));
        assert_eq!(value_of(a, "--max-seconds"), Some("5"));
        assert!(!a.iter().any(|x| x == "--rom" || x == "--web" || x == "--realtime" || x == "--tft-png"));
        assert_eq!(a.last().map(String::as_str), Some("--no-jit"));
        assert!(spec.env.is_empty());
    }

    #[test]
    fn psram_only_for_non_classic() {
        let (mut p, d) = fixture("ttgo-tdisplay");
        p.chip = "s3".into();
        p.psram_mb = 8;
        let spec = LaunchSpec::build(&p, &d, &EngineSettings::default(), &EngineFeatures::default(), BootMode::Normal, None, None);
        assert_eq!(value_of(&spec.args, "--psram-mb"), Some("8"));
    }

    #[test]
    fn masks_wifi_password() {
        assert_eq!(mask_secrets("x --wifi ssid=a,psk=s3cret --net nat"), "x --wifi ssid=a,psk=*** --net nat");
        assert_eq!(mask_secrets("x \"--wifi ssid=a b,psk=p w\""), "x \"--wifi ssid=a b,psk=***\"");
        assert_eq!(mask_secrets("--wifi ssid=a,psk=1,channel=6"), "--wifi ssid=a,psk=***,channel=6");
        assert_eq!(mask_secrets("no secrets"), "no secrets");
        assert_eq!(mask_secrets("\"--wifi ssid=x y,psk=a b\" --wifi ssid=z,psk=q --net nat"), "\"--wifi ssid=x y,psk=***\" --wifi ssid=z,psk=*** --net nat");
    }

    #[test]
    fn display_quotes_spaces() {
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = PathBuf::from("/my data/dev1");
        let spec = LaunchSpec::build(&p, &d, &EngineSettings::default(), &EngineFeatures::default(), BootMode::Normal, None, None);
        assert!(spec.display().contains("\"/my data/dev1"));
    }

    #[test]
    fn auto_reset_circuit() {
        use bridge::*;
        let p = |dtr, rts| auto_reset(ModemLines { dtr, rts });
        assert_eq!(p(false, false), ChipPins { en_low: false, io0_low: false });
        assert_eq!(p(false, true), ChipPins { en_low: true, io0_low: false });
        assert_eq!(p(true, false), ChipPins { en_low: false, io0_low: true });
        assert_eq!(p(true, true), ChipPins { en_low: false, io0_low: false });
    }

    #[test]
    fn feature_detection() {
        // help text alone
        let f = EngineFeatures::from_probe("  --serial-tcp PORT  RFC 2217 bridge", b"");
        assert!(f.serial_tcp && !f.flash_file);
        // flag literals in the binary (esp32sim --help does not list --flash-file)
        let f = EngineFeatures::from_probe("Every flag: docs/cli.md", b"\x00--flash-file\x00--web-dir\x00");
        assert!(f.flash_file && f.web_dir && !f.serial_tcp);
        assert_eq!(EngineFeatures::from_probe("", b"--flash-image"), EngineFeatures { found: true, ..Default::default() });
    }

    #[test]
    fn feature_detection_current_help() {
        let help = "  --flash-file F  read-write flash\n  --serial-tcp PORT  raw or RFC 2217\n  --quit-on-stdin-eof  save and exit 0\n  --web-dir D";
        let f = EngineFeatures::from_probe(help, b"");
        assert!(f.flash_file && f.serial_tcp && f.quit_on_stdin_eof && f.web_dir);
        assert!(!f.wifi && !f.mdns_relay);
        let f = EngineFeatures::from_probe("  --wifi SPEC\n  --hostfwd tcp:..\n", b"");
        assert!(f.wifi && f.hostfwd && !f.mdns_relay);
        assert!(EngineFeatures::from_probe("  --mdns-relay MODE\n", b"").mdns_relay);
        // esp32sim e16060c0's --help.
        let help = "  --wifi ssid=…,psk=…   a virtual access point\n                        (no psk: open; any SSID or passphrase, \\, for a comma; `any` or ssid=*: the SSID the firmware asks for)\n";
        let f = EngineFeatures::from_probe(help, b"");
        assert!(f.wifi && f.wifi_any && f.wifi_escapes && f.net().wifi_any && f.net().wifi_escapes);
        let old = EngineFeatures::from_probe("  --wifi ssid=…,psk=…   a virtual access point\n", b"");
        assert!(old.wifi && !old.wifi_any && !old.wifi_escapes);
        assert!(!f.wifi_live && !old.wifi_live);
        // esp32sim 13a9106e: the access point can be changed live ({"t":"wifi"}).
        let help = format!("{help}                        (change it while running with the web channel's {{\"t\":\"wifi\"}}: the firmware is disconnected and rejoins)\n");
        assert!(EngineFeatures::from_probe(&help, b"").wifi_live);
    }

    #[test]
    fn chip_status_from_console() {
        let mut c = ChipStatus { mode: Some(BootMode::Normal), ..Default::default() };
        c.observe("[emu] serial: EN reset, IO0 low: download mode");
        assert_eq!((c.mode, c.by_serial), (Some(BootMode::Download), true));
        c.observe("rst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)");
        assert_eq!(c.mode, Some(BootMode::Normal));
        c.observe("[emu] serial: chip stopped (illegal instruction) at t=1.2s; waiting for an EN reset from the client");
        assert!(c.halted);
        c.observe("[emu] serial: EN reset");
        assert_eq!((c.mode, c.halted), (Some(BootMode::Normal), false));
        assert!(!c.flash_saved);
        c.observe("[emu] flash: wrote 4194304 bytes to /x/flash.bin (--flash-file)");
        assert!(c.flash_saved);
        assert_eq!(rom_boot_mode("rst:0x1 (POWERON_RESET),boot:0xf (UART_BOOT(UART0))"), Some(BootMode::Download));
        assert_eq!(rom_boot_mode("rst:0x1 (POWERON_RESET),boot:0x3 (DOWNLOAD_BOOT(UART0/UART1/SDIO_REI_REO_V2))"), Some(BootMode::Download));
        assert_eq!(rom_boot_mode("rst:0x1,boot:0x0f"), Some(BootMode::Download));
        assert_eq!(rom_boot_mode("rst:0x1,boot:0x13"), Some(BootMode::Normal));
        assert_eq!(rom_boot_mode("rst:0x1 (POWERON_RESET)"), None);
    }

    #[test]
    fn feature_probe_of_missing_exe() {
        assert!(!EngineFeatures::probe(Path::new("/definitely/not/esp32sim")).found);
    }

    #[test]
    fn flash_file_and_serial_tcp_flags() {
        let (p, d) = fixture("esp32-2432s028r");
        let s = EngineSettings::default();
        let all = EngineFeatures { found: true, flash_file: true, serial_tcp: true, web_dir: true, quit_on_stdin_eof: true, ..Default::default() };
        let spec = LaunchSpec::build(&p, &d, &s, &all, BootMode::Normal, None, Some(22171));
        assert!(spec.args.iter().any(|a| a == "--quit-on-stdin-eof") && spec.portable_quit);
        assert_eq!(PathBuf::from(value_of(&spec.args, "--flash-file").unwrap()), d.flash_path());
        assert!(!spec.args.iter().any(|a| a == "--flash-image"));
        assert_eq!(value_of(&spec.args, "--serial-tcp"), Some("22171"));
        assert!(spec.flash_persist);
        assert_eq!(spec.serial_tcp, Some(22171));
        assert_eq!(spec.stop_grace, Duration::from_secs(6));
        // an engine without --serial-tcp never gets the flag, even with a port configured
        let old = EngineFeatures { found: true, flash_file: true, ..Default::default() };
        let spec = LaunchSpec::build(&p, &d, &s, &old, BootMode::Normal, None, Some(22171));
        assert!(!spec.args.iter().any(|a| a == "--serial-tcp"));
        assert!(!spec.args.iter().any(|a| a == "--quit-on-stdin-eof") && !spec.portable_quit);
        assert_eq!(spec.serial_tcp, None);
        // and an old engine falls back to the read-only image
        let spec = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Normal, None, None);
        assert!(value_of(&spec.args, "--flash-image").is_some() && !spec.flash_persist);
    }

    #[cfg(unix)]
    #[test]
    fn relay_runs_with_the_engine() {
        // Stand-ins: `sleep` as both the engine and the relay (the COM relay is hub4com on Windows).
        let tmp = tempfile::tempdir().unwrap();
        let (p, mut d) = fixture("esp32-2432s028r");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let mut spec = LaunchSpec::build(&p, &d, &EngineSettings::default(), &EngineFeatures::default(), BootMode::Normal, None, None);
        spec.program = PathBuf::from("sleep");
        spec.args = vec!["30".into()];
        spec.relay = Some(crate::combridge::Relay { program: PathBuf::from("sleep"), args: vec!["30".into()], label: "test relay".into() });
        spec.relay_note = Some("note shown at power-on".into());
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        b.start(&spec).unwrap();
        assert!(b.relay_running());
        b.stop();
        assert!(!b.relay_running() && b.relay.is_none());
        let lines: Vec<String> = b.poll().into_iter().filter_map(|e| if let EngineEvent::Console(Stream::Lab, l) = e { Some(l) } else { None }).collect();
        assert!(lines.iter().any(|l| l.contains("note shown at power-on")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("relay started: test relay")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("relay stopped")), "{lines:?}");
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_string("a\"b\\c\n"), r#""a\"b\\c\n""#);
    }

    #[test]
    fn start_fails_cleanly_without_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let s = EngineSettings { engine_exe: tmp.path().join("no-such-esp32sim"), ..Default::default() };
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        let spec = LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Normal, None, None);
        assert!(b.start(&spec).is_err());
        assert_eq!(b.state(), RunState::Stopped);
        assert!(matches!(b.touch(1, 2, true), Err(TouchError::NotConnected)));
    }

    #[cfg(unix)]
    #[test]
    fn process_lifecycle_with_fake_engine() {
        // A shell script standing in for esp32sim: prints its args and a console line, then waits.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-esp32sim");
        std::fs::write(&exe, "#!/bin/sh\necho \"args: $*\"\necho 'rst:0x1 (POWERON_RESET)' \necho err >&2\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let s = EngineSettings { engine_exe: exe, ..Default::default() };
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        b.start(&LaunchSpec::build(&p, &d, &s, &EngineFeatures::default(), BootMode::Download, None, None)).unwrap();
        assert_eq!(b.state(), RunState::Running(BootMode::Download));
        let mut lines = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lines.len() < 4 && std::time::Instant::now() < deadline {
            for e in b.poll() {
                if let EngineEvent::Console(_, l) = e {
                    lines.push(l);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(lines.iter().any(|l| l.contains("--strap 0x0f")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("POWERON_RESET")), "{lines:?}");
        assert!(lines.iter().any(|l| l == "err"), "{lines:?}");
        b.stop();
        assert_eq!(b.state(), RunState::Stopped);
    }

    #[cfg(unix)]
    fn fake_engine(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("fake-esp32sim");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    #[cfg(unix)]
    fn run_until_stopped(b: &mut ProcessBackend, secs: u64) -> Vec<String> {
        let mut lines = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            for e in b.poll() {
                if let EngineEvent::Console(_, l) = e {
                    lines.push(l);
                }
            }
            if b.state() == RunState::Stopped && lines.iter().any(|l| l.contains("power off:") && !l.contains("asking")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        lines
    }

    #[cfg(unix)]
    fn persist_spec(p: &Profile, d: &Device, exe: PathBuf, grace: u32) -> LaunchSpec {
        let s = EngineSettings { engine_exe: exe, stop_grace_secs: grace, ..Default::default() };
        let f = EngineFeatures { found: true, flash_file: true, ..Default::default() };
        LaunchSpec::build(p, d, &s, &f, BootMode::Normal, None, None)
    }

    #[cfg(unix)]
    #[test]
    fn graceful_stop_lets_the_engine_save() {
        // Like esp32sim with --flash-file: SIGTERM -> write flash -> exit 0.
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_engine(tmp.path(), "trap 'echo \"[emu] flash: wrote 16 bytes\"; exit 0' TERM\necho ready\nwhile :; do sleep 0.05; done");
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        b.start(&persist_spec(&p, &d, exe, 5)).unwrap();
        assert!(b.capabilities().flash_writeback);
        std::thread::sleep(Duration::from_millis(200)); // let the trap install
        b.request_stop();
        assert_eq!(b.state(), RunState::Stopping);
        let lines = run_until_stopped(&mut b, 8);
        assert_eq!(b.state(), RunState::Stopped, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("flash: wrote")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("flash.bin saved")), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn stubborn_engine_is_killed_after_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_engine(tmp.path(), "trap '' TERM\nwhile :; do sleep 0.05; done");
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        b.start(&persist_spec(&p, &d, exe, 1)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        b.request_stop();
        let lines = run_until_stopped(&mut b, 6);
        assert_eq!(b.state(), RunState::Stopped);
        assert!(t.elapsed() >= Duration::from_millis(900));
        assert!(lines.iter().any(|l| l.contains("NOT saved")), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn restart_waits_for_the_save_then_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = fake_engine(tmp.path(), "trap 'echo saved; exit 0' TERM\necho \"args: $*\"\nwhile :; do sleep 0.05; done");
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = tmp.path().to_path_buf();
        std::fs::write(d.flash_path(), [0xFFu8; 16]).unwrap();
        let mut b = ProcessBackend::new(Arc::new(|| {}));
        b.start(&persist_spec(&p, &d, exe.clone(), 5)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let mut spec = persist_spec(&p, &d, exe, 5);
        spec.boot = BootMode::Download;
        b.restart(spec).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut lines = Vec::new();
        while Instant::now() < deadline && b.state() != RunState::Running(BootMode::Download) {
            lines.extend(b.poll().into_iter().filter_map(|e| if let EngineEvent::Console(_, l) = e { Some(l) } else { None }));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(b.state(), RunState::Running(BootMode::Download), "{lines:?}");
        assert!(lines.iter().any(|l| l == "saved"), "{lines:?}");
        b.stop();
    }
}
