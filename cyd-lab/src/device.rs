//! Device store: one folder per emulated board.
//!
//! ```text
//! <data dir>/devices/<id>/
//!     device.toml   name, profile id, COM port, MAC, notes, view rotation
//!     flash.bin     the whole SPI flash image (profile size, starts all 0xFF)
//!     screen.png    last --tft-png snapshot written by the engine (optional)
//! ```
//!
//! A new device is a "just unboxed" board: `flash.bin` is filled with 0xFF, which
//! is what erased NOR flash reads as. The ROM bootloader then finds no valid image
//! and sits waiting, exactly like a real blank module.
//!
//! The engine reads `flash.bin` with `--flash-image` at power-on. Until it can
//! write the image back (see `engine::Capabilities::flash_writeback`), anything
//! the firmware writes to flash at run time is lost on power-off; the only ways
//! to change `flash.bin` are "Erase flash" and "Import firmware at offset" here.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::profile::Profile;

pub const DEVICE_FILE: &str = "device.toml";
pub const FLASH_FILE: &str = "flash.bin";
pub const SCREEN_FILE: &str = "screen.png";
/// First COM port number handed out. COM1..COM9 are commonly taken by real
/// hardware, and com0com pairs are usually created higher up.
pub const FIRST_COM_PORT: u32 = 20;
/// First TCP port handed out for the engine's serial bridge (`--serial-tcp`, RFC 2217).
/// 2217 is RFC 2217's own number; ten times it stays clear of the --web range (18320+).
pub const FIRST_SERIAL_TCP_PORT: u16 = 22170;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceMeta {
    pub name: String,
    /// Profile id (see `profile.rs`).
    pub profile: String,
    /// The COM port this device should appear on once the serial bridge exists
    /// (e.g. "COM20"). Metadata only for now.
    #[serde(default)]
    pub com_port: Option<String>,
    /// Factory MAC (eFuse) for the emulated chip, passed as `--mac`.
    #[serde(default)]
    pub mac: Option<String>,
    /// TCP port for the engine's RFC 2217 serial bridge (`--serial-tcp PORT`).
    /// `None` = pick a free port at power-on.
    #[serde(default)]
    pub serial_tcp_port: Option<u16>,
    #[serde(default)]
    pub notes: String,
    /// View rotation in degrees (0/90/180/270) applied to the screen in the UI.
    #[serde(default)]
    pub view_rotation: u16,
    /// Unix seconds.
    #[serde(default)]
    pub created: u64,
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Folder name; stable, never changes on rename.
    pub id: String,
    pub dir: PathBuf,
    pub meta: DeviceMeta,
}

impl Device {
    pub fn flash_path(&self) -> PathBuf {
        self.dir.join(FLASH_FILE)
    }
    pub fn screen_path(&self) -> PathBuf {
        self.dir.join(SCREEN_FILE)
    }
    pub fn flash_size(&self) -> Result<u64> {
        Ok(fs::metadata(self.flash_path())?.len())
    }
    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&self.meta)?;
        write_atomic(&self.dir.join(DEVICE_FILE), text.as_bytes())
    }
}

pub struct DeviceStore {
    pub root: PathBuf,
}

impl DeviceStore {
    /// `root` is the `devices/` folder.
    pub fn open(root: impl Into<PathBuf>) -> Result<DeviceStore> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(DeviceStore { root })
    }

    /// All devices, oldest first. Folders without a readable `device.toml` are skipped
    /// and reported in the second element.
    pub fn list(&self) -> Result<(Vec<Device>, Vec<String>)> {
        let mut devices = Vec::new();
        let mut errors = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            match self.load(&id) {
                Ok(d) => devices.push(d),
                Err(e) => errors.push(format!("{id}: {e:#}")),
            }
        }
        devices.sort_by(|a, b| a.meta.created.cmp(&b.meta.created).then(a.id.cmp(&b.id)));
        Ok((devices, errors))
    }

    pub fn load(&self, id: &str) -> Result<Device> {
        let dir = self.root.join(id);
        let text = fs::read_to_string(dir.join(DEVICE_FILE)).with_context(|| format!("reading {}", dir.join(DEVICE_FILE).display()))?;
        let meta: DeviceMeta = toml::from_str(&text)?;
        Ok(Device { id: id.to_string(), dir, meta })
    }

    /// Create a blank board of `profile`: folder, `device.toml`, `flash.bin` all 0xFF.
    pub fn create(&self, name: &str, profile: &Profile) -> Result<Device> {
        let name = name.trim();
        if name.is_empty() {
            bail!("device name is empty");
        }
        let (existing, _) = self.list()?;
        let id = unique_id(&slug(name), |c| self.root.join(c).exists());
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir)?;
        let used: Vec<String> = existing.iter().filter_map(|d| d.meta.com_port.clone()).collect();
        let now = now_secs();
        let meta = DeviceMeta {
            name: name.to_string(),
            profile: profile.id.clone(),
            com_port: Some(next_com_port(&used)),
            mac: Some(generate_mac(&id, now)),
            serial_tcp_port: Some(next_serial_tcp_port(&existing.iter().filter_map(|d| d.meta.serial_tcp_port).collect::<Vec<_>>())),
            notes: String::new(),
            view_rotation: profile.display.as_ref().map(|d| d.default_rotation).unwrap_or(0),
            created: now,
        };
        let dev = Device { id, dir, meta };
        write_blank_flash(&dev.flash_path(), profile.flash.size_bytes())?;
        dev.save()?;
        Ok(dev)
    }

    pub fn rename(&self, dev: &mut Device, new_name: &str) -> Result<()> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            bail!("device name is empty");
        }
        dev.meta.name = new_name.to_string();
        dev.save()
    }

    pub fn update_meta(&self, dev: &Device) -> Result<()> {
        dev.save()
    }

    /// Remove the device folder and everything in it. The caller asks for confirmation.
    pub fn delete(&self, dev: &Device) -> Result<()> {
        // Refuse to delete anything that is not directly inside our root.
        if dev.dir.parent() != Some(self.root.as_path()) {
            bail!("refusing to delete {} (not inside {})", dev.dir.display(), self.root.display());
        }
        fs::remove_dir_all(&dev.dir).with_context(|| format!("deleting {}", dev.dir.display()))
    }

    /// Reset the whole flash to 0xFF, keeping its size.
    pub fn erase_flash(&self, dev: &Device) -> Result<()> {
        let size = dev.flash_size().unwrap_or(0);
        if size == 0 {
            bail!("flash.bin missing or empty");
        }
        write_blank_flash(&dev.flash_path(), size)
    }

    /// Write `data` into `flash.bin` at `offset` (like `esptool write_flash OFFSET file`).
    /// Bytes outside the written range are untouched. Fails if it does not fit.
    pub fn import_firmware(&self, dev: &Device, offset: u64, data: &[u8]) -> Result<()> {
        let size = dev.flash_size()?;
        let end = offset.checked_add(data.len() as u64).ok_or_else(|| anyhow!("offset overflow"))?;
        if end > size {
            bail!(
                "image of {} bytes at 0x{offset:x} ends at 0x{end:x}, past the end of the {} MB flash (0x{size:x})",
                data.len(),
                size / (1024 * 1024)
            );
        }
        let mut f = OpenOptions::new().write(true).open(dev.flash_path())?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    }

    pub fn import_firmware_file(&self, dev: &Device, offset: u64, file: &Path) -> Result<()> {
        let data = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        self.import_firmware(dev, offset, &data)
    }
}

/// Parse "0x10000", "10000h", "65536" style offsets. Hex needs the 0x prefix or h suffix.
pub fn parse_offset(s: &str) -> Result<u64> {
    let s = s.trim().replace('_', "");
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else if let Some(h) = s.strip_suffix('h').or_else(|| s.strip_suffix('H')) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse::<u64>()
    };
    v.map_err(|_| anyhow!("bad offset {s:?} (use e.g. 0x10000)"))
}

fn write_blank_flash(path: &Path, size: u64) -> Result<()> {
    // 4-16 MB: just build it in memory.
    let buf = vec![0xFFu8; size as usize];
    write_atomic(path, &buf)
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "device".into() } else { out.chars().take(32).collect() }
}

fn unique_id(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !exists(c)).unwrap()
}

fn next_com_port(used: &[String]) -> String {
    (FIRST_COM_PORT..)
        .map(|n| format!("COM{n}"))
        .find(|c| !used.iter().any(|u| u.eq_ignore_ascii_case(c)))
        .unwrap()
}

/// The lowest port at or above [`FIRST_SERIAL_TCP_PORT`] not used by another device.
pub fn next_serial_tcp_port(used: &[u16]) -> u16 {
    (FIRST_SERIAL_TCP_PORT..u16::MAX).find(|p| !used.contains(p)).unwrap_or(FIRST_SERIAL_TCP_PORT)
}

/// Espressif OUI (24:0A:C4) + 3 bytes derived from the id and creation time.
fn generate_mac(id: &str, now: u64) -> String {
    // FNV-1a: deterministic, no extra dependency, good enough for uniqueness on one PC.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes().chain(now.to_le_bytes()).chain(std::process::id().to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("24:0A:C4:{:02X}:{:02X}:{:02X}", (h >> 16) as u8, (h >> 8) as u8, h as u8)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileLibrary;

    fn store() -> (tempfile::TempDir, DeviceStore, Profile) {
        let tmp = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(tmp.path().join("devices")).unwrap();
        let profile = ProfileLibrary::load(None).get("esp32-2432s028r").unwrap().clone();
        (tmp, store, profile)
    }

    #[test]
    fn create_makes_blank_flash_of_profile_size() {
        let (_t, store, profile) = store();
        let dev = store.create("Kitchen CYD", &profile).unwrap();
        assert_eq!(dev.id, "kitchen-cyd");
        let flash = fs::read(dev.flash_path()).unwrap();
        assert_eq!(flash.len(), 4 * 1024 * 1024);
        assert!(flash.iter().all(|&b| b == 0xFF), "new flash must be erased (0xFF)");
        let loaded = store.load(&dev.id).unwrap();
        assert_eq!(loaded.meta, dev.meta);
        assert_eq!(loaded.meta.profile, "esp32-2432s028r");
        assert_eq!(loaded.meta.com_port.as_deref(), Some("COM20"));
        assert!(loaded.meta.mac.as_deref().unwrap().starts_with("24:0A:C4:"));
    }

    #[test]
    fn ids_and_com_ports_are_unique() {
        let (_t, store, profile) = store();
        let a = store.create("Board", &profile).unwrap();
        let b = store.create("Board", &profile).unwrap();
        assert_eq!(a.id, "board");
        assert_eq!(b.id, "board-2");
        assert_ne!(a.meta.com_port, b.meta.com_port);
        assert_eq!(a.meta.serial_tcp_port, Some(FIRST_SERIAL_TCP_PORT));
        assert_eq!(b.meta.serial_tcp_port, Some(FIRST_SERIAL_TCP_PORT + 1));
        let (all, errs) = store.list().unwrap();
        assert!(errs.is_empty());
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn rename_and_delete() {
        let (_t, store, profile) = store();
        let mut dev = store.create("One", &profile).unwrap();
        store.rename(&mut dev, "  Two ").unwrap();
        assert_eq!(store.load(&dev.id).unwrap().meta.name, "Two");
        assert!(store.rename(&mut dev, "   ").is_err());
        store.delete(&dev).unwrap();
        assert!(!dev.dir.exists());
        assert!(store.list().unwrap().0.is_empty());
    }

    #[test]
    fn import_firmware_at_offset() {
        let (_t, store, profile) = store();
        let dev = store.create("fw", &profile).unwrap();
        let app = [0xE9u8, 0x03, 0x02, 0x20, 0xAA, 0x55];
        store.import_firmware(&dev, 0x10000, &app).unwrap();
        let flash = fs::read(dev.flash_path()).unwrap();
        assert_eq!(flash.len(), 4 * 1024 * 1024, "size unchanged");
        assert_eq!(&flash[0x10000..0x10000 + app.len()], &app);
        assert!(flash[..0x10000].iter().all(|&b| b == 0xFF));
        assert!(flash[0x10000 + app.len()..].iter().all(|&b| b == 0xFF));

        // merged image at 0x0 via a file
        let bin = dev.dir.join("merged.bin");
        fs::write(&bin, [1u8, 2, 3]).unwrap();
        store.import_firmware_file(&dev, 0, &bin).unwrap();
        let flash = fs::read(dev.flash_path()).unwrap();
        assert_eq!(&flash[..3], &[1, 2, 3]);
        assert_eq!(&flash[0x10000..0x10000 + app.len()], &app, "earlier write kept");

        // does not fit
        assert!(store.import_firmware(&dev, 4 * 1024 * 1024 - 2, &app).is_err());
        // exactly fits at the end
        store.import_firmware(&dev, 4 * 1024 * 1024 - 2, &app[..2]).unwrap();

        store.erase_flash(&dev).unwrap();
        let flash = fs::read(dev.flash_path()).unwrap();
        assert_eq!(flash.len(), 4 * 1024 * 1024);
        assert!(flash.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset("0x10000").unwrap(), 0x10000);
        assert_eq!(parse_offset("0X1_0000").unwrap(), 0x10000);
        assert_eq!(parse_offset("10000h").unwrap(), 0x10000);
        assert_eq!(parse_offset("4096").unwrap(), 4096);
        assert_eq!(parse_offset(" 0 ").unwrap(), 0);
        assert!(parse_offset("0xZZ").is_err());
        assert!(parse_offset("").is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("My  CYD #1!"), "my-cyd-1");
        assert_eq!(slug("!!!"), "device");
    }
}
