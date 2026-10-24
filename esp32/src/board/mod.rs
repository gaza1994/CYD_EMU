//! Boards wired for the classic ESP32, independent of S3 pin assignments.
//!
//! - `TtgoTDisplay` — the LilyGO TTGO T-Display: an ST7789 135×240 on VSPI, two buttons
//! - `Cyd2432S028R` — the Sunton ESP32-2432S028R "Cheap Yellow Display": an ILI9341 240×320 on HSPI, RGB LED
//! - `GenericDisplay` — any SPI-display board from a profile: the built-ins in `profile::BUILTIN`
//!   (the CYD and its siblings) by name, or `generic-display` with `make_board_config`
//! - `NoBoard` — a bare module (`none`, `bare`, `esp32dev`): nothing on the pins, console only
//! - `esp32dev-loopback` — the SPI loopback fixture in `crate::spi`
pub mod cyd;
pub mod profile;
pub mod tdisplay;

pub use cyd::{Cyd2432S028R, GenericDisplay};
pub use tdisplay::*;

pub fn make_board(name: &str) -> Option<esp_soc::Board> {
    match name {
        "none" | "bare" | "esp32dev" => Some(Box::new(esp_soc::NoBoard)),
        "ttgo-tdisplay" | "tdisplay" => Some(Box::new(TtgoTDisplay::new())),
        _ => match profile::builtin(name) {
            Some(p) => Some(Box::new(GenericDisplay::from_profile(p))),
            None => crate::spi::make_board(name),
        },
    }
}

/// `--board generic-display --board-config FILE` (or a built-in name with a file that overrides it):
/// the profile in FILE, which must be for `chip`.
pub fn make_board_config(path: &str, chip: &str) -> Result<esp_soc::Board, String> {
    let p = profile::Profile::from_file(path)?;
    if p.chip != chip { return Err(format!("{path}: profile '{}' is for {}, not {chip}", p.name, p.chip)); }
    Ok(Box::new(GenericDisplay::from_profile(p)))
}

/// The built-in profile names, for usage text.
pub fn profile_names() -> Vec<&'static str> {
    profile::BUILTIN.iter().map(|t| profile::Profile::parse(t).expect("built-in board profile").name).collect()
}

/// The built-in profiles as a JSON array, for `--list-boards`.
pub fn profiles_json() -> String {
    format!("[{}]", profile::BUILTIN.iter().map(|t| profile::Profile::parse(t).expect("built-in board profile").to_json()).collect::<Vec<_>>().join(",\n "))
}

#[cfg(test)]
mod tests {
    #[test]
    fn classic_boards_reject_s3_names() {
        for name in ["none", "bare", "esp32dev", "ttgo-tdisplay", "tdisplay", "cyd-2432s028r", "cyd", "cyd-2usb", "cyd-3248s035c"] { assert!(super::make_board(name).is_some()); }
        assert_eq!(super::make_board("cyd").unwrap().name(), "cyd-2432s028r");
        for name in ["generic-display", "waveshare-amoled18-v2", "waveshare-lcd4b", "atech14", "waveshare-c6-lcd147"] { assert!(super::make_board(name).is_none()); }
    }

    #[test]
    fn board_config_files_load_and_check_the_chip() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../target/profile-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mine.toml");
        std::fs::write(&file, super::profile::BUILTIN[0].replace("name = \"cyd-2432s028r\"", "name = \"mine\"")).unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(super::make_board_config(path, "esp32").unwrap().name(), "mine");
        assert!(super::make_board_config(path, "esp32s3").err().unwrap().contains("is for esp32, not esp32s3"));
        assert!(super::make_board_config("/nonexistent.toml", "esp32").is_err());
        assert_eq!(super::profile_names().len(), super::profile::BUILTIN.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn profiles_list_as_json() {
        let j = super::profiles_json();
        assert_eq!(j.matches("\"name\":").count(), super::profile::BUILTIN.len());
        assert!(j.contains("\"name\":\"cyd-2432s028r-2usb\",\"aliases\":[\"cyd-2usb\"]") && j.contains("\"width\":320,\"height\":480,\"touch\":\"gt911\""), "{j}");
    }
}
