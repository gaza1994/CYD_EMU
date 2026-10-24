//! A virtual phone: the station that joins the guest's own access point (WIFI_MODE_AP or APSTA),
//! the way a user's phone joins a device's setup hotspot. It hears the guest's beacons (`hotspots`),
//! authenticates (open system), associates, runs the supplicant half of the WPA2-PSK four-way
//! handshake when the hotspot is secured, takes a DHCP lease from the guest's server and then
//! carries `--ap-fwd` host connections to the guest as TCP from its own address, through a NAT of
//! its own (`Nat::local`). Crypto passes through as it does for `VirtualAp`: protected frames carry
//! plaintext framed by an 8-byte CCMP header and 8 bytes of MIC space.

use super::{eapol_key, ies, AirFrame, RSN_IE};
use crate::nat::{HostFwdSpec, Nat};
use crate::net::packet::{ethernet, ip_packet, udp_packet};

#[cfg(test)]
mod tests;

/// The phone's MAC: locally administered, "PHONE".
pub const PHONE_MAC: [u8; 6] = [0x02, 0x50, 0x48, 0x4f, 0x4e, 0x45];
/// Hotspots remembered at once.
pub const HOTSPOTS_MAX: usize = 8;
const RETRY_US: u64 = 500_000;
const TRIES: u32 = 6;
const NET_POLL_US: u64 = 500;
/// With no hotspot, how often the forwards' host sockets are closed out (`Nat::unlinked`).
const UNLINKED_POLL_US: u64 = 50_000;

/// `--ap-client ssid=NAME[,psk=PASS]`: which hotspot to join, any open one without `ssid`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhoneConfig { pub ssid: Option<String>, pub psk: Option<String> }

impl PhoneConfig {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut c = PhoneConfig::default();
        for kv in spec.split(',').filter(|s| !s.is_empty()) {
            match kv.split_once('=') {
                Some(("ssid", v)) if !v.is_empty() && v.len() <= 32 => c.ssid = Some(v.into()),
                Some(("psk", v)) if (8..=63).contains(&v.len()) => c.psk = Some(v.into()),
                Some(("psk", _)) => return Err("psk must be 8..=63 characters".into()),
                _ => return Err(format!("unknown or invalid key in {kv:?} (ssid=NAME,psk=PASS)")),
            }
        }
        Ok(c)
    }
}

/// `--ap-fwd` and the control channel's `fwd`: a `--hostfwd` spec whose empty guest address means
/// the hotspot's gateway, 192.168.4.1 (IDF's default SoftAP address).
pub fn fwd_spec(spec: &str) -> Result<HostFwdSpec, String> {
    let mut parsed = HostFwdSpec::parse(spec)?;
    if spec.rsplit_once('-').is_some_and(|(_, g)| g.starts_with(':')) { parsed.guest_ip = [192, 168, 4, 1]; }
    Ok(parsed)
}

/// A network the guest advertises: what its beacons say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotspot { pub ssid: String, pub bssid: [u8; 6], pub channel: u8, pub secure: bool, pub last_us: u64 }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PhoneState { #[default] Scanning, Authenticating, Associating, Handshake, Dhcp, Up }

pub struct Phone {
    pub cfg: PhoneConfig,
    pub mac: [u8; 6],
    pub state: PhoneState,
    pub hotspots: Vec<Hotspot>,
    /// the lease: our address, the guest's (DHCP server and gateway) and its MAC
    pub ip: Option<[u8; 4]>,
    pub server: [u8; 4],
    pub nat: Nat,
    pub log: bool,
    bssid: [u8; 6],
    channel: u8,
    pmk: Option<[u8; 32]>,
    ptk: Vec<u8>,
    secured: bool,
    seq: u16,
    pn: u64,
    xid: u32,
    offer: Option<([u8; 4], [u8; 4])>,
    peer_mac: [u8; 6],
    retry_at: u64,
    tries: u32,
    net_polled_us: u64,
    pub(crate) queue: Vec<AirFrame>,
    warned: bool,
}

fn mac(b: &[u8]) -> [u8; 6] { b[..6].try_into().unwrap() }
fn be16(b: &[u8]) -> u16 { u16::from_be_bytes([b[0], b[1]]) }

impl Phone {
    pub fn new(cfg: PhoneConfig, log: bool) -> Self {
        let mut nat = Nat::new(log);
        nat.local = (PHONE_MAC, [0; 4]);
        Phone { cfg, mac: PHONE_MAC, state: PhoneState::Scanning, hotspots: Vec::new(), ip: None, server: [0; 4], nat, log,
                bssid: [0; 6], channel: 1, pmk: None, ptk: Vec::new(), secured: false, seq: 0, pn: 0, xid: 0x5048_4f00, offer: None,
                peer_mac: [0; 6], retry_at: 0, tries: 0, net_polled_us: 0, queue: Vec::new(), warned: false }
    }

    /// Publish `spec` (an address on the guest's network) on the host; connections wait for a lease.
    pub fn forward(&mut self, spec: &HostFwdSpec) -> std::io::Result<std::net::SocketAddr> { self.nat.listen(spec) }

    /// A guest reboot: the hotspot goes away and the phone scans again, keeping its forwards.
    pub fn rejoin(&mut self) {
        self.state = PhoneState::Scanning; self.hotspots.clear(); self.ip = None; self.secured = false; self.queue.clear(); self.offer = None;
    }

    /// Drop the association and lease and scan again; hotspots and forwards stay.
    fn unlink(&mut self) {
        self.state = PhoneState::Scanning; self.ip = None; self.secured = false; self.queue.clear(); self.offer = None; self.nat.local = (self.mac, [0; 4]); self.warned = false;
    }

    /// The control channel's `ap_client`: join another hotspot (or the same with another password)
    /// on its next beacon.
    pub fn configure(&mut self, cfg: PhoneConfig) { self.cfg = cfg; self.unlink(); }

    /// `{"state":..,"ip":..,"join":..,"hotspots":[{"ssid","bssid","channel","secure"}..]}` for the web UI and cyd-lab.
    pub fn status_json(&self) -> String {
        let q = |s: &str| format!("\"{}\"", crate::web::json_escape(s));
        let ip = self.ip.map_or("null".into(), |a| q(&format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])));
        let list: Vec<String> = self.hotspots.iter().map(|h| format!("{{\"ssid\":{},\"bssid\":{},\"channel\":{},\"secure\":{}}}", q(&h.ssid), q(&super::mac_str(&h.bssid)), h.channel, h.secure)).collect();
        format!("{{\"state\":{},\"ip\":{},\"join\":{},\"hotspots\":[{}]}}", q(&format!("{:?}", self.state).to_lowercase()), ip, self.cfg.ssid.as_deref().map_or("null".into(), q), list.join(","))
    }

    /// The channel and signal a frame from the phone arrives with.
    pub fn rx_meta(&self) -> (u8, i8) { (self.channel, -35) }

    fn net_poll_us(&self) -> u64 { if self.state == PhoneState::Up { NET_POLL_US } else { UNLINKED_POLL_US } }

    /// True when `step` has nothing to send at `now_us`.
    pub fn idle_at(&self, now_us: u64) -> bool {
        self.queue.iter().all(|a| a.at_us > now_us)
            && (matches!(self.state, PhoneState::Scanning | PhoneState::Up) || now_us < self.retry_at)
            && now_us.wrapping_sub(self.net_polled_us) < self.net_poll_us()
    }

    fn hdr(&mut self, fc: u16, a1: &[u8; 6], a3: &[u8; 6]) -> Vec<u8> {
        let mut f = Vec::with_capacity(128);
        f.extend_from_slice(&fc.to_le_bytes()); f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(a1); f.extend_from_slice(&self.mac); f.extend_from_slice(a3);
        f.extend_from_slice(&(self.seq << 4).to_le_bytes()); self.seq = self.seq.wrapping_add(1);
        f
    }
    fn send(&mut self, at_us: u64, frame: Vec<u8>) {
        if self.log { eprintln!("[wifi] phone -> {}", super::describe(&frame)); }
        self.queue.push(AirFrame { at_us, frame });
    }
    fn arm(&mut self, now_us: u64) { self.retry_at = now_us + RETRY_US; }

    fn auth(&mut self, now_us: u64) {
        let b = self.bssid;
        let mut f = self.hdr(11 << 4, &b, &b);
        f.extend_from_slice(&[0, 0, 1, 0, 0, 0]);                        // open system, seq 1
        self.state = PhoneState::Authenticating; self.arm(now_us); self.send(now_us + 300, f);
    }
    fn assoc(&mut self, now_us: u64) {
        let b = self.bssid;
        let secure = self.pmk.is_some();
        let ssid = self.hotspots.iter().find(|h| h.bssid == b).map(|h| h.ssid.clone()).unwrap_or_default();
        let mut f = self.hdr(0, &b, &b);
        f.extend_from_slice(&(0x0401u16 | if secure { 0x10 } else { 0 }).to_le_bytes()); f.extend_from_slice(&10u16.to_le_bytes());
        f.push(0); f.push(ssid.len() as u8); f.extend_from_slice(ssid.as_bytes());
        f.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]); f.extend_from_slice(&[50, 4, 0x30, 0x48, 0x60, 0x6c]);
        if secure { f.extend_from_slice(RSN_IE); }
        self.state = PhoneState::Associating; self.arm(now_us); self.send(now_us + 300, f);
    }

    /// An Ethernet frame to the guest, as an 802.11 data frame to the DS.
    fn data(&mut self, eth: &[u8], now_us: u64) {
        if eth.len() < 14 { return; }
        let b = self.bssid; let dst = mac(&eth[0..6]);
        let protected = if self.secured { 0x4000 } else { 0 };
        let mut f = self.hdr((2 << 2) | 0x0100 | protected, &b, &dst);
        if self.secured {
            self.pn += 1; let pn = self.pn;
            f.extend_from_slice(&[pn as u8, (pn >> 8) as u8, 0, 0x20, (pn >> 16) as u8, (pn >> 24) as u8, (pn >> 32) as u8, (pn >> 40) as u8]);
        }
        f.extend_from_slice(&[0xaa, 0xaa, 0x03, 0, 0, 0]); f.extend_from_slice(&eth[12..14]); f.extend_from_slice(&eth[14..]);
        if self.secured { f.extend_from_slice(&[0; 8]); }
        self.queue.push(AirFrame { at_us: now_us + 200, frame: f });
    }

    fn dhcp(&mut self, kind: u8, now_us: u64) {
        let mut p = vec![0u8; 240];
        p[0] = 1; p[1] = 1; p[2] = 6;
        p[4..8].copy_from_slice(&self.xid.to_be_bytes());
        p[10] = 0x80;                                                    // broadcast replies
        p[28..34].copy_from_slice(&self.mac);
        p[236..240].copy_from_slice(&[99, 130, 83, 99]);
        p.extend_from_slice(&[53, 1, kind]);
        if let (3, Some((ip, server))) = (kind, self.offer) { p.extend_from_slice(&[50, 4]); p.extend_from_slice(&ip); p.extend_from_slice(&[54, 4]); p.extend_from_slice(&server); }
        p.extend_from_slice(&[12, 5]); p.extend_from_slice(b"phone");
        p.extend_from_slice(&[55, 3, 1, 3, 6, 255]);
        let udp = udp_packet(&[0; 4], &[255; 4], 68, 67, &p);
        let eth = ethernet(&[0xff; 6], &self.mac, 0x0800, &ip_packet(17, &[0; 4], &[255; 4], &udp));
        if kind == 1 { self.state = PhoneState::Dhcp; }
        self.arm(now_us);
        self.data(&eth, now_us);
    }

    fn joined(&mut self, now_us: u64) {
        if self.log { eprintln!("[wifi] phone associated with {}{}", super::mac_str(&self.bssid), if self.secured { " (WPA2)" } else { "" }); }
        self.tries = 0; self.dhcp(1, now_us + 20_000);
    }

    /// The guest sent `f` on the air at `now_us`.
    pub fn on_guest_tx(&mut self, f: &[u8], now_us: u64) {
        if f.len() < 24 { return; }
        let fc = u16::from_le_bytes([f[0], f[1]]);
        let (ty, st) = ((fc >> 2) & 3, (fc >> 4) & 0xf);
        let (a1, a2) = (mac(&f[4..10]), mac(&f[10..16]));
        let ours = a1 == self.mac && a2 == self.bssid;
        match (ty, st) {
            (0, 8) | (0, 5) if f.len() >= 36 => self.heard(f, a2, now_us),
            (0, 11) if ours && self.state == PhoneState::Authenticating && f.len() >= 30 => {
                if u16::from_le_bytes([f[26], f[27]]) == 2 && u16::from_le_bytes([f[28], f[29]]) == 0 { self.tries = 0; self.assoc(now_us); }
            }
            (0, 1) | (0, 3) if ours && self.state == PhoneState::Associating && f.len() >= 30 => {
                let status = u16::from_le_bytes([f[26], f[27]]);
                if status != 0 { if self.log { eprintln!("[wifi] phone: association refused, status {status}"); } self.state = PhoneState::Scanning; return; }
                if self.pmk.is_some() { self.state = PhoneState::Handshake; self.arm(now_us + 2_000_000); } else { self.joined(now_us); }
            }
            (0, 10) | (0, 12) if ours => {
                if self.log { eprintln!("[wifi] phone: {} by the guest", if st == 12 { "deauthenticated" } else { "disassociated" }); }
                self.state = PhoneState::Scanning; self.ip = None; self.secured = false;
            }
            (2, _) if fc & 0x0300 == 0x0200 && a2 == self.bssid && (a1 == self.mac || a1[0] & 1 != 0) => {
                if st == 4 || st == 12 { return; }
                let hdr = if st & 8 != 0 { 26 } else { 24 };
                let body = if fc & 0x4000 != 0 && f.len() > hdr + 16 { &f[hdr + 8..f.len() - 8] } else { &f[hdr..] };
                if body.len() < 8 || body[0] != 0xaa { return; }
                if body[6..8] == [0x88, 0x8e] { self.on_eapol(&body[8..], now_us); return; }
                let mut eth = Vec::with_capacity(body.len() + 6);
                eth.extend_from_slice(&f[4..10]); eth.extend_from_slice(&f[16..22]); eth.extend_from_slice(&body[6..]);
                self.on_eth(&eth, now_us);
            }
            _ => {}
        }
    }

    fn heard(&mut self, f: &[u8], bssid: [u8; 6], now_us: u64) {
        let el = ies(f, 36);
        let Some(ssid) = el.iter().find(|(id, _)| *id == 0).map(|(_, d)| String::from_utf8_lossy(d).into_owned()) else { return };
        if ssid.is_empty() { return; }
        let channel = el.iter().find(|(id, d)| *id == 3 && d.len() == 1).map_or(self.channel, |(_, d)| d[0]);
        let secure = u16::from_le_bytes([f[34], f[35]]) & 0x0010 != 0;
        match self.hotspots.iter_mut().find(|h| h.bssid == bssid) {
            Some(h) if h.ssid == ssid && h.secure == secure => { h.channel = channel; h.last_us = now_us; }
            // The same radio renamed: IDF brings up its default `ESP_xxxxxx` AP before the firmware's
            // `esp_wifi_set_config` lands, so a join begun on the old name has to start over.
            Some(h) => {
                eprintln!("[emu] device hotspot: '{}' bssid {} channel {} ({}), was '{}'", ssid, super::mac_str(&bssid), channel, if secure { "WPA2-PSK" } else { "open" }, h.ssid);
                *h = Hotspot { ssid: ssid.clone(), bssid, channel, secure, last_us: now_us };
                if self.state != PhoneState::Scanning && self.bssid == bssid {
                    self.unlink();
                }
            }
            None => {
                if self.hotspots.len() >= HOTSPOTS_MAX { self.hotspots.remove(0); }
                eprintln!("[emu] device hotspot: '{}' bssid {} channel {} ({})", ssid, super::mac_str(&bssid), channel, if secure { "WPA2-PSK" } else { "open" });
                self.hotspots.push(Hotspot { ssid: ssid.clone(), bssid, channel, secure, last_us: now_us });
            }
        }
        if self.state != PhoneState::Scanning || self.cfg.ssid.as_ref().is_some_and(|s| *s != ssid) { return; }
        if secure && self.cfg.psk.is_none() {
            if !self.warned { eprintln!("[emu] phone: hotspot '{ssid}' is secured; give its password with --ap-client ssid={ssid},psk=PASS"); self.warned = true; }
            return;
        }
        self.pmk = match (&self.cfg.psk, secure) {
            (Some(psk), true) => Some(esp_periph::crypto::pbkdf2_sha1(psk.as_bytes(), ssid.as_bytes(), 4096, 32).try_into().unwrap()),
            _ => None,
        };
        self.bssid = bssid; self.channel = channel; self.secured = false; self.tries = 0;
        if self.log { eprintln!("[wifi] phone: joining '{ssid}'"); }
        self.auth(now_us);
    }

    /// Messages 1 and 3 of the four-way handshake; the group key is not needed to pass plaintext.
    fn on_eapol(&mut self, body: &[u8], now_us: u64) {
        let Some(pmk) = self.pmk else { return };
        if body.len() < 99 || body[1] != 3 { return; }
        let key_info = be16(&body[5..7]);
        let replay = u64::from_be_bytes(body[9..17].try_into().unwrap());
        if key_info & 0x0080 == 0 { return; }                            // not from the authenticator
        if key_info & 0x0100 == 0 {
            // message 1: derive the PTK from the ANonce and our SNonce, answer with message 2
            let anonce: [u8; 32] = body[17..49].try_into().unwrap();
            let seed = esp_periph::crypto::sha1(&[&pmk[..], &self.mac[..], &anonce[..]].concat());
            let mut snonce = [0u8; 32]; for (i, b) in snonce.iter_mut().enumerate() { *b = seed[i % 20] ^ (i as u8).wrapping_mul(7); }
            let (aa, spa) = (self.bssid, self.mac);
            let (lo, hi) = if aa <= spa { (aa, spa) } else { (spa, aa) };
            let (ln, hn) = if anonce <= snonce { (anonce, snonce) } else { (snonce, anonce) };
            let data = [&lo[..], &hi[..], &ln[..], &hn[..]].concat();
            self.ptk = esp_periph::crypto::prf(&pmk, "Pairwise key expansion", &data, 384);
            let kck = self.ptk[..16].to_vec();
            let m2 = eapol_key(0x010a, replay, snonce, RSN_IE, Some(&kck));
            self.eapol_send(&m2, now_us);
        } else if key_info & 0x0040 != 0 && !self.ptk.is_empty() {
            // message 3 (install): confirm with message 4, then the keys are in
            let kck = self.ptk[..16].to_vec();
            let m4 = eapol_key(0x030a, replay, [0; 32], &[], Some(&kck));
            self.eapol_send(&m4, now_us);
            self.secured = true;
            self.joined(now_us + 10_000);
        }
    }
    fn eapol_send(&mut self, body: &[u8], now_us: u64) {
        let b = self.bssid;
        let mut f = self.hdr((2 << 2) | 0x0100, &b, &b);
        f.extend_from_slice(&[0xaa, 0xaa, 0x03, 0, 0, 0, 0x88, 0x8e]); f.extend_from_slice(body);
        self.send(now_us + 2_000, f);
    }

    fn on_eth(&mut self, eth: &[u8], now_us: u64) {
        if eth.len() < 34 { return; }
        match be16(&eth[12..14]) {
            0x0806 if eth.len() >= 42 && be16(&eth[20..22]) == 1 && Some(<[u8; 4]>::try_from(&eth[38..42]).unwrap()) == self.ip => {
                let mut a = Vec::with_capacity(28);
                a.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]); a.extend_from_slice(&self.mac); a.extend_from_slice(&eth[38..42]);
                a.extend_from_slice(&eth[22..28]); a.extend_from_slice(&eth[28..32]);
                let reply = ethernet(&mac(&eth[6..12]), &self.mac, 0x0806, &a);
                self.data(&reply, now_us);
            }
            0x0800 => {
                let ip = &eth[14..];
                let ihl = usize::from(ip[0] & 0xf) * 4;
                let total = usize::from(be16(&ip[2..4])).min(ip.len());
                if ihl < 20 || total < ihl + 8 { return; }
                let (sip, dip): ([u8; 4], [u8; 4]) = (ip[12..16].try_into().unwrap(), ip[16..20].try_into().unwrap());
                let l4 = &ip[ihl..total];
                match ip[9] {
                    17 if be16(&l4[2..4]) == 68 => self.on_dhcp(&l4[8..], mac(&eth[6..12]), now_us),
                    6 if Some(dip) == self.ip && l4.len() >= 20 && l4[13] & 0x12 != 0x02 => {   // replies only: nobody listens here
                        for e in self.nat.tcp_in(&mac(&eth[6..12]), &sip, &dip, l4, now_us) { self.data(&e, now_us); }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn on_dhcp(&mut self, p: &[u8], from: [u8; 6], now_us: u64) {
        if p.len() < 240 || p[0] != 2 || p[4..8] != self.xid.to_be_bytes() || p[28..34] != self.mac { return; }
        let yiaddr: [u8; 4] = p[16..20].try_into().unwrap();
        let (mut kind, mut server, mut router) = (0, None, None);
        let mut i = 240;
        while i + 1 < p.len() && p[i] != 255 {
            if p[i] == 0 { i += 1; continue; }
            let (code, len) = (p[i], usize::from(p[i + 1]));
            let Some(v) = p.get(i + 2..i + 2 + len) else { break };
            match (code, len) { (53, 1) => kind = v[0], (54, 4) => server = Some(<[u8; 4]>::try_from(v).unwrap()), (3, 4..) => router = Some(<[u8; 4]>::try_from(&v[..4]).unwrap()), _ => {} }
            i += 2 + len;
        }
        let server = server.or(router).unwrap_or([yiaddr[0], yiaddr[1], yiaddr[2], 1]);
        match (kind, self.state) {
            (2, PhoneState::Dhcp) if self.offer.is_none() => { self.offer = Some((yiaddr, server)); self.dhcp(3, now_us); }
            (5, PhoneState::Dhcp) => {
                self.ip = Some(yiaddr); self.server = server; self.peer_mac = from; self.state = PhoneState::Up;
                self.nat.local = (self.mac, yiaddr); self.nat.set_station(from);
                eprintln!("[emu] phone: lease {}.{}.{}.{} from {}.{}.{}.{}; --ap-fwd connections go through", yiaddr[0], yiaddr[1], yiaddr[2], yiaddr[3], server[0], server[1], server[2], server[3]);
            }
            (6, PhoneState::Dhcp) => { self.offer = None; self.dhcp(1, now_us + 100_000); }
            _ => {}
        }
    }

    /// Timers (retries) and the forwards' host sockets; returns frames due at `now_us`.
    pub fn step(&mut self, now_us: u64) -> Vec<AirFrame> {
        if !matches!(self.state, PhoneState::Scanning | PhoneState::Up) && now_us >= self.retry_at {
            self.tries += 1;
            if self.tries > TRIES {
                if self.log { eprintln!("[wifi] phone: gave up in {:?}, scanning again", self.state); }
                self.state = PhoneState::Scanning; self.offer = None;
            } else {
                match self.state {
                    PhoneState::Authenticating => self.auth(now_us),
                    PhoneState::Associating | PhoneState::Handshake => self.auth(now_us),
                    PhoneState::Dhcp => { let k = if self.offer.is_some() { 3 } else { 1 }; self.dhcp(k, now_us); }
                    _ => {}
                }
            }
        }
        if now_us.wrapping_sub(self.net_polled_us) >= self.net_poll_us() {
            self.net_polled_us = now_us;
            if self.state == PhoneState::Up {
                for e in self.nat.poll(now_us) { self.data(&e, now_us); }
            } else {
                self.nat.unlinked(now_us);
            }
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue).into_iter().partition(|a| a.at_us <= now_us);
        self.queue = later;
        due
    }
}
