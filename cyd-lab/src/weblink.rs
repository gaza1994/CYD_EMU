//! Native client for esp32sim's `--web PORT` WebSocket (protocol: esp32sim
//! `docs/web-ui.md`). One background thread per running device.
//!
//! * engine -> us: binary type 1 = TFT frame (`w u16le, h u16le`, RGB565 LE
//!   pixels). Converted to RGBA and parked in a single-slot mailbox (only the
//!   newest frame matters; older ones are dropped, so a slow UI never backs up
//!   the engine). JSON text frames (`serial`, `stat`, ...) are ignored: the
//!   console comes from stdout.
//! * us -> engine: JSON text frames, e.g. `{"t":"touch","x":10,"y":20,"down":1}`.
//!
//! Native clients send no `Origin` header, which esp32sim treats as a trusted
//! local tool.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tungstenite::Message;

use crate::engine::{EngineEvent, Waker};

/// A decoded screen frame in RGBA8, native panel orientation.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Decode an esp32sim binary TFT message (type byte included).
pub fn decode_tft_frame(msg: &[u8]) -> Option<Frame> {
    if msg.len() < 5 || msg[0] != 1 {
        return None;
    }
    let w = u16::from_le_bytes([msg[1], msg[2]]) as usize;
    let h = u16::from_le_bytes([msg[3], msg[4]]) as usize;
    let px = &msg[5..];
    if w == 0 || h == 0 || px.len() < w * h * 2 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for c in px[..w * h * 2].chunks_exact(2) {
        let p = u16::from_le_bytes([c[0], c[1]]);
        let r = ((p >> 11) & 0x1f) as u32;
        let g = ((p >> 5) & 0x3f) as u32;
        let b = (p & 0x1f) as u32;
        rgba.extend_from_slice(&[(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8, 255]);
    }
    Some(Frame { width: w, height: h, rgba })
}

/// Is this socket error just "no data / not finished yet, try again"?
///
/// Windows does not always report a socket read timeout as `WouldBlock`/`TimedOut`: std's
/// sockets are created overlapped, and a `recv` interrupted by `SO_RCVTIMEO` can come back
/// as raw OS error 997 (`ERROR_IO_PENDING` / `WSA_IO_PENDING`, "Overlapped I/O operation is
/// in progress"), which std maps to `ErrorKind::Uncategorized`. Matching on `kind()` alone
/// treated that as fatal and dropped the live link. The Winsock codes are checked by number
/// as well (10035 `WSAEWOULDBLOCK`, 10060 `WSAETIMEDOUT`, 10004 `WSAEINTR`), in case a std
/// version maps them differently. None of these numbers is a valid errno on Linux/macOS.
pub fn is_transient_io(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) || matches!(e.raw_os_error(), Some(997 | 10035 | 10060 | 10004))
}

/// The same for a tungstenite error (only I/O errors can be transient).
pub fn is_transient_ws(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if is_transient_io(io))
}

/// The release for a touch message, if `json` is a press: `{"t":"touch",...,"down":1}`.
pub fn release_for(json: &str) -> Option<Option<String>> {
    if !json.starts_with(r#"{"t":"touch""#) {
        return None;
    }
    Some(json.contains(r#""down":1"#).then(|| json.replace(r#""down":1"#, r#""down":0"#)))
}

/// Delay before reconnect attempt `n` (0-based): 250 ms doubling to 2 s.
pub fn backoff(n: u32) -> Duration {
    Duration::from_millis((250u64 << n.min(3)).min(2000))
}

pub struct WebLink {
    out: Sender<String>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl WebLink {
    /// Start connecting to `ws://127.0.0.1:port/` in the background. The engine needs a
    /// moment to bind, so the first connect retries for a while; after that a dropped link
    /// is reconnected (with backoff) until [`WebLink::close`]. Each drop sends
    /// [`EngineEvent::LinkDown`], each (re)connect [`EngineEvent::LinkUp`].
    pub fn connect(port: u16, frames: Arc<Mutex<Option<Frame>>>, events: Sender<EngineEvent>, waker: Waker) -> WebLink {
        let (out, out_rx) = channel::<String>();
        let connected = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, s2) = (connected.clone(), stop.clone());
        std::thread::Builder::new()
            .name(format!("weblink-{port}"))
            .spawn(move || {
                let addr: SocketAddr = ([127, 0, 0, 1], port).into();
                let mut held: Option<String> = None;
                let mut ever_up = false;
                let mut fails = 0u32;
                let first_deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    if s2.load(Ordering::SeqCst) {
                        return;
                    }
                    let r = session(addr, &frames, &events, &waker, &out_rx, &c2, &s2, &mut held, &mut ever_up);
                    c2.store(false, Ordering::SeqCst);
                    if s2.load(Ordering::SeqCst) {
                        return;
                    }
                    match r {
                        Session::Stopped => return,
                        Session::Dropped(why) => {
                            fails = 0;
                            let _ = events.send(EngineEvent::LinkDown(format!("{why}; reconnecting…")));
                            waker();
                        }
                        Session::NoConnect(why) => {
                            if !ever_up && Instant::now() > first_deadline {
                                let _ = events.send(EngineEvent::LinkDown(why));
                                waker();
                                return;
                            }
                            fails += 1;
                        }
                    }
                    std::thread::sleep(backoff(fails));
                }
            })
            .expect("spawn weblink thread");
        WebLink { out, connected, stop }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Queue a message. Sent when connected; while reconnecting it waits in the queue.
    pub fn send_json(&self, json: String) {
        let _ = self.out.send(json);
    }

    pub fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

enum Session {
    Stopped,
    /// Could not connect / handshake (engine not listening yet, or gone).
    NoConnect(String),
    /// Was connected, then the link broke.
    Dropped(String),
}

#[allow(clippy::too_many_arguments)]
fn session(
    addr: SocketAddr,
    frames: &Mutex<Option<Frame>>,
    events: &Sender<EngineEvent>,
    waker: &Waker,
    out_rx: &Receiver<String>,
    connected: &AtomicBool,
    stop: &AtomicBool,
    held: &mut Option<String>,
    ever_up: &mut bool,
) -> Session {
    let port = addr.port();
    let stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        Ok(s) => s,
        Err(e) => return Session::NoConnect(format!("could not connect to engine --web port {port}: {e}")),
    };
    let _ = stream.set_nodelay(true);
    let mut ws = match tungstenite::client(format!("ws://127.0.0.1:{port}/"), stream) {
        Ok((ws, _resp)) => ws,
        Err(e) => return Session::NoConnect(format!("WebSocket handshake with engine failed: {e}")),
    };
    // Non-blocking, not a read timeout: Windows reports SO_RCVTIMEO expiry on std's
    // overlapped sockets inconsistently (os error 997), and a timed-out socket is
    // "indeterminate" per the Winsock docs. Non-blocking sockets give a clean WSAEWOULDBLOCK.
    if let Err(e) = ws.get_ref().set_nonblocking(true) {
        return Session::NoConnect(format!("engine link: {e}"));
    }
    connected.store(true, Ordering::SeqCst);
    let _ = events.send(EngineEvent::LinkUp);
    waker();
    // A finger that was down when the last link broke: lift it, or the panel stays pressed.
    if *ever_up {
        if let Some(rel) = held.take() {
            let _ = ws.write(Message::text(rel));
        }
    }
    *ever_up = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Session::Stopped;
        }
        let mut busy = false;
        loop {
            match out_rx.try_recv() {
                Ok(json) => {
                    busy = true;
                    if let Some(rel) = release_for(&json) {
                        *held = rel;
                    }
                    match ws.write(Message::text(json)) {
                        Ok(()) => {}
                        Err(e) if is_transient_ws(&e) => {}
                        Err(e) => return Session::Dropped(format!("send failed: {e}")),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Session::Stopped,
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(e) if is_transient_ws(&e) => {}
            Err(e) => return Session::Dropped(format!("send failed: {e}")),
        }
        match ws.read() {
            Ok(Message::Binary(data)) => {
                busy = true;
                if let Some(f) = decode_tft_frame(&data) {
                    *frames.lock().unwrap() = Some(f);
                    waker();
                }
            }
            Ok(Message::Close(_)) => return Session::Dropped("engine closed the link".into()),
            Ok(_) => busy = true,
            Err(e) if is_transient_ws(&e) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Session::Dropped("engine closed the link".into()),
            Err(e) => return Session::Dropped(format!("link error: {e}")),
        }
        if !busy {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgb565_frame() {
        // 2x1: pure red, pure white
        let msg = [1u8, 2, 0, 1, 0, 0x00, 0xF8, 0xFF, 0xFF];
        let f = decode_tft_frame(&msg).unwrap();
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(f.rgba, vec![255, 0, 0, 255, 255, 255, 255, 255]);
        assert!(decode_tft_frame(&[2u8, 1, 0, 1, 0, 0, 0]).is_none(), "audio frame");
        assert!(decode_tft_frame(&[1u8, 4, 0, 4, 0, 0, 0]).is_none(), "truncated");
    }

    /// The Windows errors seen on Gareth's PC (os error 997) and the other Winsock "try
    /// again" codes are retryable; real disconnects are not.
    #[test]
    fn windows_pending_and_timeout_errors_are_transient() {
        use std::io::Error;
        for code in [997, 10035, 10060, 10004] {
            let e = Error::from_raw_os_error(code);
            assert!(is_transient_io(&e), "os error {code}");
            assert!(is_transient_ws(&tungstenite::Error::Io(Error::from_raw_os_error(code))), "ws os error {code}");
        }
        assert!(is_transient_io(&Error::from(ErrorKind::WouldBlock)));
        assert!(is_transient_io(&Error::from(ErrorKind::TimedOut)));
        for kind in [ErrorKind::ConnectionReset, ErrorKind::ConnectionAborted, ErrorKind::BrokenPipe, ErrorKind::UnexpectedEof] {
            assert!(!is_transient_io(&Error::from(kind)), "{kind:?}");
        }
        // WSAECONNRESET / ECONNRESET
        assert!(!is_transient_io(&Error::from_raw_os_error(10054)));
        assert!(!is_transient_io(&Error::from_raw_os_error(104)));
        assert!(!is_transient_ws(&tungstenite::Error::ConnectionClosed));
    }

    #[test]
    fn touch_release_tracking_and_backoff() {
        assert_eq!(release_for(r#"{"t":"touch","x":3,"y":4,"down":1}"#), Some(Some(r#"{"t":"touch","x":3,"y":4,"down":0}"#.to_string())));
        assert_eq!(release_for(r#"{"t":"touch","x":3,"y":4,"down":0}"#), Some(None));
        assert_eq!(release_for(r#"{"t":"reset"}"#), None);
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(9), Duration::from_millis(2000));
    }

    /// End to end against a local WebSocket server: the link comes up, a press is sent, the
    /// server drops the connection, the link reconnects by itself and lifts the finger first.
    #[test]
    fn reconnects_and_releases_touch() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (got_tx, got_rx) = channel::<(u32, String)>();
        std::thread::spawn(move || {
            for (n, s) in l.incoming().take(2).enumerate() {
                let mut ws = tungstenite::accept(s.unwrap()).unwrap();
                // first connection: read one message, then drop it hard
                let msg = ws.read().unwrap();
                got_tx.send((n as u32, msg.into_text().unwrap().to_string())).unwrap();
                if n == 1 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        let (ev_tx, ev_rx) = channel();
        let link = WebLink::connect(port, Arc::new(Mutex::new(None)), ev_tx, Arc::new(|| {}));
        assert_eq!(ev_rx.recv_timeout(Duration::from_secs(5)).unwrap(), EngineEvent::LinkUp);
        link.send_json(r#"{"t":"touch","x":5,"y":6,"down":1}"#.into());
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(5)).unwrap(), (0, r#"{"t":"touch","x":5,"y":6,"down":1}"#.into()));
        assert!(matches!(ev_rx.recv_timeout(Duration::from_secs(5)).unwrap(), EngineEvent::LinkDown(w) if w.contains("reconnecting")));
        assert_eq!(ev_rx.recv_timeout(Duration::from_secs(5)).unwrap(), EngineEvent::LinkUp);
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(5)).unwrap(), (1, r#"{"t":"touch","x":5,"y":6,"down":0}"#.into()));
        link.close();
    }
}
