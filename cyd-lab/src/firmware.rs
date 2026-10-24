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
//! An app-only image is never written at 0x0. On a board with no partition table it is
//! refused with an explanation (the like-for-like file is the merged/factory image, see
//! [`TOOL_HINTS`]); "Install anyway" ([`stock_layout`]) adds a generic ESP-IDF bootloader and
//! Arduino-ESP32's default 4 MB partition table around it.
//!
//! Several files dropped at once (a build folder: bootloader, partitions, boot_app0, app)
//! are classified one by one with [`classify`] and placed by [`plan_files`].

pub const IMAGE_MAGIC: u8 = 0xE9;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;

#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Confident: write at this offset.
    At(u64, &'static str),
    /// Probably this offset, but the user should confirm.
    Ask(u64, String),
    /// An app-only image and the device already has a partition table: offer
    /// to write it into the first app partition and reset `otadata` so it is what boots.
    AppIntoPartition { offset: u64, otadata: Option<(u64, u64)> },
    /// An app-only image and the device's flash has no partition table (a blank board):
    /// refused, with the advice to use the merged/factory image. `install_anyway`: it fits
    /// the generic layout, so [`stock_layout`] can be offered.
    AppOnBlank { install_anyway: bool },
}

/// ESP-IDF v5.5.5 bootloader for the classic ESP32 (DIO, 4 MB), as the Arduino-ESP32 core
/// 3.3.12 ships it. Apache-2.0.
pub const STOCK_BOOTLOADER: &[u8] = include_bytes!("../assets/esp32-stock/bootloader.bin");
/// Arduino-ESP32's "default" 4 MB partition table: nvs 0x9000, otadata 0xe000, app0 0x10000
/// and app1 0x150000 (0x140000 each), spiffs 0x290000, coredump 0x3F0000.
pub const STOCK_PARTITIONS_4MB: &[u8] = include_bytes!("../assets/esp32-stock/partitions-default-4mb.bin");
/// Size of each app slot in [`STOCK_PARTITIONS_4MB`].
pub const STOCK_APP_SLOT: usize = 0x14_0000;

/// How to get the merged/factory image (or flash without one) from common tools.
pub const TOOL_HINTS: &[(&str, &str)] = &[
    ("ESPHome", "Install > Manual download > Factory format (….factory.bin)"),
    ("Arduino IDE 2 / esp32 core 3.x", "Sketch > Export Compiled Binary gives <sketch>.ino.merged.bin in the sketch's build folder. Or upload straight to this device's serial port (Use with…)."),
    ("PlatformIO", "firmware.bin is app-only. Run pio run -t upload with upload_port set to this device's rfc2217:// port, or drop bootloader.bin, partitions.bin and firmware.bin together."),
    ("ESP-IDF", "idf.py -p rfc2217://… flash, or make one file with esptool merge-bin."),
];

/// Is `file` an ESP-IDF application image (0xE9 header, app descriptor at 0x20)?
#[allow(dead_code)]
pub fn is_app_image(file: &[u8]) -> bool {
    is_image(file, 0) && u32le(file, 0x20) == Some(APP_DESC_MAGIC)
}

/// The parts for "Install anyway" on a blank board: generic bootloader at 0x1000,
/// Arduino-ESP32's default 4 MB partition table at 0x8000, a blank otadata (so app0 boots)
/// and the app at 0x10000.
pub fn stock_layout(app: &[u8]) -> Vec<(u64, Vec<u8>, String)> {
    let (ota_off, ota_len) = otadata_range(STOCK_PARTITIONS_4MB, 0).unwrap_or((0xe000, 0x2000));
    vec![
        (0x1000, STOCK_BOOTLOADER.to_vec(), "generic ESP-IDF bootloader".into()),
        (0x8000, STOCK_PARTITIONS_4MB.to_vec(), "default 4 MB partition table".into()),
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

/// What a single file is, for several files dropped at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Merged,
    /// Merged image built with `--target-offset 0x1000`.
    MergedFromBootloader,
    Bootloader,
    PartitionTable,
    /// `otadata` contents (Arduino's boot_app0.bin, ESP-IDF's ota_data_initial.bin).
    OtaData,
    App,
    Unknown,
}

impl Part {
    pub fn label(self) -> &'static str {
        match self {
            Part::Merged => "merged/factory image",
            Part::MergedFromBootloader => "merged image (from the bootloader on)",
            Part::Bootloader => "bootloader",
            Part::PartitionTable => "partition table",
            Part::OtaData => "otadata (boot selection)",
            Part::App => "app-only image",
            Part::Unknown => "unknown: enter the offset",
        }
    }
}

/// Classify one file by its contents (and, for otadata, its name and size).
pub fn classify(name: &str, file: &[u8]) -> Part {
    let lname = name.to_ascii_lowercase();
    if file.len() > 0x8000 && is_image(file, 0x1000) && is_pt(file, 0x8000) {
        return Part::Merged;
    }
    if file.len() > 0x7000 && is_image(file, 0) && is_pt(file, 0x7000) {
        return Part::MergedFromBootloader;
    }
    if is_image(file, 0) {
        return if u32le(file, 0x20) == Some(APP_DESC_MAGIC) {
            Part::App
        } else if file.len() <= 0x7000 {
            Part::Bootloader
        } else {
            Part::Unknown
        };
    }
    if is_pt(file, 0) && file.len() <= 0x1000 {
        return Part::PartitionTable;
    }
    if file.len() == 0x2000 && (lname.contains("boot_app0") || lname.contains("ota_data") || lname.contains("otadata")) {
        return Part::OtaData;
    }
    Part::Unknown
}

/// Offsets for several files dropped together: bootloader 0x1000, table 0x8000, otadata and
/// app where the dropped table (else the device's) puts them, otherwise the usual 0xe000 and
/// 0x10000. `None` = unknown, the user must say.
pub fn plan_files(files: &[(String, Vec<u8>)], flash: &[u8]) -> Vec<(Option<u64>, Part)> {
    let parts: Vec<Part> = files.iter().map(|(n, d)| classify(n, d)).collect();
    let mut table = vec![0xFFu8; 0x9000];
    if let Some(k) = parts.iter().position(|p| *p == Part::PartitionTable) {
        let t = &files[k].1;
        table[0x8000..0x8000 + t.len()].copy_from_slice(t);
    } else if flash.len() >= 0x9000 {
        table.copy_from_slice(&flash[..0x9000]);
    }
    let app = first_app_offset(&table).unwrap_or(0x10000);
    let ota = otadata_range(&table, 0x8000).map(|o| o.0).unwrap_or(0xe000);
    parts
        .iter()
        .map(|p| {
            let off = match p {
                Part::Merged => Some(0),
                Part::MergedFromBootloader | Part::Bootloader => Some(0x1000),
                Part::PartitionTable => Some(0x8000),
                Part::OtaData => Some(ota),
                Part::App => Some(app),
                Part::Unknown => None,
            };
            (off, *p)
        })
        .collect()
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
        assert_eq!(place(&image(0, true, 0x14_0001), &blank), Placement::AppOnBlank { install_anyway: false });
        // ota_0 at 0x10000, ota_1 at 0x150000, otadata at 0xe000
        assert!(matches!(
            place(&app, &flash_with(&[(1, 2, 0x9000), (1, 0, 0xe000), (0, 0x10, 0x10000), (0, 0x11, 0x150000)])),
            Placement::AppIntoPartition { offset: 0x10000, otadata: Some((0xe000, _)) }
        ));
        // factory wins over ota
        assert!(matches!(place(&app, &flash_with(&[(0, 0x10, 0x10000), (0, 0, 0x200000)])), Placement::AppIntoPartition { offset: 0x200000, otadata: None }));
        assert!(matches!(place(&image(5, true, 0x40000), &blank), Placement::Ask(_, m) if m.contains("ESP32-C3")));
    }

    /// The bundled generic parts are what they claim: an ESP32 bootloader and Arduino's
    /// default table (app0 at 0x10000, otadata at 0xe000).
    #[test]
    fn stock_layout_is_esphome_default() {
        assert!(is_image(STOCK_BOOTLOADER, 0) && chip_id(STOCK_BOOTLOADER, 0) == Some(0));
        let mut flash = vec![0xFFu8; 0x9000];
        flash[0x8000..0x8000 + STOCK_PARTITIONS_4MB.len()].copy_from_slice(STOCK_PARTITIONS_4MB);
        assert_eq!(first_app_offset(&flash), Some(0x10000));
        assert_eq!(otadata_range(STOCK_PARTITIONS_4MB, 0), Some((0xe000, 0x2000)));
        let parts = stock_layout(&image(0, true, 0x1000));
        let offs: Vec<u64> = parts.iter().map(|p| p.0).collect();
        assert_eq!(offs, vec![0x1000, 0x8000, 0xe000, 0x10000]);
        assert!(parts[2].1.iter().all(|b| *b == 0xFF) && parts[2].1.len() == 0x2000);
    }

    /// A build folder dropped at once (Arduino/PlatformIO layout).
    #[test]
    fn several_files() {
        let mut ota = vec![0xFFu8; 0x2000];
        ota[0] = 1;
        let files = vec![
            ("firmware.bin".to_string(), image(0, true, 0x3000)),
            ("bootloader.bin".to_string(), image(0, false, 0x4000)),
            ("partitions.bin".to_string(), pt(&[(1, 2, 0x9000), (1, 0, 0xe000), (0, 0x10, 0x20000), (0, 0x11, 0x150000)])),
            ("boot_app0.bin".to_string(), ota),
            ("notes.txt".to_string(), b"hello".to_vec()),
        ];
        let plan = plan_files(&files, &[]);
        assert_eq!(
            plan,
            vec![(Some(0x20000), Part::App), (Some(0x1000), Part::Bootloader), (Some(0x8000), Part::PartitionTable), (Some(0xe000), Part::OtaData), (None, Part::Unknown)]
        );
        // no table dropped, blank flash: the usual offsets
        assert_eq!(plan_files(&files[..1], &vec![0xFF; 0x9000])[0], (Some(0x10000), Part::App));
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
