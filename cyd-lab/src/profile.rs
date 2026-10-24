//! Board profiles: what a "variant" of emulated board is.
//!
//! A profile is a TOML file. The defaults in `profiles/` are compiled into the
//! binary with `include_str!` so a fresh install always has them; a user
//! profiles directory (`<data dir>/profiles/*.toml`) is read at start-up and a
//! user file with the same `id` overrides the built-in one.
//!
//! Only a few fields drive the engine today (`chip`, `engine.*`, `flash.size_mb`,
//! `psram_mb`). The rest (pins, controllers, USB chip) is metadata that documents
//! the board in the UI and is there for the engine to consume later.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Profiles shipped inside the binary: (file name, contents).
pub const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("esp32-2432s028r.toml", include_str!("../profiles/esp32-2432s028r.toml")),
    ("ttgo-tdisplay.toml", include_str!("../profiles/ttgo-tdisplay.toml")),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Stable identifier, referenced from each device's `device.toml`.
    pub id: String,
    /// Human-readable name for the UI.
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub vendor: String,
    /// esp32sim `--chip` value: `esp32`, `s3`, `c3`, `c6`.
    pub chip: String,
    /// PSRAM size in MB. 0 = none. Classic ESP32 boards must be 0 (the engine
    /// rejects `--psram-mb` for `--chip esp32`, so it is never passed for it).
    #[serde(default)]
    pub psram_mb: u32,
    pub engine: EngineProfile,
    pub flash: FlashProfile,
    #[serde(default)]
    pub display: Option<DisplayProfile>,
    #[serde(default)]
    pub touch: Option<TouchProfile>,
    #[serde(default)]
    pub rgb_led: Option<RgbLedProfile>,
    #[serde(default)]
    pub ldr: Option<LdrProfile>,
    #[serde(default)]
    pub usb_serial: Option<UsbSerialProfile>,
    #[serde(default)]
    pub buttons: Vec<ButtonProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProfile {
    /// esp32sim `--board` name.
    pub board: String,
    /// Mask ROM ELF file name, resolved against the ROM directory setting.
    pub rom: String,
    /// esp32sim `--console` source (`uart0` for classic ESP32).
    #[serde(default)]
    pub console: Option<String>,
    /// Extra arguments appended verbatim to the engine command line.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashProfile {
    pub size_mb: u32,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub freq_mhz: Option<u32>,
}

impl FlashProfile {
    pub fn size_bytes(&self) -> u64 {
        self.size_mb as u64 * 1024 * 1024
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayProfile {
    pub controller: String,
    pub width: u32,
    pub height: u32,
    /// View rotation (0/90/180/270) given to new devices of this profile.
    #[serde(default)]
    pub default_rotation: u16,
    #[serde(default)]
    pub bus: Option<String>,
    #[serde(default)]
    pub pins: DisplayPins,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayPins {
    pub sck: Option<i32>,
    pub mosi: Option<i32>,
    pub miso: Option<i32>,
    pub cs: Option<i32>,
    pub dc: Option<i32>,
    pub rst: Option<i32>,
    pub backlight: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchProfile {
    pub controller: String,
    #[serde(default)]
    pub bus: Option<String>,
    #[serde(default)]
    pub pins: TouchPins,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchPins {
    pub clk: Option<i32>,
    pub mosi: Option<i32>,
    pub miso: Option<i32>,
    pub cs: Option<i32>,
    pub irq: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RgbLedProfile {
    #[serde(default)]
    pub active_low: bool,
    pub red: i32,
    pub green: i32,
    pub blue: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LdrProfile {
    pub pin: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbSerialProfile {
    pub chip: String,
    #[serde(default)]
    pub vid: Option<u16>,
    #[serde(default)]
    pub pid: Option<u16>,
    /// Board has the usual DTR/RTS -> IO0/EN auto-reset circuit.
    #[serde(default)]
    pub auto_reset: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonProfile {
    pub name: String,
    /// GPIO number, or -1 for a non-GPIO button such as EN.
    pub pin: i32,
}

impl Profile {
    pub fn parse(text: &str) -> Result<Profile> {
        let p: Profile = toml::from_str(text)?;
        p.validate()?;
        Ok(p)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("profile id is empty");
        }
        if !self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("profile id {:?} may only contain letters, digits, '-' and '_'", self.id);
        }
        if !matches!(self.chip.as_str(), "esp32" | "s3" | "esp32s3" | "c3" | "esp32c3" | "c6" | "esp32c6") {
            bail!("profile {}: unknown chip {:?}", self.id, self.chip);
        }
        if self.flash.size_mb == 0 || self.flash.size_mb > 128 {
            bail!("profile {}: flash.size_mb must be 1..=128", self.id);
        }
        if self.is_classic_esp32() && self.psram_mb != 0 {
            // Not fatal for real hardware (WROVER has PSRAM), but the engine cannot
            // model it on the classic chip, so refuse rather than silently drop it.
            bail!("profile {}: esp32sim does not support PSRAM on the classic ESP32 (psram_mb must be 0)", self.id);
        }
        if let Some(d) = &self.display {
            if !matches!(d.default_rotation, 0 | 90 | 180 | 270) {
                bail!("profile {}: display.default_rotation must be 0, 90, 180 or 270", self.id);
            }
        }
        Ok(())
    }

    pub fn is_classic_esp32(&self) -> bool {
        self.chip == "esp32"
    }

    /// Panel size in pixels (native orientation), if the board has a display.
    pub fn panel_size(&self) -> Option<(u32, u32)> {
        self.display.as_ref().map(|d| (d.width, d.height))
    }
}

/// All known profiles, keyed by id, plus any load problems (shown in the UI).
#[derive(Debug, Default)]
pub struct ProfileLibrary {
    pub profiles: BTreeMap<String, Profile>,
    pub errors: Vec<String>,
}

impl ProfileLibrary {
    /// Built-ins first, then `user_dir` (if any) overriding by id.
    pub fn load(user_dir: Option<&Path>) -> ProfileLibrary {
        let mut lib = ProfileLibrary::default();
        for (file, text) in BUILTIN_PROFILES {
            lib.add_text(&format!("built-in {file}"), text);
        }
        if let Some(dir) = user_dir {
            if let Err(e) = lib.load_dir(dir) {
                lib.errors.push(format!("{}: {e:#}", dir.display()));
            }
        }
        lib
    }

    pub fn load_dir(&mut self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            match std::fs::read_to_string(&path) {
                Ok(text) => self.add_text(&path.display().to_string(), &text),
                Err(e) => self.errors.push(format!("{}: {e}", path.display())),
            }
        }
        Ok(())
    }

    fn add_text(&mut self, origin: &str, text: &str) {
        match Profile::parse(text) {
            Ok(p) => {
                self.profiles.insert(p.id.clone(), p);
            }
            Err(e) => self.errors.push(format!("{origin}: {e:#}")),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_parse() {
        let lib = ProfileLibrary::load(None);
        assert!(lib.errors.is_empty(), "{:?}", lib.errors);
        assert_eq!(lib.profiles.len(), BUILTIN_PROFILES.len());
    }

    #[test]
    fn cyd_profile_contents() {
        let lib = ProfileLibrary::load(None);
        let p = lib.get("esp32-2432s028r").expect("CYD profile");
        assert_eq!(p.chip, "esp32");
        assert_eq!(p.engine.board, "cyd-2432s028r");
        assert_eq!(p.engine.rom, "esp32_rev300_rom.elf");
        assert_eq!(p.flash.size_mb, 4);
        assert_eq!(p.flash.size_bytes(), 4 * 1024 * 1024);
        assert_eq!(p.psram_mb, 0);
        let d = p.display.as_ref().unwrap();
        assert_eq!((d.controller.as_str(), d.width, d.height), ("ILI9341", 240, 320));
        assert_eq!(d.pins.sck, Some(14));
        assert_eq!(d.pins.mosi, Some(13));
        assert_eq!(d.pins.miso, Some(12));
        assert_eq!(d.pins.cs, Some(15));
        assert_eq!(d.pins.dc, Some(2));
        assert_eq!(d.pins.backlight, Some(21));
        let t = p.touch.as_ref().unwrap();
        assert_eq!(t.controller, "XPT2046");
        assert_eq!((t.pins.clk, t.pins.mosi, t.pins.miso, t.pins.cs, t.pins.irq), (Some(25), Some(32), Some(39), Some(33), Some(36)));
        let led = p.rgb_led.as_ref().unwrap();
        assert!(led.active_low);
        assert_eq!((led.red, led.green, led.blue), (4, 16, 17));
        assert_eq!(p.ldr.as_ref().unwrap().pin, 34);
        let usb = p.usb_serial.as_ref().unwrap();
        assert_eq!(usb.chip, "CH340");
        assert_eq!((usb.vid, usb.pid), (Some(0x1A86), Some(0x7523)));
    }

    #[test]
    fn tdisplay_profile_uses_tdisplay_board() {
        let lib = ProfileLibrary::load(None);
        let p = lib.get("ttgo-tdisplay").unwrap();
        assert_eq!(p.engine.board, "tdisplay");
        assert_eq!(p.panel_size(), Some((135, 240)));
    }

    const MINIMAL: &str = r#"
        id = "mini"
        name = "Minimal"
        chip = "esp32"
        [engine]
        board = "none"
        rom = "esp32_rev300_rom.elf"
        [flash]
        size_mb = 2
    "#;

    #[test]
    fn minimal_profile_defaults() {
        let p = Profile::parse(MINIMAL).unwrap();
        assert_eq!(p.psram_mb, 0);
        assert!(p.display.is_none() && p.touch.is_none() && p.buttons.is_empty());
        assert_eq!(p.engine.console, None);
    }

    #[test]
    fn rejects_bad_profiles() {
        assert!(Profile::parse(&MINIMAL.replace("size_mb = 2", "size_mb = 0")).is_err());
        assert!(Profile::parse(&MINIMAL.replace("chip = \"esp32\"", "chip = \"esp8266\"")).is_err());
        assert!(Profile::parse(&MINIMAL.replace("id = \"mini\"", "id = \"bad id\"")).is_err());
        assert!(Profile::parse(&format!("psram_mb = 4\n{MINIMAL}")).is_err(), "classic ESP32 + PSRAM must be rejected");
        assert!(Profile::parse(&format!("{MINIMAL}\nunknown_key = 1")).is_err(), "typos must not be silently ignored");
    }

    #[test]
    fn user_dir_overrides_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let text = BUILTIN_PROFILES[1].1.replace("name = \"TTGO T-Display (test)\"", "name = \"Mine\"");
        std::fs::write(dir.path().join("override.toml"), text).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "id = ").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not a profile").unwrap();
        let lib = ProfileLibrary::load(Some(dir.path()));
        assert_eq!(lib.get("ttgo-tdisplay").unwrap().name, "Mine");
        assert_eq!(lib.errors.len(), 1, "{:?}", lib.errors);
    }
}
