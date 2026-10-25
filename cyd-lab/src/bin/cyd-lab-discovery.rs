//! `cyd-lab-discovery`: an Arduino pluggable discovery (protocol version 1) that lists
//! the cyd-lab devices that are powered on, as network serial ports with no COM driver.
//!
//! Spec: <https://arduino.github.io/arduino-cli/dev/pluggable-discovery-specification/>
//! Commands on stdin, one JSON object per reply on stdout: HELLO, START, STOP, LIST,
//! START_SYNC, QUIT. Ports come from cyd-lab's `running.json` (see `running.rs`); each
//! one is `protocol: cydlab`, `address: rfc2217://127.0.0.1:PORT`, label "<name> (cyd-lab)".

#[path = "../running.rs"]
mod running;

use running::{RunningDevice, RunningFile};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROTOCOL: &str = "cydlab";

fn port_json(d: &RunningDevice) -> Value {
    json!({
        "address": d.address(),
        "label": d.label(),
        "protocol": PROTOCOL,
        "protocolLabel": "cyd-lab (emulated, network)",
        "hardwareId": d.id,
        "properties": { "cydlab": d.profile, "device": d.id, "name": d.name, "serial_port": d.serial_port.to_string() }
    })
}

fn current(path: &Option<PathBuf>) -> Vec<RunningDevice> {
    path.as_ref().and_then(|p| RunningFile::read(p)).map(|f| f.live_devices()).unwrap_or_default()
}

/// Replies go through one lock so sync events and command replies never interleave.
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);
impl Out {
    fn send(&self, v: Value) {
        let mut o = self.0.lock().unwrap();
        let _ = writeln!(o, "{}", serde_json::to_string_pretty(&v).unwrap());
        let _ = o.flush();
    }
    fn ok(&self, event: &str) {
        self.send(json!({"eventType": event, "message": "OK"}));
    }
    fn err(&self, event: &str, msg: &str) {
        self.send(json!({"eventType": event, "error": true, "message": msg}));
    }
}

/// The add/remove events that turn `old` into `new` (a moved port is remove + add).
fn diff(old: &[RunningDevice], new: &[RunningDevice]) -> Vec<Value> {
    let mut ev = Vec::new();
    for d in old {
        if !new.contains(d) {
            ev.push(json!({"eventType": "remove", "port": {"address": d.address(), "protocol": PROTOCOL}}));
        }
    }
    for d in new {
        if !old.contains(d) {
            ev.push(json!({"eventType": "add", "port": port_json(d)}));
        }
    }
    ev
}

fn main() {
    // argv[1], else `running-path.txt` beside this exe (written by cyd-lab's "Set up
    // Arduino IDE", so a portable cyd-lab is found), else the standard data dir.
    let beside = || {
        let f = std::env::current_exe().ok()?.with_file_name("running-path.txt");
        let t = std::fs::read_to_string(f).ok()?;
        Some(PathBuf::from(t.trim())).filter(|p| !p.as_os_str().is_empty())
    };
    let path = std::env::args().nth(1).map(PathBuf::from).or_else(beside).or_else(|| running::data_dir().map(|d| d.join(running::FILE_NAME)));
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let mut hello = false;
    let mut started = false;
    let syncing = Arc::new(AtomicBool::new(false));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cmd = line.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        match cmd.as_str() {
            "HELLO" => {
                hello = true;
                out.send(json!({"eventType": "hello", "protocolVersion": 1, "message": "OK"}));
            }
            _ if !hello => out.err(&cmd.to_ascii_lowercase(), "HELLO first"),
            "START" => {
                started = true;
                out.ok("start");
            }
            "LIST" => {
                if !started {
                    out.err("list", "discovery not STARTed");
                } else {
                    let ports: Vec<Value> = current(&path).iter().map(port_json).collect();
                    out.send(json!({"eventType": "list", "ports": ports}));
                }
            }
            "START_SYNC" => {
                if syncing.swap(true, Ordering::SeqCst) {
                    out.err("start_sync", "already in sync mode");
                    continue;
                }
                out.ok("start_sync");
                let (out, syncing, path) = (out.clone(), syncing.clone(), path.clone());
                std::thread::spawn(move || {
                    let mut known: Vec<RunningDevice> = Vec::new();
                    while syncing.load(Ordering::SeqCst) {
                        let now = current(&path);
                        for e in diff(&known, &now) {
                            out.send(e);
                        }
                        known = now;
                        std::thread::sleep(Duration::from_millis(700));
                    }
                });
            }
            "STOP" => {
                started = false;
                syncing.store(false, Ordering::SeqCst);
                out.ok("stop");
            }
            "QUIT" => {
                syncing.store(false, Ordering::SeqCst);
                out.ok("quit");
                return;
            }
            other => out.err("command_error", &format!("Command {other} not supported")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, port: u16) -> RunningDevice {
        RunningDevice { id: name.to_lowercase(), name: name.into(), profile: "cyd-2432s028r".into(), serial_port: port }
    }

    #[test]
    fn port_shape() {
        let p = port_json(&dev("Kitchen CYD", 22170));
        assert_eq!(p["address"], "rfc2217://127.0.0.1:22170");
        assert_eq!(p["label"], "Kitchen CYD (cyd-lab)");
        assert_eq!(p["protocol"], "cydlab");
        assert_eq!(p["properties"]["cydlab"], "cyd-2432s028r");
    }

    #[test]
    fn sync_diff() {
        let a = dev("A", 1);
        let b = dev("B", 2);
        let ev = diff(&[a.clone()], &[b.clone()]);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0]["eventType"], "remove");
        assert_eq!(ev[0]["port"]["address"], "rfc2217://127.0.0.1:1");
        assert_eq!(ev[1]["eventType"], "add");
        assert!(diff(&[a.clone()], &[a]).is_empty());
    }
}
