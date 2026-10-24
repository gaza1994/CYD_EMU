use super::*;
use crate::wifi::{eapol_key, RSN_IE};

const AP: [u8; 6] = [0x24, 0x6f, 0x28, 0, 0x11, 0x23];
const GUEST_IP: [u8; 4] = [192, 168, 4, 1];

fn beacon(ssid: &str, secure: bool) -> Vec<u8> {
    let mut f = vec![0x80, 0, 0, 0];
    f.extend_from_slice(&[0xff; 6]); f.extend_from_slice(&AP); f.extend_from_slice(&AP); f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0; 8]); f.extend_from_slice(&100u16.to_le_bytes());
    f.extend_from_slice(&(0x0001u16 | if secure { 0x10 } else { 0 }).to_le_bytes());
    f.push(0); f.push(ssid.len() as u8); f.extend_from_slice(ssid.as_bytes());
    f.extend_from_slice(&[3, 1, 1]);
    if secure { f.extend_from_slice(RSN_IE); }
    f
}
fn mgmt(st: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![st << 4, 0, 0, 0];
    f.extend_from_slice(&PHONE_MAC); f.extend_from_slice(&AP); f.extend_from_slice(&AP); f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(body);
    f
}
/// Guest to phone, from the DS: a1 = phone (or broadcast), a2 = BSSID, a3 = source.
fn data(dst: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x08, 0x02, 0, 0];
    f.extend_from_slice(&dst); f.extend_from_slice(&AP); f.extend_from_slice(&AP); f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0]); f.extend_from_slice(&ethertype.to_be_bytes()); f.extend_from_slice(payload);
    f
}
fn dhcp_reply(kind: u8, xid: [u8; 4]) -> Vec<u8> {
    let mut p = vec![0u8; 240];
    p[0] = 2; p[1] = 1; p[2] = 6; p[4..8].copy_from_slice(&xid);
    p[16..20].copy_from_slice(&[192, 168, 4, 2]); p[28..34].copy_from_slice(&PHONE_MAC);
    p[236..240].copy_from_slice(&[99, 130, 83, 99]);
    p.extend_from_slice(&[53, 1, kind, 54, 4]); p.extend_from_slice(&GUEST_IP); p.push(255);
    let udp = udp_packet(&GUEST_IP, &[255; 4], 67, 68, &p);
    ip_packet(17, &GUEST_IP, &[255; 4], &udp)
}
fn run(p: &mut Phone, now: &mut u64) -> Vec<Vec<u8>> {
    *now += 50_000;
    p.step(*now).into_iter().map(|a| a.frame).collect()
}
/// The DHCP message type of a phone frame, if it carries one.
fn dhcp_kind(f: &[u8]) -> Option<u8> {
    let at = f.windows(4).position(|w| w == [99, 130, 83, 99])?;
    (f[at + 4] == 53).then(|| f[at + 6])
}
fn lease(p: &mut Phone, now: &mut u64) {
    let discover = run(p, now);
    assert_eq!(p.state, PhoneState::Dhcp);
    assert_eq!(discover.iter().filter_map(|f| dhcp_kind(f)).collect::<Vec<_>>(), [1]);
    let xid = p.xid.to_be_bytes();
    p.on_guest_tx(&data([0xff; 6], 0x0800, &dhcp_reply(2, xid)), *now);
    assert_eq!(run(p, now).iter().filter_map(|f| dhcp_kind(f)).collect::<Vec<_>>(), [3]);
    p.on_guest_tx(&data([0xff; 6], 0x0800, &dhcp_reply(5, xid)), *now);
    assert_eq!((p.state, p.ip, p.server), (PhoneState::Up, Some([192, 168, 4, 2]), GUEST_IP));
    assert_eq!(p.nat.local, (PHONE_MAC, [192, 168, 4, 2]));
}

#[test]
fn config_parses_ssid_and_psk() {
    assert_eq!(PhoneConfig::parse("").unwrap(), PhoneConfig::default());
    assert_eq!(PhoneConfig::parse("ssid=NerdMinerAP,psk=MineYourCoins").unwrap(),
               PhoneConfig { ssid: Some("NerdMinerAP".into()), psk: Some("MineYourCoins".into()) });
    assert!(PhoneConfig::parse("psk=short").is_err());
    assert!(PhoneConfig::parse("bssid=1").is_err());
}

#[test]
fn joins_an_open_hotspot_and_takes_a_lease() {
    let mut p = Phone::new(PhoneConfig::default(), false);
    let mut now = 1_000;
    p.on_guest_tx(&beacon("HASP-001122", false), now);
    assert_eq!(p.hotspots[0].ssid, "HASP-001122");
    assert!(!p.hotspots[0].secure);
    let auth = run(&mut p, &mut now);
    assert_eq!((auth.len(), auth[0][0], &auth[0][4..10]), (1, 0xb0, &AP[..]));
    p.on_guest_tx(&mgmt(11, &[0, 0, 2, 0, 0, 0]), now);
    let assoc = run(&mut p, &mut now);
    assert_eq!(assoc[0][0], 0x00);
    assert!(assoc[0].windows(13).any(|w| w == b"\x00\x0bHASP-001122"));
    p.on_guest_tx(&mgmt(1, &[0x01, 0x04, 0, 0, 1, 0xc0]), now);
    lease(&mut p, &mut now);
    // the frames go up to the DS: ToDS, a1 = BSSID, a2 = phone, unprotected
    let arp = { let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1]; a.extend_from_slice(&AP); a.extend_from_slice(&GUEST_IP); a.extend_from_slice(&[0; 6]); a.extend_from_slice(&[192, 168, 4, 2]); a };
    p.on_guest_tx(&data([0xff; 6], 0x0806, &arp), now);
    let reply = run(&mut p, &mut now);
    assert_eq!((reply.len(), u16::from_le_bytes([reply[0][0], reply[0][1]])), (1, 0x0108));
    assert_eq!((&reply[0][4..10], &reply[0][10..16]), (&AP[..], &PHONE_MAC[..]));
}

#[test]
fn a_secured_hotspot_needs_the_password() {
    let mut p = Phone::new(PhoneConfig::default(), false);
    p.on_guest_tx(&beacon("NerdMinerAP", true), 0);
    assert_eq!(p.state, PhoneState::Scanning);
    assert!(p.hotspots[0].secure);
}

#[test]
fn runs_the_four_way_handshake_then_protects_its_frames() {
    let mut p = Phone::new(PhoneConfig::parse("ssid=NerdMinerAP,psk=MineYourCoins").unwrap(), false);
    let mut now = 0;
    p.on_guest_tx(&beacon("SomethingElse", false), now);
    assert_eq!(p.state, PhoneState::Scanning);
    p.on_guest_tx(&beacon("NerdMinerAP", true), now);
    run(&mut p, &mut now);
    p.on_guest_tx(&mgmt(11, &[0, 0, 2, 0, 0, 0]), now);
    let assoc = run(&mut p, &mut now);
    assert!(assoc[0].windows(RSN_IE.len()).any(|w| w == RSN_IE));
    p.on_guest_tx(&mgmt(1, &[0x11, 0x04, 0, 0, 1, 0xc0]), now);
    assert_eq!(p.state, PhoneState::Handshake);
    p.on_guest_tx(&data(PHONE_MAC, 0x888e, &eapol_key(0x008a, 1, [0x5a; 32], &[], None)), now);
    let m2 = run(&mut p, &mut now);
    let body = &m2[0][32..];
    assert_eq!((u16::from_be_bytes([body[5], body[6]]), body[16]), (0x010a, 1));
    // message 2's MIC is HMAC-SHA1 over the frame with the MIC zeroed, keyed by the KCK
    let mut zeroed = body.to_vec(); zeroed[81..97].fill(0);
    let kck = &p.ptk[..16];
    assert_eq!(&esp_periph::crypto::hmac_sha1(kck, &zeroed)[..16], &body[81..97]);
    p.on_guest_tx(&data(PHONE_MAC, 0x888e, &eapol_key(0x13ca, 2, [0x5a; 32], &[], None)), now);
    now += 20_000;
    lease(&mut p, &mut now);
    let protected: Vec<_> = { p.dhcp(1, now); run(&mut p, &mut now) };
    assert_ne!(u16::from_le_bytes([protected[0][0], protected[0][1]]) & 0x4000, 0);
}

#[test]
fn a_renamed_hotspot_restarts_the_join() {
    let mut p = Phone::new(PhoneConfig::default(), false);
    p.on_guest_tx(&beacon("ESP_001123", false), 0);
    assert_eq!(p.state, PhoneState::Authenticating);
    p.on_guest_tx(&beacon("HASP-001122", true), 10);
    assert_eq!((p.state, p.hotspots.len(), p.hotspots[0].ssid.as_str()), (PhoneState::Scanning, 1, "HASP-001122"));
}
