//! Passive mDNS listener for one hint: something on this PC's network keeps looking for an
//! ESPHome name that nothing answers, and it looks like this device's name with another MAC
//! suffix (a `name:` copied from another device's YAML, so the dashboard waits forever with
//! "Update queued"). Listens on UDP 5353 with address reuse (beside esp32sim's relay and the
//! system's mDNS service) and never sends anything.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Seen {
    first_query: Instant,
    last_query: Instant,
    queries: u32,
    last_answer: Option<Instant>,
}

#[derive(Default)]
struct Names(HashMap<String, Seen>);

impl Names {
    fn query(&mut self, name: String, now: Instant) {
        let e = self.0.entry(name).or_insert(Seen { first_query: now, last_query: now, queries: 0, last_answer: None });
        if now.duration_since(e.last_query) > Duration::from_secs(600) {
            e.first_query = now;
            e.queries = 0;
        }
        e.last_query = now;
        e.queries += 1;
    }
    fn answer(&mut self, name: String, now: Instant) {
        let e = self.0.entry(name).or_insert(Seen { first_query: now, last_query: now, queries: 0, last_answer: None });
        e.last_answer = Some(now);
    }
    fn note(&mut self, pkt: &[u8], now: Instant) {
        let Some((response, names)) = parse(pkt) else { return };
        // one count per packet, however many record types it asks for
        let mut uniq: Vec<String> = names.into_iter().filter_map(|n| host_of(&n)).collect();
        uniq.sort();
        uniq.dedup();
        for n in uniq {
            if response { self.answer(n, now) } else { self.query(n, now) }
        }
        if self.0.len() > 512 {
            self.0.retain(|_, s| now.duration_since(s.last_query) < Duration::from_secs(600));
        }
    }
    /// A name like `own` that is asked for again and again with no answer.
    fn lookalike(&self, own: &str, now: Instant) -> Option<String> {
        let own = own.to_ascii_lowercase();
        let stem = |s: &str| s.rsplit_once('-').map(|(a, _)| a.to_string()).filter(|a| !a.is_empty());
        let own_stem = stem(&own)?;
        self.0
            .iter()
            .filter(|(n, s)| {
                **n != own
                    && stem(n).as_deref() == Some(own_stem.as_str())
                    && s.queries >= 2
                    && s.last_query.duration_since(s.first_query) >= Duration::from_secs(5)
                    && now.duration_since(s.last_query) < Duration::from_secs(90)
                    && s.last_answer.is_none_or(|a| now.duration_since(a) > Duration::from_secs(300))
            })
            .max_by_key(|(_, s)| s.last_query)
            .map(|(n, _)| n.clone())
    }
}

/// `x.local` or `x._esphomelib._tcp.local` → `x`.
fn host_of(name: &str) -> Option<String> {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    let n = n.strip_suffix(".local")?;
    let h = n.strip_suffix("._esphomelib._tcp").unwrap_or(n);
    (!h.is_empty() && !h.contains('.') && !h.starts_with('_')).then(|| h.to_string())
}

fn read_name(p: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *p.get(i)? as usize;
        if len == 0 {
            return Some((out, end.unwrap_or(i + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *p.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            if ptr >= i { return None; }
            i = ptr;
            continue;
        }
        let label = p.get(i + 1..i + 1 + len)?;
        if !out.is_empty() { out.push('.'); }
        out.push_str(&String::from_utf8_lossy(label));
        i += 1 + len;
    }
    None
}

/// (is a response, question names or answer/additional record names)
fn parse(p: &[u8]) -> Option<(bool, Vec<String>)> {
    if p.len() < 12 { return None; }
    let w = |i: usize| u16::from_be_bytes([p[i], p[i + 1]]) as usize;
    let response = p[2] & 0x80 != 0;
    let (qd, an, ns, ar) = (w(4), w(6), w(8), w(10));
    let mut i = 12;
    let mut names = Vec::new();
    for _ in 0..qd.min(64) {
        let (n, next) = read_name(p, i)?;
        i = next + 4;
        if !response { names.push(n); }
    }
    for k in 0..(an + ns + ar).min(128) {
        let (n, next) = read_name(p, i)?;
        let rdlen = u16::from_be_bytes([*p.get(next + 8)?, *p.get(next + 9)?]) as usize;
        i = next + 10 + rdlen;
        if response && (k < an || k >= an + ns) { names.push(n); }
    }
    Some((response, names))
}

struct Watch(Arc<Mutex<Names>>);

fn start() -> Option<Watch> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    s.set_reuse_address(true).ok()?;
    #[cfg(unix)]
    let _ = s.set_reuse_port(true);
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353)).into()).ok()?;
    let _ = s.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), &Ipv4Addr::UNSPECIFIED);
    let sock: std::net::UdpSocket = s.into();
    let names = Arc::new(Mutex::new(Names::default()));
    let n2 = names.clone();
    std::thread::Builder::new()
        .name("mdns-watch".into())
        .spawn(move || {
            let mut buf = [0u8; 9000];
            loop {
                match sock.recv_from(&mut buf) {
                    Ok((len, _)) => n2.lock().unwrap().note(&buf[..len], Instant::now()),
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        })
        .ok()?;
    Some(Watch(names))
}

/// Starts listening on first use. `None` until a lookalike name has been asked for repeatedly
/// with no answer (or when UDP 5353 could not be opened at all).
pub fn lookalike(own: &str) -> Option<String> {
    static W: OnceLock<Option<Watch>> = OnceLock::new();
    let w = W.get_or_init(start).as_ref()?;
    w.0.lock().ok()?.lookalike(own, Instant::now())
}

pub fn hint(wanted: &str, own: &str) -> String {
    format!("Something on your network is looking for '{wanted}', but this device is '{own}'. Check name: in your YAML (for example a file copied from another device), or that the other device is on.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qname(n: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for l in n.split('.') { v.push(l.len() as u8); v.extend_from_slice(l.as_bytes()); }
        v.push(0);
        v
    }
    fn query(names: &[&str]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0, 0, names.len() as u8, 0, 0, 0, 0, 0, 0];
        for n in names { p.extend(qname(n)); p.extend_from_slice(&[0, 1, 0, 1]); }
        p
    }
    fn answer_a(name: &str) -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        p.extend(qname(name));
        p.extend_from_slice(&[0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 10, 0, 2, 15]);
        p
    }

    #[test]
    fn parses_queries_and_answers() {
        let q = query(&["esphome-web-16f414.local", "esphome-web-16f414._esphomelib._tcp.local"]);
        let (resp, names) = parse(&q).unwrap();
        assert!(!resp);
        assert_eq!(names.iter().filter_map(|n| host_of(n)).collect::<Vec<_>>(), ["esphome-web-16f414", "esphome-web-16f414"]);
        let (resp, names) = parse(&answer_a("cyd-panel-d43f84.local")).unwrap();
        assert!(resp && names == ["cyd-panel-d43f84.local"]);
        // compression pointer to the first question
        let mut c = query(&["a.local"]);
        c[5] = 2;
        c.extend_from_slice(&[0xC0, 12, 0, 28, 0, 1]);
        assert_eq!(parse(&c).unwrap().1, ["a.local", "a.local"]);
        assert!(parse(&[0; 5]).is_none());
        assert_eq!(host_of("_esphomelib._tcp.local"), None);
        assert_eq!(host_of("x.y.local"), None);
    }

    #[test]
    fn lookalike_needs_repeats_and_no_answer() {
        let t0 = Instant::now();
        let mut n = Names::default();
        let q = query(&["esphome-web-16f414.local"]);
        n.note(&q, t0);
        assert_eq!(n.lookalike("esphome-web-f3f765", t0), None, "one query is not enough");
        n.note(&q, t0 + Duration::from_secs(10));
        assert_eq!(n.lookalike("esphome-web-f3f765", t0 + Duration::from_secs(11)).as_deref(), Some("esphome-web-16f414"));
        assert_eq!(n.lookalike("ESPHome-Web-16F414", t0 + Duration::from_secs(11)), None, "its own name");
        assert_eq!(n.lookalike("cyd-panel-d43f84", t0 + Duration::from_secs(11)), None, "unrelated name");
        assert_eq!(n.lookalike("esphome-web-f3f765", t0 + Duration::from_secs(200)), None, "stale");
        // something answers for it: the other device is on, no hint
        n.note(&answer_a("esphome-web-16f414.local"), t0 + Duration::from_secs(12));
        n.note(&q, t0 + Duration::from_secs(20));
        assert_eq!(n.lookalike("esphome-web-f3f765", t0 + Duration::from_secs(21)), None);
    }
}
