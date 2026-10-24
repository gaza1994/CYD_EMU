//! Where does a dropped `.bin` go? Heuristics for classic-ESP32 images.
//!
//! * merged / factory image (esptool `merge_bin`, ESPHome `*.factory.bin`, Arduino
//!   `*.merged.bin`): starts at 0x0, 0x1000 bytes of padding, bootloader (magic 0xE9) at
//!   0x1000 and the partition table (0xAA 0x50) at 0x8000 → offset 0x0.
//! * the same merged image built with `--target-offset 0x1000` (MicroPython's
//!   ESP32_GENERIC .bin): bootloader at 0x0 of the file, partition table at 0x7000 → 0x1000.
//! * app image (Arduino `*.ino.bin`, PlatformIO `firmware.bin`, ESPHome `*.ota.bin`): 0xE9 and
//!   the app descriptor magic 0xABCD5432 right after the first segment header (file offset
//!   0x20) → the first app partition of the device's partition table (0x10000 by default).
//! * bootloader alone (0xE9, no app descriptor, small) → 0x1000.
//! * partition table alone (0xAA 0x50 at 0) → 0x8000.
//! * anything else → ask.
//!
//! An app image (ESPHome's "OTA format", formerly "legacy") is never written at 0x0, and on
//! a board with no partition table it is refused with an explanation: the like-for-like
//! file is the factory/merged image. "Install anyway" ([`stock_layout`]) adds a stock
//! ESP-IDF bootloader and ESPHome's default 4 MB partition table around it.

pub const IMAGE_MAGIC: u8 = 0xE9;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;

#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Confident: write at this offset.
    At(u64, &'static str),
    /// Probably this offset, but the user should confirm.
    Ask(u64, String),
    /// An app-only image (OTA format) and the device already has a partition table: offer
    /// to write it into the first app partition and reset `otadata` so it is what boots.
    AppIntoPartition { offset: u64, otadata: Option<(u64, u64)> },
    /// An app-only image and the device's flash has no partition table (a blank board):
    /// refused, with the advice to use the factory format. `install_anyway`: it fits the
    /// stock layout, so [`stock_layout`] can be offered.
    AppOnBlank { install_anyway: bool },
}

/// Stock ESP-IDF v5.5.6 bootloader, as ESPHome 2026.10.0 builds it for esp32dev (4 MB, DIO).
pub const STOCK_BOOTLOADER: &[u8] = include_bytes!("../assets/esp32-stock/bootloader.bin");
/// ESPHome's default 4 MB partition table (app0/ota_0 at 0x10000, 0x1C0000 bytes each).
pub const STOCK_PARTITIONS_4MB: &[u8] = include_bytes!("../assets/esp32-stock/partitions-4mb.bin");
/// Size of each app slot in [`STOCK_PARTITIONS_4MB`].
pub const STOCK_APP_SLOT: usize = 0x1C_0000;

/// Is `file` an ESP-IDF application image (0xE9 header, app descriptor at 0x20)?
#[allow(dead_code)]
pub fn is_app_image(file: &[u8]) -> bool {
    is_image(file, 0) && u32le(file, 0x20) == Some(APP_DESC_MAGIC)
}

/// The parts for "Install anyway": stock bootloader at 0x1000, ESPHome's default 4 MB
/// partition table at 0x8000, a blank otadata (so ota_0 boots) and the app at 0x10000.
pub fn stock_layout(app: &[u8]) -> Vec<(u64, Vec<u8>, String)> {
    let (ota_off, ota_len) = otadata_range(STOCK_PARTITIONS_4MB, 0).unwrap_or((0x9000, 0x2000));
    vec![
        (0x1000, STOCK_BOOTLOADER.to_vec(), "stock ESP-IDF bootloader".into()),
        (0x8000, STOCK_PARTITIONS_4MB.to_vec(), "ESPHome default 4 MB partition table".into()),
        (ota_off, vec![0xFF; ota_len as usize], "otadata (blank: boot app0)".into()),
        (0x10000, app.to_vec(), "app".into()),
    ]
}

/// The `otadata` partition (type 1, subtype 0) of a partition table found at `base` in `d`.
pub fn otadata_range(d: &[u8], base: usize) -> Option<(u64, u64)> {
    for k in 0..95 {
        let e = base + k * 32;
        if !is_pt(d, e) {
            break;
        }
        if d[e + 2] == 1 && d[e + 3] == 0 {
            return Some((u32le(d, e + 4)? as u64, u32le(d, e + 8)? as u64));
        }
    }
    None
}

fn u32le(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn is_pt(d: &[u8], at: usize) -> bool {
    d.get(at..at + 2) == Some(&[0xAA, 0x50])
}

fn is_image(d: &[u8], at: usize) -> bool {
    d.get(at) == Some(&IMAGE_MAGIC)
}

/// The chip an image header says it is for (extended header, byte 12): 0 = ESP32.
pub fn chip_id(d: &[u8], at: usize) -> Option<u16> {
    is_image(d, at).then(|| d.get(at + 12..at + 14).map(|b| u16::from_le_bytes([b[0], b[1]]))).flatten()
}

fn chip_name(id: u16) -> &'static str {
    match id {
        0 => "ESP32",
        2 => "ESP32-S2",
        5 => "ESP32-C3",
        9 => "ESP32-S3",
        12 => "ESP32-C2",
        13 => "ESP32-C6",
        16 => "ESP32-H2",
        _ => "another chip",
    }
}

/// First app partition (factory, else the lowest ota_N) in a partition table at 0x8000 of
/// `flash`, if there is a valid table.
pub fn first_app_offset(flash: &[u8]) -> Option<u64> {
    let mut best: Option<(bool, u32)> = None;
    for k in 0..95 {
        let e = 0x8000 + k * 32;
        if !is_pt(flash, e) {
            break;
        }
        let (ty, sub, off) = (flash[e + 2], flash[e + 3], u32le(flash, e + 4)?);
        if ty != 0 {
            continue;
        }
        let factory = sub == 0;
        best = match best {
            Some((true, _)) => best,
            Some((false, o)) if !factory && o <= off => best,
            _ => Some((factory, off)),
        };
    }
    best.map(|(_, o)| o as u64)
}

/// Decide where `file` goes on a classic ESP32 whose current flash is `flash`.
pub fn place(file: &[u8], flash: &[u8]) -> Placement {
    let wrong_chip = |at: usize| chip_id(file, at).filter(|c| *c != 0).map(chip_name);
    if file.len() > 0x8000 && is_image(file, 0x1000) && is_pt(file, 0x8000) {
        if let Some(c) = wrong_chip(0x1000) {
            return Placement::Ask(0, format!("This is a merged image for {c}, not a classic ESP32: it will not boot on a CYD."));
        }
        return Placement::At(0, "merged/factory image (bootloader at 0x1000, partitions at 0x8000)");
    }
    if file.len() > 0x7000 && is_image(file, 0) && is_pt(file, 0x7000) {
        return Placement::At(0x1000, "merged image starting at the bootloader (e.g. MicroPython)");
    }
    if is_image(file, 0) {
        if let Some(c) = wrong_chip(0) {
            return Placement::Ask(0x10000, format!("This image is for {c}, not a classic ESP32: it will not boot on a CYD."));
        }
        if u32le(file, 0x20) == Some(APP_DESC_MAGIC) {
            return match first_app_offset(flash) {
                Some(offset) => Placement::AppIntoPartition { offset, otadata: otadata_range(flash, 0x8000) },
                None => Placement::AppOnBlank { install_anyway: file.len() <= STOCK_APP_SLOT },
            };
        }
        if file.len() <= 0x7000 {
            return Placement::At(0x1000, "bootloader");
        }
        return Placement::Ask(0x10000, "An ESP32 image without an app descriptor: probably an app.".into());
    }
    if is_pt(file, 0) && file.len() <= 0x1000 {
        return Placement::At(0x8000, "partition table");
    }
    Placement::Ask(0, "Not a recognised ESP32 image (no 0xE9 header where expected).".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(chip: u16, app: bool, len: usize) -> Vec<u8> {
        let mut d = vec![0u8; len];
        d[0] = 0xE9;
        d[1] = 4;
        d[12..14].copy_from_slice(&chip.to_le_bytes());
        if app {
            d[0x20..0x24].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        }
        d
    }

    fn pt(entries: &[(u8, u8, u32)]) -> Vec<u8> {
        let mut d = vec![0xFFu8; 0xC00];
        for (k, (ty, sub, off)) in entries.iter().enumerate() {
            let e = k * 32;
            d[e] = 0xAA;
            d[e + 1] = 0x50;
            d[e + 2] = *ty;
            d[e + 3] = *sub;
            d[e + 4..e + 8].copy_from_slice(&off.to_le_bytes());
        }
        d
    }

    fn merged(base: usize, chip: u16) -> Vec<u8> {
        let mut f = vec![0xFFu8; 0x20000 - base];
        let bl = image(chip, false, 0x5000);
        f[0x1000 - base..0x6000 - base].copy_from_slice(&bl);
        let t = pt(&[(0, 0x10, 0x10000)]);
        f[0x8000 - base..0x8000 - base + t.len()].copy_from_slice(&t);
        f
    }

    fn flash_with(table: &[(u8, u8, u32)]) -> Vec<u8> {
        let mut f = vec![0xFFu8; 0x10000];
        let t = pt(table);
        f[0x8000..0x8000 + t.len()].copy_from_slice(&t);
        f
    }

    #[test]
    fn merged_images() {
        assert!(matches!(place(&merged(0, 0), &[]), Placement::At(0, _)));
        assert!(matches!(place(&merged(0x1000, 0), &[]), Placement::At(0x1000, _)));
        assert!(matches!(place(&merged(0, 9), &[]), Placement::Ask(0, m) if m.contains("ESP32-S3")));
    }

    #[test]
    fn app_images_follow_the_partition_table() {
        let app = image(0, true, 0x40000);
        let blank = vec![0xFFu8; 0x10000];
        assert_eq!(place(&app, &blank), Placement::AppOnBlank { install_anyway: true });
        assert_eq!(place(&image(0, true, 0x1C_0001), &blank), Placement::AppOnBlank { install_anyway: false });
        // ota_0 at 0x10000, ota_1 at 0x150000, otadata at 0xe000
        assert!(matches!(
            place(&app, &flash_with(&[(1, 2, 0x9000), (1, 0, 0xe000), (0, 0x10, 0x10000), (0, 0x11, 0x150000)])),
            Placement::AppIntoPartition { offset: 0x10000, otadata: Some((0xe000, _)) }
        ));
        // factory wins over ota
        assert!(matches!(place(&app, &flash_with(&[(0, 0x10, 0x10000), (0, 0, 0x200000)])), Placement::AppIntoPartition { offset: 0x200000, otadata: None }));
        assert!(matches!(place(&image(5, true, 0x40000), &blank), Placement::Ask(_, m) if m.contains("ESP32-C3")));
    }

    /// The bundled stock parts are what they claim: an ESP32 bootloader and a table whose
    /// first app slot is ota_0 at 0x10000 with blank-able otadata at 0x9000.
    #[test]
    fn stock_layout_is_esphome_default() {
        assert!(is_image(STOCK_BOOTLOADER, 0) && chip_id(STOCK_BOOTLOADER, 0) == Some(0));
        let mut flash = vec![0xFFu8; 0x9000];
        flash[0x8000..0x8000 + STOCK_PARTITIONS_4MB.len()].copy_from_slice(STOCK_PARTITIONS_4MB);
        assert_eq!(first_app_offset(&flash), Some(0x10000));
        assert_eq!(otadata_range(STOCK_PARTITIONS_4MB, 0), Some((0x9000, 0x2000)));
        let parts = stock_layout(&image(0, true, 0x1000));
        let offs: Vec<u64> = parts.iter().map(|p| p.0).collect();
        assert_eq!(offs, vec![0x1000, 0x8000, 0x9000, 0x10000]);
        assert!(parts[2].1.iter().all(|b| *b == 0xFF) && parts[2].1.len() == 0x2000);
    }

    /// The real ESPHome "OTA format" download from the first Windows test, when present.
    #[test]
    fn esphome_ota_download_is_refused_on_blank() {
        let Ok(d) = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../gareth-esphome.bin")) else { return };
        assert!(is_app_image(&d));
        assert_eq!(place(&d, &vec![0xFFu8; 0x9000]), Placement::AppOnBlank { install_anyway: true });
    }

    #[test]
    fn small_parts_and_unknown() {
        assert!(matches!(place(&image(0, false, 0x4000), &[]), Placement::At(0x1000, _)));
        assert!(matches!(place(&pt(&[(0, 0, 0x10000)]), &[]), Placement::At(0x8000, _)));
        assert!(matches!(place(b"hello world", &[]), Placement::Ask(0, _)));
    }
}
