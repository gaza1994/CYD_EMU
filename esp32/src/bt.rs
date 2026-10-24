//! BT baseband deep-sleep timer of the classic ESP32 (BT core at 0x3ff7_1000, BLE core +0x200).
//! Register meanings from the controller blob's use of them (libbtdm_app, IDF v5.5.6, and ROM
//! r_rwbt_isr 0x40055248): DEEPSLCNTL 0x30 bit 2 starts sleep, bit 15 reads back "asleep", bit 4 is
//! the host's soft wakeup (btdm_wakeup_request sets it only while bit 15 reads 1); DEEPSLWKUP 0x34
//! is the sleep length and DEEPSLSTAT 0x38 the slept length, both in low-power cycles (DPORT
//! 0x3ff0_00d4 divides the 40 MHz crystal). Waking raises INTSTAT 0x10 bit 2 (SLPINT ->
//! r_rwip_wakeup, which unmasks bit 0); bit 0 (CLKNINT -> r_rwip_wakeup_end) is raised at each
//! 625 µs slot boundary while awake. INTCNTL 0x0c enables, INTACK 0x18 clears. Everything else of both cores stays plain register RAM: no radio, no events.
use crate::periph::CPU_HZ;
use esp_periph::{Device, RegRam, WriteEffect};

/// BT core version (read-only): r_ld_reset (0x4003c714) asserts it reads 0x0800_0b00 (ld.c:343),
/// the literal at 0x4003c6f8.
const VERSION: (u32, u32) = (0x04, 0x0800_0b00);
/// RWBTCNTL / RWBLECNTL: master (31) and timing-generator (30) soft resets self-clear when done;
/// r_ld_reset spins on both.
const SOFT_RESETS: u32 = 3 << 30;
const INTCNTL: u32 = 0x0c;
const INTSTAT: u32 = 0x10;
const INTRAWSTAT: u32 = 0x14;
const INTACK: u32 = 0x18;
const DEEPSLCNTL: u32 = 0x30;
const DEEPSLWKUP: u32 = 0x34;
const DEEPSLSTAT: u32 = 0x38;
const DEEP_SLEEP_ON: u32 = 1 << 2;
const SOFT_WAKEUP_REQ: u32 = 1 << 4;
const DEEP_SLEEP_STAT: u32 = 1 << 15;
const CLKNINT: u32 = 1 << 0;
const SLPINT: u32 = 1 << 2;
const XTAL_HZ: u64 = 40_000_000;
const SLOT: u64 = CPU_HZ / 1600;

pub struct BtBaseband {
    ram: RegRam,
    /// Bus cycles (CPU_HZ time base).
    pub now: u64,
    asleep_since: Option<u64>,
    raw: u32,
    /// Slot (625 µs) in which CLKNINT was last raised or acknowledged.
    clkn_slot: u64,
    /// DPORT_BT_LPCK_DIV_NUM + 1.
    pub lp_div: u64,
}

impl Default for BtBaseband {
    fn default() -> Self {
        Self { ram: RegRam::new(), now: 0, asleep_since: None, raw: 0, clkn_slot: 0, lp_div: 80 }
    }
}

impl BtBaseband {
    fn lp_cycles(&self, bus: u64) -> u64 { (bus as u128 * XTAL_HZ as u128 / (self.lp_div as u128 * CPU_HZ as u128)) as u64 }
    fn bus_cycles(&self, lp: u64) -> u64 { (lp as u128 * self.lp_div as u128 * CPU_HZ as u128 / XTAL_HZ as u128) as u64 }
    fn slept(&self) -> u64 { self.asleep_since.map_or(0, |t| self.lp_cycles(self.now - t)) }
    fn wake(&mut self) {
        let slept = self.slept() as u32;
        self.ram.write(DEEPSLSTAT, slept);
        let c = self.ram.read(DEEPSLCNTL);
        self.ram.write(DEEPSLCNTL, c & !(DEEP_SLEEP_ON | SOFT_WAKEUP_REQ));
        self.asleep_since = None;
        self.raw |= SLPINT;
    }
    /// Advance to bus cycle `now`; ends a sleep whose programmed length has run out.
    pub fn advance_to(&mut self, now: u64) {
        self.now = self.now.max(now);
        if self.asleep_since.is_some() && self.slept() >= self.ram.read(DEEPSLWKUP) as u64 {
            self.wake();
        }
        if self.asleep_since.is_none() && self.now / SLOT > self.clkn_slot {
            self.clkn_slot = self.now / SLOT;
            self.raw |= CLKNINT;
        }
    }
    /// Bus cycles until a programmed sleep ends, or until the next unmasked CLKNINT.
    pub fn cycles_until_wake(&self) -> Option<u64> {
        let Some(since) = self.asleep_since else {
            let armed = self.ram.read(INTCNTL) & CLKNINT != 0 && self.raw & CLKNINT == 0;
            return armed.then(|| (self.clkn_slot + 1) * SLOT - self.now.min((self.clkn_slot + 1) * SLOT));
        };
        let end = since + self.bus_cycles(self.ram.read(DEEPSLWKUP) as u64);
        Some(end.saturating_sub(self.now))
    }
}

impl Device for BtBaseband {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            o if o == VERSION.0 => VERSION.1,
            INTSTAT => self.raw & self.ram.read(INTCNTL),
            INTRAWSTAT => self.raw,
            DEEPSLCNTL => self.ram.read(off) | if self.asleep_since.is_some() { DEEP_SLEEP_STAT } else { 0 },
            DEEPSLSTAT if self.asleep_since.is_some() => self.slept() as u32,
            _ => self.ram.read(off),
        }
    }
    fn write(&mut self, off: u32, v: u32) -> WriteEffect {
        match off {
            INTACK => {
                self.raw &= !v;
                if v & CLKNINT != 0 { self.clkn_slot = self.now / SLOT; }
            }
            INTSTAT | INTRAWSTAT => {}
            o if o == VERSION.0 => {}
            0x000 | 0x200 => self.ram.write(off, v & !SOFT_RESETS),
            DEEPSLCNTL => {
                self.ram.write(off, v & !DEEP_SLEEP_STAT);
                if self.asleep_since.is_none() && v & DEEP_SLEEP_ON != 0 {
                    self.asleep_since = Some(self.now);
                } else if self.asleep_since.is_some() && v & SOFT_WAKEUP_REQ != 0 {
                    self.wake();
                }
            }
            _ => self.ram.write(off, v),
        }
        WriteEffect::NONE
    }
    /// RWBT (source 6). The BLE core's own INTSTAT (0x210) is not modelled.
    fn irq_sources(&self) -> u64 { (self.raw & self.ram.read(INTCNTL) != 0) as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = CPU_HZ;

    #[test]
    fn soft_wakeup_ends_sleep_with_slpint_then_clknint() {
        let mut b = BtBaseband::default();
        b.write(INTCNTL, SLPINT);
        b.write(DEEPSLWKUP, 0x4c_465d);
        b.advance_to(S);
        b.write(DEEPSLCNTL, 0x8000_0007);
        assert_ne!(b.read(DEEPSLCNTL) & DEEP_SLEEP_STAT, 0);
        b.advance_to(S + S / 1000); // 1 ms = 500 LP cycles of 2 µs
        assert_eq!(b.read(DEEPSLSTAT), 500);
        assert_eq!(b.irq_sources(), 0);
        let c = b.read(DEEPSLCNTL);
        b.write(DEEPSLCNTL, c | SOFT_WAKEUP_REQ);
        assert_eq!(b.read(DEEPSLCNTL), 0x8000_0003);
        assert_eq!((b.read(INTSTAT), b.read(DEEPSLSTAT), b.irq_sources()), (SLPINT, 500, 1));
        b.write(INTACK, SLPINT | CLKNINT); // r_rwbt_sleep_wakeup clears a stale CLKNINT, unmasks it
        b.write(INTCNTL, SLPINT | CLKNINT);
        assert_eq!(b.irq_sources(), 0);
        let next = b.cycles_until_wake().unwrap();
        assert!(next > 0 && next <= SLOT);
        b.advance_to(b.now + next);
        assert_eq!(b.read(INTSTAT), CLKNINT);
        b.write(INTACK, CLKNINT);
        assert_eq!(b.irq_sources(), 0);
    }

    #[test]
    fn bt_core_version_is_read_only() {
        let mut b = BtBaseband::default();
        b.write(VERSION.0, 0);
        assert_eq!(b.read(VERSION.0), 0x0800_0b00);
        b.write(0, 0x8000_0001);
        assert_eq!(b.read(0), 1, "soft reset completes at once");
    }

    #[test]
    fn programmed_sleep_length_wakes_on_its_own() {
        let mut b = BtBaseband::default();
        b.write(INTCNTL, SLPINT);
        b.write(DEEPSLWKUP, 1000);
        b.write(DEEPSLCNTL, DEEP_SLEEP_ON);
        let left = b.cycles_until_wake().unwrap();
        assert_eq!(left, 1000 * 2 * (S / 1_000_000));
        b.advance_to(left - 2 * (S / 1_000_000));
        assert_eq!(b.irq_sources(), 0);
        b.advance_to(left);
        assert_eq!((b.read(INTSTAT), b.read(DEEPSLSTAT)), (SLPINT, 1000));
        assert_eq!(b.read(DEEPSLCNTL) & (DEEP_SLEEP_ON | DEEP_SLEEP_STAT), 0);
    }
}
