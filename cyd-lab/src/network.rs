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

/// ESPHome's native API port.
pub const ESPHOME_API_PORT: u16 = 6053;
/// ESPHome's OTA port on ESP32.
pub const ESPHOME_OTA_PORT: u16 = 3232;
/// SSID used when neither the device nor the app-wide default has one.
pub const FALLBACK_SSID: &str = "cyd-lab";

/// Wi-Fi network name and password (app-wide default for new devices).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WifiDetails {
    pub ssid: String,
    pub password: String,
}

/// An extra published port: this PC's `host` port to the firmware's `guest` port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub host: u16,
    pub guest: u16,
}

/// Per-device network settings (`[network]` in device.toml). Stored in plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceNetwork {
    /// Give the chip an access point to join (`--wifi`).
    pub wifi: bool,
    /// Network name the emulated access point uses. Empty: the app-wide default.
    pub ssid: String,
    /// Its password; empty = an open network.
    pub password: String,
    /// "Reach from my network": publish on every interface (0.0.0.0) rather than only on
    /// this PC (127.0.0.1).
    pub lan: bool,
    /// This PC's port for the native API (Home Assistant connects here).
    pub api_port: u16,
    /// The firmware's API port (ESPHome default 6053).
    pub api_guest: u16,
    /// This PC's port for OTA uploads.
    pub ota_port: u16,
    /// The firmware's OTA port (ESPHome `ota: port:`, default 3232).
    pub ota_guest: u16,
    /// More published ports (e.g. 80 for ESPHome's web_server).
    pub extra: Vec<Forward>,
}

impl Default for DeviceNetwork {
    fn default() -> Self {
        DeviceNetwork {
            wifi: true,
            ssid: String::new(),
            password: String::new(),
            lan: true,
            api_port: ESPHOME_API_PORT,
            api_guest: ESPHOME_API_PORT,
            ota_port: ESPHOME_OTA_PORT,
            ota_guest: ESPHOME_OTA_PORT,
            extra: Vec::new(),
        }
    }
}

impl DeviceNetwork {
    /// Settings for a new device: the app-wide Wi-Fi details, and ESPHome's own ports
    /// for the first device; later devices get the next free ones (6054/3233, ...).
    pub fn for_new_device(defaults: &WifiDetails, others: &[DeviceNetwork]) -> DeviceNetwork {
        let used: Vec<u16> = others.iter().flat_map(|n| n.host_ports()).collect();
        let k = (0..1000u16).find(|k| !used.contains(&(ESPHOME_API_PORT + k)) && !used.contains(&(ESPHOME_OTA_PORT + k))).unwrap_or(0);
        DeviceNetwork {
            ssid: defaults.ssid.clone(),
            password: defaults.password.clone(),
            api_port: ESPHOME_API_PORT + k,
            ota_port: ESPHOME_OTA_PORT + k,
            ota_guest: ESPHOME_OTA_PORT + k,
            ..DeviceNetwork::default()
        }
    }

    /// The SSID/password the access point will use (device, else app default, else open
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

    /// All published (host, guest) pairs, API and OTA first. Zero ports are skipped.
    pub fn forwards(&self) -> Vec<Forward> {
        let mut v = vec![Forward { host: self.api_port, guest: self.api_guest }, Forward { host: self.ota_port, guest: self.ota_guest }];
        v.extend(self.extra.iter().copied());
        v.retain(|f| f.host != 0 && f.guest != 0);
        v
    }

    pub fn host_ports(&self) -> Vec<u16> {
        self.forwards().iter().map(|f| f.host).collect()
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
        for fw in net.forwards() {
            if port_free(net.bind_addr(), fw.host) {
                a.push("--hostfwd".into());
                a.push(format!("tcp:{}:{}-:{}", net.bind_addr(), fw.host, fw.guest));
            } else {
                notes.push(format!("port {} on this PC is in use (another device or program), so the firmware's port {} is not published. Change it in the device's Network section.", fw.host, fw.guest));
            }
        }
    } else {
        notes.push("this engine build has no --hostfwd: the firmware is not reachable from this PC or the network".into());
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
    /// Joining this network failed; `auth` = the password was wrong (handshake/auth).
    Failed { ssid: String, reason: String, auth: bool },
    /// No network with the wanted name was found in a scan.
    NotFound,
    Connected,
}

fn quoted_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.strip_prefix('\'')?;
    Some(&rest[..rest.find('\'')?])
}

/// Read one console line (ANSI already stripped). Knows ESPHome's and ESP-IDF's wording.
pub fn wifi_event(line: &str) -> Option<WifiEvent> {
    // ESPHome: "[W][wifi_esp32:866]: Disconnected ssid='X' bssid=.. reason='Probe Request Unsuccessful'"
    if line.contains("Disconnected ssid=") {
        let ssid = quoted_after(line, "ssid=")?.to_string();
        let reason = quoted_after(line, "reason=").unwrap_or("").to_string();
        let r = reason.to_ascii_lowercase();
        let auth = r.contains("handshake") || r.contains("auth") || r.contains("mic failure") || r.contains("password");
        return Some(WifiEvent::Failed { ssid, reason, auth });
    }
    // ESPHome: "[I][wifi:1250]: Connecting to 'X' (any) ..."; Arduino/MicroPython vary.
    if line.contains("[wifi") && line.contains("Connecting to '") {
        return quoted_after(line, "Connecting to ").map(|s| WifiEvent::Trying(s.to_string()));
    }
    if line.contains("[wifi") && (line.contains("No networks found") || line.contains("No matching network")) {
        return Some(WifiEvent::NotFound);
    }
    if (line.contains("[wifi") && line.trim_end().ends_with("]: Connected")) || line.contains("WiFi Connected") || line.contains("sta ip:") || line.contains("got ip:") {
        return Some(WifiEvent::Connected);
    }
    None
}

/// The banner for the device, from the latest events: firmware looking for a different
/// network than the access point, or the wrong password.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiWatch {
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
                self.banner = Some(if ssid != ap.ssid {
                    mismatch(&ssid, &ap.ssid)
                } else if auth {
                    format!("Firmware found Wi-Fi \"{ssid}\" but its password doesn't match: set this device's Wi-Fi password to the one in the firmware (ESPHome: wifi_password in secrets.yaml).")
                } else {
                    format!("Firmware could not join Wi-Fi \"{ssid}\".")
                });
                self.wanted = Some(ssid);
            }
            WifiEvent::NotFound => {}
            WifiEvent::Connected => {
                self.connected = true;
                self.banner = None;
            }
        }
    }
}

fn mismatch(wanted: &str, ap: &str) -> String {
    format!("Firmware is looking for Wi-Fi \"{wanted}\", but this device's access point is \"{ap}\": set this device's Wi-Fi to match (Network > Wi-Fi network name and password).")
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
    fn ports_for_new_devices() {
        let d = WifiDetails::default();
        let first = DeviceNetwork::for_new_device(&d, &[]);
        assert_eq!((first.api_port, first.api_guest, first.ota_port, first.ota_guest), (6053, 6053, 3232, 3232));
        let second = DeviceNetwork::for_new_device(&d, &[first.clone()]);
        assert_eq!((second.api_port, second.api_guest, second.ota_port, second.ota_guest), (6054, 6053, 3233, 3233));
        let third = DeviceNetwork::for_new_device(&w("HomeNet", "secret123"), &[first, second]);
        assert_eq!((third.api_port, third.ota_port), (6055, 3234));
        assert_eq!(third.ssid, "HomeNet");
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
        n.extra.push(Forward { host: 8080, guest: 80 });
        let (a, notes) = engine_args(&n, &WifiDetails::default(), all, &|_, _| true);
        assert_eq!(a.join(" "), "--wifi ssid=HomeNet,psk=secret123 --net nat --hostfwd tcp:0.0.0.0:6053-:6053 --hostfwd tcp:0.0.0.0:3232-:3232 --hostfwd tcp:0.0.0.0:8080-:80 --mdns-relay auto");
        assert!(notes.is_empty());
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
    fn console_wifi_watch() {
        let ap = w("cyd-lab", "");
        let mut watch = WifiWatch::default();
        // real lines from ESPHome 2026.10.0 (ANSI stripped)
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
}
