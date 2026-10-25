//! "Device hotspot": many firmwares start their own access point for first setup
//! (openHASP `HASP-xxxxxx`, Tasmota `tasmota-XXXXXX`, WLED `WLED-AP`, ESPHome's fallback
//! AP) and serve a setup page at 192.168.4.1. On a real board you join that network with
//! a phone; here the engine joins it for you as a client (`--ap-client`) and forwards a
//! port on this PC to the page (`--ap-fwd`).
//!
//! With the live link up this happens at run time over the engine's WebSocket control
//! channel: `{"t":"ap_client","ssid","psk","fwd"}` makes the "phone", joins and forwards,
//! newer engines push `{"t":"ap_status"}` whenever the phone's state changes (older ones
//! only answer when asked, so the panel polls them). Without the link, [`Plan`] puts the
//! same request on the command line and the device restarts.
//!
//! Newer engines also report clients on forwarded ports as [`FwdEvent`]s.
//!
//! The browser never talks to the forwarded port directly but to [`HostProxy`], which sends
//! `Host: 192.168.4.1` as a phone would: captive portals such as WiFiManager (NerdMiner)
//! redirect any other host name, so `http://127.0.0.1:PORT/` would loop.
//!
//! The feature is hidden unless the engine's `--help` lists both flags.

use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The guest's usual address for its own access point.
pub const GUEST_AP_IP: &str = "192.168.4.1";

/// A request to join the device's hotspot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub ssid: String,
    pub password: String,
    /// This PC's port for the setup page.
    pub host_port: u16,
    /// The page's address on the device's network (WLED uses 4.3.2.1).
    pub guest_ip: String,
    pub guest_port: u16,
}

impl Plan {
    /// Engine arguments: join the hotspot and forward the page to 127.0.0.1 only.
    pub fn args(&self) -> Vec<String> {
        let mut client = format!("ssid={}", self.ssid);
        if !self.password.is_empty() {
            client.push_str(&format!(",psk={}", self.password));
        }
        vec!["--ap-client".into(), client, "--ap-fwd".into(), format!("tcp:127.0.0.1:{}-{}:{}", self.host_port, self.guest_ip, self.guest_port)]
    }
    pub fn url(&self) -> String {
        crate::open::page_url("127.0.0.1", self.host_port, "/")
    }
}

/// An access point name the firmware announced in its log, if this line has one.
/// Covers ESPHome (`AP SSID: 'name'`), Arduino/IDF prints (`softAP SSID: name`,
/// `Starting AP: name`, `AP started: name`), Tasmota (`WIF: AP1 tasmota-XXXX`-style
/// `Starting AccessPoint` lines are too varied, so only quoted names are taken there).
pub fn sniff_ssid(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    for key in ["ap ssid:", "softap ssid:", "softap ssid =", "starting ap:", "ap started:", "access point:", "hotspot:", "wifimanager ap:", "temporary access point", "opening access point"] {
        if let Some(at) = lower.find(key) {
            let rest = line[at + key.len()..].trim();
            let name = if let Some(q) = rest.strip_prefix('\'') {
                q.split('\'').next().unwrap_or("")
            } else if let Some(q) = rest.strip_prefix('"') {
                q.split('"').next().unwrap_or("")
            } else {
                rest.split_whitespace().next().unwrap_or("")
            };
            let name = name.trim_end_matches([',', '.', ';']);
            if !name.is_empty() && name.len() <= 32 {
                return Some(name.to_string());
            }
        }
    }
    quoted_after_ap(line)
}

/// `AP 'name' up` (CYD Weather Dashboard) or `AP "name" started`: a quoted name right after
/// the word AP.
fn quoted_after_ap(line: &str) -> Option<String> {
    let b = line.as_bytes();
    for (at, _) in line.match_indices(|c| c == '\'' || c == '"') {
        let q = b[at] as char;
        if at < 3 || !line[..at].ends_with(' ') {
            continue;
        }
        let word = line[..at - 1].to_ascii_lowercase();
        let ap_word = word.ends_with("softap") || (word.ends_with("ap") && (at == 3 || !b[at - 4].is_ascii_alphanumeric()));
        if !ap_word {
            continue;
        }
        let name = line[at + 1..].split(q).next().unwrap_or("");
        if !name.is_empty() && name.len() <= 32 && line[at + 1 + name.len()..].starts_with(q) {
            return Some(name.to_string());
        }
    }
    None
}

/// Remember up to 8 names, newest last, no repeats.
pub fn remember(list: &mut Vec<String>, ssid: String) {
    list.retain(|s| *s != ssid);
    list.push(ssid);
    if list.len() > 8 {
        list.remove(0);
    }
}

/// The engine's `ap_status` answer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ApStatus {
    /// The host address a forward was just bound to (`127.0.0.1:PORT`).
    pub bound: Option<String>,
    pub error: Option<String>,
    /// `None` until the first `ap_client`.
    pub phone: Option<PhoneStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PhoneStatus {
    /// scanning, authenticating, associating, handshake, dhcp, up
    pub state: String,
    pub ip: Option<String>,
    /// The network it was asked to join.
    pub join: Option<String>,
    pub hotspots: Vec<SeenHotspot>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SeenHotspot {
    pub ssid: String,
    pub bssid: String,
    pub channel: u8,
    pub secure: bool,
}

impl ApStatus {
    /// Parse an engine text message; `None` if it is not an `ap_status`.
    pub fn parse(text: &str) -> Option<ApStatus> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if v.get("t")?.as_str()? != "ap_status" {
            return None;
        }
        serde_json::from_value(v).ok()
    }
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.bound.as_deref().and_then(|b| b.parse().ok())
    }
    /// `ssid` is among the hotspots the phone sees.
    pub fn sees(&self, ssid: &str) -> bool {
        self.phone.as_ref().is_some_and(|p| p.hotspots.iter().any(|h| h.ssid == ssid))
    }
    /// Joined `ssid` and has an address.
    pub fn up_on(&self, ssid: &str) -> bool {
        self.phone.as_ref().is_some_and(|p| p.state == "up" && p.join.as_deref() == Some(ssid))
    }
}

/// A client came or went on a forwarded port (`{"t":"fwd",...}`, newer engines). Only
/// clients the guest accepted count: a refused port never sends `connect`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct FwdEvent {
    /// `connect` or `disconnect`
    pub event: String,
    /// `hostfwd` (published ports) or `ap-fwd` (the device hotspot's page)
    pub via: String,
    /// This PC's port.
    pub port: u16,
    pub peer: String,
    /// Clients on that port as of this event.
    pub clients: u32,
}

impl FwdEvent {
    pub fn parse(text: &str) -> Option<FwdEvent> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if v.get("t")?.as_str()? != "fwd" {
            return None;
        }
        serde_json::from_value(v).ok()
    }
    pub fn connected(&self) -> bool {
        self.event == "connect"
    }
}

/// Live clients per forwarded port, from [`FwdEvent`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FwdClients {
    /// (via, host port) -> (count, peers)
    pub ports: std::collections::BTreeMap<(String, u16), (u32, Vec<String>)>,
    /// The engine sends these events (so "0 clients" means something).
    pub reported: bool,
}

impl FwdClients {
    pub fn apply(&mut self, e: &FwdEvent) {
        self.reported = true;
        let entry = self.ports.entry((e.via.clone(), e.port)).or_default();
        entry.1.retain(|p| *p != e.peer);
        if e.connected() {
            entry.1.push(e.peer.clone());
        }
        entry.0 = e.clients;
        if e.clients == 0 {
            entry.1.clear();
        }
        // Keep the peer list no longer than the count the engine gives.
        while entry.1.len() > entry.0 as usize {
            entry.1.remove(0);
        }
    }
    /// Clients on a published port (`--hostfwd`).
    pub fn on_host_port(&self, port: u16) -> (u32, &[String]) {
        self.ports.get(&("hostfwd".to_string(), port)).map(|(n, p)| (*n, p.as_slice())).unwrap_or((0, &[]))
    }
}

impl PhoneStatus {
    /// "joining X: handshake" style progress for the panel.
    pub fn progress(&self) -> String {
        match (&self.join, self.state.as_str()) {
            (None, _) => "looking for hotspots".into(),
            (Some(j), "up") => format!("joined \"{j}\"{}", self.ip.as_deref().map(|ip| format!(" as {ip}")).unwrap_or_default()),
            (Some(j), "scanning") => format!("waiting for \"{j}\" to appear"),
            (Some(j), st) => format!("joining \"{j}\": {st}"),
        }
    }
}

/// The control message: make the phone (no ssid = just scan), join, and (once per run)
/// forward a port on 127.0.0.1 chosen by the engine to the page.
pub fn client_message(ssid: Option<&str>, psk: &str, fwd_to: Option<(&str, u16)>) -> String {
    let mut m = serde_json::Map::new();
    m.insert("t".into(), "ap_client".into());
    if let Some(s) = ssid.filter(|s| !s.is_empty()) {
        m.insert("ssid".into(), s.into());
        m.insert("psk".into(), psk.into());
    }
    if let Some((ip, port)) = fwd_to {
        let guest = if ip.is_empty() || ip == GUEST_AP_IP { String::new() } else { ip.to_string() };
        m.insert("fwd".into(), format!("tcp:127.0.0.1:0-{guest}:{port}").into());
    }
    serde_json::Value::Object(m).to_string()
}

pub const STATUS_MESSAGE: &str = r#"{"t":"ap_status"}"#;

/// Does `ssid` fit a catalogue name pattern? Runs of two or more `x` or `n` stand for any
/// characters (one each: `HASP-xxxxxx` fits `HASP-1A2B3C`); the rest compares without case.
pub fn ssid_matches(pattern: &str, ssid: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = ssid.chars().collect();
    if p.len() != s.len() || p.is_empty() {
        return false;
    }
    let wild = |i: usize| {
        let c = p[i];
        if c != 'x' && c != 'n' {
            return false;
        }
        let run_l = p[..i].iter().rev().take_while(|&&d| d == c).count();
        let run_r = p[i + 1..].iter().take_while(|&&d| d == c).count();
        run_l + run_r >= 1
    };
    (0..p.len()).all(|i| wild(i) || p[i].eq_ignore_ascii_case(&s[i]))
}

/// A tiny HTTP proxy on 127.0.0.1 in front of the forwarded setup page: each request goes
/// out with `Host: <guest ip>` and `Connection: close`, and a `Location` pointing at the
/// guest comes back pointing at the proxy. One request per connection keeps it simple.
pub struct HostProxy {
    pub port: u16,
    target: Arc<Mutex<Option<SocketAddr>>>,
    host: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HostProxy {
    pub fn start() -> std::io::Result<HostProxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let target: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let host = Arc::new(Mutex::new(GUEST_AP_IP.to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, h, st) = (target.clone(), host.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("hotspot-proxy".into()).spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let (to, host) = (*t.lock().unwrap(), h.lock().unwrap().clone());
                        std::thread::spawn(move || {
                            let _ = proxy_one(client, to, &host, port);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        })?;
        Ok(HostProxy { port, target, host, stop, thread: Some(thread) })
    }
    /// Where the page is now (the engine's forward for this run) and the guest's address.
    pub fn set_target(&self, to: Option<SocketAddr>, guest_ip: &str) {
        *self.target.lock().unwrap() = to;
        *self.host.lock().unwrap() = if guest_ip.is_empty() { GUEST_AP_IP.to_string() } else { guest_ip.to_string() };
    }
    pub fn target(&self) -> Option<SocketAddr> {
        *self.target.lock().unwrap()
    }
    pub fn url(&self) -> String {
        crate::open::page_url("127.0.0.1", self.port, "/")
    }
}

impl Drop for HostProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The accept loop wakes every 50 ms; joining means the port is free on return.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Read an HTTP head (up to the blank line); returns (head, bytes after it).
fn read_head(s: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(at + 4);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::other("header too long"));
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Ok((String::from_utf8_lossy(&buf).into_owned(), Vec::new()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The request head with Host and Connection replaced.
pub fn rewrite_request_head(head: &str, host: &str) -> String {
    let mut out = String::new();
    for (i, line) in head.split("\r\n").filter(|l| !l.is_empty()).enumerate() {
        let lower = line.to_ascii_lowercase();
        if i > 0 && (lower.starts_with("host:") || lower.starts_with("connection:") || lower.starts_with("proxy-connection:") || lower.starts_with("keep-alive:")) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
        if i == 0 {
            out.push_str(&format!("Host: {host}\r\nConnection: close\r\n"));
        }
    }
    out.push_str("\r\n");
    out
}

/// The response head with `Location: http://<guest>/…` turned into the proxy's address.
pub fn rewrite_response_head(head: &str, host: &str, port: u16) -> String {
    let mut out = String::new();
    for line in head.split("\r\n").filter(|l| !l.is_empty()) {
        if line.to_ascii_lowercase().starts_with("location:") {
            let v = line[9..].trim();
            let guest = format!("http://{host}");
            if let Some(rest) = v.strip_prefix(&guest) {
                let rest = rest.strip_prefix(":80").unwrap_or(rest);
                out.push_str(&format!("Location: http://127.0.0.1:{port}{}\r\n", if rest.is_empty() { "/" } else { rest }));
                continue;
            }
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out
}

fn proxy_one(mut client: TcpStream, to: Option<SocketAddr>, host: &str, port: u16) -> std::io::Result<()> {
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let (head, rest) = read_head(&mut client)?;
    if head.is_empty() {
        return Ok(());
    }
    let Some(to) = to else {
        let body = "The device's setup page is not connected yet: join its hotspot in cyd-lab first.";
        client.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())?;
        let _ = client.shutdown(std::net::Shutdown::Write);
        return Ok(());
    };
    let mut up = TcpStream::connect_timeout(&to, Duration::from_secs(5))?;
    up.set_read_timeout(Some(Duration::from_secs(60)))?;
    up.write_all(rewrite_request_head(&head, host).as_bytes())?;
    up.write_all(&rest)?;
    // The rest of a request body (form posts), client -> device.
    let (mut c2, mut u2) = (client.try_clone()?, up.try_clone()?);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c2, &mut u2);
    });
    let (rhead, rrest) = read_head(&mut up)?;
    client.write_all(rewrite_response_head(&rhead, host, port).as_bytes())?;
    client.write_all(&rrest)?;
    let _ = std::io::copy(&mut up, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_events_count_clients() {
        let c = FwdEvent::parse(r#"{"t":"fwd","event":"connect","via":"hostfwd","port":16053,"peer":"127.0.0.1:50000","clients":1}"#).unwrap();
        assert!(c.connected() && c.port == 16053 && c.via == "hostfwd");
        assert!(FwdEvent::parse(r#"{"t":"ap_status"}"#).is_none() && FwdEvent::parse("x").is_none());
        let mut live = FwdClients::default();
        assert!(!live.reported);
        live.apply(&c);
        live.apply(&FwdEvent::parse(r#"{"t":"fwd","event":"connect","via":"hostfwd","port":16053,"peer":"127.0.0.1:50001","clients":2}"#).unwrap());
        live.apply(&FwdEvent::parse(r#"{"t":"fwd","event":"connect","via":"ap-fwd","port":40000,"peer":"127.0.0.1:50002","clients":1}"#).unwrap());
        assert_eq!(live.on_host_port(16053), (2, &["127.0.0.1:50000".to_string(), "127.0.0.1:50001".to_string()][..]));
        live.apply(&FwdEvent::parse(r#"{"t":"fwd","event":"disconnect","via":"hostfwd","port":16053,"peer":"127.0.0.1:50000","clients":1}"#).unwrap());
        assert_eq!(live.on_host_port(16053), (1, &["127.0.0.1:50001".to_string()][..]));
        // Guest left its hotspot: everything on the page forward closes at once.
        live.apply(&FwdEvent::parse(r#"{"t":"fwd","event":"disconnect","via":"ap-fwd","port":40000,"peer":"127.0.0.1:50002","clients":0}"#).unwrap());
        assert_eq!(live.ports[&("ap-fwd".to_string(), 40000)], (0, vec![]));
        assert_eq!(live.on_host_port(3232), (0, &[][..]));
        assert!(live.reported);
    }

    #[test]
    fn status_and_messages() {
        let s = ApStatus::parse(r#"{"t":"ap_status","bound":"127.0.0.1:40123","phone":{"state":"up","ip":"192.168.4.2","join":"NerdMinerAP","hotspots":[{"ssid":"NerdMinerAP","bssid":"aa:bb:cc:dd:ee:ff","channel":1,"secure":true}]}}"#).unwrap();
        assert_eq!(s.bound_addr(), Some("127.0.0.1:40123".parse().unwrap()));
        assert!(s.up_on("NerdMinerAP") && !s.up_on("other"));
        assert_eq!(s.phone.as_ref().unwrap().hotspots[0].ssid, "NerdMinerAP");
        assert_eq!(s.phone.as_ref().unwrap().progress(), "joined \"NerdMinerAP\" as 192.168.4.2");
        assert_eq!(ApStatus::parse(r#"{"t":"ap_status","bound":null,"phone":null}"#).unwrap(), ApStatus::default());
        assert_eq!(ApStatus::parse(r#"{"t":"ap_status","error":"psk must be 8..=63 characters"}"#).unwrap().error.as_deref(), Some("psk must be 8..=63 characters"));
        assert!(ApStatus::parse(r#"{"t":"other"}"#).is_none() && ApStatus::parse("nope").is_none());
        assert_eq!(client_message(None, "", None), r#"{"t":"ap_client"}"#);
        let m: serde_json::Value = serde_json::from_str(&client_message(Some("HASP-1A2B3C"), "haspadmin", Some(("192.168.4.1", 80)))).unwrap();
        assert_eq!((m["ssid"].as_str(), m["psk"].as_str(), m["fwd"].as_str()), (Some("HASP-1A2B3C"), Some("haspadmin"), Some("tcp:127.0.0.1:0-:80")));
        let m: serde_json::Value = serde_json::from_str(&client_message(Some("WLED-AP"), "wled1234", Some(("4.3.2.1", 80)))).unwrap();
        assert_eq!(m["fwd"].as_str(), Some("tcp:127.0.0.1:0-4.3.2.1:80"));
    }

    #[test]
    fn catalogue_patterns() {
        assert!(ssid_matches("HASP-xxxxxx", "HASP-1A2B3C"));
        assert!(ssid_matches("NerdMinerAP", "nerdminerap"));
        assert!(ssid_matches("tasmota-xxxxxx-nnnn", "tasmota-ABCDEF-1234"));
        assert!(!ssid_matches("HASP-xxxxxx", "HASP-1A2B3"));
        assert!(!ssid_matches("WLED-AP", "WLED-AQ"));
        assert!(!ssid_matches("NerdMinerAP", "NerdMinerXP"), "a single x is a letter");
    }

    #[test]
    fn proxy_rewrites_host_and_redirects() {
        let req = "GET /wifi HTTP/1.1\r\nHost: 127.0.0.1:5555\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n";
        assert_eq!(rewrite_request_head(req, "192.168.4.1"), "GET /wifi HTTP/1.1\r\nHost: 192.168.4.1\r\nConnection: close\r\nAccept: */*\r\n\r\n");
        let resp = "HTTP/1.1 302 Found\r\nLocation: http://192.168.4.1/wifi?x=1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(rewrite_response_head(resp, "192.168.4.1", 5555), "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:5555/wifi?x=1\r\nContent-Length: 0\r\n\r\n");
        // End to end: a fake captive portal that only answers its own address.
        let dev = TcpListener::bind("127.0.0.1:0").unwrap();
        let dev_addr = dev.local_addr().unwrap();
        std::thread::spawn(move || {
            for c in dev.incoming().take(2) {
                let mut c = c.unwrap();
                let (head, _) = read_head(&mut c).unwrap();
                let reply = if head.contains("\r\nHost: 192.168.4.1\r\n") { "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello" } else { "HTTP/1.1 302 Found\r\nLocation: http://192.168.4.1/\r\nContent-Length: 0\r\n\r\n" };
                c.write_all(reply.as_bytes()).unwrap();
            }
        });
        let p = HostProxy::start().unwrap();
        let get = |port: u16| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        assert!(get(p.port).starts_with("HTTP/1.1 503"), "no target yet");
        p.set_target(Some(dev_addr), "");
        let r = get(p.port);
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("hello"), "{r}");
        p.set_target(Some(dev_addr), "10.0.0.1");
        let r = get(p.port);
        assert!(r.contains(&format!("Location: http://192.168.4.1/")), "a different guest ip leaves foreign redirects alone: {r}");
    }

    #[test]
    fn plan_args_bind_loopback() {
        let p = Plan { ssid: "HASP-ABC123".into(), password: String::new(), host_port: 18090, guest_ip: GUEST_AP_IP.into(), guest_port: 80 };
        assert_eq!(p.args(), ["--ap-client", "ssid=HASP-ABC123", "--ap-fwd", "tcp:127.0.0.1:18090-192.168.4.1:80"]);
        let p = Plan { password: "wled1234".into(), guest_ip: "4.3.2.1".into(), ..p };
        assert_eq!(p.args()[1], "ssid=HASP-ABC123,psk=wled1234");
        assert_eq!(p.url(), "http://127.0.0.1:18090/");
    }

    #[test]
    fn ssids_from_logs() {
        assert_eq!(sniff_ssid("[C][wifi:1010]: Setting up AP:   AP SSID: 'esphome-web-b13540'").as_deref(), Some("esphome-web-b13540"));
        assert_eq!(sniff_ssid("  AP SSID: 'esphome-web-b13540'").as_deref(), Some("esphome-web-b13540"));
        assert_eq!(sniff_ssid("softAP SSID: NerdMinerAP").as_deref(), Some("NerdMinerAP"));
        assert_eq!(sniff_ssid("Starting AP: \"WLED-AP\"").as_deref(), Some("WLED-AP"));
        assert_eq!(sniff_ssid("[wifi] connected to HomeNet"), None);
        // CYD Weather Dashboard, and the engine's own report of a hotspot it hears on the air.
        assert_eq!(sniff_ssid("[       444][W][NET  ] AP 'CYD-4578' up, portal at http://192.168.4.1").as_deref(), Some("CYD-4578"));
        assert_eq!(sniff_ssid("[emu] device hotspot: 'CYD-2211' bssid 24:6f:28:00:11:23 channel 1 (WPA2-PSK)").as_deref(), Some("CYD-2211"));
        assert_eq!(sniff_ssid("SoftAP \"Setup-1\" started").as_deref(), Some("Setup-1"));
        assert_eq!(sniff_ssid("map 'level1' loaded"), None);
        assert_eq!(sniff_ssid("snap 'x'"), None);
        assert_eq!(sniff_ssid("AP 'unterminated"), None);
        let mut l = vec![];
        remember(&mut l, "a".into());
        remember(&mut l, "b".into());
        remember(&mut l, "a".into());
        assert_eq!(l, ["b", "a"]);
    }

    /// End to end against a real engine and real firmware: the control channel finds the
    /// firmware's hotspot, joins it, forwards its page, and the proxy serves it.
    /// `CYD_LAB_E2E_ENGINE=…/esp32sim CYD_LAB_E2E_ROM=…/esp32_rev300_rom.elf CYD_LAB_E2E_FW=hasp|nerdminer
    /// CYD_LAB_E2E_BIN=factory.bin cargo test e2e_setup_portal -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_setup_portal() {
        use crate::engine::EngineEvent;
        use std::time::Instant;
        let (Ok(engine), Ok(fw), Ok(bin)) = (std::env::var("CYD_LAB_E2E_ENGINE"), std::env::var("CYD_LAB_E2E_FW"), std::env::var("CYD_LAB_E2E_BIN")) else {
            eprintln!("set CYD_LAB_E2E_ENGINE, CYD_LAB_E2E_FW and CYD_LAB_E2E_BIN");
            return;
        };
        let (pattern, password) = if fw == "hasp" { ("HASP-xxxxxx", "haspadmin") } else { ("NerdMinerAP", "MineYourCoins") };
        let dir = tempfile::tempdir().unwrap();
        let mut flash = std::fs::read(&bin).unwrap();
        flash.resize(4 << 20, 0xff);
        let flash_path = dir.path().join("flash.bin");
        std::fs::write(&flash_path, &flash).unwrap();
        let web = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let log = std::fs::File::create(dir.path().join("engine.log")).unwrap();
        let mut child = std::process::Command::new(&engine)
            .args(["--chip", "esp32", "--board", "cyd-2432s028r", "--boot", "rom", "--strap", "0x13", "--rom"])
            .arg(std::env::var("CYD_LAB_E2E_ROM").unwrap_or_else(|_| "/tmp/esp32sim/roms/esp32_rev300_rom.elf".into()))
            .arg("--flash-image")
            .arg(&flash_path)
            .args(["--flash-mb", "4", "--web", &web.to_string(), "--wifi", "ssid=esp32sim,psk=esp32sim-pass", "--console", "uart0", "--no-dump", "--realtime", "--max-seconds", "900"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let frames = Arc::new(Mutex::new(None));
        let (tx, rx) = std::sync::mpsc::channel();
        let link = crate::weblink::WebLink::connect(web, frames, tx, Arc::new(|| {}));
        let start = Instant::now();
        let mut status = ApStatus::default();
        let mut scanning = false;
        let mut ssid = None;
        let mut bound = None;
        let mut asked = false;
        let mut next_poll = Instant::now();
        while start.elapsed() < Duration::from_secs(400) {
            while let Ok(ev) = rx.try_recv() {
                if let EngineEvent::Control(t) = ev {
                    if let Some(st) = ApStatus::parse(&t) {
                        assert!(st.error.is_none(), "{t}");
                        if st.bound_addr().is_some() {
                            bound = st.bound_addr();
                        }
                        status = st;
                    }
                }
            }
            if link.is_connected() && !scanning {
                link.send_json(client_message(None, "", None));
                scanning = true;
            }
            if ssid.is_none() {
                ssid = status.phone.as_ref().and_then(|p| p.hotspots.iter().find(|h| ssid_matches(pattern, &h.ssid)).map(|h| h.ssid.clone()));
                if let Some(s) = &ssid {
                    eprintln!("[{:?}] hotspot seen: {s}", start.elapsed());
                }
            }
            if let (Some(s), false) = (&ssid, asked) {
                link.send_json(client_message(Some(s), password, Some((GUEST_AP_IP, 80))));
                asked = true;
            }
            if let (Some(s), Some(_)) = (&ssid, bound) {
                if status.up_on(s) {
                    break;
                }
            }
            if Instant::now() >= next_poll && scanning {
                link.send_json(STATUS_MESSAGE.to_string());
                next_poll = Instant::now() + Duration::from_millis(700);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let ssid = ssid.expect("the firmware's hotspot was never seen");
        let phone = status.phone.clone().unwrap();
        eprintln!("[{:?}] {} (bound {bound:?})", start.elapsed(), phone.progress());
        assert!(status.up_on(&ssid), "never joined: {phone:?}");
        let proxy = HostProxy::start().unwrap();
        proxy.set_target(bound, GUEST_AP_IP);
        let http = |req: String| -> String {
            let mut s = TcpStream::connect(("127.0.0.1", proxy.port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        };
        let get = |path: &str| http(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: cyd-lab-test\r\n\r\n", proxy.port));
        let home = get("/");
        eprintln!("GET / -> {} ({} bytes)", home.lines().next().unwrap_or(""), home.len());
        assert!(home.starts_with("HTTP/1.1 200") || home.starts_with("HTTP/1.0 200"), "{}", &home[..home.len().min(400)]);
        if fw == "hasp" {
            assert!(home.to_ascii_lowercase().contains("hasp"), "openHASP page");
            let body = r#"{"ssid":"esp32sim","pass":"esp32sim-pass"}"#;
            let r = http(format!("POST /api/config/wifi/ HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
            eprintln!("POST /api/config/wifi/ -> {}", r.lines().next().unwrap_or(""));
            assert!(r.contains(" 200"), "{}", &r[..r.len().min(400)]);
        } else {
            assert!(home.contains("NerdMiner") || home.contains("WiFiManager") || home.contains("wifi"), "NerdMiner portal");
            let r = get("/wifisave?s=esp32sim&p=esp32sim-pass&Poolurl=public-pool.io&Poolport=21496&btcAddress=bc1qtest&TimeZone=0");
            eprintln!("GET /wifisave -> {}", r.lines().next().unwrap_or(""));
            assert!(r.contains(" 200"), "{}", &r[..r.len().min(400)]);
        }
        // Give the firmware a moment to act on it, then show what it said.
        std::thread::sleep(Duration::from_secs(20));
        let _ = child.kill();
        let _ = child.wait();
        let log = std::fs::read_to_string(dir.path().join("engine.log")).unwrap_or_default();
        for l in log.lines().filter(|l| l.contains("[emu]") || l.to_ascii_lowercase().contains("esp32sim")).take(30) {
            eprintln!("  {l}");
        }
    }
}
