//! Firmware catalogue: a list of ready-made CYD firmwares that the "Install firmware" gallery
//! can download, verify and write in one click.
//!
//! The catalogue (`cyd-firmware.toml`, its config templates and thumbnails) is bundled into
//! the app (see `tools/catalog_assets.py`). An optional refresh downloads a newer
//! `cyd-firmware.toml` from a URL (off by default; settings.toml `[catalog]`).
//!
//! Source types (`[entries.source] type = …`):
//! * `direct`: one URL written at `offset`.
//! * `github-release`: a release asset (`asset`, optionally a member `inner` of a zip asset)
//!   at `offset`, or a list of `parts` taken from URLs, the `asset_app` asset or members of
//!   the `asset_bundle` zip (matched by `path_pattern`, `*` wildcards).
//! * `manifest`: an ESP Web Tools manifest, installed like "Install from web", then checked
//!   against `parts_verified` / `sha256_bin` when the catalogue pins them.
//! * `config-template`: not flashable; the gallery offers to copy or save the files.
//!
//! Every pinned `sha256` is checked before anything is written. A part with
//! `patch_flash_size_mb` is an ESP32 bootloader whose header flash size is rewritten (and its
//! appended SHA-256 recomputed) after the check, as install.wled.me does for WLED.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use crate::catalog_assets::{CATALOG_TOML, TEMPLATES, THUMBS};

/// The catalogue format this build understands.
pub const SCHEMA_VERSION: u32 = 1;
const MAX_DOWNLOAD: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // schema fields kept for display/debugging
pub struct Catalog {
    pub schema_version: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub profiles: Vec<CatProfile>,
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub unsupported: Vec<Unsupported>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct CatProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub chip: String,
    #[serde(default)]
    pub flash_mb: u32,
    #[serde(default)]
    pub display: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Works,
    Partial,
    Broken,
    #[serde(other)]
    Untested,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Works => "works",
            Status::Partial => "partial",
            Status::Broken => "broken",
            Status::Untested => "untested",
        }
    }
    pub fn help(self) -> &'static str {
        match self {
            Status::Works => "Boots in the emulator and shows its normal first screen.",
            Status::Partial => "Boots in the emulator but stops early or needs hardware/config the emulator lacks.",
            Status::Broken => "Does not boot in the emulator today (fine on a real board).",
            Status::Untested => "Not tried in the emulator.",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub licence: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub version: String,
    pub emulator_status: Status,
    #[serde(default)]
    pub emulator_notes: String,
    #[serde(default)]
    pub notes: String,
    /// Hardware the firmware wants that the emulator does not model.
    #[serde(default)]
    pub needs: Option<Vec<String>>,
    pub source: toml::Table,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Unsupported {
    pub name: String,
    pub reason: String,
    #[serde(default)]
    pub homepage: String,
}

/// Where one part's bytes come from.
#[derive(Debug, Clone, PartialEq)]
pub enum From {
    Url { url: String, mirror: Option<String> },
    /// A member of a zip file at `zip` (exact name, or a pattern with `*`).
    Zip { zip: String, member: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartSpec {
    pub from: From,
    pub offset: u64,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub patch_flash_size_mb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pinned {
    pub path: String,
    pub offset: u64,
    pub sha256: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    Parts(Vec<PartSpec>),
    Manifest { url: String, pinned: Vec<Pinned>, sha256_bin: Option<String>, size_bin: Option<u64> },
    Template { files: Vec<String> },
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Catalog> {
        let v: toml::Value = toml::from_str(text).context("catalogue is not valid TOML")?;
        let schema = v.get("schema_version").and_then(|s| s.as_integer()).ok_or_else(|| anyhow!("catalogue has no schema_version"))?;
        if schema != SCHEMA_VERSION as i64 {
            bail!("catalogue schema_version {schema}; this cyd-lab reads version {SCHEMA_VERSION}");
        }
        let c: Catalog = v.try_into().context("catalogue entries")?;
        for e in &c.entries {
            e.spec().with_context(|| format!("entry {}", e.id))?;
        }
        Ok(c)
    }

    pub fn bundled() -> Catalog {
        Catalog::parse(CATALOG_TOML).expect("bundled catalogue parses (checked by tests)")
    }

    /// The catalogue profile for a cyd-lab board profile (`esp32-2432s028r` -> `cyd-2432s028r`).
    pub fn profile_for(&self, board_id: &str) -> Option<&CatProfile> {
        let b = board_id.to_ascii_lowercase();
        let short = b.strip_prefix("esp32-").unwrap_or(&b);
        self.profiles.iter().find(|p| p.id == b || p.id == format!("cyd-{short}")).or_else(|| {
            self.profiles.iter().find(|p| p.aliases.iter().any(|a| a.to_ascii_lowercase() == short))
        })
    }

    /// Entries for a catalogue profile, in catalogue order; broken ones only on request.
    pub fn entries_for(&self, profile: &str, show_broken: bool) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| e.profiles.iter().any(|p| p == profile))
            .filter(|e| show_broken || e.emulator_status != Status::Broken)
            .collect()
    }

    pub fn hidden_broken(&self, profile: &str) -> usize {
        self.entries.iter().filter(|e| e.profiles.iter().any(|p| p == profile) && e.emulator_status == Status::Broken).count()
    }
}

fn s(t: &toml::Table, k: &str) -> Option<String> {
    t.get(k).and_then(|v| v.as_str()).map(str::to_string)
}
fn n(t: &toml::Table, k: &str) -> Option<u64> {
    t.get(k).and_then(|v| v.as_integer()).and_then(|i| u64::try_from(i).ok())
}
fn sha(t: &toml::Table, k: &str) -> Result<Option<String>> {
    match s(t, k) {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(Some(h.to_ascii_lowercase())),
        Some(h) => bail!("{k} is not a SHA-256: {h}"),
        None => Ok(None),
    }
}

/// `*` matches any run of characters.
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name[first.len()..].ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

impl Entry {
    pub fn kind(&self) -> &str {
        self.source.get("type").and_then(|v| v.as_str()).unwrap_or("")
    }

    pub fn is_template(&self) -> bool {
        self.kind() == "config-template"
    }

    /// What the emulator does not model that this firmware wants. Uses the entry's `needs`
    /// when the catalogue has one, else what the verification notes found.
    pub fn needs(&self) -> Vec<String> {
        if let Some(n) = &self.needs {
            return n.clone();
        }
        let v: &[&str] = match self.id.as_str() {
            "marauder" => &["SD card", "Bluetooth"],
            "esphome-web" => &["Bluetooth"],
            "wled" => &["LED strip (no screen UI)"],
            "bruce" => &["RF / IR / NRF24 add-on modules"],
            _ => &[],
        };
        v.iter().map(|s| s.to_string()).collect()
    }

    pub fn thumbnail(&self) -> Option<&'static [u8]> {
        THUMBS.iter().find(|(id, _)| *id == self.id).map(|(_, b)| *b)
    }

    /// Bundled text of a config template file (`templates/…`).
    pub fn template_text(file: &str) -> Option<&'static str> {
        TEMPLATES.iter().find(|(f, _)| *f == file).map(|(_, t)| *t)
    }

    pub fn spec(&self) -> Result<Spec> {
        let t = &self.source;
        match self.kind() {
            "direct" => Ok(Spec::Parts(vec![PartSpec {
                from: From::Url { url: s(t, "url").ok_or_else(|| anyhow!("direct source without url"))?, mirror: s(t, "cdn") },
                offset: n(t, "offset").unwrap_or(0),
                sha256: sha(t, "sha256")?,
                size: n(t, "size"),
                patch_flash_size_mb: None,
            }])),
            "github-release" => {
                let repo = s(t, "repo").ok_or_else(|| anyhow!("github-release without repo"))?;
                let tag = s(t, "tag").ok_or_else(|| anyhow!("github-release without tag"))?;
                let asset_url = |a: &str| format!("https://github.com/{repo}/releases/download/{tag}/{a}");
                if let Some(parts) = t.get("parts").and_then(|p| p.as_array()) {
                    let app = s(t, "asset_app");
                    let bundle = s(t, "asset_bundle");
                    let mut out = Vec::new();
                    for p in parts {
                        let p = p.as_table().ok_or_else(|| anyhow!("parts must be tables"))?;
                        let from = if let Some(u) = s(p, "url") {
                            From::Url { url: u, mirror: None }
                        } else if let Some(pat) = s(p, "path_pattern") {
                            match (&app, &bundle) {
                                (Some(a), _) if glob(&pat, a) => From::Url { url: asset_url(a), mirror: None },
                                (_, Some(b)) => From::Zip { zip: asset_url(b), member: pat },
                                _ => bail!("part {pat} matches no asset"),
                            }
                        } else {
                            bail!("part without url or path_pattern");
                        };
                        out.push(PartSpec {
                            from,
                            offset: n(p, "offset").ok_or_else(|| anyhow!("part without offset"))?,
                            sha256: sha(p, "sha256")?,
                            size: n(p, "size"),
                            patch_flash_size_mb: n(p, "patch_flash_size_mb").map(|m| m as u32),
                        });
                    }
                    if out.is_empty() {
                        bail!("no parts");
                    }
                    return Ok(Spec::Parts(out));
                }
                let asset = s(t, "asset").ok_or_else(|| anyhow!("github-release without asset or parts"))?;
                let from = match s(t, "inner") {
                    Some(inner) => From::Zip { zip: asset_url(&asset), member: inner },
                    None => From::Url { url: asset_url(&asset), mirror: s(t, "cdn") },
                };
                Ok(Spec::Parts(vec![PartSpec { from, offset: n(t, "offset").unwrap_or(0), sha256: sha(t, "sha256")?, size: n(t, "size"), patch_flash_size_mb: None }]))
            }
            "manifest" => {
                let mut pinned = Vec::new();
                for p in t.get("parts_verified").and_then(|p| p.as_array()).into_iter().flatten() {
                    let p = p.as_table().ok_or_else(|| anyhow!("parts_verified must be tables"))?;
                    pinned.push(Pinned {
                        path: s(p, "path").unwrap_or_default(),
                        offset: n(p, "offset").ok_or_else(|| anyhow!("parts_verified without offset"))?,
                        sha256: sha(p, "sha256")?.ok_or_else(|| anyhow!("parts_verified without sha256"))?,
                        size: n(p, "size"),
                    });
                }
                Ok(Spec::Manifest { url: s(t, "url").ok_or_else(|| anyhow!("manifest source without url"))?, pinned, sha256_bin: sha(t, "sha256_bin")?, size_bin: n(t, "size_bin") })
            }
            "config-template" => {
                let mut files: Vec<String> = t.get("files").and_then(|f| f.as_array()).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
                files.extend(s(t, "file"));
                if files.is_empty() {
                    bail!("config-template without files");
                }
                Ok(Spec::Template { files })
            }
            k => bail!("unknown source type {k:?}"),
        }
    }

    /// Is every byte that will be written pinned by a SHA-256 in the catalogue?
    pub fn pinned(&self) -> bool {
        match self.spec() {
            Ok(Spec::Parts(p)) => p.iter().all(|p| p.sha256.is_some()),
            Ok(Spec::Manifest { pinned, sha256_bin, .. }) => !pinned.is_empty() || sha256_bin.is_some(),
            _ => false,
        }
    }

    /// The URL a user could paste into "Install from web" instead (manifest entries).
    pub fn manifest_url(&self) -> Option<String> {
        match self.spec() {
            Ok(Spec::Manifest { url, .. }) => Some(url),
            _ => None,
        }
    }
}

pub fn sha256_hex(d: &[u8]) -> String {
    Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect()
}

/// Rewrite an ESP32 bootloader's header flash size (byte 3, high nibble) and, when the image
/// carries an appended SHA-256, recompute it, as esptool and install.wled.me do.
pub fn patch_bootloader_flash_size(d: &mut [u8], mb: u32) -> Result<()> {
    let nib: u8 = match mb {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        16 => 4,
        _ => bail!("unsupported flash size {mb} MB"),
    };
    if d.len() < 24 || d[0] != 0xE9 {
        bail!("not an ESP32 image (no 0xE9 magic)");
    }
    d[3] = (d[3] & 0x0F) | (nib << 4);
    let mut pos = 24usize;
    for _ in 0..d[1] {
        let len = u32::from_le_bytes(d.get(pos + 4..pos + 8).ok_or_else(|| anyhow!("truncated segment header"))?.try_into().unwrap()) as usize;
        pos += 8 + len;
    }
    // Checksum byte at the end of 16-byte padding, then the SHA-256 of everything before.
    let hs = pos + (16 - (pos + 1) % 16) % 16 + 1;
    if d[23] == 1 {
        if hs + 32 > d.len() {
            bail!("truncated image: no room for the appended SHA-256");
        }
        let h = Sha256::digest(&d[..hs]);
        d[hs..hs + 32].copy_from_slice(&h);
    }
    Ok(())
}

/// Downloads (with a cache of verified files, keyed by SHA-256).
pub struct Fetcher {
    pub cache: Option<PathBuf>,
    zips: HashMap<String, Vec<u8>>,
}

pub fn http_get(url: &str) -> Result<Vec<u8>> {
    if let Some(p) = url.strip_prefix("file://") {
        return std::fs::read(p).with_context(|| format!("reading {p}"));
    }
    let mut resp = ureq::get(url).header("User-Agent", concat!("cyd-lab/", env!("CARGO_PKG_VERSION"))).call().with_context(|| format!("GET {url}"))?;
    resp.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().with_context(|| format!("reading {url}"))
}

impl Fetcher {
    pub fn new(cache: Option<PathBuf>) -> Fetcher {
        Fetcher { cache, zips: HashMap::new() }
    }

    fn cached(&self, sha: &str) -> Option<Vec<u8>> {
        let p = self.cache.as_ref()?.join(format!("{sha}.bin"));
        let d = std::fs::read(p).ok()?;
        (sha256_hex(&d) == sha).then_some(d)
    }

    fn store(&self, sha: &str, d: &[u8]) {
        if let Some(c) = &self.cache {
            let _ = std::fs::create_dir_all(c);
            let tmp = c.join(format!("{sha}.part"));
            if std::fs::write(&tmp, d).is_ok() {
                let _ = std::fs::rename(&tmp, c.join(format!("{sha}.bin")));
            }
        }
    }

    fn zip_member(&mut self, zip: &str, member: &str, progress: &mut dyn FnMut(String)) -> Result<(String, Vec<u8>)> {
        if !self.zips.contains_key(zip) {
            progress(format!("downloading {zip}"));
            let d = http_get(zip)?;
            self.zips.insert(zip.to_string(), d);
        }
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&self.zips[zip][..])).with_context(|| format!("{zip} is not a zip file"))?;
        let names: Vec<String> = z.file_names().filter_map(|r| r.ok().map(|c| c.to_string())).collect();
        let base = |n: &str| n.rsplit('/').next().unwrap_or(n).to_string();
        let hits: Vec<&String> = names.iter().filter(|n| *n == member || glob(member, n) || glob(member, &base(n))).collect();
        let name = match hits.as_slice() {
            [one] => (*one).clone(),
            [] => bail!("{member} is not in {zip}"),
            many => bail!("{member} matches {} files in {zip}", many.len()),
        };
        let mut f = z.by_name(&name)?;
        let mut d = Vec::new();
        f.read_to_end(&mut d)?;
        Ok((name, d))
    }

    /// Bytes for one part, verified against its pinned size and SHA-256, patched if asked.
    pub fn part(&mut self, p: &PartSpec, progress: &mut dyn FnMut(String)) -> Result<(Vec<u8>, String)> {
        let label = match &p.from {
            From::Url { url, .. } => url.clone(),
            From::Zip { zip, member } => format!("{member} from {}", zip.rsplit('/').next().unwrap_or(zip)),
        };
        let mut data = match p.sha256.as_deref().and_then(|h| self.cached(h)) {
            Some(d) => {
                progress(format!("{label}: using the verified copy in the download cache"));
                d
            }
            None => {
                let d = match &p.from {
                    From::Url { url, mirror } => {
                        progress(format!("downloading {url}"));
                        match (http_get(url), mirror) {
                            (Ok(d), _) => d,
                            (Err(e), Some(m)) => {
                                progress(format!("{e:#}; trying {m}"));
                                http_get(m)?
                            }
                            (Err(e), None) => return Err(e),
                        }
                    }
                    From::Zip { zip, member } => self.zip_member(zip, member, progress)?.1,
                };
                if let Some(sz) = p.size {
                    if d.len() as u64 != sz {
                        bail!("{label}: got {} bytes, the catalogue says {sz}. The file changed upstream; nothing was written.", d.len());
                    }
                }
                if let Some(h) = &p.sha256 {
                    let got = sha256_hex(&d);
                    if &got != h {
                        bail!("{label}: SHA-256 {got} does not match the catalogue's {h}. The file changed upstream (or the download is damaged); nothing was written.");
                    }
                    progress(format!("{label}: SHA-256 OK"));
                    self.store(h, &d);
                }
                d
            }
        };
        if let Some(mb) = p.patch_flash_size_mb {
            patch_bootloader_flash_size(&mut data, mb).with_context(|| format!("{label}: reflagging the bootloader"))?;
            progress(format!("{label}: bootloader reflagged to {mb} MB flash (header + SHA-256 rewritten)"));
        }
        Ok((data, label))
    }
}

/// Download and verify everything an entry writes. Always a fresh install (erase first).
pub fn prepare(e: &Entry, flash_size: u64, cache: Option<PathBuf>, progress: &mut dyn FnMut(String)) -> Result<crate::webinstall::Prepared> {
    let mut parts = Vec::new();
    match e.spec()? {
        Spec::Template { .. } => bail!("{} is a config template: copy it, there is nothing to flash", e.name),
        Spec::Parts(specs) => {
            let mut f = Fetcher::new(cache);
            for p in &specs {
                let (d, label) = f.part(p, progress)?;
                parts.push((p.offset, d, label));
            }
        }
        Spec::Manifest { url, pinned, sha256_bin, size_bin } => {
            let p = crate::webinstall::prepare(&url, true, flash_size, progress)?;
            let hint = format!("The web installer now serves different files from what the catalogue checked. If you trust it, paste {url} into \"Install from web\".");
            for pin in &pinned {
                let Some((_, d, _)) = p.parts.iter().find(|(o, _, _)| *o == pin.offset) else {
                    bail!("the manifest has no part at 0x{:x} ({}). {hint}", pin.offset, pin.path);
                };
                if sha256_hex(d) != pin.sha256 {
                    bail!("{} (0x{:x}) does not match the catalogue's SHA-256. {hint}", pin.path, pin.offset);
                }
            }
            if let Some(h) = &sha256_bin {
                if !p.parts.iter().any(|(_, d, _)| &sha256_hex(d) == h && size_bin.is_none_or(|s| s == d.len() as u64)) {
                    bail!("the firmware does not match the catalogue's SHA-256. {hint}");
                }
            }
            if pinned.is_empty() && sha256_bin.is_none() {
                progress("this entry is not pinned to a SHA-256: installing what the web installer serves today".into());
            } else {
                progress("SHA-256 OK for every pinned part".into());
            }
            parts = p.parts;
        }
    }
    for (o, d, l) in &parts {
        if o + d.len() as u64 > flash_size {
            bail!("{l} ({} bytes at 0x{o:x}) doesn't fit in the device's {} MB flash", d.len(), flash_size >> 20);
        }
    }
    Ok(crate::webinstall::Prepared { name: e.name.clone(), version: e.version.clone(), erase: true, prompt_erase: false, improv: false, parts })
}

/// settings.toml `[catalog]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogSettings {
    /// Fetch a newer catalogue from `url` when asked. Off by default.
    pub refresh: bool,
    /// Raw URL of a cyd-firmware.toml, e.g.
    /// https://raw.githubusercontent.com/<user>/cyd-lab/main/catalog/cyd-firmware.toml
    pub url: String,
}

/// The refreshed copy saved next to settings.toml.
pub fn cached_path(data_root: &Path) -> PathBuf {
    data_root.join("catalog").join("cyd-firmware.toml")
}

/// The catalogue to use: a refreshed copy when refresh is on and the copy is valid, else the
/// bundled one. Returns a note when a saved copy was ignored.
pub fn load(data_root: &Path, s: &CatalogSettings) -> (Catalog, Option<String>) {
    let bundled = Catalog::bundled();
    if !s.refresh {
        return (bundled, None);
    }
    let p = cached_path(data_root);
    match std::fs::read_to_string(&p) {
        Err(_) => (bundled, None),
        Ok(t) => match Catalog::parse(&t) {
            Ok(c) if c.updated >= bundled.updated => (c, None),
            Ok(_) => (bundled, None),
            Err(e) => (bundled, Some(format!("ignoring {}: {e:#}", p.display()))),
        },
    }
}

/// Download a catalogue from `url`, check it and save it for `load`.
pub fn refresh(url: &str, data_root: &Path) -> Result<Catalog> {
    if !url.starts_with("https://") && !url.starts_with("file://") {
        bail!("the catalogue URL must start with https://");
    }
    let text = String::from_utf8(http_get(url)?).context("catalogue is not UTF-8")?;
    let c = Catalog::parse(&text)?;
    if c.entries.is_empty() {
        bail!("the downloaded catalogue has no entries");
    }
    let p = cached_path(data_root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, text)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalogue_parses() {
        let c = Catalog::bundled();
        assert_eq!(c.schema_version, 1);
        assert!(c.entries.len() >= 19);
        for e in &c.entries {
            let spec = e.spec().unwrap();
            if let Spec::Template { files } = spec {
                for f in files {
                    assert!(Entry::template_text(&f).is_some(), "template {f} bundled");
                }
            }
        }
        assert!(c.entries.iter().find(|e| e.id == "openhasp").unwrap().thumbnail().is_some());
    }

    #[test]
    fn profile_filter_hides_broken() {
        let c = Catalog::bundled();
        let p = c.profile_for("esp32-2432s028r").unwrap();
        assert_eq!(p.id, "cyd-2432s028r");
        assert!(c.profile_for("ttgo-tdisplay").is_none());
        let shown = c.entries_for(&p.id, false);
        assert!(shown.iter().any(|e| e.id == "nerdminer"));
        assert!(!shown.iter().any(|e| e.id == "circuitpython"), "broken hidden by default");
        assert!(c.entries_for(&p.id, true).iter().any(|e| e.id == "circuitpython"));
        assert_eq!(c.hidden_broken(&p.id), 1);
        // Other profiles only get their own entries.
        let usb = c.entries_for("cyd-2432s028r-2usb", true);
        assert!(usb.iter().all(|e| e.profiles.contains(&"cyd-2432s028r-2usb".to_string())));
        assert!(!usb.iter().any(|e| e.id == "wled"));
    }

    #[test]
    fn sources_resolve() {
        let c = Catalog::bundled();
        let get = |id: &str| c.entries.iter().find(|e| e.id == id).unwrap().spec().unwrap();
        let Spec::Parts(p) = get("openhasp") else { panic!() };
        assert_eq!(p[0].from, From::Zip { zip: "https://github.com/HASwitchPlate/openHASP/releases/download/0.7.0-rc13/sunton.zip".into(), member: "build_output/firmware/esp32-2432s028r_full_4MB_v0.7.0-rc13_042fe05.bin".into() });
        let Spec::Parts(p) = get("nerdminer") else { panic!() };
        assert_eq!(p[0].offset, 0);
        assert!(matches!(&p[0].from, From::Url { url, .. } if url == "https://github.com/BitMaker-hub/NerdMiner_v2/releases/download/nerdminer-release-V1.8.3/ESP32-2432S028R_factory.bin"));
        let Spec::Parts(p) = get("marauder") else { panic!() };
        assert_eq!(p.iter().map(|p| p.offset).collect::<Vec<_>>(), vec![0x1000, 0x8000, 0xe000, 0x10000]);
        assert!(matches!(&p[0].from, From::Zip { member, .. } if member.ends_with(".bootloader.bin")));
        assert!(matches!(&p[3].from, From::Url { url, .. } if url.ends_with("esp32_marauder_v1_19_2_20261023_cyd_2432S028.bin")), "app from asset_app");
        let Spec::Parts(p) = get("wled") else { panic!() };
        assert_eq!(p[0].patch_flash_size_mb, Some(4));
        assert!(matches!(get("bruce"), Spec::Parts(ref p) if matches!(&p[0].from, From::Url { mirror: Some(_), .. })));
        assert!(matches!(get("cyd-klipper"), Spec::Manifest { ref pinned, .. } if pinned.len() == 4));
        assert!(matches!(get("esphome-cyd-starter"), Spec::Template { ref files } if files == &["templates/esphome-cyd-2432s028r.yaml"]));
        assert!(!c.entries.iter().find(|e| e.id == "cyd-diagnostic").unwrap().pinned());
    }

    #[test]
    fn globbing() {
        assert!(glob("esp32_marauder_*_cyd_2432S028.bootloader.bin", "esp32_marauder_installer_v1_19_2_20261023_cyd_2432S028.bootloader.bin"));
        assert!(!glob("esp32_marauder_*_cyd_2432S028.bootloader.bin", "esp32_marauder_installer_v1_19_2_20261023_cyd_2432S028_2usb.bootloader.bin"));
        assert!(!glob("esp32_marauder_v1_19_2_*_cyd_2432S028.bin", "esp32_marauder_installer_v1_19_2_20261023_cyd_2432S028.bin"));
        assert!(glob("a*b*c", "a-b-c") && !glob("a*b*c", "a-c") && glob("x", "x") && !glob("ab*ba", "aba"));
    }

    /// A minimal image: 1 segment, hash appended.
    fn image(flash_nib: u8) -> Vec<u8> {
        let mut d = vec![0u8; 24];
        d[0] = 0xE9;
        d[1] = 1;
        d[3] = (flash_nib << 4) | 0x0f;
        d[23] = 1;
        d.extend(0x3fff_0000u32.to_le_bytes());
        d.extend(5u32.to_le_bytes());
        d.extend([1, 2, 3, 4, 5]);
        let pos = d.len();
        let hs = pos + (16 - (pos + 1) % 16) % 16 + 1;
        d.resize(hs, 0);
        let h = Sha256::digest(&d);
        d.extend(h);
        d.resize(4096, 0xFF);
        d
    }

    #[test]
    fn reflags_bootloader() {
        let mut d = image(3);
        patch_bootloader_flash_size(&mut d, 4).unwrap();
        assert_eq!(d, image(2), "same image as one built for 4 MB, hash included");
        assert_eq!(d[3] & 0x0f, 0x0f, "flash mode/speed nibble kept");
        assert!(patch_bootloader_flash_size(&mut [0u8; 32], 4).is_err());
        // The real WLED 8 MB bootloader (catalogue cache), checked against the catalogue's
        // tools/mkflash.py --patch-bootloader-size output.
        if let Ok(mut real) = std::fs::read("../catalog/dl/wled-bootloader_esp32_8m.bin") {
            assert_eq!(sha256_hex(&real), "dd83e828882288c738d5f8916ba74d400491e6832abd65440b91f0a03fd2308e");
            patch_bootloader_flash_size(&mut real, 4).unwrap();
            assert_eq!(real[3], 0x20);
            assert_eq!(sha256_hex(&real), "8320fd1ff9dd431c12b403a99e8c8ae2ccaf0e1a44c5cedb7c478f786b66e892");
        }
    }

    #[test]
    fn verifies_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let fw = dir.path().join("fw.bin");
        std::fs::write(&fw, b"firmware").unwrap();
        let good = sha256_hex(b"firmware");
        let url = format!("file://{}", fw.display());
        let mut f = Fetcher::new(Some(dir.path().join("cache")));
        let spec = |h: &str| PartSpec { from: From::Url { url: url.clone(), mirror: None }, offset: 0, sha256: Some(h.into()), size: Some(8), patch_flash_size_mb: None };
        let e = f.part(&spec(&"0".repeat(64)), &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
        assert_eq!(f.part(&spec(&good), &mut |_| {}).unwrap().0, b"firmware");
        std::fs::remove_file(&fw).unwrap();
        let mut log = Vec::new();
        assert_eq!(f.part(&spec(&good), &mut |l| log.push(l)).unwrap().0, b"firmware", "served from the cache");
        assert!(log[0].contains("cache"));
    }

    #[test]
    fn zip_members() {
        let dir = tempfile::tempdir().unwrap();
        let zp = dir.path().join("a.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            for (n, d) in [("out/fw_cyd.bin", b"one".as_slice()), ("out/fw_cyd_2usb.bin", b"two")] {
                w.start_file(n, o).unwrap();
                std::io::Write::write_all(&mut w, d).unwrap();
            }
            w.finish().unwrap();
        }
        let zip = format!("file://{}", zp.display());
        let mut f = Fetcher::new(None);
        let p = |m: &str| PartSpec { from: From::Zip { zip: zip.clone(), member: m.into() }, offset: 0, sha256: None, size: None, patch_flash_size_mb: None };
        assert_eq!(f.part(&p("out/fw_cyd.bin"), &mut |_| {}).unwrap().0, b"one");
        assert_eq!(f.part(&p("fw_*_2usb.bin"), &mut |_| {}).unwrap().0, b"two");
        assert!(f.part(&p("fw_*.bin"), &mut |_| {}).is_err(), "ambiguous");
    }

    #[test]
    fn refresh_checks_schema() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "schema_version = 2\n").unwrap();
        assert!(refresh(&format!("file://{}", bad.display()), dir.path()).unwrap_err().to_string().contains("schema_version 2"));
        assert!(refresh("http://example.com/x.toml", dir.path()).is_err());
        let good = dir.path().join("good.toml");
        std::fs::write(&good, CATALOG_TOML.replace("updated = \"2026-10-24\"", "updated = \"2099-01-01\"")).unwrap();
        refresh(&format!("file://{}", good.display()), dir.path()).unwrap();
        let on = CatalogSettings { refresh: true, url: String::new() };
        assert_eq!(load(dir.path(), &on).0.updated, "2099-01-01");
        assert_eq!(load(dir.path(), &CatalogSettings::default()).0.updated, "2026-10-24", "off: bundled");
    }
}
