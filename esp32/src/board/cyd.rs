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
use esp_soc::board::{BoardEdge, SpiPins, VirtualCycle};
use esp_soc::devices::{DcsPanel, Xpt2046};
use esp_soc::BoardModel;

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
const TOUCH_PINS: (u8, u8, u8) = (PIN_TOUCH_CLK, PIN_TOUCH_MOSI, PIN_TOUCH_CS);

pub const LCD_COLS: usize = 240;
pub const LCD_ROWS: usize = 320;

/// What the glass shows, as a portrait 240×320 frame. The panel shows the controller's whole RAM,
/// with the columns mirrored: the drivers' upright portrait (rotation 0) is MADCTL 0x48, MX | BGR,
/// in Adafruit_ILI9341, TFT_eSPI's ILI9341 driver and LovyanGFX's `Panel_ILI9341`, so the
/// driver's column 0 lands in RAM column 239 and that is the glass's left edge. Red and blue are
/// exchanged unless MADCTL's BGR bit is set (all three drivers set it). INVON inverts every pixel:
/// unlike the T-Display's IPS glass, this TN panel shows colours as sent with inversion off
/// (TFT_eSPI's CYD setups leave `TFT_INVERSION_ON` undefined). Asleep or off, the glass is black.
/// The backlight is not applied: LEDC dimming on GPIO 21 is common and its PWM is not a level.
pub fn lcd_visible(p: &DcsPanel) -> Vec<u16> {
    let mut out = Vec::with_capacity(LCD_COLS * LCD_ROWS);
    let exchange = p.madctl & 0x08 == 0;
    let blank = !p.on || p.sleeping;
    for y in 0..LCD_ROWS {
        let row = y * p.cols;
        for x in 0..LCD_COLS {
            let mut px = p.gram[row + LCD_COLS - 1 - x];
            if exchange { px = (px & 0x07e0) | ((px & 0xf800) >> 11) | ((px & 0x001f) << 11); }
            if p.inverted { px = !px; }
            if blank { px = 0; }
            out.push(px);
        }
    }
    out
}

/// The Sunton ESP32-2432S028R.
pub struct Cyd2432S028R {
    /// the ILI9341; `lcd_visible` is what the glass shows
    pub panel: DcsPanel,
    /// the CS line, high (deselected) until the firmware drives GPIO 15 low
    pub cs_high: bool,
    pub backlight: bool,
    /// the RGB LED as the UI's one-LED ring (each channel full on while its GPIO is low)
    pub led: [[u8; 3]; 1],
    pub led_updates: u64,
    pub gpio_events: u64,
    /// SPI bytes delivered to the controller / dropped (CS high, or not on GPIO 14/13)
    pub spi_bytes: u64, pub spi_dropped: u64,
    /// the XPT2046 and the bytes it took over hardware SPI
    pub touch: Xpt2046, pub touch_bytes: u64,
    cycle: VirtualCycle, edges: Vec<BoardEdge>, irq: bool, dout: bool,
}
impl Default for Cyd2432S028R { fn default() -> Self { Self::new() } }
impl Cyd2432S028R {
    pub fn new() -> Self {
        Cyd2432S028R { panel: DcsPanel::ili9341(), cs_high: true, backlight: false, led: [[0; 3]], led_updates: 0, gpio_events: 0, spi_bytes: 0, spi_dropped: 0,
                       touch: Xpt2046::new(), touch_bytes: 0, cycle: 0, edges: Vec::new(), irq: true, dout: false }
    }
    /// Glass pixel (portrait, as `display()`) to the controller's raw (X, Y): channel 101 runs
    /// 3800..240 across the portrait width, channel 001 200..3700 down its height.
    pub fn touch_raw(x: u16, y: u16) -> (u16, u16) {
        let (x, y) = (u32::from(x.min(LCD_COLS as u16 - 1)), u32::from(y.min(LCD_ROWS as u16 - 1)));
        ((3800 - x * 3560 / (LCD_COLS as u32 - 1)) as u16, (200 + y * 3500 / (LCD_ROWS as u32 - 1)) as u16)
    }
    /// Queue edges for PENIRQ (GPIO 36) and DOUT (GPIO 39) where the controller changed them.
    fn sync_touch_pins(&mut self) {
        let (irq, dout) = (self.touch.irq(), self.touch.dout());
        if irq != self.irq { self.irq = irq; self.edges.push(BoardEdge { cycle: self.cycle, pin: PIN_TOUCH_IRQ, level: irq }); }
        if dout != self.dout { self.dout = dout; self.edges.push(BoardEdge { cycle: self.cycle, pin: PIN_TOUCH_MISO, level: dout }); }
    }
    /// Whether a transaction with these routes is on the panel's wires; undecodable routes are
    /// taken as the panel's, as on the T-Display.
    fn on_panel_wires(pins: &SpiPins) -> bool {
        let routed = pins.sclk != 0 || pins.mosi != 0;
        !routed || (pins.sclk & (1 << PIN_LCD_SCLK) != 0 && pins.mosi & (1 << PIN_LCD_MOSI) != 0)
    }
    fn set_led(&mut self, channel: usize, level: bool) {
        let v = if level { 0 } else { 255 };
        if self.led[0][channel] != v { self.led[0][channel] = v; self.led_updates += 1; }
    }
}
impl BoardModel for Cyd2432S028R {
    fn name(&self) -> &'static str { "cyd-2432s028r" }
    fn gpio_changes(&mut self, changes: &[(u8, bool)]) {
        for &(pin, level) in changes {
            self.gpio_events += 1;
            match pin {
                PIN_LCD_CS => self.cs_high = level,
                PIN_LCD_DC => self.panel.dc = level,
                PIN_LCD_BL => self.backlight = level,
                PIN_LED_R => self.set_led(0, level),
                PIN_LED_G => self.set_led(1, level),
                PIN_LED_B => self.set_led(2, level),
                PIN_TOUCH_CLK | PIN_TOUCH_MOSI | PIN_TOUCH_CS => { self.touch.pin(TOUCH_PINS, pin, level); }
                _ => {}
            }
        }
        self.sync_touch_pins();
    }
    fn uses_spi_pins(&self) -> bool { true }
    fn spi_transfer_pins(&mut self, _host: u8, pins: SpiPins, tx: &[u8], rx_len: usize) -> Vec<u8> {
        if pins.sclk & (1 << PIN_TOUCH_CLK) != 0 && pins.mosi & (1 << PIN_TOUCH_MOSI) != 0 {
            self.touch.select(pins.cs & (1 << PIN_TOUCH_CS) != 0);
            if !self.touch.selected() { self.spi_dropped += tx.len() as u64; return vec![0; rx_len]; }
            // full duplex (MOSI and MISO phases together) unless the read phase is the longer one,
            // which is a half-duplex read clocked after the write
            let n = if rx_len > tx.len() { tx.len() + rx_len } else { tx.len() };
            let rx: Vec<u8> = (0..n).map(|i| self.touch.byte(tx.get(i).copied().unwrap_or(0))).collect();
            self.touch_bytes += n as u64;
            self.sync_touch_pins();
            return if rx_len > tx.len() { rx[tx.len()..].to_vec() } else { rx[..rx_len].to_vec() };
        }
        if !self.cs_high && Self::on_panel_wires(&pins) {
            self.spi_bytes += tx.len() as u64;
            for &b in tx { self.panel.byte(b); }
        } else {
            self.spi_dropped += tx.len() as u64;
        }
        vec![0xff; rx_len]   // MISO is wired (GPIO 12) but the controller's read commands are not modelled
    }
    fn gpio_events(&self) -> u64 { self.gpio_events }
    fn input_levels(&self) -> Vec<(u8, bool)> { vec![(PIN_BOOT, true), (PIN_TOUCH_IRQ, self.irq), (PIN_TOUCH_MISO, self.dout)] }
    fn touch(&mut self, x: u16, y: u16, down: bool) {
        self.touch.pen = down.then(|| Self::touch_raw(x, y));
        self.sync_touch_pins();
    }
    fn touch_at(&mut self, cycle: VirtualCycle, x: u16, y: u16, down: bool) { self.cycle = self.cycle.max(cycle); self.touch(x, y, down); }
    fn advance_to(&mut self, cycle: VirtualCycle) { self.cycle = cycle; }
    fn take_edges(&mut self) -> Vec<BoardEdge> { std::mem::take(&mut self.edges) }
    fn named_pin(&self, name: &str) -> Option<u8> {
        match name { "boot" | "btn1" => Some(PIN_BOOT), "ldr" => Some(PIN_LDR), "touch-irq" => Some(PIN_TOUCH_IRQ), _ => None }
    }
    fn display(&self) -> Option<(u32, u32, Vec<u16>, u64)> {
        Some((LCD_COLS as u32, LCD_ROWS as u32, lcd_visible(&self.panel), self.panel.pixels_written))
    }
    fn display_version(&self) -> u64 { self.panel.pixels_written }
    fn display_frames(&self) -> u64 { self.panel.frames }
    fn gram(&self) -> Option<(Vec<u16>, usize, usize)> { Some((self.panel.gram.clone(), self.panel.cols, self.panel.rows)) }
    fn leds(&self) -> Option<(&[[u8; 3]], u64)> { Some((&self.led, self.led_updates)) }
    fn report(&self) -> String {
        let p = &self.panel;
        format!("[emu] cyd-2432s028r: {} RAMWR, {} pixels, madctl={:#x} colmod={:#x} inverted={} on={} backlight={} bbox={:?}; spi {} bytes to the panel, {} dropped (CS high or off GPIO 14/13); rgb led={:?}; touch {} conversions, {} spi bytes, pen={:?}; gpio events {}",
                p.frames, p.pixels_written, p.madctl, p.colmod, p.inverted, p.on, self.backlight, p.bbox(), self.spi_bytes, self.spi_dropped, self.led[0],
                self.touch.conversions, self.touch_bytes, self.touch.pen, self.gpio_events)
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
        let (x, y) = Cyd2432S028R::touch_raw(120, 160);
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
}
