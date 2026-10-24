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

/// SSID used when neither the device nor the app-wide default has one.
pub const FALLBACK_SSID: &str = "cyd-lab";

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
    /// "Reach from my network": publish on every interface (0.0.0.0) rather than only on
    /// this PC (127.0.0.1).
    pub lan: bool,
    /// Published ports.
    pub forwards: Vec<Forward>,
}

impl Default for DeviceNetwork {
    fn default() -> Self {
        DeviceNetwork { wifi: true, ssid: String::new(), password: String::new(), lan: true, forwards: Vec::new() }
    }
}

impl DeviceNetwork {
    /// Settings for a new device: Wi-Fi on with the app-wide details, no ports published.
    pub fn for_new_device(defaults: &WifiDetails) -> DeviceNetwork {
        DeviceNetwork { ssid: defaults.ssid.clone(), password: defaults.password.clone(), ..DeviceNetwork::default() }
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
    pub fn add_preset(&mut self, p: &Preset, others: &[DeviceNetwork]) {
        for (label, guest, host, same) in p.forwards {
            if self.forwards.iter().any(|f| f.guest == *guest || (f.same_port && *same && f.label.contains("OTA") && label.contains("OTA"))) {
                continue;
            }
            let used: Vec<u16> = others.iter().flat_map(|n| n.host_ports()).chain(self.host_ports()).collect();
            let h = (*host..u16::MAX).find(|h| !used.contains(h)).unwrap_or(*host);
            self.forwards.push(Forward { label: label.to_string(), host: h, guest: if *same { h } else { *guest }, same_port: *same });
        }
    }

    /// The address forwards listen on.
    pub fn bind_addr(&self) -> &'static str {
        if self.lan { "0.0.0.0" } else { "127.0.0.1" }
    }
}

/// Why an SSID or password can't be given to the engine. esp32sim's `--wifi` SPEC is
/// `key=value` pairs split on `,` with no escaping, so a comma can't be carried.
pub fn validate_wifi(w: &WifiDetails) -> Result<(), String> {
    if w.ssid.is_empty() {
        return Err("the network name is empty".into());
    }
    if w.ssid.len() > 32 {
        return Err(format!("the network name is {} bytes; Wi-Fi allows at most 32", w.ssid.len()));
    }
    if w.ssid.contains(',') || w.password.contains(',') {
        return Err("the engine can't take a comma in the network name or password yet (its --wifi option splits on commas)".into());
    }
    if !w.password.is_empty() && !(8..=63).contains(&w.password.chars().count()) {
        return Err("a WPA2 password is 8 to 63 characters (or leave it empty for an open network)".into());
    }
    Ok(())
}

/// `--wifi` SPEC: `ssid=NAME` (open) or `ssid=NAME,psk=PASSWORD`. `=` inside a value is
/// fine (the engine splits each pair at the first `=`).
pub fn wifi_spec(w: &WifiDetails) -> Result<String, String> {
    validate_wifi(w)?;
    Ok(if w.password.is_empty() { format!("ssid={}", w.ssid) } else { format!("ssid={},psk={}", w.ssid, w.password) })
}

/// The engine features this needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetFeatures {
    pub wifi: bool,
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
    let spec = match wifi_spec(&w) {
        Ok(s) => s,
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
/// Sends nothing: connecting a UDP socket only picks the route.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
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
    pub fn feed(&mut self, line: &str, ap: &WifiDetails) {
        let Some(ev) = wifi_event(line) else { return };
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
    }
}

fn mismatch(wanted: &str, ap: &str) -> String {
    format!("Firmware is looking for Wi-Fi \"{wanted}\", but this device's access point offers \"{ap}\": set this device's Wi-Fi to match (Network > network name and password).")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str, p: &str) -> WifiDetails {
        WifiDetails { ssid: s.into(), password: p.into() }
    }

    #[test]
    fn wifi_spec_and_validation() {
        assert_eq!(wifi_spec(&w("HomeNet", "secret123")).unwrap(), "ssid=HomeNet,psk=secret123");
        assert_eq!(wifi_spec(&w("Cafe Guest", "")).unwrap(), "ssid=Cafe Guest");
        assert_eq!(wifi_spec(&w("a=b", "x=y=z!\"#")).unwrap(), "ssid=a=b,psk=x=y=z!\"#");
        assert!(wifi_spec(&w("Home,Net", "secret123")).unwrap_err().contains("comma"));
        assert!(wifi_spec(&w("Home", "sec,ret123")).unwrap_err().contains("comma"));
        assert!(wifi_spec(&w("Home", "short")).is_err());
        assert!(wifi_spec(&w("", "")).is_err());
        assert!(wifi_spec(&w(&"x".repeat(33), "")).is_err());
    }

    #[test]
    fn presets_and_ports() {
        let d = WifiDetails::default();
        let mut a = DeviceNetwork::for_new_device(&d);
        assert!(a.wifi && a.lan && a.forwards.is_empty());
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
        let all = NetFeatures { wifi: true, hostfwd: true, mdns_relay: true };
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
        watch.feed("Connecting to \"LabNet\"", &ap);
        assert_eq!(watch.wanted.as_deref(), Some("LabNet"));
        // not a network name
        assert_eq!(wifi_event("Connecting to WiFi..."), None);
        assert_eq!(wifi_event("Connecting to MQTT 'broker'"), None);
    }
}
