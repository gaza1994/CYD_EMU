//! XPT2046 (ADS7846-compatible) 4-wire resistive touch controller, as an SPI slave seen either a
//! byte at a time (a hardware SPI host's transaction) or as software-driven GPIO levels (SPI mode
//! 0: DIN sampled on DCLK rising, DOUT shifted on falling).
//!
//! A control byte is a byte with its start bit (bit 7) set: A2..A0 in bits 6..4 pick the channel
//! (datasheet: 101 X position, 001 Y, 011 Z1, 100 Z2, 000/111 temperature, 010 VBAT, 110 AUX),
//! bit 3 selects 8-bit conversions, bits 1..0 are PD1/PD0. The result comes out on the 16 clocks
//! after the control byte, MSB first behind one busy clock: the 16-bit word is `value << 3`
//! (12-bit) or `value << 7` (8-bit). Drivers overlap the next control byte with the first byte
//! of a result (XPT2046_Touchscreen, TFT_eSPI's `getTouchRawZ`); the model queues each result
//! behind the bytes still pending, and takes a control byte while at most one byte is pending,
//! which is what keeps their pipelines aligned and bounds the queue.
//!
//! PENIRQ is low while the panel is pressed and the last PD bits keep it enabled (PD0 = 0).
use std::collections::VecDeque;

pub struct Xpt2046 {
    /// raw 12-bit X and Y position readings while pressed; `None` is released
    pub pen: Option<(u16, u16)>,
    /// raw Z1 / Z2 pressure readings while pressed (drivers take 4095 + Z1 - Z2 as the pressure)
    pub z: (u16, u16),
    out: VecDeque<u8>,
    pd: u8,
    pub conversions: u64,
    // software SPI
    sclk: bool, mosi: bool, cs: bool, shift: u8, nbits: u8, byte_out: u8, obit: u8, refill: bool,
}

impl Default for Xpt2046 { fn default() -> Self { Self::new() } }

impl Xpt2046 {
    pub fn new() -> Self {
        Xpt2046 { pen: None, z: (600, 3400), out: VecDeque::new(), pd: 0, conversions: 0,
                  sclk: false, mosi: false, cs: true, shift: 0, nbits: 0, byte_out: 0, obit: 0, refill: false }
    }
    /// PENIRQ's level: low (asserted) while pressed with the interrupt enabled.
    pub fn irq(&self) -> bool { !(self.pen.is_some() && self.pd & 1 == 0) }
    fn sample(&self, channel: u8) -> u16 {
        let (x, y, z1, z2) = match self.pen { Some((x, y)) => (x, y, self.z.0, self.z.1), None => (0, 0, 0, 4095) };
        match channel { 5 => x, 1 => y, 3 => z1, 4 => z2, 0 | 7 => 0x2a0, _ => 0 }
    }
    /// Chip select; deselecting aborts any conversion output.
    pub fn select(&mut self, selected: bool) {
        if selected == !self.cs { return; }
        self.cs = !selected;
        self.out.clear(); self.nbits = 0; self.shift = 0; self.obit = 0; self.refill = false; self.byte_out = 0;
    }
    pub fn selected(&self) -> bool { !self.cs }
    fn next_out(&mut self) -> u8 { self.out.pop_front().unwrap_or(0) }
    fn control(&mut self, b: u8) {
        if b & 0x80 == 0 || self.out.len() > 1 { return; }
        let v = self.sample((b >> 4) & 7);
        let word = if b & 0x08 != 0 { (v >> 4) << 7 } else { (v & 0xfff) << 3 };
        self.out.extend(word.to_be_bytes());
        self.pd = b & 3;
        self.conversions += 1;
    }
    /// One byte of a selected transaction: DOUT's byte for these 8 clocks, DIN's byte taken after.
    pub fn byte(&mut self, din: u8) -> u8 { let o = self.next_out(); self.control(din); o }
    /// Software SPI: feed a GPIO output change on DCLK / DIN / CS (`pins`), in order. Returns
    /// DOUT's level after it, which changes on DCLK falling and when CS selects.
    pub fn pin(&mut self, pins: (u8, u8, u8), pin: u8, level: bool) -> bool {
        let (sclk, mosi, cs) = pins;
        if pin == mosi { self.mosi = level; }
        else if pin == cs { self.select(!level); }
        else if pin == sclk && level != self.sclk {
            self.sclk = level;
            if !self.cs && level {
                self.shift = (self.shift << 1) | self.mosi as u8;
                self.nbits += 1;
                if self.nbits == 8 { let b = self.shift; self.nbits = 0; self.shift = 0; self.control(b); self.refill = true; }
            } else if !self.cs {
                if self.refill { self.refill = false; self.byte_out = self.next_out(); }
                self.obit = self.nbits;
            }
        }
        self.dout()
    }
    pub fn dout(&self) -> bool { !self.cs && self.byte_out & (0x80 >> self.obit) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PINS: (u8, u8, u8) = (25, 32, 33);

    /// XPT2046_Touchscreen's `update()`: Z1, Z2, a dummy Y, then Y/X pairs, ending on 0xD0 (PD 00).
    fn arduino_update(t: &mut Xpt2046) -> (u16, u16, i32) {
        let x16 = |t: &mut Xpt2046, c: u8| -> u16 { u16::from_be_bytes([t.byte(c), t.byte(0)]) >> 3 };
        t.select(true);
        t.byte(0xb1);
        let z1 = x16(t, 0xc1) as i32;
        let z = z1 + 4095 - x16(t, 0x91) as i32;
        x16(t, 0x91);
        let d0 = x16(t, 0xd1); let d1 = x16(t, 0x91); let d2 = x16(t, 0xd1); let d3 = x16(t, 0x91);
        let d4 = x16(t, 0xd0); let d5 = x16(t, 0);
        t.select(false);
        assert_eq!((d0, d1), (d2, d3)); assert_eq!((d0, d1), (d4, d5));
        (d0, d1, z)   // the library's x is channel 001, its y channel 101
    }

    #[test]
    fn pipelined_reads_get_each_channel() {
        let mut t = Xpt2046::new();
        assert!(t.irq(), "released: PENIRQ high");
        assert_eq!(arduino_update(&mut t).2, 0, "released: no pressure");
        t.pen = Some((1234, 3000));
        assert!(!t.irq());
        assert_eq!(arduino_update(&mut t), (3000, 1234, 4095 + 600 - 3400));
        // TFT_eSPI getTouchRaw: hi << 5 | lo >> 3 of 0xD0 then 0x90
        t.select(true);
        t.byte(0xd0); let x = (t.byte(0) as u16) << 5 | (t.byte(0x90) as u16) >> 3;
        let y = (t.byte(0) as u16) << 5 | (t.byte(0) as u16) >> 3;
        assert_eq!((x, y), (1234, 3000));
        t.byte(0x93); t.byte(0); t.byte(0);   // PD 11: PENIRQ disabled
        assert!(t.irq());
        t.byte(0x98); assert_eq!(t.byte(0), ((3000u16 >> 4) << 7 >> 8) as u8, "8-bit mode");
        t.select(false); t.select(true);
        assert_eq!(t.byte(0), 0, "CS high aborts the pending output");
        for _ in 0..16 { t.byte(0xff); }
        assert!(t.out.len() <= 3);
    }

    #[test]
    fn software_spi_shifts_dout_on_falling_clock() {
        let mut t = Xpt2046::new();
        t.pen = Some((0xabc, 0x123));
        let xfer = |t: &mut Xpt2046, din: u8| -> u8 {
            let mut r = 0;
            for i in 0..8 {
                t.pin(PINS, 32, din & (0x80 >> i) != 0);
                let level = t.pin(PINS, 25, true);   // the master samples after its rising edge
                r = (r << 1) | level as u8;
                t.pin(PINS, 25, false);
            }
            r
        };
        assert!(!t.pin(PINS, 33, false));
        assert_eq!(xfer(&mut t, 0xd0), 0);
        let v = u16::from_be_bytes([xfer(&mut t, 0), xfer(&mut t, 0)]) >> 3;
        assert_eq!(v, 0xabc);
        assert_eq!(xfer(&mut t, 0x90), 0, "nothing pending");
        assert_eq!(u16::from_be_bytes([xfer(&mut t, 0), xfer(&mut t, 0)]) >> 3, 0x123);
        t.pin(PINS, 33, true);
        assert_eq!(t.conversions, 2);
    }
}
