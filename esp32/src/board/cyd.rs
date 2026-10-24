//! The "Cheap Yellow Display" ESP32-2432S028R (Sunton): an ESP32-WROOM-32 module (4 MB flash, no
//! PSRAM) behind a 2.8" 240×320 TN panel with an ILI9341 on HSPI, an XPT2046 resistive touch
//! controller on its own pins, an RGB LED, a light sensor and a TF slot.
//!
//! Wiring: panel SCLK GPIO 14, MOSI 13, MISO 12, CS 15, D/C 2, RST tied to EN (no GPIO),
//! backlight GPIO 21 (active high); RGB LED GPIO 4 / 16 / 17 (red, green, blue; common anode,
//! active low); LDR on GPIO 34 (ADC1 channel 6); BOOT button GPIO 0; touch CLK 25, MOSI 32,
//! MISO 39, CS 33, IRQ 36; TF card on VSPI (SCLK 18, MOSI 23, MISO 19, CS 5), not modelled.
//!
//! GPIO 12..15 are HSPI's IO_MUX pins, so the IDF driver (MicroPython's `SPI(1)`, Arduino's
//! `SPIClass(HSPI)` in TFT_eSPI and LovyanGFX setups for this board) connects them directly
//! instead of through the GPIO matrix. As on the T-Display the panel is selected by its pins: the
//! bus hands the board the routes of each transaction (`SpiPins`, which resolve IO_MUX function 1
//! as well as matrix routes), and bytes whose SCLK/MOSI are on GPIO 14/13 while CS is low reach
//! the controller from either host; everything else (the TF card on VSPI) is counted and dropped.
//!
//! The XPT2046 is on pins neither host has in IO_MUX, so drivers either route VSPI to them through
//! the GPIO matrix (XPT2046_Touchscreen with `SPIClass(VSPI)` in the usual CYD sketches,
//! MicroPython's `SPI(2, sck=25, ...)`) or bit-bang them (`SoftSPI`): both reach the same
//! `Xpt2046`, transactions on CLK 25 / MOSI 32 while CS 33 is low, or the GPIO levels themselves,
//! with DOUT on GPIO 39 driven as a board input. A touch from the page or a `touch x y 0|1` script
//! action is in glass pixels (240×320 portrait) and becomes raw readings with the calibration the
//! common CYD sketches carry: on rotation 1 (landscape) they map XPT2046_Touchscreen's x
//! (channel 001) 200..3700 onto 0..319 and y (channel 101) 240..3800 onto 0..239.
//!
//! The ESP32-2432S028R is the first of a family of `GenericDisplay` boards described by a profile
//! (`profile.rs`; built-ins in `profiles/`, any other with `--board generic-display --board-config
//! FILE.toml`): another controller and glass size, a mirror, colour order and IPS inversion, the
//! XPT2046 on its own pins or sharing the panel's (then its CS picks it), a GT911 or CST816S/CST820
//! over I2C instead, the backlight and LED active levels. LEDC PWM on the backlight pin scales the
//! picture by its duty; a plain level on it is not applied.
use std::sync::{Arc, Mutex};
use esp_soc::board::{BoardEdge, SpiPins, VirtualCycle};
use esp_soc::devices::touch_i2c::{Cst820, Gt911, TouchState};
use esp_soc::devices::{DcsPanel, Xpt2046};
use esp_soc::BoardModel;
use super::profile::{I2cTouch, Profile, Touch};

pub const PIN_LCD_SCLK: u8 = 14;
pub const PIN_LCD_MOSI: u8 = 13;
pub const PIN_LCD_MISO: u8 = 12;
pub const PIN_LCD_CS: u8 = 15;
pub const PIN_LCD_DC: u8 = 2;
pub const PIN_LCD_BL: u8 = 21;
pub const PIN_LED_R: u8 = 4;
pub const PIN_LED_G: u8 = 16;
pub const PIN_LED_B: u8 = 17;
pub const PIN_LDR: u8 = 34;
pub const PIN_BOOT: u8 = 0;
pub const PIN_TOUCH_IRQ: u8 = 36;
pub const PIN_TOUCH_CLK: u8 = 25;
pub const PIN_TOUCH_MOSI: u8 = 32;
pub const PIN_TOUCH_MISO: u8 = 39;
pub const PIN_TOUCH_CS: u8 = 33;

pub const LCD_COLS: usize = 240;
pub const LCD_ROWS: usize = 320;

/// The ESP32-2432S028R: the `cyd-2432s028r` profile.
pub type Cyd2432S028R = GenericDisplay;

/// An ESP32 with an SPI display, wired as its `Profile` says.
pub struct GenericDisplay {
    pub profile: Profile,
    /// the controller; `lcd_visible` is what the glass shows
    pub panel: DcsPanel,
    /// the CS line, high (deselected) until the firmware drives it low
    pub cs_high: bool,
    pub backlight: bool,
    /// LEDC duty on the backlight pin (0..65535), while PWM drives it
    pub backlight_duty: Option<u32>, backlight_changes: u64,
    /// the RGB LED as the UI's one-LED ring (each channel full on while its GPIO is active)
    pub led: [[u8; 3]; 1],
    pub led_updates: u64,
    pub gpio_events: u64,
    /// SPI bytes delivered to the controller / dropped (CS high, or not on its SCLK/MOSI)
    pub spi_bytes: u64, pub spi_dropped: u64,
    /// the XPT2046 and the bytes it took over hardware SPI
    pub touch: Xpt2046, pub touch_bytes: u64,
    /// what an I2C touch controller reports
    pub touch_state: Arc<Mutex<TouchState>>,
    cycle: VirtualCycle, edges: Vec<BoardEdge>, irq: bool, dout: bool,
}
impl Default for GenericDisplay { fn default() -> Self { Self::new() } }
impl GenericDisplay {
    /// The ESP32-2432S028R.
    pub fn new() -> Self { Self::from_profile(super::profile::builtin("cyd-2432s028r").expect("built-in cyd profile")) }
    pub fn from_profile(profile: Profile) -> Self {
        let panel = DcsPanel::by_name(&profile.controller).expect("profile controller");
        GenericDisplay { profile, panel, cs_high: true, backlight: false, backlight_duty: None, backlight_changes: 0, led: [[0; 3]], led_updates: 0, gpio_events: 0, spi_bytes: 0, spi_dropped: 0,
                         touch: Xpt2046::new(), touch_bytes: 0, touch_state: Default::default(), cycle: 0, edges: Vec::new(), irq: true, dout: false }
    }
    /// Glass pixel to the XPT2046's raw (X, Y), from the profile's readings at the first and last
    /// pixel; the ESP32-2432S028R's channel 101 runs 3800..240 across the portrait width, channel
    /// 001 200..3700 down its height.
    pub fn touch_raw(&self, x: u16, y: u16) -> (u16, u16) {
        let Touch::Xpt2046 { raw_x, raw_y, swap_xy, .. } = self.profile.touch else { return (0, 0) };
        let lerp = |(a, b): (i32, i32), g: u16, n: usize| (a + i32::from(g).min(n as i32 - 1) * (b - a) / (n as i32 - 1)).clamp(0, 4095) as u16;
        let (w, h) = (self.profile.width, self.profile.height);
        if swap_xy { (lerp(raw_x, y, h), lerp(raw_y, x, w)) } else { (lerp(raw_x, x, w), lerp(raw_y, y, h)) }
    }
    /// What the glass shows, as a `width`×`height` frame. On the ESP32-2432S028R the panel shows the controller's whole RAM,
    /// with the columns mirrored: the drivers' upright portrait (rotation 0) is MADCTL 0x48, MX | BGR,
    /// in Adafruit_ILI9341 and TFT_eSPI's ILI9341 drivers, so the driver's column 0 lands in RAM
    /// column 239 and that is the glass's left edge. Display Function Control (B6h) can reverse the
    /// scans: GS shows the rows bottom to top, SS undoes the column mirror (its effect on the R/B dot
    /// order is not modelled). LovyanGFX's `Panel_ILI9341` sends GS with MADCTL 0x08 as rotation 0,
    /// which is why CYD setups for it use `offset_rotation = 2` (MX | MY): unadjusted, as in openHASP's
    /// `esp32-2432s028r` build, the picture is upside down (rotated 180°) until its `rotate` is 2. Red and blue are
    /// exchanged unless MADCTL's BGR bit is set (all three drivers set it). INVON inverts every pixel:
    /// unlike the T-Display's IPS glass, this TN panel shows colours as sent with inversion off
    /// (TFT_eSPI's CYD setups leave `TFT_INVERSION_ON` undefined). Asleep or off, the glass is black.
    pub fn lcd_visible(&self) -> Vec<u16> {
        let (p, pr) = (&self.panel, &self.profile);
        let exchange = (p.madctl & 0x08 != 0) != pr.bgr;
        let invert = p.inverted != pr.ips;
        let blank = !p.on || p.sleeping;
        let mx = pr.mirror_x != (pr.scan_control && p.source_reversed);
        let my = pr.flip_y != (pr.scan_control && p.gate_reversed);
        let level = self.backlight_duty.map(|d| if pr.backlight_high { d } else { 65535 - d.min(65535) });
        let mut out = Vec::with_capacity(pr.width * pr.height);
        for gy in 0..pr.height {
            let ry = gy + pr.offset.1;
            let row = if my { p.rows - 1 - ry } else { ry } * p.cols;
            for gx in 0..pr.width {
                let rx = gx + pr.offset.0;
                let mut px = p.gram[row + if mx { p.cols - 1 - rx } else { rx }];
                if exchange { px = (px & 0x07e0) | ((px & 0xf800) >> 11) | ((px & 0x001f) << 11); }
                if invert { px = !px; }
                if blank { px = 0; }
                if let Some(l) = level {
                    let scale = |v: u16| (u32::from(v) * l / 65535) as u16;
                    px = (scale(px >> 11) << 11) | (scale((px >> 5) & 0x3f) << 5) | scale(px & 0x1f);
                }
                out.push(px);
            }
        }
        out
    }
    /// Queue edges for PENIRQ and DOUT where the XPT2046 changed them.
    fn sync_touch_pins(&mut self) {
        let Touch::Xpt2046 { irq: irq_pin, miso, .. } = self.profile.touch else { return };
        let (irq, dout) = (self.touch.irq(), self.touch.dout());
        if irq != self.irq { self.irq = irq; self.edges.push(BoardEdge { cycle: self.cycle, pin: irq_pin, level: irq }); }
        if dout != self.dout { self.dout = dout; self.edges.push(BoardEdge { cycle: self.cycle, pin: miso, level: dout }); }
    }
    /// Whether a transaction with these routes is on the panel's wires; undecodable routes are
    /// taken as the panel's, as on the T-Display.
    fn on_panel_wires(&self, pins: &SpiPins) -> bool {
        let routed = pins.sclk != 0 || pins.mosi != 0;
        !routed || (pins.sclk & (1 << self.profile.sclk) != 0 && pins.mosi & (1 << self.profile.mosi) != 0)
    }
    fn set_led(&mut self, channel: usize, level: bool) {
        let v = if level == self.profile.led_high { 255 } else { 0 };
        if self.led[0][channel] != v { self.led[0][channel] = v; self.led_updates += 1; }
    }
    fn touch_irq_pin(&self) -> Option<u8> {
        match self.profile.touch { Touch::Xpt2046 { irq, .. } => Some(irq), Touch::I2c { int, .. } => int, Touch::None => None }
    }
}
impl BoardModel for GenericDisplay {
    fn name(&self) -> &'static str { self.profile.name }
    fn gpio_changes(&mut self, changes: &[(u8, bool)]) {
        let pr = &self.profile;
        let (cs, dc, rst, bl, led) = (pr.cs, pr.dc, pr.rst, pr.backlight, pr.led);
        let xpt = if let Touch::Xpt2046 { sclk, mosi, cs, .. } = pr.touch { Some((sclk, mosi, cs)) } else { None };
        for &(pin, level) in changes {
            self.gpio_events += 1;
            if pin == cs { self.cs_high = level; }
            if pin == dc { self.panel.dc = level; }
            if rst == Some(pin) && !level { self.panel.reset(); }
            if bl == Some(pin) { self.backlight = level == self.profile.backlight_high; }
            if let Some(i) = led.and_then(|l| l.iter().position(|&p| p == pin)) { self.set_led(i, level); }
            if let Some(t) = xpt.filter(|t| pin == t.0 || pin == t.1 || pin == t.2) { self.touch.pin(t, pin, level); }
        }
        self.sync_touch_pins();
    }
    fn uses_spi_pins(&self) -> bool { true }
    fn spi_transfer_pins(&mut self, _host: u8, pins: SpiPins, tx: &[u8], rx_len: usize) -> Vec<u8> {
        if let Touch::Xpt2046 { sclk, mosi, cs, .. } = self.profile.touch {
            if pins.sclk & (1 << sclk) != 0 && pins.mosi & (1 << mosi) != 0 {
                let selected = pins.cs & (1 << cs) != 0;
                self.touch.select(selected);
                if selected {
                    // full duplex (MOSI and MISO phases together) unless the read phase is the longer one,
                    // which is a half-duplex read clocked after the write
                    let n = if rx_len > tx.len() { tx.len() + rx_len } else { tx.len() };
                    let rx: Vec<u8> = (0..n).map(|i| self.touch.byte(tx.get(i).copied().unwrap_or(0))).collect();
                    self.touch_bytes += n as u64;
                    self.sync_touch_pins();
                    return if rx_len > tx.len() { rx[tx.len()..].to_vec() } else { rx[..rx_len].to_vec() };
                }
                // its own wires: nothing else listens; shared with the panel: the panel's CS decides
                if !self.profile.shared_touch_bus() { self.spi_dropped += tx.len() as u64; return vec![0; rx_len]; }
            }
        }
        if !self.cs_high && self.on_panel_wires(&pins) {
            self.spi_bytes += tx.len() as u64;
            for &b in tx { self.panel.byte(b); }
        } else {
            self.spi_dropped += tx.len() as u64;
        }
        vec![0xff; rx_len]   // MISO is wired but the controller's read commands are not modelled
    }
    fn gpio_events(&self) -> u64 { self.gpio_events }
    fn input_levels(&self) -> Vec<(u8, bool)> {
        let mut v = vec![(self.profile.boot, true)];
        match self.profile.touch {
            Touch::Xpt2046 { irq, miso, .. } => { v.push((irq, self.irq)); v.push((miso, self.dout)); }
            Touch::I2c { int: Some(int), .. } => v.push((int, !self.touch_state.lock().unwrap().down)),
            _ => {}
        }
        v
    }
    fn i2c_devices(&mut self) -> Vec<(u8, u8, Box<dyn esp_periph::i2c::I2cDevice>)> {
        let Touch::I2c { chip, addr, .. } = self.profile.touch else { return Vec::new() };
        let (w, h) = (self.profile.width as u16, self.profile.height as u16);
        // the bus the firmware routes to SDA/SCL is not known here: both controllers get it, and a
        // GT911 answers at both of its addresses (its INT level at reset picks one)
        let mut v: Vec<(u8, u8, Box<dyn esp_periph::i2c::I2cDevice>)> = Vec::new();
        for bus in [0, 1] {
            match chip {
                I2cTouch::Gt911 => for a in [addr, if addr == 0x5d { 0x14 } else { 0x5d }] { v.push((bus, a, Box::new(Gt911::new(self.touch_state.clone(), w, h)))); },
                I2cTouch::Cst820 => v.push((bus, addr, Box::new(Cst820::new(self.touch_state.clone())))),
            }
        }
        v
    }
    fn touch(&mut self, x: u16, y: u16, down: bool) {
        match self.profile.touch {
            Touch::Xpt2046 { .. } => { self.touch.pen = down.then(|| self.touch_raw(x, y)); self.sync_touch_pins(); }
            Touch::I2c { swap_xy, invert_x, invert_y, int, .. } => {
                let (w, h) = (self.profile.width as u16, self.profile.height as u16);
                let (x, y) = (x.min(w - 1), y.min(h - 1));
                let (x, y) = (if invert_x { w - 1 - x } else { x }, if invert_y { h - 1 - y } else { y });
                let (x, y) = if swap_xy { (y, x) } else { (x, y) };
                let was = self.touch_state.lock().unwrap().down;
                self.touch_state.lock().unwrap().update(x, y, down);
                if let Some(pin) = int.filter(|_| was != down) { self.edges.push(BoardEdge { cycle: self.cycle, pin, level: !down }); }
            }
            Touch::None => {}
        }
    }
    fn touch_at(&mut self, cycle: VirtualCycle, x: u16, y: u16, down: bool) { self.cycle = self.cycle.max(cycle); self.touch(x, y, down); }
    fn advance_to(&mut self, cycle: VirtualCycle) { self.cycle = cycle; }
    fn take_edges(&mut self) -> Vec<BoardEdge> { std::mem::take(&mut self.edges) }
    fn named_pin(&self, name: &str) -> Option<u8> {
        match name { "boot" | "btn1" => Some(self.profile.boot), "ldr" => self.profile.ldr, "touch-irq" => self.touch_irq_pin(), "backlight" => self.profile.backlight, _ => None }
    }
    fn display(&self) -> Option<(u32, u32, Vec<u16>, u64)> {
        Some((self.profile.width as u32, self.profile.height as u32, self.lcd_visible(), self.panel.pixels_written))
    }
    fn display_version(&self) -> u64 { self.panel.pixels_written + self.backlight_changes }
    fn display_frames(&self) -> u64 { self.panel.frames }
    fn gram(&self) -> Option<(Vec<u16>, usize, usize)> { Some((self.panel.gram.clone(), self.panel.cols, self.panel.rows)) }
    fn leds(&self) -> Option<(&[[u8; 3]], u64)> { self.profile.led.map(|_| (&self.led[..], self.led_updates)) }
    fn pwm_pins(&self) -> u64 { self.profile.backlight.map_or(0, |p| 1 << p) }
    fn pwm_duty(&mut self, _pin: u8, duty: Option<u32>) { if self.backlight_duty != duty { self.backlight_duty = duty; self.backlight_changes += 1; } }
    fn report(&self) -> String {
        let p = &self.panel;
        let pwm = self.backlight_duty.map_or(String::new(), |d| format!(" (pwm duty {}%)", d * 100 / 65535));
        let touch = match self.profile.touch {
            Touch::Xpt2046 { .. } => format!("touch {} conversions, {} spi bytes, pen={:?}", self.touch.conversions, self.touch_bytes, self.touch.pen),
            Touch::I2c { chip, .. } => { let t = *self.touch_state.lock().unwrap(); format!("touch {chip:?} down={} at ({}, {})", t.down, t.x, t.y) }
            Touch::None => "no touch".into(),
        };
        format!("[emu] {}: {} RAMWR, {} pixels, madctl={:#x} (written {:x?}) gs={} ss={} colmod={:#x} inverted={} on={} backlight={}{pwm} bbox={:?}; spi {} bytes to the panel, {} dropped (CS high or off GPIO {}/{}); rgb led={:?}; {touch}; gpio events {}",
                self.profile.name, p.frames, p.pixels_written, p.madctl, p.madctl_writes, p.gate_reversed, p.source_reversed, p.colmod, p.inverted, p.on, self.backlight, p.bbox(),
                self.spi_bytes, self.spi_dropped, self.profile.sclk, self.profile.mosi, self.led[0], self.gpio_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::Bus;

    /// the panel's routes as the bus reports them: SCLK on 14, MOSI on 13, MISO 12, CS 15 low
    fn routes() -> SpiPins { SpiPins { sclk: 1 << PIN_LCD_SCLK, mosi: 1 << PIN_LCD_MOSI, miso: Some(PIN_LCD_MISO), cs: 1 << PIN_LCD_CS } }
    fn cmd(b: &mut Cyd2432S028R, c: u8, params: &[u8]) {
        b.gpio_changes(&[(PIN_LCD_CS, false), (PIN_LCD_DC, false)]); b.spi_transfer_pins(2, routes(), &[c], 0);
        if !params.is_empty() { b.gpio_changes(&[(PIN_LCD_DC, true)]); b.spi_transfer_pins(2, routes(), params, 0); }
        b.gpio_changes(&[(PIN_LCD_CS, true)]);
    }
    /// Adafruit_ILI9341's bring-up, abridged: SWRESET, vendor power/gamma commands, MADCTL, COLMOD
    /// 16-bit, SLPOUT, DISPON
    fn bring_up(b: &mut Cyd2432S028R, madctl: u8) {
        cmd(b, 0x01, &[]);
        cmd(b, 0xef, &[0x03, 0x80, 0x02]); cmd(b, 0xcf, &[0x00, 0xc1, 0x30]); cmd(b, 0xc0, &[0x23]); cmd(b, 0xc5, &[0x3e, 0x28]);
        cmd(b, 0xb1, &[0x00, 0x18]); cmd(b, 0xb6, &[0x08, 0x82, 0x27]); cmd(b, 0xf2, &[0x00]); cmd(b, 0x26, &[0x01]);
        cmd(b, 0xe0, &[0x0f, 0x31, 0x2b, 0x0c, 0x0e, 0x08, 0x4e, 0xf1, 0x37, 0x07, 0x10, 0x03, 0x0e, 0x09, 0x00]);
        cmd(b, 0x36, &[madctl]); cmd(b, 0x3a, &[0x55]); cmd(b, 0x11, &[]); cmd(b, 0x29, &[]);
        b.gpio_changes(&[(PIN_LCD_BL, true)]);
    }
    fn window(b: &mut Cyd2432S028R, x0: u16, x1: u16, y0: u16, y1: u16) {
        let (xs, xe, ys, ye) = (x0.to_be_bytes(), x1.to_be_bytes(), y0.to_be_bytes(), y1.to_be_bytes());
        cmd(b, 0x2a, &[xs[0], xs[1], xe[0], xe[1]]); cmd(b, 0x2b, &[ys[0], ys[1], ye[0], ye[1]]);
    }

    /// Rotation 0 (MADCTL 0x48, MX | BGR): the driver's (x, y) is the glass's (x, y), colours as
    /// sent; the vendor commands are accepted and ignored.
    #[test]
    fn rotation_0_is_upright_portrait() {
        let mut b = Cyd2432S028R::new();
        bring_up(&mut b, 0x48);
        assert!(!b.panel.sleeping && b.panel.on && b.backlight && b.panel.colmod == 0x55);
        window(&mut b, 3, 4, 7, 7);
        cmd(&mut b, 0x2c, &[0xf8, 0x00, 0x07, 0xe0]);              // red, green, high byte first
        let (w, h, px, _) = b.display().unwrap();
        assert_eq!((w, h), (240, 320));
        assert_eq!((px[7 * 240 + 3], px[7 * 240 + 4], px[7 * 240 + 5]), (0xf800, 0x07e0, 0));
        assert_eq!(b.panel.bbox(), Some((235, 7, 236, 7)), "MX: RAM column 239-x");
        for (x, y) in [(0, 0), (239, 0), (0, 319), (239, 319)] { window(&mut b, x, x, y, y); cmd(&mut b, 0x2c, &[0xff, 0xff]); }
        let (_, _, px, _) = b.display().unwrap();
        assert_eq!((px[0], px[239], px[319 * 240], px[319 * 240 + 239]), (0xffff, 0xffff, 0xffff, 0xffff));
        assert_eq!(px.iter().filter(|&&p| p != 0).count(), 6);
    }

    /// Rotation 1 (MADCTL 0x28, MV | BGR), the drivers' landscape 320×240: x walks RAM rows, y RAM
    /// columns, so landscape (x, y) is glass (239 - y, x) — the landscape's top edge is the
    /// portrait glass's right edge.
    #[test]
    fn rotation_1_is_the_glass_turned() {
        let mut b = Cyd2432S028R::new();
        bring_up(&mut b, 0x28);
        window(&mut b, 10, 11, 20, 20);
        cmd(&mut b, 0x2c, &[0x07, 0xe0, 0x00, 0x1f]);
        let (_, _, px, _) = b.display().unwrap();
        assert_eq!((px[10 * 240 + 219], px[11 * 240 + 219]), (0x07e0, 0x001f));
        for (x, y) in [(0, 0), (319, 239)] { window(&mut b, x, x, y, y); cmd(&mut b, 0x2c, &[0xff, 0xff]); }
        let (_, _, px, _) = b.display().unwrap();
        assert_eq!((px[239], px[319 * 240]), (0xffff, 0xffff), "landscape (0,0) top-right, (319,239) bottom-left");
    }

    /// LovyanGFX's `Panel_ILI9341` bring-up: B6h `08 C2 27` (GS) and MADCTL 0x08 as rotation 0
    /// show the picture rotated 180° (openHASP's CYD build); its CYD `offset_rotation = 2`, MADCTL 0xD4 |
    /// BGR (MY | MX | ML | MH), is upright. SS undoes the column mirror.
    #[test]
    fn display_function_control_reverses_the_scans() {
        let mut b = Cyd2432S028R::new();
        bring_up(&mut b, 0x08);
        cmd(&mut b, 0xb6, &[0x08, 0xc2, 0x27]);
        window(&mut b, 0, 1, 0, 0);
        cmd(&mut b, 0x2c, &[0xf8, 0x00, 0x07, 0xe0]);
        let px = b.display().unwrap().2;
        assert_eq!((px[319 * 240 + 239], px[319 * 240 + 238]), (0xf800, 0x07e0), "MADCTL 0x08 with GS: rotated 180 degrees");
        cmd(&mut b, 0x2c, &[0, 0, 0, 0]); cmd(&mut b, 0x36, &[0xdc]);
        window(&mut b, 0, 1, 0, 0);
        cmd(&mut b, 0x2c, &[0xf8, 0x00, 0x07, 0xe0]);
        let px = b.display().unwrap().2;
        assert_eq!((px[0], px[1]), (0xf800, 0x07e0), "MX | MY with GS: upright");
        cmd(&mut b, 0xb6, &[0x08, 0xa2, 0x27]);
        let px = b.display().unwrap().2;
        assert_eq!((px[319 * 240 + 239], px[319 * 240 + 238]), (0xf800, 0x07e0), "SS without GS: MY's flip and MX shown as written");
        assert!(b.report().contains("(written [8, dc])"), "{}", b.report());
    }

    #[test]
    fn colour_order_inversion_blanking_and_wires() {
        let mut b = Cyd2432S028R::new();
        bring_up(&mut b, 0x40);                                    // MX without BGR: red and blue exchanged
        window(&mut b, 0, 1, 0, 0);
        cmd(&mut b, 0x2c, &[0xf8, 0x00, 0x00, 0x1f]);
        let px = b.display().unwrap().2;
        assert_eq!((px[0], px[1]), (0x001f, 0xf800));
        cmd(&mut b, 0x36, &[0x48]); cmd(&mut b, 0x21, &[]);         // INVON on the TN glass
        let px = b.display().unwrap().2;
        assert_eq!((px[0], px[2]), (!0xf800, 0xffff), "inverted, black shows white");
        cmd(&mut b, 0x20, &[]); cmd(&mut b, 0x2c, &[]);
        // CS high, or the clock on the TF card's VSPI pins: not this controller
        b.gpio_changes(&[(PIN_LCD_CS, true), (PIN_LCD_DC, true)]);
        b.spi_transfer_pins(2, routes(), &[0x07, 0xe0], 0);
        b.gpio_changes(&[(PIN_LCD_CS, false)]);
        assert_eq!(b.spi_transfer_pins(3, SpiPins { sclk: 1 << 18, mosi: 1 << 23, miso: Some(19), cs: 1 << 5 }, &[0x07, 0xe0], 2), vec![0xff; 2]);
        assert_eq!((b.panel.pixels_written, b.spi_dropped), (2, 4));
        // a VSPI host routed to the panel's pins reaches it too
        b.spi_transfer_pins(3, routes(), &[0x07, 0xe0], 0);
        assert_eq!(b.panel.pixels_written, 3);
        cmd(&mut b, 0x28, &[]);
        assert!(b.display().unwrap().2.iter().all(|&p| p == 0), "display off: black, RAM kept");
        cmd(&mut b, 0x29, &[]); cmd(&mut b, 0x10, &[]);
        assert!(b.display().unwrap().2.iter().all(|&p| p == 0), "asleep: black");
    }

    #[test]
    fn rgb_led_is_active_low_and_inputs_idle_high() {
        let mut b = Cyd2432S028R::new();
        assert_eq!(b.leds(), Some((&[[0, 0, 0]][..], 0)));
        b.gpio_changes(&[(PIN_LED_R, true), (PIN_LED_G, false), (PIN_LED_B, true)]);
        assert_eq!(b.leds(), Some((&[[0, 255, 0]][..], 1)));
        b.gpio_changes(&[(PIN_LED_G, true), (PIN_LED_B, false)]);
        assert_eq!(b.leds(), Some((&[[0, 0, 255]][..], 3)));
        assert_eq!(b.input_levels(), vec![(PIN_BOOT, true), (PIN_TOUCH_IRQ, true), (PIN_TOUCH_MISO, false)], "BOOT released, touch IRQ not asserted");
        assert_eq!((b.named_pin("boot"), b.named_pin("ldr"), b.named_pin("touch-irq"), b.named_pin("knob")), (Some(0), Some(34), Some(36), None));
        assert!(b.report().starts_with("[emu] cyd-2432s028r: 0 RAMWR"));
    }

    /// The whole path through the classic SoC bus, as the IDF driver sets up HSPI on its own pins:
    /// IO_MUX function 1 on GPIO 12..15 (HSPIQ, HSPICS0, HSPICLK, HSPID), no GPIO-matrix route,
    /// D/C as a plain GPIO output, the hardware CS0 asserted by the controller, SPI2 CPU transfers.
    #[test]
    fn hspi_on_its_iomux_pins_reaches_the_glass() {
        const GPIO: u32 = 0x3ff4_4000;
        const IO_MUX: u32 = 0x3ff4_9000;
        const SPI2: u32 = 0x3ff6_4000;
        // IO_MUX pad registers (soc/esp32/register/soc/io_mux_reg.h): GPIO12 0x34, GPIO13 0x38,
        // GPIO14 0x30, GPIO15 0x3c, GPIO2 0x40, GPIO21 0x7c; MCU_SEL bits 14:12 (1 = HSPI, 2 = GPIO), FUN_IE bit 9
        let mut b = crate::bus::SocBus::new(4 << 20, [0; 6]);
        b.board = Box::new(Cyd2432S028R::new());
        b.attach_board_devices();
        for off in [0x34, 0x38, 0x30, 0x3c] { b.write32(IO_MUX + off, (1 << 12) | (1 << 9)).unwrap(); }
        for off in [0x40, 0x7c] { b.write32(IO_MUX + off, 2 << 12).unwrap(); }
        for pin in [2, 21] { b.write32(GPIO + 0x530 + 4 * pin, 256).unwrap(); }
        b.write32(GPIO + 0x24, (1 << 2) | (1 << 21)).unwrap();                         // ENABLE_W1TS
        b.write32(SPI2 + 0x34, !1 & 0x7).unwrap();                                      // PIN: CS0 enabled, active low
        let xfer = |b: &mut crate::bus::SocBus, dc: bool, bytes: &[u8]| {
            b.write32(GPIO + if dc { 0x08 } else { 0x0c }, 1 << 2).unwrap();
            for (i, chunk) in bytes.chunks(4).enumerate() { let mut w = 0u32; for (k, &v) in chunk.iter().enumerate() { w |= u32::from(v) << (8 * k); } b.write32(SPI2 + 0x80 + 4 * i as u32, w).unwrap(); }
            b.write32(SPI2 + 0x1c, 1 << 27).unwrap();                                       // USER: MOSI phase
            b.write32(SPI2 + 0x28, bytes.len() as u32 * 8 - 1).unwrap();                     // MOSI_DLEN
            b.write32(SPI2, 1 << 18).unwrap();                                              // CMD.USR
            assert_eq!(b.read32(SPI2).unwrap() & (1 << 18), 0, "the transaction completes at once");
        };
        let cmd = |b: &mut crate::bus::SocBus, c: u8, params: &[u8]| { xfer(b, false, &[c]); if !params.is_empty() { xfer(b, true, params); } };
        cmd(&mut b, 0x11, &[]); cmd(&mut b, 0x3a, &[0x55]); cmd(&mut b, 0x36, &[0x48]); cmd(&mut b, 0x29, &[]);
        cmd(&mut b, 0x2a, &[0, 10, 0, 11]); cmd(&mut b, 0x2b, &[0, 20, 0, 20]);
        cmd(&mut b, 0x2c, &[0xf8, 0x00, 0x00, 0x1f]);
        b.write32(GPIO + 0x08, 1 << 21).unwrap();                                           // backlight on
        let (w, h, px, _) = b.board.display().unwrap();
        assert_eq!((w, h), (240, 320));
        assert_eq!((px[20 * 240 + 10], px[20 * 240 + 11]), (0xf800, 0x001f));
        assert_eq!(px.iter().filter(|&&p| p != 0).count(), 2);
        assert_eq!(b.periph.spi[0].transfers, 12, "7 commands, 5 parameter blocks on SPI2");
        let report = b.board.report();
        assert!(report.contains("1 RAMWR, 2 pixels, madctl=0x48") && report.contains("backlight=true") && report.contains("spi 21 bytes to the panel, 0 dropped"), "{report}");
    }

    #[test]
    fn touch_reads_over_routed_vspi_and_bit_banged_pins() {
        let mut b = Cyd2432S028R::new();
        let vspi = SpiPins { sclk: 1 << PIN_TOUCH_CLK, mosi: 1 << PIN_TOUCH_MOSI, miso: Some(PIN_TOUCH_MISO), cs: 1 << PIN_TOUCH_CS };
        b.touch_at(100, 120, 160, true);
        assert_eq!(b.take_edges(), vec![BoardEdge { cycle: 100, pin: PIN_TOUCH_IRQ, level: false }], "pressed: PENIRQ falls");
        let (x, y) = b.touch_raw(120, 160);
        assert_eq!((x, y), (2013, 1955));
        let rx = b.spi_transfer_pins(3, vspi, &[0xd0, 0, 0], 3);
        assert_eq!(u16::from_be_bytes([rx[1], rx[2]]) >> 3, x, "full duplex: X behind the control byte");
        assert_eq!(b.spi_transfer_pins(3, vspi, &[0x90], 2), vec![(y << 3 >> 8) as u8, (y << 3) as u8], "half duplex read after the write");
        assert_eq!(b.spi_transfer_pins(3, SpiPins { cs: 0, ..vspi }, &[0x90], 2), vec![0; 2], "CS high: dropped");
        // SoftSPI mode 0 on the same pins: 0x90, then 16 clocks reading DOUT on GPIO 39's edges
        let mut level = false;
        let mut clock = |b: &mut Cyd2432S028R, bit: bool| -> bool {
            b.gpio_changes(&[(PIN_TOUCH_MOSI, bit), (PIN_TOUCH_CLK, true)]);
            let sample = level;
            b.gpio_changes(&[(PIN_TOUCH_CLK, false)]);
            for e in b.take_edges() { if e.pin == PIN_TOUCH_MISO { level = e.level; } }
            sample
        };
        b.gpio_changes(&[(PIN_TOUCH_CS, true)]);
        b.gpio_changes(&[(PIN_TOUCH_CS, false)]);
        for i in 0..8 { clock(&mut b, 0x90 & (0x80 >> i) != 0); }
        let word = (0..16).fold(0u16, |w, _| w << 1 | clock(&mut b, false) as u16);
        assert_eq!(word >> 3, y);
        b.touch(0, 0, false);
        assert!(b.take_edges().iter().any(|e| e.pin == PIN_TOUCH_IRQ && e.level), "released: PENIRQ rises");
        assert!(b.report().contains("touch 3 conversions, 6 spi bytes, pen=None"), "{}", b.report());
    }

    fn variant(name: &str) -> GenericDisplay { GenericDisplay::from_profile(crate::board::profile::builtin(name).unwrap()) }

    /// The two-USB board's 320x240 glass as ESPHome drives it (model ILI9341, 320x240, MADCTL
    /// 0x48, LVGL rotation 180 drawing content (x, y) at (319 - x, 239 - y)): upright, and a tap
    /// through ESPHome's xpt2046 (swap_xy, calibration x 208..3823 y 282..3835 inverted) and the
    /// rotation lands where it was made.
    #[test]
    fn two_usb_esphome_rotation_180_is_upright_and_taps_land() {
        let mut b = variant("cyd-2usb");
        bring_up(&mut b, 0x48);
        window(&mut b, 319 - 3, 319 - 3, 239 - 7, 239 - 7);
        cmd(&mut b, 0x2c, &[0xf8, 0x00]);
        let (w, h, px, _) = b.display().unwrap();
        assert_eq!((w, h), (320, 240));
        assert_eq!(px[7 * 320 + 3], 0xf800);
        assert_eq!(px.iter().filter(|&&p| p != 0).count(), 1);
        let norm = |v: i32, lo: i32, hi: i32| 0xfff - (0xfff * (v.clamp(lo, hi) - lo) / (hi - lo));    // inverted
        for gy in [40u16, 120, 200] {
            for gx in [53u16, 160, 266] {
                let (x, y) = b.touch_raw(gx, gy);
                let (tx, ty) = (norm(y.into(), 208, 3823) * 320 / 4096, norm(x.into(), 282, 3835) * 240 / 4096);
                let (cx, cy) = (319 - tx, 239 - ty);
                assert!((cx - i32::from(gx)).abs() <= 2 && (cy - i32::from(gy)).abs() <= 2, "glass ({gx}, {gy}) read as ({cx}, {cy})");
            }
        }
    }

    /// 2.4", 3.2" and 3.5" resistive boards: the XPT2046 shares SCLK 14 / MOSI 13 and its CS 33
    /// picks it; the panel's transactions still reach the panel.
    #[test]
    fn shared_bus_touch_is_picked_by_its_cs() {
        let mut b = variant("cyd-2432s024r");
        b.touch(0, 0, true);
        let touch = SpiPins { cs: 1 << 33, ..routes() };
        let rx = b.spi_transfer_pins(2, touch, &[0xd0, 0, 0], 3);
        assert_eq!(u16::from_be_bytes([rx[1], rx[2]]) >> 3, 3800);
        bring_up(&mut b, 0x48);
        window(&mut b, 0, 0, 0, 0);
        cmd(&mut b, 0x2c, &[0xf8, 0x00]);
        assert_eq!((b.display().unwrap().2[0], b.panel.pixels_written, b.touch_bytes), (0xf800, 1, 3));
        assert!(b.report().starts_with("[emu] cyd-2432s024r:") && b.report().contains("off GPIO 14/13"));
    }

    /// The 3.2" ST7789 IPS glass is RGB and shows colours as sent only with INVON.
    #[test]
    fn ips_rgb_glass_wants_invon_and_no_bgr() {
        let mut b = variant("cyd-2432s032r");
        bring_up(&mut b, 0x00);
        cmd(&mut b, 0x21, &[]);
        window(&mut b, 0, 0, 0, 0);
        cmd(&mut b, 0x2c, &[0xf8, 0x00]);
        assert_eq!(b.display().unwrap().2[0], 0xf800);
        cmd(&mut b, 0x20, &[]);
        assert_eq!(b.display().unwrap().2[0], !0xf800);
    }

    /// GT911 on I2C (both buses, both addresses), reporting glass pixels; INT falls while pressed.
    #[test]
    fn capacitive_boards_report_glass_pixels_over_i2c() {
        let mut b = variant("cyd-3248s035c");
        let devs = b.i2c_devices();
        assert_eq!(devs.iter().map(|d| (d.0, d.1)).collect::<Vec<_>>(), [(0, 0x5d), (0, 0x14), (1, 0x5d), (1, 0x14)]);
        assert_eq!(b.input_levels(), vec![(0, true), (21, true)]);
        b.touch_at(5, 300, 470, true);
        assert_eq!(b.take_edges(), vec![BoardEdge { cycle: 5, pin: 21, level: false }]);
        let t = *b.touch_state.lock().unwrap();
        assert_eq!((t.down, t.x, t.y), (true, 300, 470));
        assert_eq!(b.display().unwrap().0, 320);
        assert_eq!(variant("cyd-2432s024c").i2c_devices().len(), 2, "CST816S at 0x15 on each bus");
    }

    /// LEDC duty on the backlight scales the picture; without PWM the level is not applied.
    #[test]
    fn backlight_pwm_dims_the_glass() {
        let mut b = Cyd2432S028R::new();
        assert_eq!(b.pwm_pins(), 1 << PIN_LCD_BL);
        bring_up(&mut b, 0x48);
        window(&mut b, 0, 0, 0, 0);
        cmd(&mut b, 0x2c, &[0xff, 0xff]);
        let v = b.display_version();
        b.pwm_duty(PIN_LCD_BL, Some(32768));
        assert_eq!(b.display().unwrap().2[0], (15 << 11) | (31 << 5) | 15);
        assert_ne!(b.display_version(), v, "a duty change repaints");
        assert!(b.report().contains("backlight=true (pwm duty 50%)"), "{}", b.report());
        b.pwm_duty(PIN_LCD_BL, None);
        assert_eq!(b.display().unwrap().2[0], 0xffff);
    }
}
