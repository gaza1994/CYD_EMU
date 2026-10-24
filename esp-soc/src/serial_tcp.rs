//! `--serial-tcp`: a chip UART as a TCP stream on the loopback interface, for esptool, arduino-cli,
//! esphome or a terminal. A client whose first byte is a Telnet IAC (pyserial's `rfc2217://`) gets
//! RFC 2217: its baud, framing, flow and purge requests are acknowledged and its DTR/RTS lines are
//! kept for the board's auto-reset ([`SerialTcp::reset_lines`]). Any other client (`socket://`, nc)
//! is a plain byte stream with both lines released. One client at a time: a new connection
//! replaces the old one. Bytes are not timed, so every baud works.
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

const IAC: u8 = 255;
const SB: u8 = 250;
const SE: u8 = 240;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;
/// Telnet options accepted in both directions: BINARY, SUPPRESS-GO-AHEAD, COM-PORT-OPTION (RFC 2217).
const ACCEPTED: [u8; 3] = [0, 3, 44];
const COM_PORT: u8 = 44;
/// Client bytes queued toward the chip before the reader stops taking more, so a fast sender is
/// held back by TCP rather than overrunning the UART FIFO.
const INPUT_MAX: usize = 1 << 16;
/// Chip output kept for a client that does not read; older bytes are dropped.
const OUTPUT_MAX: usize = 1 << 20;

#[derive(Default)]
struct Link {
    /// Connection number of the current client (0 before the first).
    client: u64,
    stream: Option<TcpStream>,
    input: VecDeque<u8>,
    /// Bytes for the wire, already Telnet-escaped for an RFC 2217 client.
    output: Vec<u8>,
    rfc2217: bool,
    dtr: bool,
    rts: bool,
    /// EN went low since the machine last looked: a pulse shorter than its poll still resets.
    en_fell: bool,
    baud: u32,
    /// Options agreed: bit n of `[them, us]` for option n < 64.
    agreed: [u64; 2],
}
impl Link {
    fn en_low(&self) -> bool { self.rts && !self.dtr }
    fn set_lines(&mut self, dtr: bool, rts: bool) {
        (self.dtr, self.rts) = (dtr, rts);
        if self.en_low() { self.en_fell = true; }
    }
    fn reply(&mut self, cmd: u8, value: &[u8]) {
        self.output.extend([IAC, SB, COM_PORT, cmd + 100]);
        for &b in value { if b == IAC { self.output.push(IAC); } self.output.push(b); }
        self.output.extend([IAC, SE]);
    }
    /// WILL/WONT/DO/DONT: agree to the accepted options, refuse the rest, and answer only a change
    /// so the two sides cannot loop.
    fn negotiate(&mut self, verb: u8, opt: u8) {
        let side = (verb == DO || verb == DONT) as usize;
        let bit = if opt < 64 { 1u64 << opt } else { 0 };
        let on = self.agreed[side] & bit != 0;
        let (yes, no) = if side == 0 { (DO, DONT) } else { (WILL, WONT) };
        let answer = match verb {
            WILL | DO if on => None,
            WILL | DO if ACCEPTED.contains(&opt) => { self.agreed[side] |= bit; Some(yes) }
            WILL | DO => Some(no),
            _ if on => { self.agreed[side] &= !bit; Some(no) }
            _ => None,
        };
        if let Some(a) = answer { self.output.extend([IAC, a, opt]); }
    }
    /// One COM-PORT-OPTION request (RFC 2217 section 3): apply it, answer with the server code.
    fn com_port(&mut self, cmd: u8, value: &[u8]) {
        let v = value.first().copied().unwrap_or(0);
        match cmd {
            1 if value.len() == 4 => {
                let baud = u32::from_be_bytes(value.try_into().unwrap());
                if baud != 0 { self.baud = baud; }
                self.reply(cmd, &self.baud.to_be_bytes());
            }
            2..=4 => self.reply(cmd, &[if v != 0 { v } else { [8, 1, 1][cmd as usize - 2] }]),
            5 => {
                let answer = match v {
                    8 | 9 => { self.set_lines(v == 8, self.rts); v }
                    11 | 12 => { self.set_lines(self.dtr, v == 11); v }
                    7 => if self.dtr { 8 } else { 9 },
                    10 => if self.rts { 11 } else { 12 },
                    0 => 1,
                    4 => 6,
                    13 => 14,
                    _ => v,
                };
                self.reply(cmd, &[answer]);
            }
            0 => self.reply(cmd, b"esp32sim"),
            10 | 11 => self.reply(cmd, &[v]),
            12 => { if v & 1 != 0 { self.input.clear(); } self.reply(cmd, &[v]); }
            _ => {}
        }
    }
}

/// The reader's place in the Telnet stream.
#[derive(Clone, Copy, PartialEq)]
enum Telnet { Data, Iac, Verb(u8), Sub, SubIac }

pub struct SerialTcp {
    link: Arc<(Mutex<Link>, Condvar)>,
    pub port: u16,
    /// `[run, download]` strapping values the auto-reset samples IO0 into; None keeps the strap.
    pub straps: Option<[u32; 2]>,
}
impl SerialTcp {
    /// Listen on `addr` (`PORT` means 127.0.0.1:PORT; 0 picks a free port).
    pub fn start(addr: &str) -> std::io::Result<SerialTcp> {
        let addr = if addr.contains(':') { addr.to_string() } else { format!("127.0.0.1:{addr}") };
        let listener = TcpListener::bind(addr)?;
        let port = listener.local_addr()?.port();
        let link = Arc::new((Mutex::new(Link::default()), Condvar::new()));
        let shared = link.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_nodelay(true);
                let (Ok(reader), Ok(keep)) = (stream.try_clone(), stream.try_clone()) else { continue };
                let id = {
                    let mut l = shared.0.lock().unwrap();
                    if let Some(old) = l.stream.take() { let _ = old.shutdown(Shutdown::Both); }
                    *l = Link { client: l.client + 1, stream: Some(keep), baud: 115_200, ..Link::default() };
                    shared.1.notify_all();
                    l.client
                };
                let (r, w) = (shared.clone(), shared.clone());
                std::thread::spawn(move || read_client(&r, id, reader));
                std::thread::spawn(move || write_client(&w, id, stream));
            }
        });
        Ok(SerialTcp { link, port, straps: None })
    }
    fn lock(&self) -> MutexGuard<'_, Link> { self.link.0.lock().unwrap() }
    pub fn connected(&self) -> bool { self.lock().stream.is_some() }
    /// Up to `max` client bytes for the chip.
    pub fn take_input(&self, max: usize) -> Vec<u8> {
        let mut l = self.lock();
        let n = max.min(l.input.len());
        let d: Vec<u8> = l.input.drain(..n).collect();
        if n != 0 { self.link.1.notify_all(); }
        d
    }
    /// Chip output for the client; false (nothing queued) without one.
    pub fn send(&self, data: &[u8]) -> bool {
        let mut l = self.lock();
        if l.stream.is_none() { return false; }
        if l.rfc2217 { for &b in data { if b == IAC { l.output.push(IAC); } l.output.push(b); } } else { l.output.extend_from_slice(data); }
        if l.output.len() > OUTPUT_MAX { let cut = l.output.len() - OUTPUT_MAX; l.output.drain(..cut); }
        self.link.1.notify_all();
        true
    }
    /// The CH340 boards' two-transistor auto-reset as `(en_low, io0_low, en_fell)`: EN is pulled
    /// low while RTS is asserted without DTR, IO0 while DTR is asserted without RTS. `en_fell`
    /// latches a low EN since the last call.
    pub fn reset_lines(&self) -> (bool, bool, bool) {
        let mut l = self.lock();
        (l.en_low(), l.dtr && !l.rts, std::mem::take(&mut l.en_fell))
    }
    /// The client's last SET-BAUDRATE (115200 until it sends one).
    pub fn baud(&self) -> u32 { self.lock().baud }
}

/// End `id`'s connection, if it is still the current one: its lines drop as a closed port's do.
fn disconnect(link: &(Mutex<Link>, Condvar), id: u64) {
    let mut l = link.0.lock().unwrap();
    if l.client != id { return; }
    if let Some(s) = l.stream.take() { let _ = s.shutdown(Shutdown::Both); }
    l.set_lines(false, false);
    l.input.clear();
    l.output.clear();
    link.1.notify_all();
}

fn read_client(link: &(Mutex<Link>, Condvar), id: u64, mut stream: TcpStream) {
    let (mut buf, mut state, mut sub, mut first) = ([0u8; 4096], Telnet::Data, Vec::new(), true);
    loop {
        let n = match stream.read(&mut buf) { Ok(0) | Err(_) => return disconnect(link, id), Ok(n) => n };
        let mut l = link.0.lock().unwrap();
        if l.client != id { return; }
        if first { l.rfc2217 = buf[0] == IAC; first = false; }
        for &b in &buf[..n] {
            if !l.rfc2217 { l.input.push_back(b); continue; }
            state = match (state, b) {
                (Telnet::Data, IAC) => Telnet::Iac,
                (Telnet::Data, _) => { l.input.push_back(b); Telnet::Data }
                (Telnet::Iac, IAC) => { l.input.push_back(IAC); Telnet::Data }
                (Telnet::Iac, SB) => { sub.clear(); Telnet::Sub }
                (Telnet::Iac, WILL..=DONT) => Telnet::Verb(b),
                (Telnet::Iac, _) => Telnet::Data,
                (Telnet::Verb(verb), _) => { l.negotiate(verb, b); Telnet::Data }
                (Telnet::Sub, IAC) => Telnet::SubIac,
                (Telnet::Sub, _) => { sub.push(b); Telnet::Sub }
                (Telnet::SubIac, IAC) => { sub.push(IAC); Telnet::Sub }
                (Telnet::SubIac, _) => {
                    if sub.len() >= 2 && sub[0] == COM_PORT { l.com_port(sub[1], &sub[2..]); }
                    Telnet::Data
                }
            };
        }
        link.1.notify_all();
        while l.client == id && l.input.len() > INPUT_MAX { l = link.1.wait(l).unwrap(); }
    }
}

fn write_client(link: &(Mutex<Link>, Condvar), id: u64, mut stream: TcpStream) {
    loop {
        let out = {
            let mut l = link.0.lock().unwrap();
            while l.client == id && l.stream.is_some() && l.output.is_empty() { l = link.1.wait(l).unwrap(); }
            if l.client != id || l.stream.is_none() { return; }
            std::mem::take(&mut l.output)
        };
        if stream.write_all(&out).is_err() { return disconnect(link, id); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..500 { if let Some(v) = f() { return v; } std::thread::sleep(Duration::from_millis(2)); }
        panic!("timed out");
    }
    fn read_some(c: &mut TcpStream, n: usize) -> Vec<u8> {
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut v = vec![0; n];
        c.read_exact(&mut v).unwrap();
        v
    }

    #[test]
    fn raw_client_is_a_byte_stream_with_lines_released() {
        let s = SerialTcp::start("0").unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        c.write_all(&[0xc0, 0xff, 1]).unwrap();
        assert_eq!(wait(|| Some(s.take_input(8)).filter(|d| d.len() == 3)), [0xc0, 0xff, 1]);
        assert!(s.send(&[0xff, 2]));
        assert_eq!(read_some(&mut c, 2), [0xff, 2]);
        assert_eq!(s.reset_lines(), (false, false, false));
    }

    /// pyserial's opening: options, SET-BAUDRATE, then the esptool reset sequence; data 0xff is
    /// doubled both ways.
    #[test]
    fn rfc2217_client_negotiates_and_drives_the_auto_reset() {
        let s = SerialTcp::start("0").unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        c.write_all(&[IAC, WILL, COM_PORT, IAC, DO, 1]).unwrap();
        assert_eq!(read_some(&mut c, 6), [IAC, DO, COM_PORT, IAC, WONT, 1]);
        c.write_all(&[IAC, SB, COM_PORT, 1, 0, 0x07, 0x08, 0x00, IAC, SE]).unwrap();
        assert_eq!(read_some(&mut c, 10), [IAC, SB, COM_PORT, 101, 0, 0x07, 0x08, 0x00, IAC, SE]);
        assert_eq!(s.baud(), 460_800);
        let control = |c: &mut TcpStream, v: u8| {
            c.write_all(&[IAC, SB, COM_PORT, 5, v, IAC, SE]).unwrap();
            assert_eq!(read_some(c, 7), [IAC, SB, COM_PORT, 105, v, IAC, SE]);
        };
        control(&mut c, 9); control(&mut c, 11);   // DTR off, RTS on: EN low
        assert_eq!(s.reset_lines(), (true, false, true));
        control(&mut c, 8); control(&mut c, 12);   // DTR on, RTS off: EN released, IO0 low
        assert_eq!(s.reset_lines(), (false, true, false));
        c.write_all(&[0xc0, IAC, IAC, 0xc0]).unwrap();
        assert_eq!(wait(|| Some(s.take_input(8)).filter(|d| d.len() == 3)), [0xc0, 0xff, 0xc0]);
        assert!(s.send(&[0xff]));
        assert_eq!(read_some(&mut c, 2), [IAC, IAC]);
        drop(c);
        wait(|| (!s.connected()).then_some(()));
        assert_eq!(s.reset_lines(), (false, false, false));
    }
}
