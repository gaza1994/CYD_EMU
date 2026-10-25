//! A device's network: the virtual access point the emulated chip joins (`--wifi`), the
//! ports published on this PC (`--hostfwd`), mDNS (`--mdns-relay`), and spotting Wi-Fi
//! join failures in the console.
//!
//! The firmware is the same binary as on the real board, with the user's home SSID and
//! password compiled in. So instead of changing the firmware, the emulated access point
//! takes that SSID and password, and the chip joins it as it would at home. Behind the
//! access point is esp32sim's user-mode NAT (station 10.0.2.15), and `--hostfwd` makes
//! the firmware's API/OTA/web ports reachable at this PC's address.

use serde::{Deserialize, Serialize};

/// SSID used when neither the device nor the app-wide default has one (devices made
/// before new devices got [`NEW_DEVICE_SSID`]).
pub const FALLBACK_SSID: &str = "cyd-lab";

/// The emulated network a new device offers when no app-wide default is set.
pub const NEW_DEVICE_SSID: &str = "esp32sim";

/// Wi-Fi network name and password (app-wide default for new devices).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WifiDetails {
    pub ssid: String,
    pub password: String,
}

/// A published port: this PC's `host` port to the firmware's `guest` port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    /// What it is for ("ESPHome API", "HTTP", ...). Shown in the UI only.
    #[serde(default)]
    pub label: String,
    pub host: u16,
    pub guest: u16,
    /// The tool connects to the port the firmware was built with (OTA uploads), so the
    /// host port should equal the firmware's.
    #[serde(default)]
    pub same_port: bool,
}

/// One-click sets of forwards for common firmware.
pub struct Preset {
    pub name: &'static str,
    pub tip: &'static str,
    /// (label, firmware port, preferred port on this PC, same_port)
    pub forwards: &'static [(&'static str, u16, u16, bool)],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "ESPHome (API + OTA)",
        tip: "Native API 6053 for Home Assistant and OTA 3232 for ESPHome Builder uploads",
        forwards: &[("ESPHome API", 6053, 6053, false), ("ESPHome OTA", 3232, 3232, true)],
    },
    Preset { name: "Arduino OTA", tip: "ArduinoOTA's default port 3232 (Arduino IDE network port, PlatformIO espota)", forwards: &[("Arduino OTA", 3232, 3232, true)] },
    Preset { name: "Web page (HTTP 80)", tip: "A web server in the firmware (port 80), on this PC's port 8080", forwards: &[("HTTP", 80, 8080, false)] },
];

/// Per-device network settings (`[network]` in device.toml). Stored in plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceNetwork {
    /// Offer the chip an access point to join (`--wifi`).
    pub wifi: bool,
    /// Network name the emulated access point offers. Empty: the app-wide default.
    pub ssid: String,
    /// Its password; empty = an open network.
    pub password: String,
    /// "Accept any network name": the access point answers whatever name the firmware looks
    /// for (`--wifi ssid=*`, with `psk` when a password is set). Needs an engine with it.
    pub accept_any: bool,
    /// "Reach from my network": publish on every interface (0.0.0.0) rather than only on
    /// this PC (127.0.0.1).
    pub lan: bool,
    /// Published ports.
    pub forwards: Vec<Forward>,
}

impl Default for DeviceNetwork {
    fn default() -> Self {
        DeviceNetwork { wifi: true, ssid: String::new(), password: String::new(), accept_any: false, lan: true, forwards: Vec::new() }
    }
}

impl DeviceNetwork {
    /// Settings for a new device: Wi-Fi on with the app-wide details (else an open
    /// "esp32sim"), no ports published.
    pub fn for_new_device(defaults: &WifiDetails) -> DeviceNetwork {
        let (ssid, password) = if defaults.ssid.is_empty() { (NEW_DEVICE_SSID.to_string(), String::new()) } else { (defaults.ssid.clone(), defaults.password.clone()) };
        DeviceNetwork { ssid, password, ..DeviceNetwork::default() }
    }

    /// The SSID/password the access point will offer (device, else app default, else open
    /// "cyd-lab").
    pub fn effective(&self, defaults: &WifiDetails) -> WifiDetails {
        if !self.ssid.is_empty() {
            WifiDetails { ssid: self.ssid.clone(), password: self.password.clone() }
        } else if !defaults.ssid.is_empty() {
            defaults.clone()
        } else {
            WifiDetails { ssid: FALLBACK_SSID.into(), password: String::new() }
        }
    }

    /// Published (host, guest) pairs, without zero ports.
    pub fn active_forwards(&self) -> Vec<&Forward> {
        self.forwards.iter().filter(|f| f.host != 0 && f.guest != 0).collect()
    }

    pub fn host_ports(&self) -> Vec<u16> {
        self.forwards.iter().map(|f| f.host).collect()
    }

    /// The forward to this firmware port, if any.
    pub fn to_guest(&self, guest: u16) -> Option<&Forward> {
        self.forwards.iter().find(|f| f.guest == guest)
    }

    /// Add a preset's forwards (skipping firmware ports already published). The PC port is
    /// the preferred one unless another device or this one uses it: then the next free
    /// one, and for `same_port` forwards the firmware port moves with it (the firmware's
    /// config must then say that port, which the UI explains).
    #[cfg(test)]
    pub fn add_preset(&mut self, p: &Preset, others: &[DeviceNetwork]) {
        self.add_preset_free(p, others, &|_| true);
    }

    /// [`DeviceNetwork::add_preset`], also skipping PC ports `port_free` says are taken
    /// right now (another program listening). Returns how many forwards were added.
    pub fn add_preset_free(&mut self, p: &Preset, others: &[DeviceNetwork], port_free: &dyn Fn(u16) -> bool) -> usize {
        let mut added = 0;
        for (label, guest, host, same) in p.forwards {
            if self.forwards.iter().any(|f| f.guest == *guest || (f.same_port && *same && f.label.contains("OTA") && label.contains("OTA"))) {
                continue;
            }
            let used: Vec<u16> = others.iter().flat_map(|n| n.host_ports()).chain(self.host_ports()).collect();
            let h = (*host..u16::MAX).find(|h| !used.contains(h) && port_free(*h)).unwrap_or(*host);
            added += 1;
            self.forwards.push(Forward { label: label.to_string(), host: h, guest: if *same { h } else { *guest }, same_port: *same });
        }
        added
    }

    /// The address forwards listen on.
    pub fn bind_addr(&self) -> &'static str {
        if self.lan { "0.0.0.0" } else { "127.0.0.1" }
    }
}

/// Why an SSID or password can't be given to the engine. `escapes`: the engine takes `\,`
/// and `\\` in its `--wifi` SPEC; older builds split on `,` with no escaping, so a comma
/// can't be carried.
pub fn validate_wifi(w: &WifiDetails, escapes: bool) -> Result<(), String> {
    if w.ssid.is_empty() {
        return Err("the network name is empty".into());
    }
    if w.ssid.len() > 32 {
        return Err(format!("the network name is {} bytes; Wi-Fi allows at most 32", w.ssid.len()));
    }
    if !escapes && (w.ssid.contains(',') || w.password.contains(',')) {
        return Err("the engine can't take a comma in the network name or password yet (its --wifi option splits on commas)".into());
    }
    let raw_key = w.password.len() == 64 && w.password.bytes().all(|b| b.is_ascii_hexdigit());
    if !w.password.is_empty() && !raw_key && !(8..=63).contains(&w.password.chars().count()) {
        return Err("a WPA2 password is 8 to 63 characters (or leave it empty for an open network)".into());
    }
    Ok(())
}

/// `--wifi` SPEC: `ssid=NAME` (open) or `ssid=NAME,psk=PASSWORD`. `=` inside a value is
/// fine (the engine splits each pair at the first `=`); with `escapes`, `\` and `,` are
/// written `\\` and `\,`.
pub fn wifi_spec(w: &WifiDetails, escapes: bool) -> Result<String, String> {
    validate_wifi(w, escapes)?;
    let e = |v: &str| if escapes { spec_escape(v) } else { v.to_string() };
    Ok(if w.password.is_empty() { format!("ssid={}", e(&w.ssid)) } else { format!("ssid={},psk={}", e(&w.ssid), e(&w.password)) })
}

/// A `--wifi` SPEC value: `\` -> `\\`, `,` -> `\,`.
pub fn spec_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace(',', "\\,")
}

/// The engine's `wifi_wanted` event: `{"t":"wifi_wanted","ssid":"X","secure":true|false|null,"served":bool}`,
/// the network the firmware asked for (a directed probe, or the security it picked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiWanted {
    pub ssid: String,
    /// `Some(true)` WPA2, `Some(false)` open, `None` not known yet (only probed).
    pub secure: Option<bool>,
    /// The access point lets that join through.
    pub served: bool,
}

impl WifiWanted {
    pub fn parse(text: &str) -> Option<WifiWanted> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if v.get("t")?.as_str()? != "wifi_wanted" {
            return None;
        }
        let ssid = v.get("ssid")?.as_str()?;
        // Not a name to offer: empty, or control bytes / invalid UTF-8 (seen from openHASP's
        // setup mode: a probe for "\u{2}").
        if ssid.is_empty() || ssid.chars().any(|c| c.is_control() || c == '\u{FFFD}') {
            return None;
        }
        Some(WifiWanted { ssid: ssid.to_string(), secure: v.get("secure").and_then(|s| s.as_bool()), served: v.get("served").and_then(|s| s.as_bool()).unwrap_or(false) })
    }

    /// "secured", "open" or "security not known yet".
    pub fn security(&self) -> &'static str {
        match self.secure {
            Some(true) => "secured",
            Some(false) => "open",
            None => "security not known yet",
        }
    }

    /// Joining needs a password this device doesn't have: WPA2 and not let through.
    pub fn needs_password(&self) -> bool {
        self.secure == Some(true) && !self.served
    }
}

/// The engine features this needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetFeatures {
    pub wifi: bool,
    /// `--wifi any` / `ssid=*,psk=…` (answer any network name).
    pub wifi_any: bool,
    /// `\,` and `\\` escapes in the `--wifi` SPEC.
    pub wifi_escapes: bool,
    pub hostfwd: bool,
    pub mdns_relay: bool,
}

/// The network part of the engine command line, plus notes for the console.
/// `port_free(addr, port)` says whether this PC's port can be listened on.
pub fn engine_args(net: &DeviceNetwork, defaults: &WifiDetails, f: NetFeatures, port_free: &dyn Fn(&str, u16) -> bool) -> (Vec<String>, Vec<String>) {
    let (mut a, mut notes) = (Vec::new(), Vec::new());
    if !net.wifi {
        return (a, notes);
    }
    if !f.wifi {
        notes.push("Wi-Fi off: this engine build has no --wifi".into());
        return (a, notes);
    }
    let w = net.effective(defaults);
    let spec = match wifi_spec(&w, f.wifi_escapes) {
        Ok(_) if net.accept_any && f.wifi_any => {
            if w.password.is_empty() { "any".to_string() } else { format!("ssid=*,psk={}", spec_escape(&w.password)) }
        }
        Ok(s) => {
            if net.accept_any {
                notes.push(format!("\"Accept any network name\" needs a newer esp32sim; offering \"{}\" only", w.ssid));
            }
            s
        }
        Err(e) => {
            notes.push(format!("Wi-Fi off: {e}. Fix it in the device's Network section."));
            return (a, notes);
        }
    };
    a.push("--wifi".into());
    a.push(spec);
    a.push("--net".into());
    a.push("nat".into());
    if f.hostfwd {
        for fw in net.active_forwards() {
            if port_free(net.bind_addr(), fw.host) {
                a.push("--hostfwd".into());
                a.push(format!("tcp:{}:{}-:{}", net.bind_addr(), fw.host, fw.guest));
            } else {
                notes.push(format!("port {} on this PC is in use (another device or program), so the firmware's port {} is not published. Change it in the device's Network section.", fw.host, fw.guest));
            }
        }
    } else {
        if !net.forwards.is_empty() {
            notes.push("this engine build has no --hostfwd: the firmware's ports are not reachable from this PC or the network".into());
        }
    }
    if f.mdns_relay && net.lan {
        a.push("--mdns-relay".into());
        a.push("auto".into());
    }
    (a, notes)
}

/// This PC's address on the local network (the one a default route would use), if any.
/// Sends nothing: connecting a UDP socket only picks the route. On Windows, when that
/// route goes through a virtual adapter (WSL / Hyper-V vEthernet, VirtualBox, VMware, a
/// VPN such as Tailscale), the first real adapter with a gateway is used instead.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    let routed = s.connect("192.0.2.1:9").ok().and_then(|_| s.local_addr().ok()).map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    pick_lan_ip(routed, &adapters())
}

/// A network adapter: its name, IPv4 addresses and whether it has a default gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    pub name: String,
    pub v4: Vec<std::net::Ipv4Addr>,
    pub gateway: bool,
}

/// Adapter names (friendly name or description) that are not the PC's real LAN.
pub fn is_virtual_adapter(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["vethernet", "wsl", "hyper-v", "virtualbox", "vmware", "tailscale", "zerotier", "wireguard", "loopback", "docker", "openvpn", "tap-windows", "npcap", "bluetooth"].iter().any(|k| n.contains(k))
}

/// Choose the LAN address: the routed one unless it belongs to a virtual adapter, then the
/// first private IPv4 of a real adapter with a gateway, then any real private IPv4.
pub fn pick_lan_ip(routed: Option<std::net::IpAddr>, adapters: &[Adapter]) -> Option<std::net::IpAddr> {
    let owner = |ip: std::net::IpAddr| adapters.iter().find(|a| a.v4.iter().any(|v| std::net::IpAddr::V4(*v) == ip));
    if let Some(ip) = routed {
        match owner(ip) {
            Some(a) if is_virtual_adapter(&a.name) => {}
            _ => return Some(ip),
        }
    }
    let real = || adapters.iter().filter(|a| !is_virtual_adapter(&a.name));
    let usable = |v: &&std::net::Ipv4Addr| v.is_private() && !v.is_link_local();
    real().filter(|a| a.gateway).flat_map(|a| a.v4.iter()).find(usable).or_else(|| real().flat_map(|a| a.v4.iter()).find(usable)).map(|v| std::net::IpAddr::V4(*v)).or(routed)
}

/// The PC's adapters (Windows: GetAdaptersAddresses). Elsewhere empty: the routed address
/// is used as it is.
#[cfg(windows)]
pub fn adapters() -> Vec<Adapter> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH};
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    let mut out = Vec::new();
    let mut len: u32 = 32 * 1024;
    for _ in 0..3 {
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let p = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        let r = unsafe { GetAdaptersAddresses(AF_INET as u32, GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST, std::ptr::null(), p, &mut len) };
        if r == 111 {
            continue; // ERROR_BUFFER_OVERFLOW: len now holds the size needed
        }
        if r != 0 {
            return out;
        }
        let wide = |w: *const u16| -> String {
            if w.is_null() {
                return String::new();
            }
            let mut n = 0;
            unsafe {
                while *w.add(n) != 0 {
                    n += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(w, n))
            }
        };
        let mut a = p as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let ad = unsafe { &*a };
            if ad.OperStatus == IfOperStatusUp {
                let mut v4 = Vec::new();
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let ua = unsafe { &*u };
                    let sa = ua.Address.lpSockaddr;
                    if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                        let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                        v4.push(std::net::Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr })));
                    }
                    u = ua.Next;
                }
                let name = format!("{} {}", wide(ad.FriendlyName), wide(ad.Description));
                out.push(Adapter { name, v4, gateway: !ad.FirstGatewayAddress.is_null() });
            }
            a = ad.Next;
        }
        return out;
    }
    out
}

#[cfg(not(windows))]
pub fn adapters() -> Vec<Adapter> {
    Vec::new()
}

/// What the console says about joining Wi-Fi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiEvent {
    /// The firmware is trying this network.
    Trying(String),
    /// Joining failed. `ssid` when the line names it; `auth` = the password was wrong
    /// (auth/handshake failure) rather than the network not being found.
    Failed { ssid: Option<String>, reason: String, auth: bool },
    Connected,
}

fn quoted_after(line: &str, key: &str) -> Option<String> {
    let rest = &line[line.find(key)? + key.len()..];
    let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let rest = &rest[1..];
    Some(rest[..rest.find(q)?].to_string())
}

/// ESP-IDF `wifi_err_reason_t` codes and names that mean "wrong password".
const AUTH_REASONS: &[&str] = &["AUTH_FAIL", "AUTH_EXPIRE", "4WAY_HANDSHAKE_TIMEOUT", "HANDSHAKE_TIMEOUT", "MIC_FAILURE", "reason: 202", "reason: 15", "reason: 204", "Reason: 202", "Reason: 15", "Reason: 204"];
/// ... and "no such network".
const NOT_FOUND_REASONS: &[&str] = &["NO_AP_FOUND", "reason: 201", "Reason: 201", "Could not find network", "No matching network", "Could not find WiFi", "SSID not found"];

/// Read one console line (ANSI already stripped). Knows the usual wording of ESP-IDF's and
/// Arduino-ESP32's Wi-Fi logs, ESPHome's, and common sketch prints.
pub fn wifi_event(line: &str) -> Option<WifiEvent> {
    let lower = line.to_ascii_lowercase();
    // ESPHome: "[W][wifi_esp32:866]: Disconnected ssid='X' bssid=.. reason='Probe Request Unsuccessful'"
    if line.contains("Disconnected ssid=") {
        let ssid = quoted_after(line, "ssid=");
        let reason = quoted_after(line, "reason=").unwrap_or_default();
        let r = reason.to_ascii_lowercase();
        let auth = r.contains("handshake") || r.contains("auth") || r.contains("mic failure") || r.contains("password");
        return Some(WifiEvent::Failed { ssid, reason, auth });
    }
    if let Some(r) = AUTH_REASONS.iter().find(|r| line.contains(**r)) {
        return Some(WifiEvent::Failed { ssid: None, reason: r.to_string(), auth: true });
    }
    if let Some(r) = NOT_FOUND_REASONS.iter().find(|r| line.contains(**r)) {
        let ssid = quoted_after(line, "network ").or_else(|| line.split("Could not find network ").nth(1).and_then(|t| t.split_whitespace().next()).map(str::to_string));
        return Some(WifiEvent::Failed { ssid, reason: r.to_string(), auth: false });
    }
    // "Connecting to 'X'" / "Connecting to \"X\"" (ESPHome, many sketches). Unquoted names
    // are ignored: "Connecting to WiFi..." would read as a network called "WiFi...".
    if lower.contains("connecting to ") && !lower.contains("mqtt") {
        let i = lower.find("connecting to ").unwrap() + "connecting to ".len();
        if let Some(s) = quoted_after(&line[i - 1..], " ") {
            return Some(WifiEvent::Trying(s));
        }
    }
    if (line.contains("[wifi") && line.trim_end().ends_with("]: Connected"))
        || lower.contains("wifi connected")
        || line.contains("WL_CONNECTED")
        || lower.contains("sta ip:")
        || lower.contains("got ip:")
        // openHASP: "WIFI: Received IP address 10.0.2.15"
        || lower.contains("received ip address")
        || line.contains("STA_GOT_IP")
    {
        return Some(WifiEvent::Connected);
    }
    None
}

/// The banner for the device, from the latest events: firmware looking for a different
/// network than the access point offers, or the wrong password.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiWatch {
    /// The network the firmware asked for, when a line named it.
    pub wanted: Option<String>,
    pub banner: Option<String>,
    pub connected: bool,
}

impl WifiWatch {
    /// Returns true when this line says the firmware is on Wi-Fi.
    pub fn feed(&mut self, line: &str, ap: &WifiDetails) -> bool {
        // "Connected to esp32sim" (openHASP, many sketches) names this device's network.
        let named = !ap.ssid.is_empty() && line.contains(&format!("Connected to {}", ap.ssid)) && !line.to_ascii_lowercase().contains("disconnected");
        let Some(ev) = wifi_event(line).or(named.then_some(WifiEvent::Connected)) else { return false };
        let on = ev == WifiEvent::Connected;
        match ev {
            WifiEvent::Trying(s) => {
                if s != ap.ssid {
                    self.banner = Some(mismatch(&s, &ap.ssid));
                }
                self.wanted = Some(s);
            }
            WifiEvent::Failed { ssid, auth, .. } => {
                self.connected = false;
                let ssid = ssid.or_else(|| self.wanted.clone());
                self.banner = Some(match &ssid {
                    Some(s) if *s != ap.ssid => mismatch(s, &ap.ssid),
                    _ if auth => format!("The firmware reached Wi-Fi \"{}\" but its password doesn't match: set this device's Wi-Fi password to the one built into the firmware.", ap.ssid),
                    Some(s) => format!("The firmware could not join Wi-Fi \"{s}\"."),
                    None => format!("The firmware can't find the Wi-Fi network it wants. This device's access point offers \"{}\": set this device's Wi-Fi to the network name and password built into the firmware.", ap.ssid),
                });
                if ssid.is_some() {
                    self.wanted = ssid;
                }
            }
            WifiEvent::Connected => {
                self.connected = true;
                self.banner = None;
            }
        }
        on
    }
}

fn mismatch(wanted: &str, ap: &str) -> String {
    format!("Firmware is looking for Wi-Fi \"{wanted}\", but this device's access point offers \"{ap}\": set this device's Wi-Fi to match (Network > network name and password).")
}

/// An ESPHome native-API client connecting, as the firmware logs it
/// (`[D][api.connection:2816]: Home Assistant 2026.10.1 (10.0.2.2): connected`): the client's
/// name. `None` for any other line (also "disconnected").
#[cfg(test)]
pub fn api_client_connected(line: &str) -> Option<String> {
    api_client_event(line).filter(|e| e.connected).map(|e| e.name)
}

/// Who an API client is, by the name it gave the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiClientKind {
    /// "Home Assistant 2026.10.1": the device is added to Home Assistant.
    HomeAssistant,
    /// "ESPHome Logs 2026.10.0": the ESPHome dashboard's log viewer, not Home Assistant.
    DashboardLogs,
    /// aioesphomeapi scripts, other tools.
    Other,
}

pub fn api_client_kind(name: &str) -> ApiClientKind {
    if name.starts_with("Home Assistant") {
        ApiClientKind::HomeAssistant
    } else if name.starts_with("ESPHome Logs") || name.starts_with("ESPHome Dashboard") {
        ApiClientKind::DashboardLogs
    } else {
        ApiClientKind::Other
    }
}

/// An API client connecting or going away, from ESPHome's `[api.connection]` log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiClientEvent {
    /// "name (address)", as logged: tells two clients with one name apart.
    pub key: String,
    pub name: String,
    pub connected: bool,
}

pub fn api_client_event(line: &str) -> Option<ApiClientEvent> {
    let rest = &line[line.find("[api.connection")?..];
    let msg = rest.split_once("]:")?.1.trim();
    let (who, how) = msg.rsplit_once(": ")?;
    let how = how.trim().to_ascii_lowercase();
    let connected = match how.as_str() {
        "connected" | "connected successfully" => true,
        "disconnected" | "disconnecting" | "connection closed" | "socket closed" | "closed" => false,
        _ if how.starts_with("disconnect") || how.contains("closed") || how.contains("reading failed") || how.contains("timeout") => false,
        _ => return None,
    };
    let who = who.trim();
    let name = who.rsplit_once(" (").map(|(n, _)| n).unwrap_or(who).trim();
    (!name.is_empty()).then(|| ApiClientEvent { key: who.to_string(), name: name.to_string(), connected })
}

/// The API clients the firmware's log says are connected, this run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiClients {
    /// (key, name), oldest first.
    pub list: Vec<(String, String)>,
}

impl ApiClients {
    pub fn feed(&mut self, line: &str) {
        let Some(e) = api_client_event(line) else { return };
        self.list.retain(|(k, _)| *k != e.key);
        if e.connected {
            self.list.push((e.key, e.name));
        }
    }
    /// The connected Home Assistant's name ("Home Assistant 2026.10.1").
    pub fn home_assistant(&self) -> Option<&str> {
        self.list.iter().rev().map(|(_, n)| n.as_str()).find(|n| api_client_kind(n) == ApiClientKind::HomeAssistant)
    }
    pub fn dashboard_logs(&self) -> bool {
        self.list.iter().any(|(_, n)| api_client_kind(n) == ApiClientKind::DashboardLogs)
    }
    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// A log line saying Home Assistant refused an action the device asked for (the device
/// is not allowed to perform Home Assistant actions / make service calls).
pub fn ha_action_refused(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    (l.contains("not allowed") || l.contains("refus") || l.contains("denied"))
        && (l.contains("home assistant action") || l.contains("service call") || l.contains("perform action") || l.contains("homeassistant.action") || l.contains("homeassistant.service"))
        || l.contains("allow the device to perform home assistant actions")
        || l.contains("allow the device to make home assistant service calls")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_clients_from_the_log() {
        assert_eq!(api_client_connected("[D][api.connection:2816]: aioesphomeapi (10.0.2.2): connected").as_deref(), Some("aioesphomeapi"));
        assert_eq!(api_client_connected("[12:00:01][D][api.connection:2816]: Home Assistant 2026.10.1 (10.0.2.2): connected").as_deref(), Some("Home Assistant 2026.10.1"));
        assert_eq!(api_client_connected("[D][api.connection:2816]: aioesphomeapi (10.0.2.2): disconnected"), None);
        assert_eq!(api_client_connected("[D][api:323]: Accept 10.0.2.2"), None);
    }

    #[test]
    fn api_clients_told_apart() {
        let mut c = ApiClients::default();
        c.feed("[D][api.connection:2816]: ESPHome Logs 2026.10.0 (10.0.2.2): connected");
        assert!(c.dashboard_logs() && c.home_assistant().is_none());
        c.feed("[12:00:01][I][api.connection:2816]: Home Assistant 2026.10.1 (10.0.2.2): connected");
        assert_eq!(c.home_assistant(), Some("Home Assistant 2026.10.1"));
        c.feed("[W][api.connection:200]: Home Assistant 2026.10.1 (10.0.2.2): Connection closed");
        assert!(c.home_assistant().is_none() && c.dashboard_logs());
        c.feed("[D][api.connection:2816]: ESPHome Logs 2026.10.0 (10.0.2.2): disconnected");
        assert!(c.list.is_empty());
        assert_eq!(api_client_kind("aioesphomeapi"), ApiClientKind::Other);
        c.feed("[D][api.connection:2816]: aioesphomeapi (10.0.2.2): connected");
        assert!(c.home_assistant().is_none() && !c.dashboard_logs() && c.list.len() == 1);
        assert!(ha_action_refused("WARNING Can't call action light.toggle: the device is not allowed to perform Home Assistant actions"));
        assert!(ha_action_refused("[W][homeassistant.action]: service call refused"));
        assert!(!ha_action_refused("[D][api.connection]: Home Assistant 2026.10.1 (10.0.2.2): connected"));
    }

    #[test]
    fn lan_ip_skips_virtual_adapters() {
        use std::net::{IpAddr, Ipv4Addr};
        let ad = |name: &str, ip: [u8; 4], gw: bool| Adapter { name: name.into(), v4: vec![Ipv4Addr::from(ip)], gateway: gw };
        let list = vec![
            ad("vEthernet (WSL (Hyper-V firewall)) Hyper-V Virtual Ethernet Adapter", [172, 28, 32, 1], false),
            ad("Tailscale Tailscale Tunnel", [100, 101, 1, 2], false),
            ad("Ethernet Intel(R) Ethernet Connection", [192, 168, 1, 50], true),
            ad("VirtualBox Host-Only Network", [192, 168, 56, 1], false),
        ];
        let v4 = |a: [u8; 4]| Some(IpAddr::V4(Ipv4Addr::from(a)));
        // the route already picks the real adapter
        assert_eq!(pick_lan_ip(v4([192, 168, 1, 50]), &list), v4([192, 168, 1, 50]));
        // the route goes through WSL's switch: use the real one
        assert_eq!(pick_lan_ip(v4([172, 28, 32, 1]), &list), v4([192, 168, 1, 50]));
        // no route at all
        assert_eq!(pick_lan_ip(None, &list), v4([192, 168, 1, 50]));
        // no adapter list (Linux): the routed address as it is
        assert_eq!(pick_lan_ip(v4([10, 0, 0, 7]), &[]), v4([10, 0, 0, 7]));
        assert!(is_virtual_adapter("VMware Network Adapter VMnet8"));
        assert!(!is_virtual_adapter("Wi-Fi Intel(R) Wi-Fi 6 AX201 160MHz"));
    }

    #[test]
    fn preset_skips_ports_in_use() {
        let mut n = DeviceNetwork::default();
        let added = n.add_preset_free(&PRESETS[0], &[], &|p| p != 6053 && p != 3232);
        assert_eq!(added, 2);
        let got: Vec<(u16, u16)> = n.forwards.iter().map(|f| (f.host, f.guest)).collect();
        assert_eq!(got, vec![(6054, 6053), (3233, 3233)]);
        assert_eq!(n.add_preset_free(&PRESETS[0], &[], &|_| true), 0, "already there");
    }

    fn w(s: &str, p: &str) -> WifiDetails {
        WifiDetails { ssid: s.into(), password: p.into() }
    }

    fn wifi_spec_old(w: &WifiDetails) -> Result<String, String> {
        wifi_spec(w, false)
    }

    #[test]
    fn wifi_spec_escapes_and_wanted_events() {
        let e = |s: &str, p: &str| wifi_spec(&w(s, p), true);
        assert_eq!(e("Home,Net", "sec,ret\\12").unwrap(), "ssid=Home\\,Net,psk=sec\\,ret\\\\12");
        assert_eq!(e("Café 5G", "").unwrap(), "ssid=Café 5G");
        assert!(e("x", &"ab".repeat(32)).is_ok(), "a 64-hex-digit raw key");
        assert!(e("x", &"zz".repeat(32)).is_err());
        let any = NetFeatures { wifi: true, wifi_any: true, wifi_escapes: true, hostfwd: false, mdns_relay: false };
        let n = DeviceNetwork { ssid: "esp32sim".into(), password: "pa,ss\\word".into(), accept_any: true, lan: false, ..Default::default() };
        assert_eq!(engine_args(&n, &WifiDetails::default(), any, &|_, _| true).0.join(" "), "--wifi ssid=*,psk=pa\\,ss\\\\word --net nat");
        let ev = WifiWanted::parse(r#"{"t":"wifi_wanted","ssid":"cyd-lab","secure":true,"served":false}"#).unwrap();
        assert_eq!(ev, WifiWanted { ssid: "cyd-lab".into(), secure: Some(true), served: false });
        assert!(ev.needs_password());
        let probe = WifiWanted::parse(r#"{"t":"wifi_wanted","ssid":"Home Net","secure":null,"served":true}"#).unwrap();
        assert_eq!((probe.secure, probe.served, probe.security(), probe.needs_password()), (None, true, "security not known yet", false));
        assert_eq!(WifiWanted::parse(r#"{"t":"ap_status"}"#), None);
        assert_eq!(WifiWanted::parse("not json"), None);
        assert_eq!(WifiWanted::parse(r#"{"t":"wifi_wanted","ssid":"\u0002","secure":null,"served":false}"#), None);
        assert_eq!(WifiWanted::parse(r#"{"t":"wifi_wanted","ssid":"","secure":null,"served":false}"#), None);
    }

    #[test]
    fn wifi_spec_and_validation() {
        assert_eq!(wifi_spec_old(&w("HomeNet", "secret123")).unwrap(), "ssid=HomeNet,psk=secret123");
        assert_eq!(wifi_spec_old(&w("Cafe Guest", "")).unwrap(), "ssid=Cafe Guest");
        assert_eq!(wifi_spec_old(&w("a=b", "x=y=z!\"#")).unwrap(), "ssid=a=b,psk=x=y=z!\"#");
        assert!(wifi_spec_old(&w("Home,Net", "secret123")).unwrap_err().contains("comma"));
        assert!(wifi_spec_old(&w("Home", "sec,ret123")).unwrap_err().contains("comma"));
        assert!(wifi_spec_old(&w("Home", "short")).is_err());
        assert!(wifi_spec_old(&w("", "")).is_err());
        assert!(wifi_spec_old(&w(&"x".repeat(33), "")).is_err());
    }

    #[test]
    fn presets_and_ports() {
        let d = WifiDetails::default();
        let mut a = DeviceNetwork::for_new_device(&d);
        assert!(a.wifi && a.lan && a.forwards.is_empty());
        assert_eq!((a.ssid.as_str(), a.password.as_str(), a.accept_any), ("esp32sim", "", false));
        a.add_preset(&PRESETS[0], &[]);
        let pairs: Vec<(u16, u16)> = a.forwards.iter().map(|f| (f.host, f.guest)).collect();
        assert_eq!(pairs, vec![(6053, 6053), (3232, 3232)]);
        a.add_preset(&PRESETS[1], &[]); // Arduino OTA: an OTA forward exists already
        a.add_preset(&PRESETS[2], &[]);
        assert_eq!(a.forwards.len(), 3);
        assert_eq!((a.forwards[2].host, a.forwards[2].guest), (8080, 80));
        // a second device: API keeps the firmware port, OTA moves both
        let mut b = DeviceNetwork::for_new_device(&w("HomeNet", "secret123"));
        b.add_preset(&PRESETS[0], &[a.clone()]);
        let pairs: Vec<(u16, u16)> = b.forwards.iter().map(|f| (f.host, f.guest)).collect();
        assert_eq!(pairs, vec![(6054, 6053), (3233, 3233)]);
        assert_eq!(b.ssid, "HomeNet");
    }

    #[test]
    fn effective_wifi() {
        let mut n = DeviceNetwork::default();
        assert_eq!(n.effective(&WifiDetails::default()), w("cyd-lab", ""));
        assert_eq!(n.effective(&w("Home", "12345678")), w("Home", "12345678"));
        n.ssid = "Lab".into();
        assert_eq!(n.effective(&w("Home", "12345678")), w("Lab", ""));
    }

    #[test]
    fn engine_command_line() {
        let all = NetFeatures { wifi: true, wifi_any: false, wifi_escapes: false, hostfwd: true, mdns_relay: true };
        let mut n = DeviceNetwork { ssid: "HomeNet".into(), password: "secret123".into(), ..Default::default() };
        let (a, notes) = engine_args(&n, &WifiDetails::default(), all, &|_, _| true);
        assert_eq!(a.join(" "), "--wifi ssid=HomeNet,psk=secret123 --net nat --mdns-relay auto");
        assert!(notes.is_empty());
        n.add_preset(&PRESETS[0], &[]);
        n.add_preset(&PRESETS[2], &[]);
        let (a, _) = engine_args(&n, &WifiDetails::default(), all, &|_, _| true);
        assert_eq!(a.join(" "), "--wifi ssid=HomeNet,psk=secret123 --net nat --hostfwd tcp:0.0.0.0:6053-:6053 --hostfwd tcp:0.0.0.0:3232-:3232 --hostfwd tcp:0.0.0.0:8080-:80 --mdns-relay auto");
        // only this PC, no mDNS relay, busy port skipped with a note
        n.lan = false;
        let (a, notes) = engine_args(&n, &WifiDetails::default(), NetFeatures { mdns_relay: false, ..all }, &|addr, p| addr == "127.0.0.1" && p != 3232);
        assert_eq!(a.join(" "), "--wifi ssid=HomeNet,psk=secret123 --net nat --hostfwd tcp:127.0.0.1:6053-:6053 --hostfwd tcp:127.0.0.1:8080-:80");
        assert!(notes[0].contains("3232"));
        // Wi-Fi off, or an engine without it
        assert!(engine_args(&DeviceNetwork { wifi: false, ..n.clone() }, &WifiDetails::default(), all, &|_, _| true).0.is_empty());
        let (a, notes) = engine_args(&n, &WifiDetails::default(), NetFeatures::default(), &|_, _| true);
        assert!(a.is_empty() && notes[0].contains("--wifi"));
        // a comma can't be passed: Wi-Fi off with the reason
        let (a, notes) = engine_args(&DeviceNetwork { ssid: "a,b".into(), ..n }, &WifiDetails::default(), all, &|_, _| true);
        assert!(a.is_empty() && notes[0].contains("comma"));
    }

    #[test]
    fn accept_any_network_name() {
        let f = NetFeatures { wifi: true, wifi_any: false, wifi_escapes: false, hostfwd: false, mdns_relay: false };
        let mut n = DeviceNetwork { ssid: "esp32sim".into(), accept_any: true, lan: false, ..Default::default() };
        // An engine without match-any: the named network, with a note.
        let (a, notes) = engine_args(&n, &WifiDetails::default(), f, &|_, _| true);
        assert_eq!(a.join(" "), "--wifi ssid=esp32sim --net nat");
        assert!(notes[0].contains("Accept any network name"));
        let any = NetFeatures { wifi_any: true, ..f };
        assert_eq!(engine_args(&n, &WifiDetails::default(), any, &|_, _| true).0.join(" "), "--wifi any --net nat");
        n.password = "esp32sim-pass".into();
        assert_eq!(engine_args(&n, &WifiDetails::default(), any, &|_, _| true).0.join(" "), "--wifi ssid=*,psk=esp32sim-pass --net nat");
        // Saved in device.toml; older files without the field read as off.
        let t = toml::to_string(&n).unwrap();
        assert!(t.contains("accept_any = true"));
        assert_eq!(toml::from_str::<DeviceNetwork>(&t).unwrap(), n);
        assert!(!toml::from_str::<DeviceNetwork>("ssid = \"x\"").unwrap().accept_any);
    }

    #[test]
    fn console_wifi_watch_esphome() {
        let ap = w("cyd-lab", "");
        let mut watch = WifiWatch::default();
        // lines as ESPHome 2026.10.0 prints them (ANSI stripped)
        watch.feed("[I][wifi:1250]: Connecting to 'MyHomeWiFi' (any) (priority 0, attempt 1/1 in phase RETRY_HIDDEN)...", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("looking for Wi-Fi \"MyHomeWiFi\""));
        watch.feed("[W][wifi_esp32:866]: Disconnected ssid='MyHomeWiFi' reason='Probe Request Unsuccessful'", &ap);
        assert_eq!(watch.wanted.as_deref(), Some("MyHomeWiFi"));
        watch.feed("[I][wifi:1732]: Connected", &ap);
        assert!(watch.banner.is_none() && watch.connected);
        let ap = w("HomeNet", "wrongpass");
        let mut watch = WifiWatch::default();
        watch.feed("[I][wifi:1250]: Connecting to 'HomeNet' (any) ...", &ap);
        assert!(watch.banner.is_none());
        watch.feed("[W][wifi_esp32:866]: Disconnected ssid='HomeNet' bssid=02:53:49:4D:00:01 reason='4-Way Handshake Timeout'", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("password"));
    }

    #[test]
    fn console_wifi_watch_idf_arduino() {
        let ap = w("LabNet", "arduino123");
        // Arduino-ESP32 core 3.x (core debug level >= warning)
        let mut watch = WifiWatch::default();
        watch.feed("[  5012][W][STA.cpp:137] _onStaArduinoEvent(): Reason: 201 - NO_AP_FOUND", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("can't find the Wi-Fi network"));
        watch.feed("[  9012][W][STA.cpp:137] _onStaArduinoEvent(): Reason: 15 - 4WAY_HANDSHAKE_TIMEOUT", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("password"));
        watch.feed("[ 12000][V][STA.cpp:..] _onStaArduinoEvent(): STA Got New IP:10.0.2.15 (ARDUINO_EVENT_WIFI_STA_GOT_IP)", &ap);
        assert!(watch.banner.is_none());
        // ESP-IDF example output and sketch prints
        let mut watch = WifiWatch::default();
        watch.feed("W (5123) wifi:reason: 202 AUTH_FAIL", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("password"));
        watch.feed("I (6123) esp_netif_handlers: sta ip: 10.0.2.15, mask: 255.255.255.0, gw: 10.0.2.2", &ap);
        assert!(watch.banner.is_none());
        watch.feed("Could not find network HomeNet (status 1), retrying", &ap);
        assert!(watch.banner.as_deref().unwrap().contains("looking for Wi-Fi \"HomeNet\""));
        // openHASP after its setup page saved the Wi-Fi
        let mut watch = WifiWatch::default();
        let ap8 = w("esp32sim", "esp32sim-pass");
        assert!(watch.feed("[       2.966][77812/87700 11][42468/42476  1] WIFI: Connected to esp32sim, requesting IP...", &ap8) && watch.connected);
        assert!(watch.feed("WIFI: Received IP address 10.0.2.15", &ap8));
        assert!(!watch.feed("WIFI: Disconnected from esp32sim (Reason: 201)", &ap8));
        let mut watch = WifiWatch::default();
        watch.feed("Connecting to \"LabNet\"", &ap);
        assert_eq!(watch.wanted.as_deref(), Some("LabNet"));
        // not a network name
        assert_eq!(wifi_event("Connecting to WiFi..."), None);
        assert_eq!(wifi_event("Connecting to MQTT 'broker'"), None);
    }
}
