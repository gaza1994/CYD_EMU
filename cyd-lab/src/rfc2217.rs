//! A tiny RFC 2217 client for the engine's `--serial-tcp` port, used for one thing:
//! pressing EN with a changed BOOT level *in-process*. esp32sim's WebSocket `reset`
//! keeps the strap, but its serial port models the board's auto-reset circuit
//! (EN low = RTS && !DTR, IO0 low = DTR && !RTS; IO0 low within 250 ms after EN rises
//! = download mode), so cyd-lab drives those lines exactly like esptool does.
//!
//! The engine serves one client at a time: connecting replaces a connected tool, and
//! while connected the chip's UART0 output comes to us instead of the engine's stdout,
//! so whatever arrives is forwarded to the console.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::engine::{EngineEvent, Stream, Waker};

const IAC: u8 = 255;
const SB: u8 = 250;
const SE: u8 = 240;
const WILL: u8 = 251;
const COM_PORT: u8 = 44;
const SET_CONTROL: u8 = 5;
pub const DTR_ON: u8 = 8;
pub const DTR_OFF: u8 = 9;
pub const RTS_ON: u8 = 11;
pub const RTS_OFF: u8 = 12;

/// One SET-CONTROL subnegotiation.
pub fn set_control(value: u8) -> [u8; 7] {
    [IAC, SB, COM_PORT, SET_CONTROL, value, IAC, SE]
}

/// The line changes for one EN press, as (SET-CONTROL value, pause after it).
/// Mirrors esptool's classic reset: EN low, then release EN with IO0 low (download)
/// or high (normal boot).
pub fn en_sequence(download: bool) -> Vec<(u8, Duration)> {
    let ms = Duration::from_millis;
    let mut seq = vec![(DTR_OFF, ms(0)), (RTS_ON, ms(100))]; // EN low
    if download {
        seq.extend([(DTR_ON, ms(0)), (RTS_OFF, ms(400)), (DTR_OFF, ms(0))]); // IO0 low as EN rises, then release
    } else {
        seq.push((RTS_OFF, ms(400))); // EN high, IO0 high: the 250 ms window passes → normal boot
    }
    seq
}

/// Strip Telnet commands from a chunk of RFC 2217 data. `state` carries a split
/// sequence over chunk boundaries (0 = data, 1 = after IAC, 2 = in SB, 3 = IAC in SB, 4 = verb option).
pub fn strip_telnet(state: &mut u8, input: &[u8], out: &mut Vec<u8>) {
    for &b in input {
        *state = match (*state, b) {
            (0, IAC) => 1,
            (0, _) => { out.push(b); 0 }
            (1, IAC) => { out.push(IAC); 0 }
            (1, SB) => 2,
            (1, 251..=254) => 4,
            (1, _) => 0,
            (2, IAC) => 3,
            (2, _) => 2,
            (3, SE) => 0,
            (3, _) => 2,
            (4, _) => 0,
            _ => 0,
        };
    }
}

/// Press EN through the serial port: connect (synchronously, so a refused port is an
/// error now), then run the line sequence and forward chip output on a thread.
pub fn pulse_en(port: u16, download: bool, tx: Sender<EngineEvent>, waker: Waker) -> std::io::Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    s.set_nodelay(true)?;
    s.write_all(&[IAC, WILL, COM_PORT])?;
    std::thread::spawn(move || {
        let mut reader = s.try_clone().ok();
        let forward = std::thread::spawn(move || {
            let Some(r) = reader.as_mut() else { return };
            let _ = r.set_read_timeout(Some(Duration::from_millis(100)));
            let (mut state, mut line, mut buf) = (0u8, Vec::<u8>::new(), [0u8; 4096]);
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut data = Vec::new();
                        strip_telnet(&mut state, &buf[..n], &mut data);
                        for b in data {
                            if b == b'\n' {
                                let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
                                let _ = tx.send(EngineEvent::Console(Stream::Stdout, text));
                                line.clear();
                                waker();
                            } else {
                                line.push(b);
                            }
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
            if !line.is_empty() {
                let _ = tx.send(EngineEvent::Console(Stream::Stdout, String::from_utf8_lossy(&line).into_owned()));
                waker();
            }
        });
        for (value, pause) in en_sequence(download) {
            if s.write_all(&set_control(value)).is_err() {
                break;
            }
            std::thread::sleep(pause);
        }
        let _ = forward.join();
        let _ = s.shutdown(std::net::Shutdown::Both);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bridge::{auto_reset, ModemLines};

    /// Replay a sequence through the auto-reset circuit: the pins seen after each step.
    fn pins(download: bool) -> Vec<(bool, bool)> {
        let mut l = ModemLines::default();
        en_sequence(download)
            .into_iter()
            .map(|(v, _)| {
                match v {
                    DTR_ON => l.dtr = true,
                    DTR_OFF => l.dtr = false,
                    RTS_ON => l.rts = true,
                    RTS_OFF => l.rts = false,
                    _ => unreachable!(),
                }
                let p = auto_reset(l);
                (p.en_low, p.io0_low)
            })
            .collect()
    }

    #[test]
    fn en_sequences_drive_the_auto_reset_circuit() {
        // normal: EN low, then released with IO0 high
        assert_eq!(pins(false), [(false, false), (true, false), (false, false)]);
        // download: EN low, then both asserted (neither pin), then IO0 low with EN high, then released
        assert_eq!(pins(true), [(false, false), (true, false), (false, false), (false, true), (false, false)]);
    }

    #[test]
    fn telnet_is_stripped_across_chunks() {
        let mut st = 0;
        let mut out = Vec::new();
        strip_telnet(&mut st, &[b'a', IAC], &mut out);
        strip_telnet(&mut st, &[IAC, b'b', IAC, SB, COM_PORT, 105, 8, IAC], &mut out);
        strip_telnet(&mut st, &[SE, IAC, 253, 44, b'c'], &mut out);
        assert_eq!(out, [b'a', 0xff, b'b', b'c']);
        assert_eq!(set_control(RTS_ON), [IAC, SB, COM_PORT, 5, 11, IAC, SE]);
    }
}
