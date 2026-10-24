//! `cyd-lab-monitor`: an Arduino pluggable monitor (protocol version 1) for cyd-lab
//! boards. It bridges the IDE's / arduino-cli's TCP data channel to the board's
//! network serial port: `rfc2217://host:port` (the engine's `--serial-tcp`, Telnet
//! commands stripped / 0xFF escaped) or `socket://host:port` / `host:port` (raw TCP).
//!
//! Spec: <https://arduino.github.io/arduino-cli/dev/pluggable-monitor-specification/>
//! Commands: HELLO, DESCRIBE, CONFIGURE, OPEN, CLOSE, QUIT.
//!
//! It never touches DTR/RTS, so opening the monitor does not reset the board (the
//! engine models the CYD's auto-reset circuit on those lines). The baud rate is
//! accepted and ignored: an emulated UART has no line speed.

use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const IAC: u8 = 255;
const SB: u8 = 250;
const SE: u8 = 240;
const BAUDS: [&str; 12] = ["300", "1200", "2400", "4800", "9600", "19200", "38400", "57600", "74880", "115200", "230400", "460800"];

/// Strip Telnet commands from RFC 2217 data; `state` carries a sequence split across reads.
fn strip_telnet(state: &mut u8, input: &[u8], out: &mut Vec<u8>) {
    for &b in input {
        *state = match (*state, b) {
            (0, IAC) => 1,
            (0, _) => {
                out.push(b);
                0
            }
            (1, IAC) => {
                out.push(IAC);
                0
            }
            (1, SB) => 2,
            (1, 251..=254) => 4,
            (1, _) => 0,
            (2, IAC) => 3,
            (2, _) => 2,
            (3, SE) => 0,
            (3, _) => 2,
            _ => 0,
        };
    }
}

/// Escape data bytes for Telnet (0xFF is doubled).
fn escape_iac(input: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(input.len());
    for &b in input {
        v.push(b);
        if b == IAC {
            v.push(IAC);
        }
    }
    v
}

/// `(host:port, telnet?)` for a port address.
fn parse_address(a: &str) -> Result<(String, bool), String> {
    let (rest, telnet) = if let Some(r) = a.strip_prefix("rfc2217://") {
        (r, true)
    } else if let Some(r) = a.strip_prefix("socket://") {
        (r, false)
    } else if a.contains("://") {
        return Err(format!("unsupported address {a:?} (use rfc2217://host:port or socket://host:port)"));
    } else {
        (a, true)
    };
    let hostport = rest.split(['?', '/']).next().unwrap_or("");
    if hostport.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).is_none() {
        return Err(format!("no port number in {a:?}"));
    }
    Ok((hostport.to_string(), telnet))
}

#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);
impl Out {
    fn send(&self, v: Value) {
        let mut o = self.0.lock().unwrap();
        let _ = writeln!(o, "{}", serde_json::to_string_pretty(&v).unwrap());
        let _ = o.flush();
    }
    fn ok(&self, e: &str) {
        self.send(json!({"eventType": e, "message": "OK"}));
    }
    fn err(&self, e: &str, m: &str) {
        self.send(json!({"eventType": e, "error": true, "message": m}));
    }
}

struct Session {
    board: TcpStream,
    client: TcpStream,
}

impl Session {
    fn close(&self) {
        let _ = self.board.shutdown(Shutdown::Both);
        let _ = self.client.shutdown(Shutdown::Both);
    }
}

fn open(out: &Out, client_addr: &str, port_addr: &str) -> Result<Session, String> {
    let (hostport, telnet) = parse_address(port_addr)?;
    let board = TcpStream::connect(&hostport).map_err(|e| format!("cannot connect to {hostport}: {e}"))?;
    let client = TcpStream::connect(client_addr).map_err(|e| format!("cannot connect to client {client_addr}: {e}"))?;
    let _ = board.set_nodelay(true);
    let _ = client.set_nodelay(true);
    // board -> client
    {
        let (mut b, mut c, out) = (board.try_clone().unwrap(), client.try_clone().unwrap(), out.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut st = 0u8;
            let mut clean = Vec::with_capacity(4096);
            loop {
                match b.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = if telnet {
                            clean.clear();
                            strip_telnet(&mut st, &buf[..n], &mut clean);
                            &clean[..]
                        } else {
                            &buf[..n]
                        };
                        if c.write_all(data).is_err() {
                            return; // client went away: CLOSE / QUIT handles the rest
                        }
                    }
                }
            }
            let _ = c.shutdown(Shutdown::Both);
            out.send(json!({"eventType": "port_closed"}));
        });
    }
    // client -> board
    {
        let (mut b, mut c) = (board.try_clone().unwrap(), client.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = if telnet { escape_iac(&buf[..n]) } else { buf[..n].to_vec() };
                        if b.write_all(&data).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = b.shutdown(Shutdown::Both);
        });
    }
    std::thread::sleep(Duration::from_millis(10));
    Ok(Session { board, client })
}

fn main() {
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let mut baud = "115200".to_string();
    let mut session: Option<Session> = None;
    let mut hello = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        let Some(cmd) = words.next().map(|c| c.to_ascii_uppercase()) else { continue };
        match cmd.as_str() {
            "HELLO" => {
                hello = true;
                out.send(json!({"eventType": "hello", "protocolVersion": 1, "message": "OK"}));
            }
            _ if !hello && cmd != "QUIT" => out.err(&cmd.to_ascii_lowercase(), "HELLO first"),
            "DESCRIBE" => out.send(json!({
                "eventType": "describe",
                "message": "OK",
                "port_description": {
                    "protocol": "cydlab",
                    "configuration_parameters": {
                        "baudrate": { "label": "Baudrate", "type": "enum", "value": BAUDS, "selected": baud }
                    }
                }
            })),
            "CONFIGURE" => match (words.next(), words.next()) {
                (Some("baudrate"), Some(v)) if BAUDS.contains(&v) => {
                    baud = v.to_string();
                    out.ok("configure");
                }
                (Some(k), Some(v)) => out.err("configure", &format!("invalid value {v} for {k}")),
                _ => out.err("configure", "usage: CONFIGURE <parameter> <value>"),
            },
            "OPEN" => {
                if session.is_some() {
                    out.err("open", "port already opened");
                    continue;
                }
                let (Some(client), Some(port)) = (words.next(), words.next()) else {
                    out.err("open", "usage: OPEN <client address> <port address>");
                    continue;
                };
                match open(&out, client, port) {
                    Ok(s) => {
                        session = Some(s);
                        out.ok("open");
                    }
                    Err(e) => out.err("open", &e),
                }
            }
            "CLOSE" => match session.take() {
                Some(s) => {
                    s.close();
                    out.ok("close");
                }
                None => out.err("close", "port already closed"),
            },
            "QUIT" => {
                if let Some(s) = session.take() {
                    s.close();
                }
                out.ok("quit");
                return;
            }
            other => out.err("command_error", &format!("Command {other} not supported")),
        }
    }
    if let Some(s) = session {
        s.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(parse_address("rfc2217://127.0.0.1:22170").unwrap(), ("127.0.0.1:22170".into(), true));
        assert_eq!(parse_address("rfc2217://127.0.0.1:22170?ign_set_control").unwrap(), ("127.0.0.1:22170".into(), true));
        assert_eq!(parse_address("socket://localhost:4000").unwrap(), ("localhost:4000".into(), false));
        assert!(parse_address("COM3").is_err());
        assert!(parse_address("serial:///dev/ttyUSB0").is_err());
    }

    #[test]
    fn telnet_framing() {
        let mut st = 0;
        let mut out = Vec::new();
        // WILL COM-PORT, data, escaped 0xFF, a split SB…SE
        strip_telnet(&mut st, &[IAC, 251, 44, b'h', b'i', IAC, IAC, IAC, SB, 44], &mut out);
        strip_telnet(&mut st, &[105, 5, IAC, SE, b'!'], &mut out);
        assert_eq!(out, vec![b'h', b'i', 0xFF, b'!']);
        assert_eq!(escape_iac(&[1, 0xFF, 2]), vec![1, 0xFF, 0xFF, 2]);
    }

    /// OPEN bridges a fake board (rfc2217) and a fake IDE listener in both directions.
    #[test]
    fn bridges_both_ways() {
        use std::net::TcpListener;
        let board_l = TcpListener::bind("127.0.0.1:0").unwrap();
        let ide_l = TcpListener::bind("127.0.0.1:0").unwrap();
        let out = Out(Arc::new(Mutex::new(std::io::stdout())));
        let port = format!("rfc2217://{}", board_l.local_addr().unwrap());
        let s = open(&out, &ide_l.local_addr().unwrap().to_string(), &port).unwrap();
        let (mut board, _) = board_l.accept().unwrap();
        let (mut ide, _) = ide_l.accept().unwrap();
        board.write_all(&[b'o', b'k', IAC, 251, 44, b'\n']).unwrap();
        let mut got = [0u8; 3];
        ide.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok\n");
        ide.write_all(&[b'x', 0xFF]).unwrap();
        let mut got = [0u8; 3];
        board.read_exact(&mut got).unwrap();
        assert_eq!(got, [b'x', 0xFF, 0xFF]);
        s.close();
    }
}
