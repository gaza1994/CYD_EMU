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

pub const IMAGE_MAGIC: u8 = 0xE9;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;

#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Confident: write at this offset.
    At(u64, &'static str),
    /// Probably this offset, but the user should confirm.
    Ask(u64, String),
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
                Some(o) => Placement::At(o, "app image (into the device's first app partition)"),
                None => Placement::Ask(
                    0x10000,
                    "This is an app-only image, but the device's flash has no bootloader/partition table yet, so it will not boot alone. Use the merged/factory .bin if you have it (ESPHome: *.factory.bin, Arduino: Export Compiled Binary → *.merged.bin), or write it at 0x10000 on top of one.".into(),
                ),
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
        assert!(matches!(place(&app, &blank), Placement::Ask(0x10000, m) if m.contains("no bootloader")));
        // ota_0 at 0x10000, ota_1 at 0x150000
        assert_eq!(place(&app, &flash_with(&[(1, 2, 0x9000), (0, 0x10, 0x10000), (0, 0x11, 0x150000)])), Placement::At(0x10000, "app image (into the device's first app partition)"));
        // factory wins over ota
        assert!(matches!(place(&app, &flash_with(&[(0, 0x10, 0x10000), (0, 0, 0x200000)])), Placement::At(0x200000, _)));
        assert!(matches!(place(&image(5, true, 0x40000), &blank), Placement::Ask(_, m) if m.contains("ESP32-C3")));
    }

    #[test]
    fn small_parts_and_unknown() {
        assert!(matches!(place(&image(0, false, 0x4000), &[]), Placement::At(0x1000, _)));
        assert!(matches!(place(&pt(&[(0, 0, 0x10000)]), &[]), Placement::At(0x8000, _)));
        assert!(matches!(place(b"hello world", &[]), Placement::Ask(0, _)));
    }
}
