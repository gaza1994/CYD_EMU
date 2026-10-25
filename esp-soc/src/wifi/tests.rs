use super::*;

fn config() -> ApConfig {
    ApConfig { ssid: "esp32sim".into(), bssid: [2, 0x53, 0x49, 0x4d, 0, 1], channel: 6, psk: None, any: false }
}

fn station_frame(fc: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 24];
    frame[..2].copy_from_slice(&fc.to_le_bytes());
    frame[4..10].copy_from_slice(&config().bssid);
    frame[10..16].copy_from_slice(&[2, 3, 4, 5, 6, 7]);
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn auth_rejects_every_truncated_fixed_body() {
    let valid = station_frame(11 << 4, &[0, 0, 1, 0, 0, 0]);
    for len in 0..valid.len() {
        let mut ap = VirtualAp::new(config(), true);
        assert!(ap.on_station_tx(&valid[..len], 0).is_none());
        assert_eq!(ap.state, StaState::Idle, "length {len}");
        assert!(ap.queue.is_empty(), "length {len}");
    }
    let mut ap = VirtualAp::new(config(), false);
    assert!(ap.on_station_tx(&valid, 0).is_none());
    assert_eq!(ap.state, StaState::Authenticated);
    assert_eq!(ap.queue.len(), 1);
}

fn eapol_message4() -> Vec<u8> {
    let mut body = vec![0; 99];
    body[0] = 2;
    body[1] = 3;
    body[2..4].copy_from_slice(&95u16.to_be_bytes());
    body[4] = 2;
    body[5..7].copy_from_slice(&0x0300u16.to_be_bytes());
    body
}

fn send_eapol(ap: &mut VirtualAp, body: &[u8]) {
    ap.state = StaState::Associated;
    let mut payload = vec![0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e];
    payload.extend_from_slice(body);
    assert!(ap.on_station_tx(&station_frame(0x0108, &payload), 0).is_none());
}

#[test]
fn eapol_rejects_declared_lengths_shorter_than_key_header() {
    for len in 0..95u16 {
        let mut body = eapol_message4();
        body[2..4].copy_from_slice(&len.to_be_bytes());
        // Exercise the message-2 slices as well as the message-4 state transition.
        for (state, key_info) in [(WpaState::AwaitingMessage2, 0x0100u16), (WpaState::AwaitingMessage4, 0x0300)] {
            body[5..7].copy_from_slice(&key_info.to_be_bytes());
            let mut ap = VirtualAp::new(config(), false);
            ap.wpa.state = state;
            send_eapol(&mut ap, &body);
            assert_eq!(ap.wpa.state, state, "declared length {len}");
            assert!(ap.queue.is_empty());
        }
    }
}

#[test]
fn eapol_rejects_truncated_declared_payload_and_key_data() {
    for (declared, key_data) in [(96u16, 0u16), (95, 1), (u16::MAX, 0)] {
        let mut body = eapol_message4();
        body[2..4].copy_from_slice(&declared.to_be_bytes());
        body[97..99].copy_from_slice(&key_data.to_be_bytes());
        let mut ap = VirtualAp::new(config(), false);
        ap.wpa.state = WpaState::AwaitingMessage4;
        send_eapol(&mut ap, &body);
        assert_eq!(ap.wpa.state, WpaState::AwaitingMessage4);
        assert!(ap.queue.is_empty());
    }
}

#[test]
fn eapol_allows_bytes_after_declared_payload() {
    let mut body = eapol_message4();
    body.extend_from_slice(&[0xff; 8]);
    let mut ap = VirtualAp::new(config(), false);
    ap.wpa.state = WpaState::AwaitingMessage4;
    send_eapol(&mut ap, &body);
    assert_eq!(ap.wpa.state, WpaState::Installed);
}

#[test]
fn wpa_handshake_installs_keys_only_after_message4() {
    let mut cfg = config();
    cfg.psk = Some("esp32sim-pass".into());
    let mut ap = VirtualAp::new(cfg, false);
    assert_eq!(ap.wpa.state, WpaState::Idle);
    ap.on_station_tx(&station_frame(11 << 4, &[0, 0, 1, 0, 0, 0]), 0);
    ap.on_station_tx(&station_frame(0, &[1, 0, 0, 0]), 0);
    assert_eq!(ap.wpa.state, WpaState::AwaitingMessage2);
    let m1 = &ap.queue.last().unwrap().frame[32..];
    assert_eq!(&m1[5..7], &0x008au16.to_be_bytes());
    assert_eq!(&m1[81..97], &[0; 16]);
    ap.queue.clear();

    let mut m2 = eapol_message4();
    m2[5..7].copy_from_slice(&0x010au16.to_be_bytes());
    m2[17..49].fill(0x5a);
    send_eapol(&mut ap, &m2);
    assert_eq!(ap.wpa.state, WpaState::AwaitingMessage4);
    let m3 = &ap.queue.last().unwrap().frame[32..];
    assert_eq!(&m3[5..7], &0x13cau16.to_be_bytes());
    assert_ne!(&m3[81..97], &[0; 16]);
    let ethernet = [0u8; 14];
    assert_eq!(ap.data_from_ds(&ethernet).unwrap()[1] & 0x40, 0);

    send_eapol(&mut ap, &eapol_message4());
    assert_eq!(ap.wpa.state, WpaState::Installed);
    let protected = ap.data_from_ds(&ethernet).unwrap();
    assert_eq!(protected[1] & 0x40, 0x40);
    assert_eq!(protected.len(), 24 + 8 + 8 + 8); // MAC, CCMP, LLC/SNAP and MIC
}

#[test]
fn data_to_eth_rejects_every_truncated_header() {
    let mut payload = vec![0xaa, 0xaa, 3, 0, 0, 0, 0x08, 0];
    payload.extend_from_slice(&[1, 2, 3]);
    for fc in [0x0108, 0x0188] {
        let mut frame = station_frame(fc, &[]);
        if fc == 0x0188 { frame.extend_from_slice(&[0; 2]); }
        let header_len = frame.len() + 8;
        frame.extend_from_slice(&payload);
        for len in 0..header_len {
            assert!(data_to_eth(&frame[..len]).is_none(), "length {len}");
        }
        assert_eq!(data_to_eth(&frame).unwrap()[12..], [0x08, 0, 1, 2, 3]);
    }
}

#[test]
fn ap_configuration_keeps_defaults_and_recognizes_aliases() {
    let cfg = ApConfig::parse("").unwrap();
    assert_eq!(cfg.ssid, "esp32sim");
    assert_eq!(cfg.bssid, [2, 0x53, 0x49, 0x4d, 0, 1]);
    assert_eq!(cfg.channel, 6);
    assert!(cfg.psk.is_none());
    for channel in ["chan", "channel", "ch"] {
        for psk in ["psk", "password", "pass"] {
            let spec = format!("ssid=test,{channel}=11,{psk}=s=ecret!,bssid=02:ab:CD:00:12:ff");
            let cfg = ApConfig::parse(&spec).unwrap();
            assert_eq!(cfg.ssid, "test");
            assert_eq!(cfg.channel, 11);
            assert_eq!(cfg.psk.as_deref(), Some("s=ecret!"));
            assert_eq!(cfg.bssid, [2, 0xab, 0xcd, 0, 0x12, 0xff]);
        }
    }
}

#[test]
fn ap_configuration_rejects_unknown_options_and_invalid_values() {
    for spec in [
        "passwd=secret", "pass", "ssid=x,", "=x", "channel=0", "channel=15",
        "channel=256", "channel=-1", "channel=no", "bssid=02:53:49:4d:00",
        "bssid=02:53:49:4d:00:01:02", "bssid=02:xx:53:49:4d:00:01",
        "bssid=02:53:49:4d:00:GG", "bssid=2:53:49:4d:00:01", "bssid=+2:53:49:4d:00:01",
    ] {
        assert!(ApConfig::parse(spec).is_err(), "accepted {spec}");
    }
}

/// What `next_rx` may change: the access point's timers, queue and counts, and the link's queues
/// and pacing. `nothing_due` must only say so when none of them would change.
fn link_state(l: &StationLink) -> (u64, usize, (u64, u64, u64), u16, usize, u64) {
    let ap = l.ap().unwrap();
    (ap.next_beacon_us, ap.queue.len(), ap.stats, ap.seq, l.eth_rx.len(), l.last_rx_us)
}

#[test]
fn nothing_due_means_next_rx_delivers_and_changes_nothing() {
    for (case, relay) in [("network", false), ("relay", true)] {
        for t in (0..400_000).step_by(997) {
            // fresh, a frame queued for later, a frame from the network, and a frame just delivered
            for setup in 0..4 {
                let mut l = StationLink::default();
                l.attach(Some(VirtualAp::new(config(), false)), None);
                l.set_relay(relay);
                match setup {
                    1 => l.ap_mut().unwrap().queue.push(AirFrame { at_us: t + 1_000, frame: vec![0x40, 0, 0, 0] }),
                    2 => l.eth_rx.push(vec![0; 60]),
                    3 => { l.last_rx_us = t.saturating_sub(100); l.ap_mut().unwrap().queue.push(AirFrame { at_us: 0, frame: vec![0x40, 0, 0, 0] }); }
                    _ => {}
                }
                if !l.nothing_due(t) { continue; }
                let before = link_state(&l);
                assert_eq!(l.next_rx(t, false), None, "{case}, setup {setup}, t {t}: nothing_due but a frame came");
                assert_eq!(link_state(&l), before, "{case}, setup {setup}, t {t}: nothing_due but next_rx changed state");
            }
        }
    }
    // and it does not hide what is due: the first beacon, a queued frame, a frame from the network
    let mut l = StationLink::default();
    l.attach(Some(VirtualAp::new(config(), false)), None);
    let first_beacon = l.ap().unwrap().next_beacon_us;
    assert!(l.nothing_due(first_beacon - 1) && !l.nothing_due(first_beacon));
    l.ap_mut().unwrap().queue.push(AirFrame { at_us: 0, frame: vec![0x40, 0, 0, 0] });
    assert!(!l.nothing_due(1_000));
    let mut l = StationLink::default();
    l.attach(Some(VirtualAp::new(config(), false)), None);
    l.eth_rx.push(vec![0; 60]);
    assert!(!l.nothing_due(1_000));
}

/// A link with an access point and frames queued at it: `frames` are (at_us, frame type byte).
fn link_with_queue(frames: &[(u64, u8)]) -> StationLink {
    let mut l = StationLink::default();
    let mut ap = VirtualAp::new(config(), false);
    ap.next_beacon_us = u64::MAX;   // only the queued frames
    for &(at_us, fc) in frames { ap.queue.push(AirFrame { at_us, frame: vec![fc, 0, 0, 0] }); }
    l.attach(Some(ap), None);
    l
}

#[test]
fn next_rx_delivers_every_due_frame_one_gap_apart_management_first() {
    // two beacons (0x80) and a probe response (0x50), all due at once
    let mut l = link_with_queue(&[(10, 0x80), (20, 0x80), (30, 0x50)]);
    let mut got = Vec::new();
    let mut t = 1_000;
    while got.len() < 3 && t < 10_000 {
        if let Some(f) = l.next_rx(t, false) { got.push((t, f[0])); l.rx_delivered(0x1000); }
        t += 100;
    }
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [0x50, 0x80, 0x80], "management first, then beacons in time order, none lost");
    assert!(got.windows(2).all(|w| w[1].0 - w[0].0 >= RX_GAP_US), "one airtime gap apart: {got:?}");
    assert_eq!(l.rx_frames(), 3);
}

#[test]
fn a_busy_ring_holds_the_air_for_50_ms_only() {
    let mut l = link_with_queue(&[(0, 0x80), (0, 0x80)]);
    assert!(l.next_rx(1_000, false).is_some());
    l.rx_delivered(0x1000);
    assert_eq!(l.next_rx(1_000 + RX_GAP_US, true), None, "software has not recycled the last descriptor");
    assert_eq!(l.next_rx(1_000 + 50_000 - 1, true), None, "still within the stall time");
    assert!(l.next_rx(1_000 + 50_000, true).is_some(), "after 50 ms the frame goes anyway");
}

#[test]
fn a_reboot_keeps_the_host_side_only() {
    let mut l = link_with_queue(&[(0, 0x80)]);
    let ap = l.ap.take();
    l.attach(ap, Some(crate::net::VirtualNet::new(false)));
    l.set_relay(true);
    l.eth_tx.push(vec![1]); l.eth_rx.push(vec![2]);
    assert!(l.next_rx(1_000, false).is_some());
    l.rx_delivered(0x1000); l.drop_rx(); l.station_tx(&[0; 24], 2_000);
    let l = l.surviving_reboot();
    assert!(l.ap().is_some() && l.net().is_some() && l.relay(), "the access point, network and relay mode stay");
    assert_eq!((l.eth_tx.len(), l.eth_rx.len(), l.last_rx_us, l.last_rx_desc, l.net_polled_us), (0, 0, 0, 0, 0), "queues and pacing start again");
    assert_eq!((l.tx_frames(), l.rx_frames(), l.rx_dropped(), l.tx_dropped()), (0, 0, 0, 0), "counts start again");
}

// ------------------------------------------------------------------ networks around the station

fn nearby() -> Vec<BeaconAp> {
    vec![
        BeaconAp { ssid: "Home".into(), bssid: [0x02, 1, 2, 3, 4, 5], channel: 1, rssi: -60, secure: true },
        BeaconAp { ssid: "Cafe".into(), bssid: [0x02, 9, 8, 7, 6, 5], channel: 11, rssi: -75, secure: false },
    ]
}

/// A probe request from the station to the broadcast address and BSSID, with the given elements.
fn probe_request(elements: &[u8]) -> Vec<u8> {
    let mut f = station_frame(4 << 4, elements);
    f[4..10].copy_from_slice(&[0xff; 6]);
    f[16..22].copy_from_slice(&[0xff; 6]);
    f
}

/// (BSSID, capability, the DSSS element's channel, an RSN element present) of a probe response.
fn response_summary(r: &[u8]) -> ([u8; 6], u16, u8, bool) {
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&r[16..22]);
    let cap = u16::from_le_bytes([r[34], r[35]]);
    let elements = ies(r, 36);
    let chan = elements.iter().find(|(id, _)| *id == 3).map(|(_, d)| d[0]).expect("DSSS Parameter Set");
    (bssid, cap, chan, elements.iter().any(|(id, _)| *id == 48))
}

#[test]
fn nearby_networks_answer_a_wildcard_probe_after_the_access_point() {
    let mut ap = VirtualAp::new(config(), false);
    ap.beacons = nearby();
    assert!(ap.on_station_tx(&probe_request(&[0, 0]), 1000).is_none());
    assert_eq!(ap.queue.len(), 3);
    let (own, cap, chan, rsn) = response_summary(&ap.queue[0].frame);
    assert_eq!((own, cap & 0x11, chan, rsn), (config().bssid, 0x01, 6, false), "the open access point first: ESS, no Privacy");
    let (b, cap, chan, rsn) = response_summary(&ap.queue[1].frame);
    assert_eq!((b, cap & 0x11, chan, rsn), ([0x02, 1, 2, 3, 4, 5], 0x11, 1, true), "Home: Privacy and RSN, channel 1");
    let (b, cap, chan, rsn) = response_summary(&ap.queue[2].frame);
    assert_eq!((b, cap & 0x11, chan, rsn), ([0x02, 9, 8, 7, 6, 5], 0x01, 11, false), "Cafe: open, channel 11");
    assert!(ap.queue[0].at_us < ap.queue[1].at_us && ap.queue[1].at_us < ap.queue[2].at_us, "one after the other");
    for q in &ap.queue {
        assert_eq!(&q.frame[4..10], &[2, 3, 4, 5, 6, 7], "to the station");
        assert_eq!(&q.frame[10..16], &q.frame[16..22], "from its own BSSID");
    }
}

#[test]
fn a_directed_probe_gets_only_the_named_nearby_network() {
    let mut ap = VirtualAp::new(config(), false);
    ap.beacons = nearby();
    assert!(ap.on_station_tx(&probe_request(&[0, 4, b'C', b'a', b'f', b'e']), 1000).is_none());
    assert_eq!(ap.queue.len(), 1, "not the access point, not Home");
    assert_eq!(response_summary(&ap.queue[0].frame).0, [0x02, 9, 8, 7, 6, 5]);
    ap.queue.clear();
    assert!(ap.on_station_tx(&probe_request(&[0, 5, b'O', b't', b'h', b'e', b'r']), 2000).is_none());
    assert!(ap.queue.is_empty(), "nobody answers for an unknown SSID");
}

#[test]
fn frames_from_a_nearby_network_carry_its_channel_and_signal() {
    let mut ap = VirtualAp::new(config(), false);
    ap.beacons = nearby();
    let mut from_home = vec![0u8; 24];
    from_home[10..16].copy_from_slice(&[0x02, 1, 2, 3, 4, 5]);
    let mut from_ap = vec![0u8; 24];
    from_ap[10..16].copy_from_slice(&config().bssid);
    assert_eq!(ap.rx_meta(&from_home), (1, -60));
    assert_eq!(ap.rx_meta(&from_ap), (6, -40));
    assert_eq!(ap.rx_meta(&[0; 8]), (6, -40), "too short for an address: the access point's");
}

#[test]
fn authentication_to_a_nearby_network_goes_unanswered() {
    let mut ap = VirtualAp::new(config(), false);
    ap.beacons = nearby();
    let mut auth = station_frame(11 << 4, &[0, 0, 1, 0, 0, 0]);
    auth[4..10].copy_from_slice(&[0x02, 1, 2, 3, 4, 5]);
    auth[16..22].copy_from_slice(&[0x02, 1, 2, 3, 4, 5]);
    assert!(ap.on_station_tx(&auth, 0).is_none());
    assert_eq!(ap.state, StaState::Idle);
    assert!(ap.queue.is_empty(), "nothing answers: the network is out of reach");
}

#[test]
fn nearby_specs_parse_with_defaults_and_the_access_points_own_ssid_or_bssid_is_left_out() {
    let home = BeaconAp::parse("ssid=Home").unwrap();
    assert_eq!((home.channel, home.rssi, home.secure, home.bssid[0]), (1, -60, true, 0x02), "defaults; a locally administered unicast BSSID");
    assert_eq!(home.bssid, BeaconAp::parse("ssid=Home,chan=3").unwrap().bssid, "the same BSSID every time");
    assert_ne!(home.bssid, BeaconAp::parse("ssid=Cafe").unwrap().bssid);
    let cafe = BeaconAp::parse("ssid=Cafe,ch=11,rssi=-75,secure=0,bssid=02:09:08:07:06:05").unwrap();
    assert_eq!(cafe, BeaconAp { ssid: "Cafe".into(), bssid: [0x02, 9, 8, 7, 6, 5], channel: 11, rssi: -75, secure: false });
    for bad in ["chan=1", "ssid=", "ssid=Home,chan=15", "ssid=Home,rssi=low", "ssid=Home,secure=maybe", "ssid=Home,bssid=02:09", "ssid=Home,psk=x", "ssid=Home,chan"] {
        assert!(BeaconAp::parse(bad).is_err(), "{bad}");
    }
    assert!(BeaconAp::parse_all(&vec!["ssid=x".to_string(); 33]).is_err(), "more than 32");
    let mut link = StationLink::default();
    assert!(link.set_beacons(nearby()).is_none(), "no access point yet");
    link.attach(Some(VirtualAp::new(config(), false)), None);
    let mut list = nearby();
    list.push(BeaconAp { ssid: "esp32sim".into(), ..home.clone() });
    list.push(BeaconAp { ssid: "Office".into(), bssid: config().bssid, ..home });
    let left_out = link.set_beacons(list).unwrap();
    assert_eq!(link.ap().unwrap().beacons, nearby(), "the access point's own SSID or BSSID is that access point");
    assert_eq!(left_out.iter().map(|(b, why)| (b.ssid.as_str(), *why)).collect::<Vec<_>>(),
        [("esp32sim", "the access point's SSID"), ("Office", "the access point's BSSID")]);
}

/// Two networks around on one BSSID would both answer from it: `parse_all` refuses them, whether
/// the BSSID is derived from one SSID twice or given twice.
#[test]
fn nearby_networks_need_a_bssid_each() {
    let specs = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(BeaconAp::parse_all(&specs(&["ssid=Home", "ssid=Home,chan=6"])).unwrap_err().contains("share the BSSID"));
    assert!(BeaconAp::parse_all(&specs(&["ssid=Home,bssid=02:00:00:00:00:42", "ssid=Cafe,bssid=02:00:00:00:00:42"])).unwrap_err()
        .contains("'Home' and 'Cafe' share the BSSID 02:00:00:00:00:42"));
    assert_eq!(BeaconAp::parse_all(&specs(&["ssid=Home,bssid=02:00:00:00:00:42", "ssid=Home,bssid=02:00:00:00:00:43"])).unwrap().len(), 2,
        "one SSID on two BSSIDs is two networks");
}

/// A network around answers a probe request sent to the broadcast address or its own, with a
/// wildcard BSSID or its own (IEEE 802.11-2020 11.1.4.3.4), and no other.
#[test]
fn a_nearby_network_answers_only_probes_addressed_to_it() {
    let home = [0x02, 1, 2, 3, 4, 5];
    for (a1, a3, answers) in [([0xff; 6], [0xff; 6], true), (home, [0xff; 6], true), ([0xff; 6], home, true), (home, home, true),
        ([0x02, 9, 8, 7, 6, 5], [0xff; 6], false), ([0xff; 6], [0x02, 9, 8, 7, 6, 5], false)] {
        let mut ap = VirtualAp::new(config(), false);
        ap.beacons = vec![nearby().remove(0)];
        let mut probe = probe_request(&[0, 4, b'H', b'o', b'm', b'e']);
        probe[4..10].copy_from_slice(&a1);
        probe[16..22].copy_from_slice(&a3);
        assert!(ap.on_station_tx(&probe, 1000).is_none());
        assert_eq!(ap.queue.len(), usize::from(answers), "a1 {a1:02x?} a3 {a3:02x?}");
    }
}

const STA: [u8; 6] = [2, 3, 4, 5, 6, 7];

/// A frame from the station to `bssid` (as receiver and BSSID).
fn to(bssid: [u8; 6], fc: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = station_frame(fc, payload);
    f[4..10].copy_from_slice(&bssid);
    f[16..22].copy_from_slice(&bssid);
    f
}

fn ssid_element(s: &[u8]) -> Vec<u8> { [&[0, s.len() as u8][..], s].concat() }

/// The SSID element of a probe response or beacon.
fn ssid_of(r: &[u8]) -> Vec<u8> { ies(r, 36).into_iter().find(|(id, _)| *id == 0).map(|(_, d)| d.to_vec()).unwrap() }

/// A station joins `bssid` as IDF's does: open-system authentication, association and, with
/// `psk`, the four-way handshake with its own PMK and real MICs, checking the access point's
/// message 3 MIC, so both ends must agree on the PMK for `ssid`. Returns the authentication status.
fn join(ap: &mut VirtualAp, bssid: [u8; 6], ssid: &[u8], psk: Option<&str>) -> u16 {
    ap.queue.clear();
    ap.on_station_tx(&to(bssid, 11 << 4, &[0, 0, 1, 0, 0, 0]), 0);
    let auth = &ap.queue[0].frame;
    assert_eq!((&auth[10..16], &auth[16..22]), (&bssid[..], &bssid[..]), "the response comes from the BSSID asked");
    let status = u16::from_le_bytes([auth[28], auth[29]]);
    if status != 0 { return status; }
    ap.on_station_tx(&to(bssid, 0, &[0x11, 0, 0, 0]), 0);
    assert_eq!(ap.state, StaState::Associated);
    assert_eq!(&ap.queue[1].frame[10..16], &bssid[..]);
    let Some(psk) = psk else {
        assert_eq!(ap.queue.len(), 2, "open: no EAPOL message 1");
        assert_eq!(u16::from_le_bytes([ap.queue[1].frame[24], ap.queue[1].frame[25]]) & 0x10, 0, "open: no Privacy");
        return 0;
    };
    let m1 = ap.queue.last().unwrap().frame[32..].to_vec();
    assert_eq!(&m1[5..7], &0x008au16.to_be_bytes());
    let anonce: [u8; 32] = m1[17..49].try_into().unwrap();
    let snonce = [0x5a; 32];
    let (lo_mac, hi_mac) = if bssid <= STA { (bssid, STA) } else { (STA, bssid) };
    let (lo_n, hi_n) = if anonce <= snonce { (anonce, snonce) } else { (snonce, anonce) };
    let ptk = esp_periph::crypto::prf(&pmk(psk, ssid), "Pairwise key expansion", &[&lo_mac[..], &hi_mac, &lo_n, &hi_n].concat(), 384);
    let eapol_to = |ap: &mut VirtualAp, body: Vec<u8>| { ap.queue.clear(); ap.on_station_tx(&to(bssid, 0x0108, &[&[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e][..], &body].concat()), 0); };
    let replay = u64::from_be_bytes(m1[9..17].try_into().unwrap());
    eapol_to(ap, eapol_key(0x010a, replay, snonce, RSN_IE, Some(&ptk[..16])));
    assert_eq!(ap.wpa.state, WpaState::AwaitingMessage4);
    let mut m3 = ap.queue.last().unwrap().frame[32..].to_vec();
    let recv = m3[81..97].to_vec();
    m3[81..97].fill(0);
    assert_eq!(&esp_periph::crypto::hmac_sha1(&ptk[..16], &m3)[..16], &recv[..], "message 3's MIC: the access point's PMK is the station's");
    eapol_to(ap, eapol_key(0x030a, u64::from_be_bytes(m3[9..17].try_into().unwrap()), [0; 32], &[], Some(&ptk[..16])));
    assert_eq!(ap.wpa.state, WpaState::Installed);
    0
}

#[test]
fn any_ssid_and_passphrase_join_with_the_handshake() {
    for (spec, ssid, psk) in [
        (r"ssid=My Home Wi-Fi\, 2.4 GHz,psk=correct horse battery staple", "My Home Wi-Fi, 2.4 GHz", "correct horse battery staple"),
        ("ssid=Café ☕ Wi-Fi 🛜,psk=pässwörd über", "Café ☕ Wi-Fi 🛜", "pässwörd über"),
        (r"ssid=back\\slash=x,psk=a\,b\\c d e f", r"back\slash=x", r"a,b\c d e f"),
        ("ssid=0123456789abcdef0123456789abcdef,psk=12345678", "0123456789abcdef0123456789abcdef", "12345678"),
        ("ssid=raw,psk=00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF", "raw", "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF"),
    ] {
        let cfg = ApConfig::parse(spec).unwrap();
        assert_eq!((cfg.ssid.as_str(), cfg.psk.as_deref(), cfg.any), (ssid, Some(psk), false), "{spec}");
        let mut ap = VirtualAp::new(cfg.clone(), false);
        ap.on_station_tx(&probe_request(&ssid_element(ssid.as_bytes())), 0);
        assert_eq!(ap.queue.len(), 1, "{spec}");
        assert_eq!(ssid_of(&ap.queue[0].frame), ssid.as_bytes());
        assert_eq!(response_summary(&ap.queue[0].frame).1 & 0x10, 0x10, "Privacy");
        assert!(response_summary(&ap.queue[0].frame).3, "RSN");
        assert_eq!(join(&mut ap, cfg.bssid, ssid.as_bytes(), Some(psk)), 0, "{spec}");
        assert_eq!(ap.wanted, Some(Wanted { ssid: ssid.into(), secure: Some(true) }));
    }
    assert_eq!(pmk("00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF", b"x")[31], 0xff, "64 hex digits are the PMK");
}

#[test]
fn wifi_specs_are_validated_and_an_empty_psk_is_open() {
    assert!(ApConfig::parse("ssid=cyd-lab,psk=").unwrap().psk.is_none());
    assert!(ApConfig::parse("ssid=cyd-lab").unwrap().psk.is_none());
    let any = ApConfig::parse("any").unwrap();
    assert!(any.any && any.ssid.is_empty() && any.psk.is_none());
    let any = ApConfig::parse("ssid=*,psk=esp32sim-pass,chan=11").unwrap();
    assert!(any.any && any.ssid.is_empty() && any.channel == 11 && any.psk.as_deref() == Some("esp32sim-pass"));
    assert_eq!(ApConfig::parse("ssid=any").unwrap().ssid, "any", "a network named any");
    for spec in ["ssid=", "ssid=0123456789abcdef0123456789abcdef!", "ssid=x,psk=1234567", "ssid=x,psk=0123456789012345678901234567890123456789012345678901234567890123x",
                 "ssid=x,psk=00112233445566778899aabbccddeeff00112233445566778899aabbccddeegg", "any,ssid=x", "ssid=x,any", "ssid=*,ssid=y"] {
        assert!(ApConfig::parse(spec).is_err(), "accepted {spec}");
    }
    assert_eq!(split_spec(r"a\,b,c\\,d\e"), ["a,b", r"c\", r"d\e"]);
}

#[test]
fn an_open_network_has_no_rsn_and_no_handshake() {
    let cfg = ApConfig::parse("ssid=cyd-lab").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"cyd-lab")), 0);
    let (_, cap, _, rsn) = response_summary(&ap.queue[0].frame);
    assert_eq!((cap & 0x10, rsn), (0, false));
    assert_eq!(join(&mut ap, cfg.bssid, b"cyd-lab", None), 0);
    assert_eq!(ap.wpa.state, WpaState::Idle);
    let data = to(cfg.bssid, 0x0108, &[0xaa, 0xaa, 3, 0, 0, 0, 8, 0, 0x45]);
    assert!(ap.on_station_tx(&data, 0).is_some(), "data passes in the clear");
    assert_eq!(ap.data_from_ds(&[0u8; 14]).unwrap()[1] & 0x40, 0, "and comes back unprotected");
}

#[test]
fn match_any_is_silent_until_a_directed_probe_then_serves_that_ssid_open_and_wpa2() {
    let cfg = ApConfig::parse("any").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    assert!(ap.step(1_000_000).is_empty(), "no beacons without an SSID");
    ap.on_station_tx(&probe_request(&[0, 0]), 1_000_000);
    assert!(ap.queue.is_empty() && ap.wanted.is_none(), "a wildcard probe names nothing");
    ap.on_station_tx(&probe_request(&ssid_element(b"cyd-lab")), 1_000_000);
    assert_eq!(ap.wanted, Some(Wanted { ssid: "cyd-lab".into(), secure: None }));
    assert_eq!(ap.cfg.ssid, "cyd-lab");
    assert_eq!(ap.queue.len(), 2);
    let sec = secure_bssid(&cfg.bssid);
    assert_eq!(response_summary(&ap.queue[0].frame), (cfg.bssid, 0x0401, 6, false), "open first");
    assert_eq!(response_summary(&ap.queue[1].frame), (sec, 0x0411, 6, true), "then WPA2");
    assert!(ap.queue.iter().all(|q| ssid_of(&q.frame) == b"cyd-lab"));
    assert_eq!((ap.rx_meta(&ap.queue[0].frame).1, ap.rx_meta(&ap.queue[1].frame).1), (-40, -46), "the open one is stronger");
    ap.queue.clear();
    let beacons = ap.step(2_000_000);
    assert_eq!(beacons.len(), 2);
    assert!(beacons.iter().all(|b| is_beacon(&b.frame) && ssid_of(&b.frame) == b"cyd-lab"));
}

#[test]
fn match_any_completes_an_open_join() {
    let cfg = ApConfig::parse("any").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    ap.on_station_tx(&probe_request(&ssid_element("test-net's IoT ✓".as_bytes())), 0);
    assert_eq!(join(&mut ap, cfg.bssid, "test-net's IoT ✓".as_bytes(), None), 0);
    assert_eq!(ap.wanted, Some(Wanted { ssid: "test-net's IoT ✓".into(), secure: Some(false) }));
    assert!(ap.serves(false));
    // joined: another SSID is recorded but not taken
    ap.on_station_tx(&probe_request(&ssid_element(b"other")), 0);
    assert_eq!(ap.cfg.ssid, "test-net's IoT ✓");
    ap.on_station_tx(&to(cfg.bssid, 12 << 4, &[3, 0]), 0);
    ap.on_station_tx(&probe_request(&ssid_element(b"next")), 0);
    assert_eq!((ap.cfg.ssid.as_str(), ap.wanted.as_ref().unwrap().ssid.as_str()), ("next", "next"), "after a deauthentication it follows");
}

#[test]
fn match_any_with_a_psk_completes_wpa2_on_the_ssid_asked() {
    let cfg = ApConfig::parse("ssid=*,psk=esp32sim-pass").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"esp32sim")), 0);
    assert_eq!(join(&mut ap, secure_bssid(&cfg.bssid), b"esp32sim", Some("esp32sim-pass")), 0);
    assert_eq!(ap.wanted, Some(Wanted { ssid: "esp32sim".into(), secure: Some(true) }));
    assert_eq!(ap.wanted.as_ref().unwrap().json(ap.serves(true)), r#"{"t":"wifi_wanted","ssid":"esp32sim","secure":true,"served":true}"#);
}

#[test]
fn match_any_without_a_psk_refuses_wpa2_and_reports_it() {
    let cfg = ApConfig::parse("any").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"my \"home\"")), 0);
    assert_eq!(ap.wanted.as_ref().unwrap().json(true), r#"{"t":"wifi_wanted","ssid":"my \"home\"","secure":null,"served":true}"#);
    assert_eq!(join(&mut ap, secure_bssid(&cfg.bssid), b"my \"home\"", Some("whatever!")), 1, "refused: unspecified failure");
    assert_eq!(ap.state, StaState::Idle);
    assert_eq!(ap.wanted, Some(Wanted { ssid: "my \"home\"".into(), secure: Some(true) }));
    assert!(!ap.serves(true));
    assert!(ap.on_station_tx(&to(secure_bssid(&cfg.bssid), 0, &[0x11, 0, 0, 0]), 0).is_none());
    assert_eq!(ap.state, StaState::Idle, "no association without authentication");
}

#[test]
fn a_fixed_access_point_records_but_does_not_answer_other_ssids() {
    let mut ap = VirtualAp::new(config(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"cyd-lab")), 0);
    assert!(ap.queue.is_empty());
    assert_eq!(ap.wanted, Some(Wanted { ssid: "cyd-lab".into(), secure: None }));
    assert_eq!(ap.cfg.ssid, "esp32sim");
}

/// A SoftAP beacon from the guest, as `heard_ap` reads it.
fn guest_beacon(bssid: [u8; 6], ssid: &str, secure: bool, channel: u8) -> Vec<u8> {
    let mut f = vec![0x80, 0, 0, 0];
    f.extend_from_slice(&[0xff; 6]); f.extend_from_slice(&bssid); f.extend_from_slice(&bssid); f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0; 8]); f.extend_from_slice(&100u16.to_le_bytes());
    f.extend_from_slice(&(0x0001u16 | if secure { 0x10 } else { 0 }).to_le_bytes());
    f.push(0); f.push(ssid.len() as u8); f.extend_from_slice(ssid.as_bytes());
    f.extend_from_slice(&[3, 1, channel]);
    f
}

#[test]
fn guest_hotspot_is_announced_once_without_a_phone() {
    let b = [0x24, 0x6f, 0x28, 0, 0x11, 0x23];
    let mut l = StationLink::default();
    l.attach(Some(VirtualAp::new(config(), false)), None);
    // IDF's default AP first, then the firmware's name on the same radio (CYD Weather Dashboard).
    assert_eq!(l.announce_hotspot(&guest_beacon(b, "ESP_001123", false, 1)).as_deref(), Some("[emu] device hotspot: 'ESP_001123' bssid 24:6f:28:00:11:23 channel 1 (open)"));
    assert_eq!(l.announce_hotspot(&guest_beacon(b, "CYD-2211", true, 1)).as_deref(), Some("[emu] device hotspot: 'CYD-2211' bssid 24:6f:28:00:11:23 channel 1 (WPA2-PSK), was 'ESP_001123'"));
    for _ in 0..5 { assert_eq!(l.announce_hotspot(&guest_beacon(b, "CYD-2211", true, 1)), None); }
    // Not beacons: the station's own frames, a hidden SSID.
    assert_eq!(l.announce_hotspot(&station_frame(11 << 4, &[0; 40])), None);
    assert_eq!(l.announce_hotspot(&guest_beacon([2, 0, 0, 0, 0, 9], "", false, 1)), None);
    // A guest reboot forgets it, so the next run's hotspot is announced again.
    let mut l = l.surviving_reboot();
    assert!(l.announce_hotspot(&guest_beacon(b, "CYD-2211", true, 1)).is_some());
    // station_tx reaches it; with a phone on the air the phone reports instead.
    let mut l = StationLink::default();
    l.station_tx(&guest_beacon(b, "HASP-001122", false, 1), 0);
    assert_eq!(l.heard.len(), 1);
}

/// The reason code of a deauthentication from the access point to `STA`, or None for another frame.
fn deauth_reason(f: &[u8], from: [u8; 6]) -> Option<u16> {
    (f.len() >= 26 && f[0] == 0xc0 && f[4..10] == STA && f[10..16] == from && f[16..22] == from).then(|| u16::from_le_bytes([f[24], f[25]]))
}

#[test]
fn a_live_change_deauthenticates_the_joined_station_and_serves_the_new_network() {
    let cfg = ApConfig::parse("ssid=cyd-lab").unwrap();
    let mut ap = VirtualAp::new(cfg.clone(), false);
    assert_eq!(join(&mut ap, cfg.bssid, b"cyd-lab", None), 0);
    ap.queue.clear();
    // open to WPA2 under a new name, with a comma and unicode
    let next = ap.cfg.retarget(Some("other, net ☕"), Some("pässword 123"), false).unwrap();
    assert_eq!((next.bssid, next.channel), (cfg.bssid, cfg.channel), "the BSSID and channel stay");
    assert!(ap.reconfigure(next));
    assert_eq!(ap.state, StaState::Idle);
    assert_eq!(ap.queue.len(), 1);
    assert_eq!(deauth_reason(&ap.queue[0].frame, cfg.bssid), Some(3), "deauthenticated: the AP is leaving");
    assert_eq!(ap.step(0).len(), 1, "due at once");
    assert!(ap.on_station_tx(&to(cfg.bssid, 0x0108, &[0xaa, 0xaa, 3, 0, 0, 0, 8, 0, 0x45]), 0).is_none(), "no data after the change");
    let beacons = ap.step(10_000_000);
    assert_eq!(beacons.len(), 1);
    assert_eq!(ssid_of(&beacons[0].frame), "other, net ☕".as_bytes());
    ap.on_station_tx(&probe_request(&ssid_element("other, net ☕".as_bytes())), 0);
    assert!(response_summary(&ap.queue[0].frame).3, "RSN now");
    assert_eq!(join(&mut ap, cfg.bssid, "other, net ☕".as_bytes(), Some("pässword 123")), 0, "the handshake uses the new PMK");
    // and back to open: the joined WPA2 station is told too
    ap.queue.clear();
    assert!(ap.reconfigure(ap.cfg.retarget(Some("cyd-lab"), None, false).unwrap()));
    assert_eq!(deauth_reason(&ap.queue[0].frame, cfg.bssid), Some(3));
    ap.queue.clear();
    ap.on_station_tx(&probe_request(&ssid_element(b"cyd-lab")), 0);
    assert!(!response_summary(&ap.queue[0].frame).3, "no RSN");
    assert_eq!(join(&mut ap, cfg.bssid, b"cyd-lab", None), 0);
}

#[test]
fn a_live_change_to_the_wanted_name_clears_wanted_and_the_next_probe_is_answered() {
    let mut ap = VirtualAp::new(ApConfig::parse("ssid=cyd-lab").unwrap(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"other-net")), 0);
    assert!(ap.queue.is_empty(), "a mismatch is not answered");
    assert_eq!(ap.wanted.as_ref().unwrap().json(ap.cfg.ssid == "other-net"), r#"{"t":"wifi_wanted","ssid":"other-net","secure":null,"served":false}"#);
    assert!(!ap.reconfigure(ap.cfg.retarget(Some("other-net"), Some(""), false).unwrap()), "nobody joined: nothing to disconnect");
    assert!(ap.wanted.is_none() && ap.queue.is_empty());
    ap.on_station_tx(&probe_request(&ssid_element(b"other-net")), 0);
    assert_eq!(ap.queue.len(), 1, "answered now");
    assert_eq!(ap.wanted, Some(Wanted { ssid: "other-net".into(), secure: None }));
    let b = ap.cfg.bssid;
    assert_eq!(join(&mut ap, b, b"other-net", None), 0);
    // the same settings again change nothing and keep the station
    ap.queue.clear();
    assert!(!ap.reconfigure(ap.cfg.retarget(Some("other-net"), None, false).unwrap()));
    assert_eq!((ap.state, ap.queue.len()), (StaState::Associated, 0));
}

#[test]
fn a_live_change_to_match_any_serves_the_name_asked_for_at_once() {
    let mut ap = VirtualAp::new(ApConfig::parse("ssid=cyd-lab,psk=cyd-lab-pass").unwrap(), false);
    ap.on_station_tx(&probe_request(&ssid_element(b"other-net")), 0);
    assert!(!ap.reconfigure(ap.cfg.retarget(Some("ignored"), None, true).unwrap()));
    assert!(ap.cfg.any);
    assert_eq!(ap.cfg.ssid, "other-net", "the wanted name, not the ignored one");
    let beacons = ap.step(10_000_000);
    assert_eq!(beacons.len(), 2, "both profiles (WPA2 is refused without a psk)");
    assert!(beacons.iter().all(|b| ssid_of(&b.frame) == b"other-net"));
    let b = ap.cfg.bssid;
    assert_eq!(join(&mut ap, b, b"other-net", None), 0);
    // match-any with a psk while joined: the station goes, the name stays, WPA2 is served too
    ap.queue.clear();
    assert!(ap.reconfigure(ap.cfg.retarget(None, Some("esp32sim-pass"), true).unwrap()));
    assert_eq!(ap.cfg.ssid, "other-net");
    assert!(deauth_reason(&ap.step(20_000_000)[0].frame, ap.cfg.bssid).is_some(), "the deauthentication first");
    assert_eq!(ap.step(30_000_000).iter().filter(|b| is_beacon(&b.frame)).count(), 2);
    let b = secure_bssid(&ap.cfg.bssid);
    assert_eq!(join(&mut ap, b, b"other-net", Some("esp32sim-pass")), 0);
}

#[test]
fn live_changes_are_validated_like_the_flag() {
    let cfg = config();
    for (ssid, psk, any) in [(None, None, false), (Some(""), None, false), (Some("*"), None, false), (Some("x"), Some("short"), false),
                             (Some("0123456789abcdef0123456789abcdef!"), None, false), (None, Some("1234567"), true)] {
        assert!(cfg.retarget(ssid, psk, any).is_err(), "accepted {ssid:?} {psk:?} {any}");
    }
    let c = cfg.retarget(Some(r"a\b,c=d"), Some(r"p,a\ss=word"), false).unwrap();
    assert_eq!((c.ssid.as_str(), c.psk.as_deref()), (r"a\b,c=d", Some(r"p,a\ss=word")), "escaped through the spec");
    assert_eq!(c.describe(), r"'a\b,c=d' (WPA2-PSK)");
    assert_eq!(cfg.retarget(None, None, true).unwrap().describe(), "any network (open)");
    // a nearby network that would clash with the new name is left out
    let mut ap = VirtualAp::new(cfg, false);
    ap.beacons = vec![BeaconAp { ssid: "neighbour".into(), bssid: [2, 0, 0, 0, 0, 9], channel: 1, rssi: -70, secure: true }];
    ap.reconfigure(ap.cfg.retarget(Some("neighbour"), None, false).unwrap());
    assert!(ap.beacons.is_empty());
}
