//! Improv Wi-Fi over serial (<https://www.improv-wifi.com/serial/>): the way ESP Web Tools
//! hands a freshly flashed board its Wi-Fi details over USB. ESPHome, WLED and other
//! firmware listen for it on UART0, so cyd-lab can do the same over the engine's
//! `--serial-tcp` port.
//!
//! A packet is `IMPROV`, version 1, type, length, data, and a checksum (the low byte of the
//! sum of every byte before it). The board answers with its state (ready, provisioning,
//! provisioned), an error, or an RPC result whose first string for "send Wi-Fi" is the
//! address of its web page.
//!
//! The session connects as a plain byte client (no Telnet, so the reset lines stay
//! released). The engine serves one client at a time: connecting replaces a connected
//! tool, and while connected the chip's UART0 output comes here instead of the engine's
//! stdout, so everything that is not an Improv packet is forwarded to the console.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::engine::{EngineEvent, Stream, Waker};

pub const HEADER: &[u8; 6] = b"IMPROV";
pub const VERSION: u8 = 1;

pub const TYPE_STATE: u8 = 0x01;
pub const TYPE_ERROR: u8 = 0x02;
pub const TYPE_RPC: u8 = 0x03;
pub const TYPE_RESULT: u8 = 0x04;

pub const CMD_WIFI: u8 = 0x01;
pub const CMD_STATE: u8 = 0x02;
pub const CMD_INFO: u8 = 0x03;

/// The board's provisioning state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Ready,
    Provisioning,
    Provisioned,
    Other(u8),
}

impl State {
    pub fn from_byte(b: u8) -> State {
        match b {
            0x00 => State::Stopped,
            0x02 => State::Ready,
            0x03 => State::Provisioning,
            0x04 => State::Provisioned,
            x => State::Other(x),
        }
    }
    pub fn label(self) -> String {
        match self {
            State::Stopped => "stopped".into(),
            State::Ready => "waiting for Wi-Fi details".into(),
            State::Provisioning => "connecting to Wi-Fi…".into(),
            State::Provisioned => "connected to Wi-Fi".into(),
            State::Other(x) => format!("state 0x{x:02x}"),
        }
    }
}

pub fn error_label(code: u8) -> String {
    match code {
        0x00 => "no error".into(),
        0x01 => "invalid RPC packet".into(),
        0x02 => "unknown RPC command".into(),
        0x03 => "unable to connect to the network (wrong name or password, or the emulated access point is off)".into(),
        0x04 => "not authorised".into(),
        0x05 => "bad hostname".into(),
        0xFF => "unknown error".into(),
        x => format!("error 0x{x:02x}"),
    }
}

/// One packet from the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    State(State),
    Error(u8),
    /// RPC result: the command it answers and its strings.
    Result { cmd: u8, strings: Vec<String> },
    Other { kind: u8, data: Vec<u8> },
}

/// A whole packet: header, version, type, length, data, checksum.
pub fn encode(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(data.len() + 10);
    p.extend_from_slice(HEADER);
    p.extend([VERSION, kind, data.len() as u8]);
    p.extend_from_slice(data);
    let sum = p.iter().fold(0u32, |a, &b| a + b as u32);
    p.push((sum & 0xFF) as u8);
    p
}

/// An RPC command packet: `[cmd, len, data]`.
pub fn rpc(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut d = vec![cmd, data.len() as u8];
    d.extend_from_slice(data);
    encode(TYPE_RPC, &d)
}

pub fn request_state() -> Vec<u8> {
    rpc(CMD_STATE, &[])
}

pub fn request_info() -> Vec<u8> {
    rpc(CMD_INFO, &[])
}

/// "Send Wi-Fi settings": length-prefixed SSID and password. Each is at most 255 bytes
/// and the whole packet data at most 255, which Wi-Fi limits (32 / 64) keep well inside.
pub fn wifi_settings(ssid: &str, password: &str) -> Result<Vec<u8>, String> {
    let (s, p) = (ssid.as_bytes(), password.as_bytes());
    if s.is_empty() || s.len() > 32 {
        return Err("the network name must be 1 to 32 bytes".into());
    }
    if p.len() > 64 {
        return Err("the password must be at most 64 bytes".into());
    }
    let mut d = vec![s.len() as u8];
    d.extend_from_slice(s);
    d.push(p.len() as u8);
    d.extend_from_slice(p);
    Ok(rpc(CMD_WIFI, &d))
}

fn decode(kind: u8, data: &[u8]) -> Packet {
    match kind {
        TYPE_STATE if !data.is_empty() => Packet::State(State::from_byte(data[0])),
        TYPE_ERROR if !data.is_empty() => Packet::Error(data[0]),
        TYPE_RESULT if data.len() >= 2 => {
            let cmd = data[0];
            let end = (2 + data[1] as usize).min(data.len());
            let mut strings = Vec::new();
            let mut i = 2;
            while i < end {
                let n = data[i] as usize;
                let s = &data[(i + 1).min(end)..(i + 1 + n).min(end)];
                strings.push(String::from_utf8_lossy(s).into_owned());
                i += 1 + n;
            }
            Packet::Result { cmd, strings }
        }
        _ => Packet::Other { kind, data: data.to_vec() },
    }
}

/// Splits a byte stream into Improv packets and everything else (the firmware's log).
/// A packet split over reads is kept until it completes; a bad checksum lets the bytes
/// through as text.
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    /// Feed bytes; returns (text bytes, packets).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<Packet>) {
        self.pending.extend_from_slice(input);
        let (mut text, mut packets) = (Vec::new(), Vec::new());
        let mut i = 0;
        let buf = std::mem::take(&mut self.pending);
        while i < buf.len() {
            if buf[i] != b'I' {
                text.push(buf[i]);
                i += 1;
                continue;
            }
            let rest = &buf[i..];
            // Could this still become a packet? Keep it for the next read.
            let head = rest.len().min(HEADER.len());
            if rest[..head] != HEADER[..head] {
                text.push(buf[i]);
                i += 1;
                continue;
            }
            if rest.len() < 9 {
                self.pending.extend_from_slice(rest);
                return (text, packets);
            }
            let len = rest[8] as usize;
            if rest.len() < 10 + len {
                self.pending.extend_from_slice(rest);
                return (text, packets);
            }
            let sum = rest[..9 + len].iter().fold(0u32, |a, &b| a + b as u32);
            if rest[6] == VERSION && rest[9 + len] == (sum & 0xFF) as u8 {
                packets.push(decode(rest[7], &rest[9..9 + len]));
                i += 10 + len;
            } else {
                text.push(buf[i]);
                i += 1;
            }
        }
        (text, packets)
    }
}

/// What a session reports to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    State(State),
    Error(u8),
    /// Device info: firmware, version, chip, device name.
    Info(Vec<String>),
    /// The address the board gave after joining (its web page).
    Url(String),
    /// The session ended. `Err` = could not talk to the port or no answer.
    Done(Result<String, String>),
}

/// What to do in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Ask for the state and device info; a quiet board means no Improv.
    Probe,
    /// Send the Wi-Fi details and wait for the board to join (or fail).
    Provision { ssid: String, password: String },
}

/// Console forwarding: complete lines go to the console as chip output.
struct Lines {
    tx: Sender<EngineEvent>,
    waker: Waker,
    line: Vec<u8>,
}

impl Lines {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.flush_line();
            } else {
                self.line.push(b);
            }
        }
    }
    fn flush_line(&mut self) {
        let text = String::from_utf8_lossy(&self.line).trim_end_matches('\r').to_string();
        self.line.clear();
        let _ = self.tx.send(EngineEvent::Console(Stream::Stdout, text));
        (self.waker)();
    }
    fn finish(&mut self) {
        if !self.line.is_empty() {
            self.flush_line();
        }
    }
}

/// Run one session on a thread. Connects synchronously so a refused port is an error now.
pub fn start(port: u16, job: Job, console: Sender<EngineEvent>, waker: Waker, events: Sender<Event>) -> std::io::Result<()> {
    let first = match &job {
        Job::Probe => request_state(),
        Job::Provision { ssid, password } => {
            wifi_settings(ssid, password).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            request_state()
        }
    };
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    s.set_nodelay(true)?;
    s.write_all(&first)?;
    s.write_all(b"\n")?;
    std::thread::spawn(move || {
        let w = waker.clone();
        let result = session(&mut s, job, Lines { tx: console, waker, line: Vec::new() }, &events, &w);
        let _ = s.shutdown(std::net::Shutdown::Both);
        let _ = events.send(Event::Done(result));
        w();
    });
    Ok(())
}

fn session(s: &mut TcpStream, job: Job, mut lines: Lines, events: &Sender<Event>, waker: &Waker) -> Result<String, String> {
    let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
    let mut parser = Parser::default();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    // Probe: two asks for the state over 3 s. Provision: up to 60 s to join (WLED takes about 30 s).
    let deadline = match job {
        Job::Probe => Duration::from_millis(3000),
        Job::Provision { .. } => Duration::from_secs(60),
    };
    let mut state: Option<State> = None;
    let mut asked_info = false;
    let mut resent = false;
    let mut sent_wifi = false;
    let mut url: Option<String> = None;
    let mut provisioned_at: Option<Instant> = None;
    let send = |s: &mut TcpStream, p: Vec<u8>| -> Result<(), String> {
        s.write_all(&p).and_then(|_| s.write_all(b"\n")).map_err(|e| format!("serial port: {e}"))
    };
    let outcome = loop {
        if start.elapsed() > deadline {
            break match (&job, state) {
                (_, None) => Err("no Improv answer: this firmware does not seem to support Improv over serial (or it has not started listening yet)".into()),
                (Job::Probe, Some(st)) => Ok(st.label()),
                (Job::Provision { .. }, Some(State::Provisioning)) => Err("the board was still connecting after 60 s".into()),
                (Job::Provision { .. }, Some(st)) => Err(format!("no result; last state: {}", st.label())),
            };
        }
        if state.is_none() && !resent && start.elapsed() > Duration::from_millis(1200) {
            resent = true;
            send(s, request_state())?;
        }
        match s.read(&mut buf) {
            Ok(0) => break Err("the serial port closed (another tool connected, or the device stopped)".into()),
            Ok(n) => {
                let (text, packets) = parser.feed(&buf[..n]);
                lines.push(&text);
                for p in packets {
                    match p {
                        Packet::State(st) => {
                            state = Some(st);
                            let _ = events.send(Event::State(st));
                            if !asked_info {
                                asked_info = true;
                                send(s, request_info())?;
                            }
                            if let Job::Provision { ssid, password } = &job {
                                if st == State::Ready && !sent_wifi {
                                    sent_wifi = true;
                                    send(s, wifi_settings(ssid, password)?)?;
                                }
                            }
                        }
                        Packet::Error(0) => {}
                        Packet::Error(code) => {
                            let _ = events.send(Event::Error(code));
                            if sent_wifi {
                                lines.finish();
                                return Err(error_label(code));
                            }
                        }
                        Packet::Result { cmd: CMD_INFO, strings } => {
                            let _ = events.send(Event::Info(strings));
                        }
                        Packet::Result { cmd, strings } if cmd == CMD_WIFI || cmd == CMD_STATE => {
                            if let Some(u) = strings.into_iter().find(|s| !s.is_empty()) {
                                url = Some(u.clone());
                                let _ = events.send(Event::Url(u));
                            }
                        }
                        _ => {}
                    }
                    waker();
                }
                // Done?
                match (&job, state) {
                    (Job::Probe, Some(_)) if asked_info && start.elapsed() > Duration::from_millis(800) => break Ok(state.unwrap().label()),
                    (Job::Provision { .. }, Some(State::Provisioned)) if sent_wifi && url.is_some() => break Ok(format!("connected; page at {}", url.clone().unwrap())),
                    (Job::Provision { .. }, Some(State::Provisioned)) if !sent_wifi => {
                        break Ok(match &url { Some(u) => format!("already connected to Wi-Fi; page at {u}"), None => "already connected to Wi-Fi".into() })
                    }
                    _ => {}
                }
            }
            Err(e) if crate::weblink::is_transient_io(&e) => {
                // Joined but no address packet: allow a few seconds for it, then finish.
                if let (Job::Provision { .. }, Some(State::Provisioned)) = (&job, state) {
                    if sent_wifi && provisioned_at.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(3) {
                        break Ok("connected to Wi-Fi (the firmware gave no page address)".into());
                    }
                }
            }
            Err(e) => break Err(format!("serial port: {e}")),
        }
    };
    lines.finish();
    outcome
}

/// The board's page address as seen from this PC: the firmware reports its own address
/// on the emulated network (e.g. `http://10.0.2.15`), which this PC reaches through a
/// published port. `forwards` are (this PC's port, firmware's port).
pub fn local_url(url: &str, forwards: &[(u16, u16)]) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" {
        return None;
    }
    let (hostport, path) = match rest.find('/') {
        Some(k) => (&rest[..k], &rest[k..]),
        None => (rest, "/"),
    };
    let port = match hostport.rsplit_once(':') {
        Some((_, p)) => p.parse().ok()?,
        None => 80,
    };
    let (host, _) = forwards.iter().find(|(_, g)| *g == port)?;
    Some(format!("http://127.0.0.1:{host}{path}"))
}

/// Spot an Improv packet in a console line (the board printed it to stdout because no
/// serial client was connected). Bytes above 0x7F are lost by then, so only the type and
/// the first data byte are trusted.
pub fn sniff_console(line: &str) -> Option<State> {
    let b = line.as_bytes();
    let at = line.find("IMPROV")?;
    let r = &b[at..];
    if r.len() >= 10 && r[6] == VERSION && r[7] == TYPE_STATE && r[8] == 1 {
        return Some(State::from_byte(r[9]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_state_matches_esp_web_tools() {
        // IMPROV 01 03 02 02 00, checksum = (sum) & 0xff
        let p = request_state();
        assert_eq!(&p[..6], b"IMPROV");
        assert_eq!(&p[6..11], &[1, 3, 2, 2, 0]);
        let sum: u32 = p[..11].iter().map(|&b| b as u32).sum();
        assert_eq!(p[11], (sum & 0xff) as u8);
        assert_eq!(p[11], 0xE5);
    }

    #[test]
    fn wifi_packet_layout() {
        let p = wifi_settings("net", "pw").unwrap();
        assert_eq!(&p[6..9], &[1, TYPE_RPC, 9]);
        assert_eq!(&p[9..18], &[CMD_WIFI, 7, 3, b'n', b'e', b't', 2, b'p', b'w']);
        assert_eq!(p.len(), 19);
        assert!(wifi_settings("", "x").is_err());
        assert!(wifi_settings(&"a".repeat(33), "x").is_err());
        assert!(wifi_settings("a", &"b".repeat(65)).is_err());
    }

    #[test]
    fn parser_splits_packets_from_log_text_across_reads() {
        let state = encode(TYPE_STATE, &[0x02]);
        let mut result_data = vec![CMD_WIFI, 0];
        let url = b"http://192.168.4.7";
        result_data.push(url.len() as u8);
        result_data.extend_from_slice(url);
        result_data[1] = (result_data.len() - 2) as u8;
        let result = encode(TYPE_RESULT, &result_data);
        let mut stream = b"[I][wifi] hello IMP\n".to_vec();
        stream.extend(&state);
        stream.extend(b"\nIn between\n");
        stream.extend(&result);
        let mut p = Parser::default();
        let (mut text, mut packets) = (Vec::new(), Vec::new());
        for chunk in stream.chunks(5) {
            let (t, k) = p.feed(chunk);
            text.extend(t);
            packets.extend(k);
        }
        assert_eq!(String::from_utf8(text).unwrap(), "[I][wifi] hello IMP\n\nIn between\n");
        assert_eq!(packets, vec![Packet::State(State::Ready), Packet::Result { cmd: CMD_WIFI, strings: vec!["http://192.168.4.7".into()] }]);
    }

    #[test]
    fn page_address_through_forwards() {
        assert_eq!(local_url("http://10.0.2.15", &[(8080, 80)]).as_deref(), Some("http://127.0.0.1:8080/"));
        assert_eq!(local_url("http://10.0.2.15:8123/x?y", &[(8080, 80), (18123, 8123)]).as_deref(), Some("http://127.0.0.1:18123/x?y"));
        assert_eq!(local_url("http://10.0.2.15", &[(6053, 6053)]), None);
        assert_eq!(local_url("https://10.0.2.15", &[(443, 443)]), None);
    }

    #[test]
    fn bad_checksum_passes_as_text() {
        let mut bad = encode(TYPE_STATE, &[0x02]);
        *bad.last_mut().unwrap() ^= 1;
        let (text, packets) = Parser::default().feed(&bad);
        assert!(packets.is_empty());
        assert_eq!(text, bad);
    }

    #[test]
    fn info_result_and_console_sniff() {
        let mut d = vec![CMD_INFO, 0];
        for s in ["ESPHome", "2024.6.0", "ESP32", "esphome-web-1234"] {
            d.push(s.len() as u8);
            d.extend_from_slice(s.as_bytes());
        }
        d[1] = (d.len() - 2) as u8;
        let (_, k) = Parser::default().feed(&encode(TYPE_RESULT, &d));
        assert_eq!(k, vec![Packet::Result { cmd: CMD_INFO, strings: vec!["ESPHome".into(), "2024.6.0".into(), "ESP32".into(), "esphome-web-1234".into()] }]);
        let line = String::from_utf8_lossy(&encode(TYPE_STATE, &[0x02])).into_owned();
        assert_eq!(sniff_console(&line), Some(State::Ready));
        assert_eq!(sniff_console("IMPROVE your code"), None);
    }
}
