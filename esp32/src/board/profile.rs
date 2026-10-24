//! Board profiles for `GenericDisplay` (`--board generic-display --board-config FILE.toml`, or a
//! built-in by name): an ESP32 behind one SPI display controller, a touch controller and the usual
//! CYD extras. The built-ins in `profiles/` are written in the same format; `docs/board-profiles.md`
//! describes every key. Pins are GPIO numbers; unknown keys are refused, so a typo is an error.
use esp_soc::toml::{self, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I2cTouch { Gt911, Cst820 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Touch {
    None,
    /// raw readings at the glass's first and last pixel: X (channel 101, `0xD0`) along the glass's
    /// x and Y (channel 001, `0x90`) along its y, or the other way round with `swap_xy`
    Xpt2046 { sclk: u8, mosi: u8, miso: u8, cs: u8, irq: u8, raw_x: (i32, i32), raw_y: (i32, i32), swap_xy: bool },
    /// reports glass pixels, optionally swapped or mirrored first
    I2c { chip: I2cTouch, sda: u8, scl: u8, addr: u8, int: Option<u8>, rst: Option<u8>, swap_xy: bool, invert_x: bool, invert_y: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str, pub aliases: Vec<String>, pub chip: String, pub description: String, pub sources: Vec<String>,
    /// `DcsPanel::by_name`; the glass is `width`×`height` of its RAM from `offset`
    pub controller: String, pub width: usize, pub height: usize, pub offset: (usize, usize),
    /// glass column `x` shows RAM column `cols - 1 - x` (row `rows - 1 - y`), each undone by the
    /// controller's SS (GS) scan bit when `scan_control`
    pub mirror_x: bool, pub flip_y: bool, pub scan_control: bool,
    /// the glass's colour order (red and blue exchanged unless MADCTL's BGR bit matches) and
    /// whether it is IPS (colours as sent only with INVON)
    pub bgr: bool, pub ips: bool,
    pub sclk: u8, pub mosi: u8, pub miso: Option<u8>, pub cs: u8, pub dc: u8, pub rst: Option<u8>,
    pub backlight: Option<u8>, pub backlight_high: bool,
    pub touch: Touch,
    pub led: Option<[u8; 3]>, pub led_high: bool,
    pub boot: u8, pub ldr: Option<u8>,
    /// not modelled, listed by the report: TF card (SCLK, MOSI, MISO, CS) and speaker
    pub sd: Option<[u8; 4]>, pub speaker: Option<u8>,
}

pub const BUILTIN: &[&str] = &[
    include_str!("profiles/cyd-2432s028r.toml"), include_str!("profiles/cyd-2432s028r-2usb.toml"),
    include_str!("profiles/cyd-2432s024r.toml"), include_str!("profiles/cyd-2432s024c.toml"),
    include_str!("profiles/cyd-2432s032r.toml"), include_str!("profiles/cyd-2432s032c.toml"),
    include_str!("profiles/cyd-3248s035r.toml"), include_str!("profiles/cyd-3248s035c.toml"),
];

/// A built-in profile by name or alias.
pub fn builtin(name: &str) -> Option<Profile> {
    BUILTIN.iter().map(|t| Profile::parse(t).expect("built-in board profile")).find(|p| p.name == name || p.aliases.iter().any(|a| a == name))
}

struct Keys { t: Vec<(String, Value, bool)> }
impl Keys {
    fn take(&mut self, k: &str) -> Option<Value> { self.t.iter_mut().find(|e| e.0 == k && !e.2).map(|e| { e.2 = true; e.1.clone() }) }
    fn int(&mut self, k: &str) -> Result<Option<i64>, String> { self.take(k).map(|v| v.as_int().ok_or(format!("{k}: expected an integer"))).transpose() }
    fn pin(&mut self, k: &str) -> Result<Option<u8>, String> {
        match self.int(k)? { None => Ok(None), Some(v) if (0..=48).contains(&v) => Ok(Some(v as u8)), Some(v) => Err(format!("{k}: GPIO {v} out of range")) }
    }
    fn need_pin(&mut self, k: &str) -> Result<u8, String> { self.pin(k)?.ok_or(format!("{k} is required")) }
    fn bool(&mut self, k: &str, default: bool) -> Result<bool, String> { self.take(k).map_or(Ok(default), |v| v.as_bool().ok_or(format!("{k}: expected true or false"))) }
    fn str(&mut self, k: &str) -> Result<Option<String>, String> { self.take(k).map(|v| v.as_str().map(str::to_string).ok_or(format!("{k}: expected a string"))).transpose() }
    fn ints(&mut self, k: &str, n: usize) -> Result<Option<Vec<i64>>, String> {
        let Some(v) = self.take(k) else { return Ok(None) };
        let a: Option<Vec<i64>> = v.as_array().and_then(|a| a.iter().map(Value::as_int).collect());
        a.filter(|a| a.len() == n).map(Some).ok_or(format!("{k}: expected {n} integers"))
    }
    fn strs(&mut self, k: &str) -> Result<Vec<String>, String> {
        let Some(v) = self.take(k) else { return Ok(Vec::new()) };
        v.as_array().and_then(|a| a.iter().map(|s| s.as_str().map(str::to_string)).collect()).ok_or(format!("{k}: expected an array of strings"))
    }
    /// `"high"` or `"low"`
    fn active(&mut self, k: &str, default: bool) -> Result<bool, String> {
        match self.str(k)?.as_deref() { None => Ok(default), Some("high") => Ok(true), Some("low") => Ok(false), Some(v) => Err(format!("{k}: expected \"high\" or \"low\", not \"{v}\"")) }
    }
}

impl Profile {
    pub fn parse(text: &str) -> Result<Profile, String> {
        let mut k = Keys { t: toml::parse(text)?.into_iter().map(|(k, v)| (k, v, false)).collect() };
        let name = k.str("name")?.ok_or("name is required")?;
        let controller = k.str("display.controller")?.ok_or("display.controller is required")?;
        let panel = esp_soc::devices::DcsPanel::by_name(&controller).ok_or(format!("display.controller: unknown '{controller}' (st7735, st7789, ili9341, ili9342, st7796, ili9488, gc9a01)"))?;
        let ram = (panel.cols, panel.rows);
        let offset = k.ints("display.offset", 2)?.map_or((0, 0), |o| (o[0] as usize, o[1] as usize));
        let width = k.int("display.width")?.map_or(ram.0, |v| v as usize);
        let height = k.int("display.height")?.map_or(ram.1, |v| v as usize);
        if width == 0 || height == 0 || offset.0 + width > ram.0 || offset.1 + height > ram.1 { return Err(format!("display: {width}x{height} at {offset:?} is outside {controller}'s {}x{} RAM", ram.0, ram.1)); }
        let range = |k: &mut Keys, key: &str, default: (i32, i32)| -> Result<(i32, i32), String> {
            Ok(k.ints(key, 2)?.map_or(default, |r| (r[0] as i32, r[1] as i32)))
        };
        let touch = match k.str("touch.controller")?.as_deref() {
            None | Some("none") => Touch::None,
            Some("xpt2046") => Touch::Xpt2046 {
                sclk: k.need_pin("touch.sclk")?, mosi: k.need_pin("touch.mosi")?, miso: k.need_pin("touch.miso")?, cs: k.need_pin("touch.cs")?, irq: k.need_pin("touch.irq")?,
                raw_x: range(&mut k, "touch.raw_x", (3800, 240))?, raw_y: range(&mut k, "touch.raw_y", (200, 3700))?, swap_xy: k.bool("touch.swap_xy", false)?,
            },
            Some(c @ ("gt911" | "cst820" | "cst816s")) => {
                let chip = if c == "gt911" { I2cTouch::Gt911 } else { I2cTouch::Cst820 };
                Touch::I2c { chip, sda: k.need_pin("touch.sda")?, scl: k.need_pin("touch.scl")?, addr: k.int("touch.address")?.unwrap_or(if chip == I2cTouch::Gt911 { 0x5d } else { 0x15 }) as u8,
                             int: k.pin("touch.int")?, rst: k.pin("touch.rst")?, swap_xy: k.bool("touch.swap_xy", false)?, invert_x: k.bool("touch.invert_x", false)?, invert_y: k.bool("touch.invert_y", false)? }
            }
            Some(c) => return Err(format!("touch.controller: unknown '{c}' (xpt2046, gt911, cst820, cst816s, none)")),
        };
        let led = k.ints("led.pins", 3)?.map(|p| p.iter().map(|&v| v as u8).collect::<Vec<_>>().try_into().unwrap());
        let sd = k.ints("board.sd", 4)?.map(|p| p.iter().map(|&v| v as u8).collect::<Vec<_>>().try_into().unwrap());
        let p = Profile {
            name: Box::leak(name.into_boxed_str()), aliases: k.strs("aliases")?, chip: k.str("chip")?.unwrap_or("esp32".into()),
            description: k.str("description")?.unwrap_or_default(), sources: k.strs("sources")?,
            controller, width, height, offset,
            mirror_x: k.bool("display.mirror_x", false)?, flip_y: k.bool("display.flip_y", false)?, scan_control: k.bool("display.scan_control", false)?,
            bgr: k.bool("display.bgr", false)?, ips: k.bool("display.ips", false)?,
            sclk: k.need_pin("display.sclk")?, mosi: k.need_pin("display.mosi")?, miso: k.pin("display.miso")?, cs: k.need_pin("display.cs")?, dc: k.need_pin("display.dc")?, rst: k.pin("display.rst")?,
            backlight: k.pin("display.backlight")?, backlight_high: k.active("display.backlight_active", true)?,
            touch, led, led_high: k.active("led.active", false)?,
            boot: k.pin("board.boot")?.unwrap_or(0), ldr: k.pin("board.ldr")?, sd, speaker: k.pin("board.speaker")?,
        };
        if let Some(e) = k.t.iter().find(|e| !e.2) { return Err(format!("unknown key {}", e.0)); }
        Ok(p)
    }
    pub fn from_file(path: &str) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Profile::parse(&text).map_err(|e| format!("{path}: {e}"))
    }
    /// One JSON object for `--list-boards`: what a front end needs to offer and draw the board.
    pub fn to_json(&self) -> String {
        let q = |s: &str| format!("\"{}\"", s.chars().flat_map(|c| match c { '"' | '\\' => vec!['\\', c], c if (c as u32) < 32 => format!("\\u{:04x}", c as u32).chars().collect(), c => vec![c] }).collect::<String>());
        let list = |v: &[String]| format!("[{}]", v.iter().map(|s| q(s)).collect::<Vec<_>>().join(","));
        let touch = match &self.touch { Touch::None => "none", Touch::Xpt2046 { .. } => "xpt2046", Touch::I2c { chip: I2cTouch::Gt911, .. } => "gt911", Touch::I2c { chip: I2cTouch::Cst820, .. } => "cst816" };
        format!("{{\"name\":{},\"aliases\":{},\"chip\":{},\"description\":{},\"controller\":{},\"width\":{},\"height\":{},\"touch\":\"{touch}\",\"rgb_led\":{},\"sources\":{}}}",
                q(self.name), list(&self.aliases), q(&self.chip), q(&self.description), q(&self.controller), self.width, self.height, self.led.is_some(), list(&self.sources))
    }
    /// Whether the touch controller shares the panel's SCLK and MOSI (the 2.4", 3.2" and 3.5" boards).
    pub fn shared_touch_bus(&self) -> bool { matches!(self.touch, Touch::Xpt2046 { sclk, mosi, .. } if sclk == self.sclk && mosi == self.mosi) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses_and_names_are_unique() {
        let all: Vec<Profile> = BUILTIN.iter().map(|t| Profile::parse(t).unwrap()).collect();
        let mut names: Vec<&str> = all.iter().flat_map(|p| std::iter::once(p.name).chain(p.aliases.iter().map(String::as_str))).collect();
        let n = names.len();
        names.sort(); names.dedup();
        assert_eq!(names.len(), n, "{names:?}");
        assert!(all.iter().all(|p| !p.sources.is_empty() && p.chip == "esp32"));
        let cyd = builtin("cyd").unwrap();
        assert_eq!((cyd.name, cyd.controller.as_str(), cyd.width, cyd.height, cyd.mirror_x, cyd.scan_control, cyd.bgr), ("cyd-2432s028r", "ili9341", 240, 320, true, true, true));
        assert!(!cyd.shared_touch_bus() && builtin("cyd-2432s024r").unwrap().shared_touch_bus());
        assert_eq!(builtin("cyd-2usb").unwrap().touch, Touch::Xpt2046 { sclk: 25, mosi: 32, miso: 39, cs: 33, irq: 36, raw_x: (282, 3835), raw_y: (208, 3823), swap_xy: true });
    }

    #[test]
    fn mistakes_are_named() {
        let base = "name = \"x\"\n[display]\ncontroller = \"st7789\"\nsclk = 18\nmosi = 19\ncs = 5\ndc = 16\n";
        assert_eq!(Profile::parse(base).unwrap().width, 240);
        for (extra, err) in [("colour = 1\n", "unknown key display.colour"), ("width = 300\n", "outside st7789's 240x320 RAM"), ("backlight = 99\n", "GPIO 99 out of range"),
                             ("backlight_active = \"on\"\n", "expected \"high\" or \"low\""), ("[touch]\ncontroller = \"ft6236\"\n", "unknown 'ft6236'"), ("[touch]\ncontroller = \"gt911\"\n", "touch.sda is required")] {
            let e = Profile::parse(&format!("{base}{extra}")).unwrap_err();
            assert!(e.contains(err), "{extra}: {e}");
        }
        assert!(Profile::parse("name = \"x\"\n[display]\ncontroller = \"ssd1306\"\n").unwrap_err().contains("unknown 'ssd1306'"));
    }
}
