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
//! * **lifetime** – `stop()` kills the child; `Drop` does too, and on Windows
//!   every child is put in a Job object with KILL_ON_JOB_CLOSE so engines die
//!   even if cyd-lab crashes.
//!
//! A future `LibraryBackend` can link the engine crates directly and implement
//! the same trait; the UI would not change.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    /// Engine writes flash changes back to `flash.bin`. Not in esp32sim yet.
    pub flash_writeback: bool,
    /// Device is reachable on a host COM port. Not in esp32sim yet.
    pub com_bridge: bool,
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
    /// Power off.
    fn stop(&mut self);
    /// EN button: today a full process restart, so a changed BOOT strap takes effect.
    fn reset(&mut self, spec: &LaunchSpec) -> Result<()> {
        self.stop();
        self.start(spec)
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
    /// Stop after N emulated seconds (`--max-seconds`). None = run until powered off.
    pub max_seconds: Option<u32>,
    /// Extra arguments for every device.
    pub extra_args: Vec<String>,
    /// Static files for the engine's own browser UI (`--web-dir`). cyd-lab only uses the
    /// WebSocket, so by default it points the engine at an empty per-device folder: without
    /// `--web-dir` esp32sim looks for `web/` next to its exe and panics if it is missing.
    pub web_dir: Option<PathBuf>,
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
            extra_args: Vec::new(),
            web_dir: None,
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
}

impl LaunchSpec {
    /// Map (profile, device, settings, BOOT state) to the esp32sim command line.
    /// See the README "Engine contract" table; keep the two in sync.
    pub fn build(profile: &Profile, device: &Device, settings: &EngineSettings, boot: BootMode, web_port: Option<u16>) -> LaunchSpec {
        let mut a: Vec<String> = Vec::new();
        let mut push = |k: &str, v: String| {
            a.push(k.to_string());
            a.push(v);
        };
        push("--chip", profile.chip.clone());
        push("--board", profile.engine.board.clone());
        push("--boot", "rom".into());
        if let Some(dir) = settings.rom_dir.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            push("--rom", path_str(&dir.join(&profile.engine.rom)));
        }
        push("--flash-image", path_str(&device.flash_path()));
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
        LaunchSpec { program: settings.engine_exe.clone(), args: a, env, boot, web_port, flash_image: device.flash_path(), web_dir }
    }

    /// Shell-style rendering for the UI and logs.
    pub fn display(&self) -> String {
        std::iter::once(path_str(&self.program))
            .chain(self.args.iter().cloned())
            .map(|s| if s.contains(' ') { format!("\"{s}\"") } else { s })
            .collect::<Vec<_>>()
            .join(" ")
    }
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
    state: RunState,
    tx: Sender<EngineEvent>,
    rx: Receiver<EngineEvent>,
    waker: Waker,
    link: Option<WebLink>,
    frame_slot: Arc<Mutex<Option<Frame>>>,
    last_command: Option<String>,
    web_port: Option<u16>,
    generation: u64,
}

impl ProcessBackend {
    pub fn new(waker: Waker) -> ProcessBackend {
        let (tx, rx) = channel();
        ProcessBackend {
            child: None,
            state: RunState::Stopped,
            tx,
            rx,
            waker,
            link: None,
            frame_slot: Arc::new(Mutex::new(None)),
            last_command: None,
            web_port: None,
            generation: 0,
        }
    }

    fn lab(&self, msg: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Console(Stream::Lab, msg.into()));
        (self.waker)();
    }
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

impl Backend for ProcessBackend {
    fn capabilities(&self) -> Capabilities {
        let linked = self.link.as_ref().is_some_and(|l| l.is_connected());
        Capabilities { live_screen: linked, touch: linked, png_snapshot: true, flash_writeback: false, com_bridge: false }
    }

    fn start(&mut self, spec: &LaunchSpec) -> Result<()> {
        if self.child.is_some() {
            bail!("already running");
        }
        if !spec.flash_image.exists() {
            bail!("flash image {} is missing", spec.flash_image.display());
        }
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        if let Some(dir) = &spec.web_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        self.last_command = Some(spec.display());
        self.lab(format!("[cyd-lab] power on ({}): {}", if spec.boot == BootMode::Download { "BOOT held, download mode" } else { "normal boot" }, spec.display()));
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
        self.child = Some(child);
        self.state = RunState::Running(spec.boot);
        self.web_port = spec.web_port;
        if let Some(port) = spec.web_port {
            *self.frame_slot.lock().unwrap() = None;
            self.link = Some(WebLink::connect(port, self.frame_slot.clone(), self.tx.clone(), self.waker.clone()));
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(link) = self.link.take() {
            link.close();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.lab("[cyd-lab] power off");
        }
        if let Some(p) = self.web_port.take() {
            release_port(p);
        }
        self.state = RunState::Stopped;
    }

    fn state(&self) -> RunState {
        self.state
    }

    fn poll(&mut self) -> Vec<EngineEvent> {
        let mut events: Vec<EngineEvent> = self.rx.try_iter().collect();
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let code = status.code();
                self.child = None;
                if let Some(link) = self.link.take() {
                    link.close();
                }
                if let Some(p) = self.web_port.take() {
                    release_port(p);
                }
                self.state = RunState::Exited(code);
                // Late output still in the pipes arrives on later polls.
                events.extend(self.rx.try_iter());
                events.push(EngineEvent::Console(Stream::Lab, format!("[cyd-lab] engine exited ({})", code.map_or("signal".into(), |c| format!("code {c}")))));
                events.push(EngineEvent::Exited(code));
            }
        }
        events
    }

    fn take_frame(&mut self) -> Option<Frame> {
        self.frame_slot.lock().unwrap().take()
    }

    fn touch(&mut self, x: u16, y: u16, down: bool) -> std::result::Result<(), TouchError> {
        match &self.link {
            Some(l) if l.is_connected() => {
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
}

impl Drop for ProcessBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

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
// COM-port bridge (designed, not implemented: the engine has no serial port API yet)

#[cfg_attr(not(test), allow(dead_code))] // auto_reset/ModemLines: used by the future bridge
pub mod bridge {
    //! How a device will appear on a host COM port.
    //!
    //! Plan (Windows): a com0com / "null-modem emulator" pair per device, e.g.
    //! `COM20 <-> CNCB20`. Tools (esptool, Arduino IDE, `esphome run`) open
    //! `COM20`; cyd-lab opens the other end and shovels bytes to/from the
    //! engine's UART0, honouring baud changes. The modem-control lines are
    //! mapped through the classic two-transistor auto-reset circuit
    //! ([`auto_reset`]) so esptool's DTR/RTS dance drives EN and BOOT exactly
    //! like on a CH340 board. Needs from the engine: an 8-bit-clean UART0
    //! byte channel and EN / GPIO0 control without restarting the process.

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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum BridgeState {
        NotBridged,
    }

    /// Placeholder bridge: always reports "not bridged yet".
    pub struct NullBridge {
        pub port_name: Option<String>,
    }

    impl NullBridge {
        pub fn state(&self) -> BridgeState {
            BridgeState::NotBridged
        }
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
                notes: String::new(),
                view_rotation: 0,
                created: 0,
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
        let normal = LaunchSpec::build(&p, &d, &s, BootMode::Normal, None);
        let dl = LaunchSpec::build(&p, &d, &s, BootMode::Download, None);
        assert_eq!(value_of(&normal.args, "--strap"), Some("0x13"));
        assert_eq!(value_of(&dl.args, "--strap"), Some("0x0f"));
    }

    #[test]
    fn cyd_command_line() {
        let (p, d) = fixture("esp32-2432s028r");
        let s = EngineSettings { engine_exe: "C:/emu/esp32sim.exe".into(), rom_dir: Some("/roms".into()), ..Default::default() };
        let spec = LaunchSpec::build(&p, &d, &s, BootMode::Normal, Some(18320));
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
        let spec = LaunchSpec::build(&p, &d, &s, BootMode::Normal, None);
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
        let spec = LaunchSpec::build(&p, &d, &EngineSettings::default(), BootMode::Normal, None);
        assert_eq!(value_of(&spec.args, "--psram-mb"), Some("8"));
    }

    #[test]
    fn display_quotes_spaces() {
        let (p, mut d) = fixture("ttgo-tdisplay");
        d.dir = PathBuf::from("/my data/dev1");
        let spec = LaunchSpec::build(&p, &d, &EngineSettings::default(), BootMode::Normal, None);
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
        let spec = LaunchSpec::build(&p, &d, &s, BootMode::Normal, None);
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
        b.start(&LaunchSpec::build(&p, &d, &s, BootMode::Download, None)).unwrap();
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
}
