//! "Device hotspot": many firmwares start their own access point for first setup
//! (openHASP `HASP-xxxxxx`, Tasmota `tasmota-XXXXXX`, WLED `WLED-AP`, ESPHome's fallback
//! AP) and serve a setup page at 192.168.4.1. On a real board you join that network with
//! a phone; here the engine joins it for you as a client (`--ap-client`) and forwards a
//! port on this PC to the page (`--ap-fwd`).
//!
//! Those engine flags are still being added, so this module only plans the arguments and
//! collects the advertised network names the firmware prints; the UI shows the feature
//! greyed out ("needs newer engine") until the engine's `--help` lists both flags.

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
        format!("http://127.0.0.1:{}/", self.host_port)
    }
}

/// An access point name the firmware announced in its log, if this line has one.
/// Covers ESPHome (`AP SSID: 'name'`), Arduino/IDF prints (`softAP SSID: name`,
/// `Starting AP: name`, `AP started: name`), Tasmota (`WIF: AP1 tasmota-XXXX`-style
/// `Starting AccessPoint` lines are too varied, so only quoted names are taken there).
pub fn sniff_ssid(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    for key in ["ap ssid:", "softap ssid:", "softap ssid =", "starting ap:", "ap started:", "access point:", "hotspot:", "wifimanager ap:"] {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut l = vec![];
        remember(&mut l, "a".into());
        remember(&mut l, "b".into());
        remember(&mut l, "a".into());
        assert_eq!(l, ["b", "a"]);
    }
}
