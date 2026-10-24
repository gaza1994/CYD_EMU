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

pub struct WebLink {
    out: Sender<String>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl WebLink {
    /// Start connecting to `ws://127.0.0.1:port/` in the background. The engine
    /// needs a moment to bind, so this retries for a while.
    pub fn connect(port: u16, frames: Arc<Mutex<Option<Frame>>>, events: Sender<EngineEvent>, waker: Waker) -> WebLink {
        let (out, out_rx) = channel::<String>();
        let connected = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, s2) = (connected.clone(), stop.clone());
        std::thread::Builder::new()
            .name(format!("weblink-{port}"))
            .spawn(move || {
                let reason = run(port, frames, &events, &waker, out_rx, &c2, &s2);
                c2.store(false, Ordering::SeqCst);
                if !s2.load(Ordering::SeqCst) {
                    let _ = events.send(EngineEvent::LinkDown(reason));
                    waker();
                }
            })
            .expect("spawn weblink thread");
        WebLink { out, connected, stop }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn send_json(&self, json: String) {
        let _ = self.out.send(json);
    }

    pub fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn run(
    port: u16,
    frames: Arc<Mutex<Option<Frame>>>,
    events: &Sender<EngineEvent>,
    waker: &Waker,
    out_rx: Receiver<String>,
    connected: &AtomicBool,
    stop: &AtomicBool,
) -> String {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let deadline = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        if stop.load(Ordering::SeqCst) {
            return "stopped".into();
        }
        match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            Ok(s) => break s,
            Err(e) if Instant::now() > deadline => return format!("could not connect to engine --web port {port}: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    };
    let _ = stream.set_nodelay(true);
    let mut ws = match tungstenite::client(format!("ws://127.0.0.1:{port}/"), stream) {
        Ok((ws, _resp)) => ws,
        Err(e) => return format!("WebSocket handshake with engine failed: {e}"),
    };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
    connected.store(true, Ordering::SeqCst);
    let _ = events.send(EngineEvent::LinkUp);
    waker();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            return "stopped".into();
        }
        loop {
            match out_rx.try_recv() {
                Ok(json) => {
                    if let Err(e) = ws.send(Message::text(json)) {
                        return format!("send failed: {e}");
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return "stopped".into(),
            }
        }
        match ws.read() {
            Ok(Message::Binary(data)) => {
                if let Some(f) = decode_tft_frame(&data) {
                    *frames.lock().unwrap() = Some(f);
                    waker();
                }
            }
            Ok(Message::Close(_)) => return "engine closed the link".into(),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return format!("link error: {e}"),
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
}
