//! `--mdns-relay`: the guest's mDNS (RFC 6762) on the host's LAN, so Home Assistant and the
//! ESPHome dashboard find an emulated device the way they find a real one. The guest's
//! responses are rewritten for the outside (its A record becomes the host's address, SRV ports
//! become the `--hostfwd` ports, AAAA and link-local records are dropped), multicast from a
//! shared 0.0.0.0:5353 socket and cached. LAN queries for the guest's names are answered from
//! that cache while it is fresh, and passed into the guest when it is not, so its own answer
//! renews the records. Guest queries stay inside: the guest cannot resolve LAN names.

use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};
use crate::net::packet::{ethernet, ip_packet, udp_packet, GATEWAY_MAC};

pub const PORT: u16 = 5353;
pub const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_TXT: u16 = 16;
const T_AAAA: u16 = 28;
const T_SRV: u16 = 33;
const T_NSEC: u16 = 47;
const T_ANY: u16 = 255;
const TOP_BIT: u16 = 0x8000;   // cache-flush in a record's class, unicast-response in a question's
const RESPONSE: u16 = 0x8400;  // QR + AA
const MAX_CACHE: usize = 64;
const RECV_BATCH: usize = 16;
const FORWARD_GAP: Duration = Duration::from_millis(250);
const LEGACY_TTL: u32 = 10;    // RFC 6762 §6.7 for answers to a one-shot (non-5353) querier
const GUEST_SRC: [u8; 4] = [10, 0, 2, 2];  // queries reach the guest from the gateway

/// A domain name as its labels, compared without regard to ASCII case.
pub type Name = Vec<Vec<u8>>;

pub fn name_eq(a: &Name, b: &Name) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// One record for the `--debug net` log: name, type, TTL and the data that matters here.
pub fn rec_str(r: &Record) -> String {
    let data = match &r.data {
        Rdata::A(a) => format!("A {}", Ipv4Addr::from(*a)),
        Rdata::Name(n) => format!("PTR {}", name_str(n)),
        Rdata::Srv { port, target, .. } => format!("SRV {}:{port}", name_str(target)),
        Rdata::Nsec { .. } => "NSEC".into(),
        Rdata::Raw(b) => format!("type {} ({} bytes)", r.rtype, b.len()),
    };
    format!("{} {data} ttl {}", name_str(&r.name), r.ttl)
}

pub fn name_str(n: &Name) -> String {
    n.iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect::<Vec<_>>().join(".")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rdata { A([u8; 4]), Name(Name), Srv { prio: u16, weight: u16, port: u16, target: Name }, Nsec { next: Name, bitmap: Vec<u8> }, Raw(Vec<u8>) }

#[derive(Clone, Debug, PartialEq)]
pub struct Record { pub name: Name, pub rtype: u16, pub class: u16, pub ttl: u32, pub data: Rdata }

#[derive(Clone, Debug, PartialEq)]
pub struct Question { pub name: Name, pub qtype: u16, pub qclass: u16 }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message { pub id: u16, pub flags: u16, pub questions: Vec<Question>, pub answers: Vec<Record>, pub authority: Vec<Record>, pub additional: Vec<Record> }

impl Message {
    pub fn is_response(&self) -> bool { self.flags & 0x8000 != 0 }

    /// Decode a message, following compression pointers; `None` for anything malformed.
    pub fn parse(msg: &[u8]) -> Option<Message> {
        let word = |p: usize| msg.get(p..p + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        let mut m = Message { id: word(0)?, flags: word(2)?, ..Default::default() };
        let counts = [word(4)?, word(6)?, word(8)?, word(10)?];
        let mut pos = 12;
        for _ in 0..counts[0] {
            let (name, p) = read_name(msg, pos)?;
            m.questions.push(Question { name, qtype: word(p)?, qclass: word(p + 2)? });
            pos = p + 4;
        }
        for (section, &count) in counts[1..].iter().enumerate() {
            for _ in 0..count {
                let (name, p) = read_name(msg, pos)?;
                let (rtype, class) = (word(p)?, word(p + 2)?);
                let ttl = u32::from_be_bytes(msg.get(p + 4..p + 8)?.try_into().ok()?);
                let (start, end) = (p + 10, p + 10 + word(p + 8)? as usize);
                let rd = msg.get(start..end)?;
                let data = match rtype {
                    T_A if rd.len() == 4 => Rdata::A(rd.try_into().ok()?),
                    T_PTR | 2 | 5 => Rdata::Name(read_name(msg, start)?.0),
                    T_SRV if rd.len() > 6 => Rdata::Srv { prio: word(start)?, weight: word(start + 2)?, port: word(start + 4)?, target: read_name(msg, start + 6)?.0 },
                    T_NSEC => { let (next, p) = read_name(msg, start)?; Rdata::Nsec { next, bitmap: msg.get(p..end)?.to_vec() } }
                    _ => Rdata::Raw(rd.to_vec()),
                };
                let rec = Record { name, rtype, class, ttl, data };
                [&mut m.answers, &mut m.authority, &mut m.additional][section].push(rec);
                pos = end;
            }
        }
        Some(m)
    }

    /// Encode without compression: the names are short and the result stays well under the MTU.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        for v in [self.id, self.flags, self.questions.len() as u16, self.answers.len() as u16, self.authority.len() as u16, self.additional.len() as u16] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for q in &self.questions {
            write_name(&mut out, &q.name);
            out.extend_from_slice(&q.qtype.to_be_bytes());
            out.extend_from_slice(&q.qclass.to_be_bytes());
        }
        for r in self.answers.iter().chain(&self.authority).chain(&self.additional) {
            write_name(&mut out, &r.name);
            out.extend_from_slice(&r.rtype.to_be_bytes());
            out.extend_from_slice(&r.class.to_be_bytes());
            out.extend_from_slice(&r.ttl.to_be_bytes());
            let len_at = out.len();
            out.extend_from_slice(&[0, 0]);
            match &r.data {
                Rdata::A(a) => out.extend_from_slice(a),
                Rdata::Name(n) => write_name(&mut out, n),
                Rdata::Srv { prio, weight, port, target } => {
                    for v in [prio, weight, port] { out.extend_from_slice(&v.to_be_bytes()); }
                    write_name(&mut out, target);
                }
                Rdata::Nsec { next, bitmap } => { write_name(&mut out, next); out.extend_from_slice(bitmap); }
                Rdata::Raw(b) => out.extend_from_slice(b),
            }
            let len = (out.len() - len_at - 2) as u16;
            out[len_at..len_at + 2].copy_from_slice(&len.to_be_bytes());
        }
        out
    }
}

fn read_name(msg: &[u8], mut pos: usize) -> Option<(Name, usize)> {
    let (mut labels, mut end, mut jumps) = (Vec::new(), None, 0);
    loop {
        let len = *msg.get(pos)? as usize;
        match len & 0xc0 {
            0xc0 => {
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > 32 { return None; }
                pos = (len & 0x3f) << 8 | *msg.get(pos + 1)? as usize;
            }
            0 if len == 0 => return Some((labels, end.unwrap_or(pos + 1))),
            0 => { labels.push(msg.get(pos + 1..pos + 1 + len)?.to_vec()); pos += 1 + len; }
            _ => return None,
        }
    }
}

fn write_name(out: &mut Vec<u8>, n: &Name) {
    for l in n { out.push(l.len() as u8); out.extend_from_slice(l); }
    out.push(0);
}

/// Clear one type from an NSEC type bitmap (RFC 4034 §4.1.2), dropping a window left empty.
fn clear_type(bitmap: &mut Vec<u8>, rtype: u16) {
    let (window, byte, bit) = ((rtype >> 8) as u8, (rtype & 0xff) as usize / 8, 0x80u8 >> (rtype % 8));
    let mut p = 0;
    while p + 2 <= bitmap.len() {
        let len = bitmap[p + 1] as usize;
        if p + 2 + len > bitmap.len() { return; }
        if bitmap[p] == window && byte < len {
            bitmap[p + 2 + byte] &= !bit;
            let mut keep = len;
            while keep > 0 && bitmap[p + 1 + keep] == 0 { keep -= 1; }
            if keep == 0 { bitmap.drain(p..p + 2 + len); } else { bitmap[p + 1] = keep as u8; bitmap.drain(p + 2 + keep..p + 2 + len); }
            return;
        }
        p += 2 + len;
    }
}

/// How the guest's records are made true outside: its address, and its forwarded ports.
pub struct Rewrite<'a> { pub guest_ip: [u8; 4], pub host_ip: [u8; 4], pub ports: &'a [(u16, u16)] }

impl Rewrite<'_> {
    /// The station's A record becomes the host's; other guest-subnet and link-local addresses,
    /// and every AAAA, mean nothing on the LAN and go. SRV ports with a `--hostfwd` map to it.
    pub fn records(&self, records: Vec<Record>) -> Vec<Record> {
        records.into_iter().filter_map(|mut r| {
            if r.rtype == T_AAAA { return None; }
            match &mut r.data {
                Rdata::A(a) if *a == self.guest_ip => *a = self.host_ip,
                Rdata::A(a) if a[..3] == self.guest_ip[..3] || a[..2] == [169, 254] => return None,
                Rdata::Srv { port, .. } => if let Some(&(_, host)) = self.ports.iter().find(|(g, _)| g == port) { *port = host },
                Rdata::Nsec { bitmap, .. } => clear_type(bitmap, T_AAAA),
                _ => {}
            }
            Some(r)
        }).collect()
    }
}

struct Cached { rec: Record, at: Instant }

impl Cached {
    fn left(&self, now: Instant) -> u32 { self.rec.ttl.saturating_sub(now.saturating_duration_since(self.at).as_secs() as u32) }
    fn same(&self, r: &Record) -> bool { self.rec.rtype == r.rtype && self.rec.data == r.data && name_eq(&self.rec.name, &r.name) }
}

/// The rewritten records the guest last announced, each counting its TTL down from arrival.
#[derive(Default)]
pub struct Cache { entries: Vec<Cached>, known: Vec<(Name, u16)> }

impl Cache {
    pub fn insert(&mut self, rec: Record, now: Instant) {
        if !self.known.iter().any(|(n, t)| *t == rec.rtype && name_eq(n, &rec.name)) { self.known.push((rec.name.clone(), rec.rtype)); }
        if rec.ttl == 0 { self.entries.retain(|c| !c.same(&rec)); return; }   // goodbye
        if rec.class & TOP_BIT != 0 {   // cache-flush: this is now the whole set for the name and type
            self.entries.retain(|c| c.rec.rtype != rec.rtype || !name_eq(&c.rec.name, &rec.name) || c.same(&rec));
        }
        match self.entries.iter_mut().find(|c| c.same(&rec)) {
            Some(c) => *c = Cached { rec, at: now },
            None => {
                if self.entries.len() >= MAX_CACHE {
                    let first = self.entries.iter().enumerate().min_by_key(|(_, c)| c.left(now)).unwrap().0;
                    self.entries.remove(first);
                }
                self.entries.push(Cached { rec, at: now });
            }
        }
    }

    /// Whether a question is about the guest: a name and type it has announced, even if expired.
    pub fn relevant(&self, q: &Question) -> bool {
        self.known.iter().any(|(n, t)| (q.qtype == T_ANY || q.qtype == *t) && *t != T_NSEC && name_eq(n, &q.name))
    }

    /// Live answers to the questions with their TTLs counted down, the records they point at
    /// (SRV and TXT for a PTR, A for an SRV) as additionals, and whether any relevant question
    /// had no answer in the first half of its TTL, which the guest should then renew.
    pub fn answer(&self, questions: &[Question], now: Instant) -> (Vec<Record>, Vec<Record>, bool) {
        let (mut answers, mut additional, mut stale) = (Vec::new(), Vec::new(), false);
        let live = |c: &&Cached| c.left(now) > 0;
        let aged = |c: &Cached| Record { ttl: c.left(now), ..c.rec.clone() };
        for q in questions.iter().filter(|q| self.relevant(q)) {
            let hits: Vec<&Cached> = self.entries.iter().filter(live).filter(|c| (q.qtype == T_ANY || c.rec.rtype == q.qtype) && name_eq(&c.rec.name, &q.name)).collect();
            stale |= hits.is_empty() || hits.iter().any(|c| c.left(now) * 2 < c.rec.ttl);
            for c in hits { if !answers.iter().any(|r: &Record| c.same(r)) { answers.push(aged(c)); } }
        }
        let mut i = 0;
        while i < answers.len() + additional.len() {
            let r = if i < answers.len() { &answers[i] } else { &additional[i - answers.len()] };
            let (target, types): (Name, &[u16]) = match &r.data {
                Rdata::Name(n) if r.rtype == T_PTR => (n.clone(), &[T_SRV, T_TXT]),
                Rdata::Srv { target, .. } => (target.clone(), &[T_A]),
                _ => { i += 1; continue }
            };
            for c in self.entries.iter().filter(live).filter(|c| types.contains(&c.rec.rtype) && name_eq(&c.rec.name, &target)) {
                if !answers.iter().chain(&additional).any(|r| c.same(r)) { additional.push(aged(c)); }
            }
            i += 1;
        }
        (answers, additional, stale)
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

/// The relay: one shared host socket on the LAN group, and the cache behind it.
pub struct MdnsRelay {
    sock: UdpSocket,
    pub host_ip: Ipv4Addr,
    ports: Vec<(u16, u16)>,
    cache: Cache,
    last_forward: Option<Instant>,
    log: bool,
    pub announced: u64, pub answered: u64, pub forwarded: u64, pub send_errors: u64,
}

impl MdnsRelay {
    /// Join 224.0.0.251 on `host_ip`'s interface from 0.0.0.0:5353, sharing the port with the
    /// host's own responder (Avahi, systemd-resolved, Bonjour, Windows' DNS Client).
    pub fn open(host_ip: Ipv4Addr, log: bool) -> io::Result<Self> {
        UdpSocket::bind((host_ip, 0)).map_err(|e| io::Error::new(e.kind(), format!("{host_ip} is not an address of this host ({e})")))?;
        let sock = sys::shared_udp(PORT).map_err(|e| io::Error::new(e.kind(), format!("cannot share UDP port {PORT} ({e})")))?;
        sock.join_multicast_v4(&GROUP, &host_ip)?;
        sys::multicast_if(&sock, host_ip)?;
        sock.set_multicast_ttl_v4(255)?;
        sock.set_multicast_loop_v4(true)?;   // browsers on this same host hear it too
        sock.set_nonblocking(true)?;
        Ok(Self { sock, host_ip, ports: Vec::new(), cache: Cache::default(), last_forward: None, log,
            announced: 0, answered: 0, forwarded: 0, send_errors: 0 })
    }

    /// The `--hostfwd` table as guest port -> host port, for SRV records.
    pub fn set_ports(&mut self, ports: Vec<(u16, u16)>) { self.ports = ports; }

    fn send(&mut self, msg: &Message, to: SocketAddr) {
        if let Err(e) = self.sock.send_to(&msg.encode(), to) {
            self.send_errors += 1;
            if self.log { eprintln!("[mdns] send to {to} failed: {e}"); }
        }
    }

    /// A datagram the guest sent to port 5353: responses are rewritten, cached and multicast on
    /// the LAN; queries (its probes and lookups) stay inside.
    pub fn from_guest(&mut self, guest_ip: &[u8; 4], payload: &[u8]) {
        let Some(m) = Message::parse(payload) else { return };
        if !m.is_response() { return; }
        let rw = Rewrite { guest_ip: *guest_ip, host_ip: self.host_ip.octets(), ports: &self.ports };
        let out = Message { id: 0, flags: RESPONSE, questions: Vec::new(), answers: rw.records(m.answers), authority: Vec::new(), additional: rw.records(m.additional) };
        if out.answers.is_empty() && out.additional.is_empty() { return; }
        let now = Instant::now();
        for r in out.answers.iter().chain(&out.additional) {
            if self.log { eprintln!("[mdns] guest {} -> LAN", rec_str(r)); }
            self.cache.insert(r.clone(), now);
        }
        self.announced += 1;
        self.send(&out, SocketAddrV4::new(GROUP, PORT).into());
    }

    /// Answer LAN queries about the guest; return frames carrying the ones the cache could not
    /// answer freshly into the guest, as multicast to `sta_mac` from the gateway.
    pub fn poll(&mut self, sta_mac: &[u8; 6]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut buf = [0u8; 9000];
        for _ in 0..RECV_BATCH {
            let (n, src) = match self.sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => continue,   // e.g. Windows' WSAECONNRESET after an unreachable reply
            };
            let Some(q) = Message::parse(&buf[..n]) else { continue };
            if q.is_response() || !q.questions.iter().any(|x| self.cache.relevant(x)) { continue; }
            let now = Instant::now();
            let (mut answers, additional, stale) = self.cache.answer(&q.questions, now);
            let legacy = src.port() != PORT;
            if self.log { eprintln!("[mdns] LAN query from {src}: {} ({} cached answers{})", q.questions.iter().map(|x| format!("{} type {}", name_str(&x.name), x.qtype)).collect::<Vec<_>>().join(", "), answers.len(), if stale { ", stale" } else { "" }); }
            if legacy && !answers.is_empty() {
                // RFC 6762 §6.7: unicast back to the querier's port, echoing its id and questions.
                let cap = |mut r: Record| { r.ttl = r.ttl.min(LEGACY_TTL); r.class &= !TOP_BIT; r };
                let reply = Message { id: q.id, flags: RESPONSE, questions: q.questions.clone(), answers: answers.into_iter().map(cap).collect(), authority: Vec::new(), additional: additional.into_iter().map(cap).collect() };
                self.answered += 1;
                self.send(&reply, src);
            } else if !stale {
                // Known-answer suppression (§7.1): skip what the querier holds with half its TTL left.
                answers.retain(|a| !q.answers.iter().any(|k| k.rtype == a.rtype && k.data == a.data && name_eq(&k.name, &a.name) && k.ttl * 2 >= a.ttl));
                if answers.is_empty() { continue; }
                self.answered += 1;
                self.send(&Message { id: 0, flags: RESPONSE, questions: Vec::new(), answers, authority: Vec::new(), additional }, SocketAddrV4::new(GROUP, PORT).into());
            }
            if stale && self.last_forward.is_none_or(|t| now.duration_since(t) >= FORWARD_GAP) && *sta_mac != [0; 6] {
                let questions = q.questions.iter().filter(|x| self.cache.relevant(x)).map(|x| Question { qclass: x.qclass & !TOP_BIT, ..x.clone() }).collect();
                let ask = Message { questions, ..Default::default() }.encode();
                let group = GROUP.octets();
                frames.push(ethernet(sta_mac, &GATEWAY_MAC, 0x0800, &ip_packet(17, &GUEST_SRC, &group, &udp_packet(&GUEST_SRC, &group, PORT, PORT, &ask))));
                self.last_forward = Some(now);
                self.forwarded += 1;
            }
        }
        frames
    }

    pub fn cached(&self) -> usize { self.cache.len() }
}

/// The host's primary LAN address: the source the routing table picks for a public address.
/// Connecting a UDP socket sends nothing.
pub fn primary_ipv4() -> io::Result<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect("192.0.2.1:9").or_else(|_| s.connect("8.8.8.8:53"))?;
    match s.local_addr()? {
        SocketAddr::V4(a) if !a.ip().is_unspecified() && !a.ip().is_loopback() => Ok(*a.ip()),
        _ => Err(io::Error::other("no IPv4 route off this host")),
    }
}

/// Sockets std cannot make: SO_REUSEADDR (and SO_REUSEPORT) before bind, and IP_MULTICAST_IF.
#[cfg(unix)]
mod sys {
    use std::io;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::os::fd::{AsRawFd, FromRawFd};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const OPT: (i32, i32, i32, i32) = (1, 2, 15, 32);          // SOL_SOCKET, SO_REUSEADDR, SO_REUSEPORT, IP_MULTICAST_IF
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const OPT: (i32, i32, i32, i32) = (0xffff, 4, 0x200, 9);    // the BSDs and macOS
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[repr(C)]
    struct SockaddrIn { family: u16, port: [u8; 2], addr: [u8; 4], zero: [u8; 8] }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    #[repr(C)]
    struct SockaddrIn { len: u8, family: u8, port: [u8; 2], addr: [u8; 4], zero: [u8; 8] }
    extern "C" {
        fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
        fn bind(fd: i32, addr: *const SockaddrIn, len: u32) -> i32;
    }

    pub fn shared_udp(port: u16) -> io::Result<UdpSocket> {
        // SAFETY: plain socket calls on a descriptor owned by the UdpSocket from here on.
        unsafe {
            let fd = socket(2, 2, 0);   // AF_INET, SOCK_DGRAM
            if fd < 0 { return Err(io::Error::last_os_error()); }
            let sock = UdpSocket::from_raw_fd(fd);
            let one = 1i32.to_ne_bytes();
            for name in [OPT.1, OPT.2] {
                if setsockopt(fd, OPT.0, name, one.as_ptr(), 4) != 0 { return Err(io::Error::last_os_error()); }
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let sa = SockaddrIn { family: 2, port: port.to_be_bytes(), addr: [0; 4], zero: [0; 8] };
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let sa = SockaddrIn { len: 16, family: 2, port: port.to_be_bytes(), addr: [0; 4], zero: [0; 8] };
            if bind(fd, &sa, 16) != 0 { return Err(io::Error::last_os_error()); }
            Ok(sock)
        }
    }

    pub fn multicast_if(sock: &UdpSocket, ip: Ipv4Addr) -> io::Result<()> {
        // SAFETY: an in_addr for a live descriptor.
        if unsafe { setsockopt(sock.as_raw_fd(), 0, OPT.3, ip.octets().as_ptr(), 4) } != 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
}

/// Winsock: SO_REUSEADDR is the only sharing there is (no SO_REUSEPORT), and it shares with any
/// socket not bound SO_EXCLUSIVEADDRUSE; multicast reaches every sharer, unicast only one.
#[cfg(windows)]
mod sys {
    use std::io;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::os::windows::io::{AsRawSocket, FromRawSocket};
    #[repr(C)]
    struct SockaddrIn { family: u16, port: [u8; 2], addr: [u8; 4], zero: [u8; 8] }
    #[link(name = "ws2_32")]
    extern "system" {
        fn socket(af: i32, ty: i32, protocol: i32) -> usize;
        fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
        fn bind(s: usize, addr: *const SockaddrIn, len: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }
    fn last() -> io::Error { io::Error::from_raw_os_error(unsafe { WSAGetLastError() }) }

    pub fn shared_udp(port: u16) -> io::Result<UdpSocket> {
        drop(UdpSocket::bind("127.0.0.1:0"));   // std's WSAStartup, before the first raw call
        // SAFETY: plain Winsock calls on a socket owned by the UdpSocket from here on.
        unsafe {
            let s = socket(2, 2, 17);   // AF_INET, SOCK_DGRAM, IPPROTO_UDP
            if s == usize::MAX { return Err(last()); }
            let sock = UdpSocket::from_raw_socket(s as u64);
            let one = 1i32.to_ne_bytes();
            if setsockopt(s, 0xffff, 4, one.as_ptr(), 4) != 0 { return Err(last()); }   // SOL_SOCKET, SO_REUSEADDR
            let sa = SockaddrIn { family: 2, port: port.to_be_bytes(), addr: [0; 4], zero: [0; 8] };
            if bind(s, &sa, 16) != 0 { return Err(last()); }
            Ok(sock)
        }
    }

    pub fn multicast_if(sock: &UdpSocket, ip: Ipv4Addr) -> io::Result<()> {
        // SAFETY: IP_MULTICAST_IF (9) takes the interface address in network order.
        if unsafe { setsockopt(sock.as_raw_socket() as usize, 0, 9, ip.octets().as_ptr(), 4) } != 0 { return Err(last()); }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use std::io;
    use std::net::{Ipv4Addr, UdpSocket};
    pub fn shared_udp(_: u16) -> io::Result<UdpSocket> { Err(io::ErrorKind::Unsupported.into()) }
    pub fn multicast_if(_: &UdpSocket, _: Ipv4Addr) -> io::Result<()> { Err(io::ErrorKind::Unsupported.into()) }
}

#[cfg(test)]
mod tests;
