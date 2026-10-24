//! "Install from web installer": read an [ESP Web Tools](https://esphome.github.io/esp-web-tools/)
//! manifest (a URL or a local `manifest.json`), pick the `ESP32` build, download its parts and
//! write them into a powered-off device's `flash.bin`, the way the browser installer would
//! write them over Web Serial.
//!
//! Manifest rules followed (esp-web-tools README):
//! * `builds[]` is searched for `chipFamily == "ESP32"` (the CYD's chip).
//! * Each part's `path` is relative to the manifest's own URL (or folder, for a local file);
//!   absolute URLs are allowed. `offset` is a byte offset (number, or a "0x..." string).
//! * A new install erases the flash first. `new_install_prompt_erase: true` leaves that
//!   choice to the user, so then cyd-lab's "erase first" checkbox decides.
//! * `improv: true` means the installer would offer Wi-Fi setup over Improv Serial
//!   afterwards; cyd-lab doesn't do that (configure the firmware through its own UI).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The chip family a CYD reports to ESP Web Tools.
pub const CHIP_FAMILY: &str = "ESP32";
/// Refuse to download more than this per part (the biggest ESP32 flash is 16 MB).
const MAX_PART: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Manifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub new_install_prompt_erase: bool,
    #[serde(default)]
    pub builds: Vec<Build>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    pub chip_family: String,
    #[serde(default)]
    pub improv: bool,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Part {
    pub path: String,
    #[serde(deserialize_with = "de_offset")]
    pub offset: u64,
}

fn de_offset<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Off {
        N(u64),
        S(String),
    }
    match Off::deserialize(d)? {
        Off::N(n) => Ok(n),
        Off::S(s) => crate::device::parse_offset(&s).map_err(serde::de::Error::custom),
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest> {
        let m: Manifest = serde_json::from_str(text).context("not an ESP Web Tools manifest (JSON with name/builds)")?;
        if m.builds.is_empty() {
            bail!("manifest has no builds");
        }
        Ok(m)
    }

    /// The build for the CYD's chip, or an error listing what the manifest has.
    pub fn esp32_build(&self) -> Result<&Build> {
        self.builds.iter().find(|b| b.chip_family.eq_ignore_ascii_case(CHIP_FAMILY)).ok_or_else(|| {
            let have: Vec<&str> = self.builds.iter().map(|b| b.chip_family.as_str()).collect();
            anyhow!("no build for chipFamily \"{CHIP_FAMILY}\" in this manifest (it has: {})", have.join(", "))
        })
    }

    /// Erase before writing? A new install always erases, unless the manifest leaves it to
    /// the user (`new_install_prompt_erase`), in which case `user_choice` decides.
    pub fn erase_first(&self, user_choice: bool) -> bool {
        !self.new_install_prompt_erase || user_choice
    }
}

/// Where a manifest came from; parts are resolved relative to it.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Url(url::Url),
    File(PathBuf),
}

impl Source {
    /// A URL (http/https/file) or a path to a local manifest.json.
    pub fn parse(s: &str) -> Result<Source> {
        let s = s.trim();
        if s.is_empty() {
            bail!("paste a manifest URL or pick a manifest.json");
        }
        if s.starts_with("http://") || s.starts_with("https://") {
            return Ok(Source::Url(url::Url::parse(s).with_context(|| format!("bad URL {s}"))?));
        }
        if let Some(rest) = s.strip_prefix("file://") {
            return Ok(Source::File(PathBuf::from(rest)));
        }
        Ok(Source::File(PathBuf::from(s)))
    }

    /// Resolve a part path against this manifest's location.
    pub fn join(&self, path: &str) -> Result<Source> {
        if path.starts_with("http://") || path.starts_with("https://") {
            return Source::parse(path);
        }
        match self {
            Source::Url(u) => Ok(Source::Url(u.join(path).with_context(|| format!("bad part path {path}"))?)),
            Source::File(f) => Ok(Source::File(f.parent().unwrap_or(Path::new(".")).join(path))),
        }
    }

    pub fn fetch(&self) -> Result<Vec<u8>> {
        match self {
            Source::File(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display())),
            Source::Url(u) => {
                let mut resp = ureq::get(u.as_str())
                    .header("User-Agent", concat!("cyd-lab/", env!("CARGO_PKG_VERSION")))
                    .call()
                    .with_context(|| format!("GET {u}"))?;
                resp.body_mut().with_config().limit(MAX_PART).read_to_vec().with_context(|| format!("reading {u}"))
            }
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Url(u) => write!(f, "{u}"),
            Source::File(p) => write!(f, "{}", p.display()),
        }
    }
}

/// Everything needed to write the firmware, already downloaded.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: String,
    pub version: String,
    pub erase: bool,
    pub prompt_erase: bool,
    pub improv: bool,
    pub parts: Vec<(u64, Vec<u8>, String)>,
}

impl Prepared {
    #[allow(dead_code)]
    pub fn total_bytes(&self) -> usize {
        self.parts.iter().map(|p| p.1.len()).sum()
    }
}

/// Fetch the manifest and all parts of its ESP32 build. `progress` gets one line per step.
pub fn prepare(src: &str, erase_choice: bool, flash_size: u64, progress: &mut dyn FnMut(String)) -> Result<Prepared> {
    let source = Source::parse(src)?;
    progress(format!("fetching manifest {source}"));
    let text = String::from_utf8(source.fetch()?).context("manifest is not UTF-8")?;
    let m = Manifest::parse(&text)?;
    let build = m.esp32_build()?;
    progress(format!(
        "{} {}: {} part(s) for {CHIP_FAMILY}{}",
        if m.name.is_empty() { "firmware" } else { &m.name },
        m.version,
        build.parts.len(),
        if build.improv { ", improv Wi-Fi setup (not done by cyd-lab)" } else { "" }
    ));
    let mut parts = Vec::new();
    for p in &build.parts {
        let s = source.join(&p.path)?;
        progress(format!("downloading {s} -> 0x{:x}", p.offset));
        let data = s.fetch()?;
        let end = p.offset + data.len() as u64;
        if end > flash_size {
            bail!("{} ({} bytes at 0x{:x}) doesn't fit in the device's {} MB flash", p.path, data.len(), p.offset, flash_size / (1024 * 1024));
        }
        parts.push((p.offset, data, s.to_string()));
    }
    Ok(Prepared {
        name: m.name.clone(),
        version: m.version.clone(),
        erase: m.erase_first(erase_choice),
        prompt_erase: m.new_install_prompt_erase,
        improv: build.improv,
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENHASP: &str = r#"{
        "name": "openHASP", "version": "v0.7.0", "home_assistant_domain": "openhasp",
        "new_install_prompt_erase": true,
        "builds": [ { "chipFamily": "ESP32", "improv": false,
            "parts": [ { "path": "esp32-2432s028r_full_4MB_v0.7.0-rc12_cd34d89.bin", "offset": 0 } ] } ] }"#;

    #[test]
    fn parses_real_manifests() {
        let m = Manifest::parse(OPENHASP).unwrap();
        assert_eq!(m.name, "openHASP");
        assert!(m.new_install_prompt_erase);
        let b = m.esp32_build().unwrap();
        assert_eq!(b.parts[0].offset, 0);
        // ESPHome-style: several chips, string offsets.
        let m = Manifest::parse(r#"{"name":"x","builds":[{"chipFamily":"ESP32-S3","parts":[]},
            {"chipFamily":"ESP32","improv":true,"parts":[{"path":"b.bin","offset":"0x1000"},{"path":"https://h/a.bin","offset":65536}]}]}"#).unwrap();
        let b = m.esp32_build().unwrap();
        assert!(b.improv);
        assert_eq!((b.parts[0].offset, b.parts[1].offset), (0x1000, 0x10000));
        assert!(!m.new_install_prompt_erase);
        let e = Manifest::parse(r#"{"builds":[{"chipFamily":"ESP8266","parts":[]}]}"#).unwrap().esp32_build().unwrap_err();
        assert!(e.to_string().contains("ESP8266"), "{e}");
    }

    #[test]
    fn erase_rules() {
        let mut m = Manifest::parse(OPENHASP).unwrap();
        assert!(m.erase_first(true));
        assert!(!m.erase_first(false));
        m.new_install_prompt_erase = false;
        assert!(m.erase_first(false), "a new install erases unless the manifest leaves it to the user");
    }

    #[test]
    fn resolves_parts() {
        let s = Source::parse("https://nightly.openhasp.com/firmware/esp32-2432s028r_full_4MB.json").unwrap();
        assert_eq!(s.join("a.bin").unwrap().to_string(), "https://nightly.openhasp.com/firmware/a.bin");
        assert_eq!(s.join("../x/b.bin").unwrap().to_string(), "https://nightly.openhasp.com/x/b.bin");
        assert_eq!(s.join("https://cdn/c.bin").unwrap().to_string(), "https://cdn/c.bin");
        let f = Source::parse("/tmp/fw/manifest.json").unwrap();
        assert_eq!(f.join("a.bin").unwrap(), Source::File(PathBuf::from("/tmp/fw/a.bin")));
        assert!(Source::parse("  ").is_err());
    }

    #[test]
    fn local_manifest_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.bin"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.path().join("m.json"), r#"{"name":"t","builds":[{"chipFamily":"ESP32","parts":[{"path":"app.bin","offset":4096}]}]}"#).unwrap();
        let mut log = Vec::new();
        let p = prepare(dir.path().join("m.json").to_str().unwrap(), false, 4 << 20, &mut |l| log.push(l)).unwrap();
        assert!(p.erase);
        assert_eq!(p.parts[0].0, 4096);
        assert_eq!(p.parts[0].1, vec![1, 2, 3]);
        assert!(prepare(dir.path().join("m.json").to_str().unwrap(), false, 4096, &mut |_| {}).is_err(), "must not overflow the flash");
    }
}
