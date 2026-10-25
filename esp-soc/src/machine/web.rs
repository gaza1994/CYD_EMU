//! Browser input validation and publication of machine state.
use super::*;
use crate::web::{frame, json_escape};

/// `[[r,g,b],…]` for the web protocol.
fn leds_json(leds: &[[u8; 3]]) -> String {
    leds.iter().map(|c| format!("[{},{},{}]", c[0], c[1], c[2])).collect::<Vec<_>>().join(",")
}

fn display_message(width: u32, height: u32, pixels: &[u16]) -> Vec<u8> {
    let mut message = Vec::with_capacity(5 + pixels.len() * 2);
    message.extend_from_slice(&[1, width as u8, (width >> 8) as u8, height as u8, (height >> 8) as u8]);
    message.extend(pixels.iter().flat_map(|pixel| pixel.to_le_bytes()));
    message
}

/// How many of `set` are clients of the same forwarded port as `e`.
fn count(set: &[(&str, crate::nat::FwdClient)], e: &(&str, crate::nat::FwdClient)) -> usize {
    set.iter().filter(|(v, c)| *v == e.0 && c.host_port == e.1.host_port).count()
}

/// `{"t":"fwd",…}`: a client of a forwarded port connected or went; `clients` is how many that port has now.
fn fwd_event(event: &str, via: &str, c: &crate::nat::FwdClient, clients: usize) -> String {
    format!("{{\"t\":\"fwd\",\"event\":\"{event}\",\"via\":\"{via}\",\"port\":{},\"peer\":\"{}\",\"clients\":{clients}}}", c.host_port, c.peer)
}

fn camera_message(rgb: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(5 + rgb.len());
    message.extend_from_slice(&[4, 64, 1, 240, 0]); // 320 by 240 preview, little-endian u16
    message.extend_from_slice(rgb);
    message
}

impl<S: Soc> Machine<S> {
    // ------------------------------------------------------------------ web UI
    /// Send display / audio / ring updates to the browser (called ~50x per emulated second).
    pub(super) fn web_push(&mut self) {
        if self.rt.log {
            let now = std::time::Instant::now();
            let (i0, i1) = (self.cores[0].insn_count(), self.cores.get(1).map_or(0, |c| c.insn_count()));
            if let Some(last) = self.rt.log_last {
                let dt = now.duration_since(last).as_secs_f64() * 1e3;
                if dt > 40.0 {
                    let (p0, p1) = (self.cores[0].pc(), self.cores.get(1).map_or(0, |c| c.pc()));
                    eprintln!("[rt] t={:.2}s window took {:.0} ms: core0 {} insns (pc {:08x} {}), core1 {} insns (pc {:08x} {})", self.seconds(), dt,
                              i0 - self.rt.log_insns.0, p0, self.sym(p0), i1 - self.rt.log_insns.1, p1, self.sym(p1));
                }
            }
            self.rt.log_last = Some(now); self.rt.log_insns = (i0, i1);
        }
        let Some(w) = self.web.clone() else { return };
        self.drain_console();
        let board = self.bus.board_ref();
        let ver = board.display_version();
        // Prefer one quiet push interval for pixel streams, but never defer a changed frame
        // twice: continuous drawing must remain visible. This is a UI snapshot, not scanout.
        let changed = ver != self.ws.px_sent;
        let due = changed && (!board.display_quiet_push() || ver == self.ws.px_pending || self.ws.px_deferred);
        self.ws.px_pending = ver;
        self.ws.px_deferred = changed && !due;
        if due {
            if let Some((w_, h_, px, _)) = board.display() {
                self.ws.px_sent = ver;
                w.send_binary(&display_message(w_, h_, &px));
            }
        }
        let board = self.bus.board_ref();
        if self.ws.cam_pushed != self.bus.camera_frames() / 20 || !self.ws.cam_sent {
            if let Some(rgb) = board.camera_preview(320, 240) {
                w.send_binary(&camera_message(&rgb)); self.ws.cam_sent = true;
            }
            self.ws.cam_pushed = self.bus.camera_frames() / 20;
        }
        let (pcm, rate) = self.bus.audio();
        if pcm.len() > self.ws.audio_sent {
            let chunk = &pcm[self.ws.audio_sent..];
            let mut b = vec![2u8];
            b.extend_from_slice(&rate.to_le_bytes());
            for s in chunk { b.extend_from_slice(&s.to_le_bytes()); }
            w.send_binary(&b);
            self.ws.audio_sent = pcm.len();
        }
        let board = self.bus.board_ref();
        let grids: Vec<(&'static str, Vec<[u8; 3]>, u64)> =
            board.led_grids().into_iter().map(|(id, leds, updates)| (id, leds.to_vec(), updates)).collect();
        self.ws.grid_updates.resize(grids.len(), u64::MAX);
        for (i, (id, leds, updates)) in grids.iter().enumerate() {
            if self.ws.grid_updates[i] == *updates { continue; }
            self.ws.grid_updates[i] = *updates;
            w.send_text(&format!("{{\"t\":\"grid\",\"id\":\"{}\",\"leds\":[{}]}}", id, leds_json(leds)));
        }
        let board = self.bus.board_ref();
        if let Some((leds, updates)) = board.leds() { if updates != self.ws.ring_updates {
            self.ws.ring_updates = updates;
            w.send_text(&format!("{{\"t\":\"ring\",\"leds\":[{}]}}", leds_json(leds)));
        } }
        // the phone's state on every change (state, ip, join, hotspots), as `ap_status` with no `bound`
        let ap = self.bus.station_link().and_then(|l| l.phone().map(|p| p.status_json()));
        if let Some(ap) = ap.filter(|a| *a != self.ws.ap_sent) {
            w.send_text(&format!("{{\"t\":\"ap_status\",\"bound\":null,\"phone\":{ap}}}"));
            self.ws.ap_sent = ap;
        }
        // the network the firmware asks for, on every change (`--wifi any` takes it as its SSID)
        let wanted = self.bus.station_link().and_then(|l| l.ap()).and_then(|ap| ap.wanted.as_ref().map(|w| w.json(w.secure.is_none_or(|s| ap.serves(s)) && (ap.cfg.any || w.ssid == ap.cfg.ssid))));
        if let Some(j) = wanted.filter(|j| *j != self.ws.wanted_sent) { w.send_text(&j); self.ws.wanted_sent = j; }
        // clients of forwarded ports, both networks, as connect/disconnect events
        let fwd: Vec<(&'static str, crate::nat::FwdClient)> = self.bus.station_link().map_or(Vec::new(), |l| {
            let host = l.net().and_then(|n| n.nat.as_ref()).map_or(Vec::new(), |n| n.forward_clients());
            let phone = l.phone().map_or(Vec::new(), |p| p.nat.forward_clients());
            host.into_iter().map(|c| ("hostfwd", c)).chain(phone.into_iter().map(|c| ("ap-fwd", c))).collect()
        });
        if fwd != self.ws.fwd_sent {
            // each event's `clients` counts that port right after it, as if they had arrived one by one
            let mut live = self.ws.fwd_sent.clone();
            for e in self.ws.fwd_sent.iter().filter(|e| !fwd.contains(e)) { live.retain(|l| l != e); w.send_text(&fwd_event("disconnect", e.0, &e.1, count(&live, e))); }
            for e in fwd.iter().filter(|e| !self.ws.fwd_sent.contains(e)) { live.push(*e); w.send_text(&fwd_event("connect", e.0, &e.1, count(&live, e))); }
            self.ws.fwd_sent = fwd;
        }
        let board = self.bus.board_ref();
        // snapshot for late-joining clients: backlog, frame, ring
        if w.needs_hello() {
            let mut hello: Vec<Vec<u8>> = Vec::new();
            hello.push(frame(1, format!("{{\"t\":\"serial\",\"src\":\"uart0\",\"data\":\"{}\"}}", json_escape(&String::from_utf8_lossy(&self.console.uart0))).as_bytes()));
            hello.push(frame(1, format!("{{\"t\":\"serial\",\"src\":\"usb\",\"data\":\"{}\"}}", json_escape(&String::from_utf8_lossy(&self.console.usb))).as_bytes()));
            hello.push(frame(1, format!("{{\"t\":\"board\",\"name\":\"{}\",\"chip\":\"{}\"}}", board.name(), S::NAME).as_bytes()));
            if let Some((w_, h_, px, _)) = board.display() { hello.push(frame(2, &display_message(w_, h_, &px))); }
            if let Some(rgb) = board.camera_preview(320, 240) { hello.push(frame(2, &camera_message(&rgb))); }
            if let Some((leds, _)) = board.leds() { hello.push(frame(1, format!("{{\"t\":\"ring\",\"leds\":[{}]}}", leds_json(leds)).as_bytes())); }
            for (id, leds, _) in board.led_grids() { hello.push(frame(1, format!("{{\"t\":\"grid\",\"id\":\"{}\",\"leds\":[{}]}}", id, leds_json(leds)).as_bytes())); }
            for (i, e) in self.ws.fwd_sent.iter().enumerate() { hello.push(frame(1, fwd_event("connect", e.0, &e.1, count(&self.ws.fwd_sent[..=i], e)).as_bytes())); }
            if !self.ws.wanted_sent.is_empty() { hello.push(frame(1, self.ws.wanted_sent.as_bytes())); }
            if !self.ws.ap_sent.is_empty() { hello.push(frame(1, format!("{{\"t\":\"ap_status\",\"bound\":null,\"phone\":{}}}", self.ws.ap_sent).as_bytes())); }
            w.set_hello(hello);
        }
        w.send_text(&format!("{{\"t\":\"stat\",\"time\":{:.2},\"insns\":{},\"frames\":{},\"behind\":{:.2},\"resyncs\":{},\"speed\":{},\"cam\":{},\"gpio_in\":\"{:x}\"}}", self.seconds(), self.insns(), board.display_frames(), self.rt.behind, self.rt.resyncs, self.rt.speed.map_or_else(|| "null".to_string(), |s| format!("{:.3}", s)), self.bus.camera_frames(), self.bus.gpio_input()));
    }

    pub(super) fn poll_camera_input(&mut self) {
        if let Some(input) = &self.camera_input {
            let picture = input.lock().unwrap().take();
            if let Some(picture) = picture {
                if let Err(error) = self.bus.board().set_camera_picture(picture) { eprintln!("[camera] {error}"); }
            }
        }
    }

    // Host input is accepted at run boundaries without advancing device time. The periodic
    // poll remains necessary for native callers that run continuously rather than in slices.
    pub(super) fn web_poll_input(&mut self) {
        self.poll_camera_input();
        let Some(w) = self.web.clone() else { return };
        use crate::json::{parse_json, Json};
        for b in w.poll_incoming_bin() {
            // type 3: camera picture from the browser — [3][w u16 le][h u16 le][RGBA...]
            if b.len() >= 5 && b[0] == 3 {
                let wd = u16::from_le_bytes([b[1], b[2]]) as u32;
                let ht = u16::from_le_bytes([b[3], b[4]]) as u32;
                if let Some(picture) = crate::picture::Picture::from_rgba(wd, ht, &b[5..]) {
                    if let Err(error) = self.bus.board().set_camera_picture(picture) {
                        w.send_text(&format!("{{\"t\":\"camera-error\",\"message\":\"{error}\"}}"));
                    }
                }
            }
        }
        for m in w.poll_incoming() {
            let Ok(message) = parse_json(&m) else { continue };
            let field = |key: &str| message.get(key).and_then(Json::scalar_text);
            let t = field("t").unwrap_or_default();
            match t.as_str() {
                "btn" => { let pin: u8 = field("pin").and_then(|x| x.parse().ok()).unwrap_or(0); let v = field("v").unwrap_or_default() == "1";
                           self.bus.gpio_set_input(pin, !v); *self.bus.irq_dirty() = true; }
                "knobpress" => { let v = field("v").unwrap_or_default() == "1"; if let Some(sw) = self.bus.board_ref().named_pin("sw") { self.bus.gpio_set_input(sw, !v); *self.bus.irq_dirty() = true; } }
                "knob" => {
                    // Bound each browser message to about one second of encoder motion.
                    let d = field("d").and_then(|x| x.parse::<i32>().ok()).unwrap_or(1).clamp(-64, 64);
                    let Some((clk, dt)) = self.bus.board_ref().encoder() else { continue };
                    let step = S::CPU_HZ / 500;   // 2 ms per phase
                    let mut tc = (self.bus.cycles() + step).max(self.script.knob_next);   // queue detents back to back, never overlapping
                    for _ in 0..d.unsigned_abs() { for (pn, l) in Self::quadrature(clk, dt, d > 0) { self.script.events.push((tc, ScriptAction::Gpio(pn, l))); tc += step; } tc += step * 4; }
                    self.script.knob_next = tc;
                    // Keep already consumed events before the cursor; pending events at the
                    // current horizon have not necessarily run when input arrives at run entry.
                    self.script.events[self.script.pos..].sort_by_key(|e| e.0);
                }
                // a line with its newline, or `key`: bytes exactly as typed (a terminal on the console)
                "serial" | "key" => {
                    let data = if t == "key" { field("data").unwrap_or_default() } else { format!("{}\n", field("line").unwrap_or_default()) };
                    match field("src").as_deref() {
                        Some("uart0") => self.bus.uart_input(0, data.as_bytes()),
                        Some("uart1") => self.bus.uart_input(1, data.as_bytes()),
                        _ => self.bus.serial_input(data.as_bytes()),
                    }
                }
                // the page's Restart on a native run: the board's reset button
                "reset" if self.web_restart => { self.button_reset = true; self.bus.request_reset(esp_periph::RST_POWERON); *self.bus.irq_dirty() = true; }
                // a front end closing down: the run ends as if signalled (the flash file is written)
                "quit" => self.quit_requested = true,
                // a phone for the guest's own hotspot (`--ap-client`): `ssid`/`psk` to (re)join, `fwd` a
                // `--ap-fwd` spec to publish; every `ap_client` and `ap_status` answers with `ap_status`
                "ap_client" | "ap_status" => {
                    let reply = match self.bus.station_link() {
                        None => r#"{"t":"ap_status","error":"this chip has no SoftAP phone"}"#.to_string(),
                        Some(link) => match if t == "ap_client" { Self::ap_client(link, &field) } else { Ok(String::new()) } {
                            Err(e) => format!("{{\"t\":\"ap_status\",\"error\":\"{}\"}}", crate::web::json_escape(&e)),
                            Ok(bound) => {
                                let phone = link.phone().map_or("null".into(), |p| p.status_json());
                                if phone != "null" { self.ws.ap_sent = phone.clone(); }   // already told: no echo push
                                format!("{{\"t\":\"ap_status\",\"bound\":{},\"phone\":{phone}}}", if bound.is_empty() { "null".into() } else { format!("\"{bound}\"") })
                            }
                        },
                    };
                    w.send_text(&reply);
                }
                // the emulated access point's settings, live (`--wifi` at run time): `ssid`, `psk`
                // (absent or empty: open), `any`; a joined station is deauthenticated and rejoins
                // under its own retry logic. Answered with `wifi_status`.
                "wifi" => {
                    let any = matches!(field("any").as_deref(), Some("true" | "1"));
                    let reply = match self.bus.station_link().and_then(|l| l.ap_mut()) {
                        None => r#"{"t":"wifi_status","error":"no emulated access point: run with --wifi"}"#.to_string(),
                        Some(ap) => match ap.cfg.retarget(field("ssid").as_deref(), field("psk").as_deref(), any) {
                            Err(e) => format!("{{\"t\":\"wifi_status\",\"error\":\"{}\"}}", crate::web::json_escape(&e)),
                            Ok(cfg) => {
                                let disconnected = ap.reconfigure(cfg);
                                eprintln!("[emu] wifi: the access point is now {}{}", ap.cfg.describe(),
                                          if disconnected { "; the station was disconnected and will rejoin" } else { "" });
                                format!("{{\"t\":\"wifi_status\",\"ssid\":\"{}\",\"secure\":{},\"any\":{},\"disconnected\":{disconnected}}}",
                                        crate::web::json_escape(&ap.cfg.ssid), ap.cfg.psk.is_some(), ap.cfg.any)
                            }
                        },
                    };
                    // `wanted` was cleared: the next one is pushed even if it reads the same
                    self.ws.wanted_sent.clear();
                    w.send_text(&reply);
                }
                "touch" => { let x: u16 = field("x").and_then(|v| v.parse().ok()).unwrap_or(0); let y: u16 = field("y").and_then(|v| v.parse().ok()).unwrap_or(0);
                             let down = field("down").unwrap_or_default() == "1"; self.bus.touch_input(x, y, down); }
                _ => {}
            }
        }
    }


    /// The `ap_client` control message: a phone on first use, its hotspot choice and a forward.
    fn ap_client(link: &mut crate::wifi::StationLink, field: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
        use crate::wifi::phone::{fwd_spec, Phone, PhoneConfig};
        let ssid = field("ssid").filter(|s| !s.is_empty());
        let spec: Vec<String> = [ssid.map(|s| format!("ssid={s}")), field("psk").filter(|s| !s.is_empty()).map(|p| format!("psk={p}"))].into_iter().flatten().collect();
        let cfg = PhoneConfig::parse(&spec.join(","))?;
        let fwd = field("fwd").filter(|s| !s.is_empty()).map(|f| fwd_spec(&f)).transpose()?;
        if link.phone().is_none() { link.set_phone(Some(Phone::new(cfg.clone(), false))); }
        let phone = link.phone_mut().unwrap();
        if field("ssid").is_some() || field("psk").is_some() { phone.configure(cfg); }
        let Some(fwd) = fwd else { return Ok(String::new()) };
        let bound = phone.forward(&fwd).map_err(|e| format!("fwd: {e}"))?;
        eprintln!("[emu] ap-fwd tcp {} -> {}.{}.{}.{}:{} on the device hotspot", bound, fwd.guest_ip[0], fwd.guest_ip[1], fwd.guest_ip[2], fwd.guest_ip[3], fwd.guest_port);
        Ok(bound.to_string())
    }
}
