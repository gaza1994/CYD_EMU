use super::*;

fn name(s: &str) -> Name { s.split('.').map(|l| l.as_bytes().to_vec()).collect() }
fn rec(n: &str, rtype: u16, ttl: u32, data: Rdata) -> Record { Record { name: name(n), rtype, class: 1 | TOP_BIT, ttl, data } }
fn q(n: &str, qtype: u16) -> Question { Question { name: name(n), qtype, qclass: 1 } }

/// An ESPHome-style announcement, compressed the way ESP-IDF's mdns writes it.
fn announcement() -> Vec<u8> {
    let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 2];
    let rr = |m: &mut Vec<u8>, rtype: u16, ttl: u32, rdata: &[u8]| {
        m.extend_from_slice(&rtype.to_be_bytes()); m.extend_from_slice(&[0x80, 1]);
        m.extend_from_slice(&ttl.to_be_bytes()); m.extend_from_slice(&(rdata.len() as u16).to_be_bytes()); m.extend_from_slice(rdata);
    };
    let service = m.len() as u8;
    m.extend_from_slice(b"\x0b_esphomelib\x04_tcp\x05local\x00");
    let local = service + 17;
    let instance = m.len() as u8 + 10;
    m.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0x11, 0x94, 0, 10]);   // PTR, shared (no cache-flush)
    m.extend_from_slice(b"\x07cyd-emu"); m.extend_from_slice(&[0xc0, service]);
    m.extend_from_slice(&[0xc0, instance]);
    let host = m.len() as u8 + 16;
    rr(&mut m, T_SRV, 120, &[&[0, 0, 0, 0, 0x17, 0xa5][..], b"\x07cyd-emu", &[0xc0, local]].concat());
    m.extend_from_slice(&[0xc0, instance]);
    rr(&mut m, T_TXT, 4500, b"\x0bversion=2.0");
    m.extend_from_slice(&[0xc0, host]);
    rr(&mut m, T_A, 120, &[10, 0, 2, 15]);
    m.extend_from_slice(&[0xc0, host]);
    rr(&mut m, T_AAAA, 120, &[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    m.extend_from_slice(&[0xc0, host]);
    rr(&mut m, T_NSEC, 120, &[0xc0, host, 0, 4, 0x40, 0, 0, 0x08]);
    m
}

const RW: Rewrite = Rewrite { guest_ip: [10, 0, 2, 15], host_ip: [192, 168, 1, 20], ports: &[(6053, 16053), (3232, 3232)] };

#[test]
fn parses_compressed_names_and_round_trips() {
    let m = Message::parse(&announcement()).unwrap();
    assert!(m.is_response());
    assert_eq!(m.answers.len(), 4);
    assert_eq!(m.additional.len(), 2);
    assert_eq!(m.answers[0].data, Rdata::Name(name("cyd-emu._esphomelib._tcp.local")));
    assert_eq!(m.answers[1].data, Rdata::Srv { prio: 0, weight: 0, port: 6053, target: name("cyd-emu.local") });
    assert_eq!(m.answers[3].data, Rdata::A([10, 0, 2, 15]));
    assert_eq!(Message::parse(&m.encode()).unwrap(), m);
}

#[test]
fn rejects_malformed_messages() {
    let a = announcement();
    for cut in [5, 20, 60, a.len() - 1] { assert!(Message::parse(&a[..cut]).is_none(), "cut at {cut}"); }
    let mut looped = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    looped.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]);   // a pointer to itself
    assert!(Message::parse(&looped).is_none());
}

#[test]
fn rewrite_publishes_host_address_and_forwarded_ports() {
    let m = Message::parse(&announcement()).unwrap();
    let ans = RW.records(m.answers);
    assert_eq!(ans[1].data, Rdata::Srv { prio: 0, weight: 0, port: 16053, target: name("cyd-emu.local") });
    assert_eq!(ans[3].data, Rdata::A([192, 168, 1, 20]));
    let add = RW.records(m.additional);
    assert_eq!(add.len(), 1, "AAAA dropped");
    assert_eq!(add[0].data, Rdata::Nsec { next: name("cyd-emu.local"), bitmap: vec![0, 1, 0x40] }, "AAAA cleared from the NSEC bitmap");
}

#[test]
fn rewrite_drops_guest_subnet_and_link_local_addresses_and_keeps_unforwarded_ports() {
    let recs = vec![rec("a.local", T_A, 120, Rdata::A([10, 0, 2, 9])), rec("b.local", T_A, 120, Rdata::A([169, 254, 3, 4])),
        rec("c.local", T_A, 120, Rdata::A([8, 8, 8, 8])), rec("x._http._tcp.local", T_SRV, 120, Rdata::Srv { prio: 0, weight: 0, port: 80, target: name("c.local") })];
    let out = RW.records(recs);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].data, Rdata::A([8, 8, 8, 8]));
    assert!(matches!(out[1].data, Rdata::Srv { port: 80, .. }));
}

#[test]
fn nsec_bitmap_window_disappears_when_emptied() {
    let mut b = vec![0, 4, 0, 0, 0, 0x08];
    clear_type(&mut b, T_AAAA);
    assert!(b.is_empty());
}

fn cache() -> (Cache, Instant) {
    let now = Instant::now();
    let mut c = Cache::default();
    let m = Message::parse(&announcement()).unwrap();
    for r in RW.records(m.answers).into_iter().chain(RW.records(m.additional)) { c.insert(r, now); }
    (c, now)
}

#[test]
fn ptr_question_answers_with_srv_txt_and_a_as_additionals() {
    let (c, now) = cache();
    let (ans, add, stale) = c.answer(&[q("_esphomelib._tcp.local", T_PTR)], now);
    assert!(!stale);
    assert_eq!(ans.len(), 1);
    assert_eq!(add.iter().map(|r| r.rtype).collect::<Vec<_>>(), vec![T_SRV, T_TXT, T_A]);
}

#[test]
fn ttls_count_down_and_half_life_marks_stale() {
    let (c, now) = cache();
    let (ans, _, stale) = c.answer(&[q("CYD-EMU.local", T_A)], now + Duration::from_secs(30));
    assert_eq!((ans[0].ttl, stale), (90, false), "case-insensitive, 30 s off 120");
    let (ans, _, stale) = c.answer(&[q("cyd-emu.local", T_A)], now + Duration::from_secs(70));
    assert_eq!((ans[0].ttl, stale), (50, true));
    let (ans, _, stale) = c.answer(&[q("cyd-emu.local", T_A)], now + Duration::from_secs(121));
    assert!(ans.is_empty() && stale, "expired: still the guest's name, so ask the guest");
}

#[test]
fn unknown_names_and_aaaa_questions_are_not_relevant() {
    let (c, now) = cache();
    assert!(!c.relevant(&q("other.local", T_A)));
    assert!(!c.relevant(&q("cyd-emu.local", T_AAAA)));
    assert!(!c.relevant(&q("cyd-emu._esphomelib._tcp.local", T_A)), "an instance name has no address");
    assert!(c.relevant(&q("cyd-emu._esphomelib._tcp.local", T_ANY)));
    let (ans, add, stale) = c.answer(&[q("other.local", T_A)], now);
    assert!(ans.is_empty() && add.is_empty() && !stale);
}

#[test]
fn goodbye_and_cache_flush_replace_records() {
    let (mut c, now) = cache();
    c.insert(rec("cyd-emu.local", T_A, 120, Rdata::A([192, 168, 1, 21])), now);
    let (ans, _, _) = c.answer(&[q("cyd-emu.local", T_A)], now);
    assert_eq!(ans.iter().map(|r| r.data.clone()).collect::<Vec<_>>(), vec![Rdata::A([192, 168, 1, 21])], "cache-flush replaced the old address");
    c.insert(rec("cyd-emu.local", T_A, 0, Rdata::A([192, 168, 1, 21])), now);
    assert!(c.answer(&[q("cyd-emu.local", T_A)], now).0.is_empty(), "goodbye removed it");
}

#[test]
fn relay_publishes_on_the_lan_and_answers_queries() {
    let Ok(mut relay) = MdnsRelay::open(Ipv4Addr::LOCALHOST, false) else { return };   // no multicast here: nothing to test
    relay.set_ports(vec![(6053, 16053)]);
    relay.from_guest(&[10, 0, 2, 15], &announcement());
    assert_eq!((relay.announced, relay.cached()), (1, 5));
    // A one-shot querier on an ordinary port gets a unicast answer with capped TTLs.
    let asker = UdpSocket::bind("127.0.0.1:0").unwrap();
    asker.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let ask = Message { id: 0x4242, questions: vec![q("cyd-emu.local", T_A)], ..Default::default() };
    asker.send_to(&ask.encode(), ("127.0.0.1", PORT)).unwrap();
    let mut buf = [0u8; 1500];
    for _ in 0..200 {
        assert!(relay.poll(&[2, 0, 0, 0, 0, 1]).is_empty());
        if relay.answered > 0 { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    let n = asker.recv(&mut buf).unwrap();
    let reply = Message::parse(&buf[..n]).unwrap();
    assert_eq!((reply.id, reply.questions.len()), (0x4242, 1));
    assert_eq!(reply.answers, vec![Record { name: name("cyd-emu.local"), rtype: T_A, class: 1, ttl: LEGACY_TTL, data: Rdata::A([127, 0, 0, 1]) }]);
}
