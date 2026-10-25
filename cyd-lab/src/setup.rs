//! First run: the bundled engine and the mask ROM.
//!
//! The engine boots from Espressif's mask ROM ELF (Apache-2.0, espressif/esp-rom-elfs),
//! which we may not ship inside the zip without care, so it is fetched once, pinned by
//! release and SHA-256 exactly like esp32sim's `tools/fetch-rom-elfs.sh`, and cached in
//! `<data dir>/roms`. The release is fetched by its download URL (no GitHub API, so no
//! rate limits).

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const ROM_RELEASE: &str = "20260528";
pub const ROM_URL: &str = "https://github.com/espressif/esp-rom-elfs/releases/download/20260528/esp-rom-elfs-20260528.tar.gz";
/// Size of that tarball, for the progress bar when the server sends no length.
pub const ROM_TARBALL_SIZE: u64 = 4_902_355;
pub const ROM_TARBALL_SHA256: &str = "caa463d3cbef2430a5a35847c1d9f2f152403b17a802050927ff60c8da54fe46";
/// The ROMs cyd-lab needs (the CYD is a classic ESP32 rev 3). Same hashes as the engine's script.
pub const ROMS: &[(&str, &str)] = &[("esp32_rev300_rom.elf", "920b70635440517866aab2230964a570d2cf2b676658d93c52fbac108c1cca31")];

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Every ROM present in `dir` with the right hash?
pub fn roms_ok(dir: &Path) -> bool {
    ROMS.iter().all(|(name, sha)| std::fs::read(dir.join(name)).map(|d| sha256_hex(&d) == *sha).unwrap_or(false))
}

/// Progress of a ROM download, sent to the UI thread.
#[derive(Debug, Clone)]
pub enum RomMsg {
    /// Bytes so far, total if the server said.
    Progress(u64, Option<u64>),
    Done(std::result::Result<(), String>),
}

/// Download, verify and unpack the ROMs into `dir`. Blocking; run it on a thread.
pub fn fetch_roms(dir: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    if roms_ok(dir) {
        return Ok(());
    }
    let resp = ureq::get(ROM_URL).call().with_context(|| format!("downloading {ROM_URL}"))?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).or(Some(ROM_TARBALL_SIZE));
    progress(0, total);
    let mut reader = resp.into_body().into_with_config().limit(256 * 1024 * 1024).reader();
    let mut tarball = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        tarball.extend_from_slice(&buf[..n]);
        progress(tarball.len() as u64, total);
    }
    install_from_tarball(dir, &tarball)
}

/// Verify the tarball and copy the wanted ROMs out of it (split out for tests).
pub fn install_from_tarball(dir: &Path, tarball: &[u8]) -> Result<()> {
    let got = sha256_hex(tarball);
    if got != ROM_TARBALL_SHA256 {
        bail!("esp-rom-elfs {ROM_RELEASE}: SHA-256 mismatch (got {got}); the download was damaged or changed");
    }
    extract_roms(dir, tarball, ROMS)
}

fn extract_roms(dir: &Path, tarball: &[u8], roms: &[(&str, &str)]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut found = vec![false; roms.len()];
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in ar.entries().context("reading the tarball")? {
        let mut entry = entry.context("reading the tarball")?;
        let name = entry.path().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let Some(k) = roms.iter().position(|(n, _)| *n == name) else { continue };
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if sha256_hex(&data) != roms[k].1 {
            bail!("{name}: SHA-256 mismatch");
        }
        let tmp = dir.join(format!("{name}.part"));
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, dir.join(&name))?;
        found[k] = true;
    }
    if let Some(k) = found.iter().position(|f| !f) {
        bail!("{} is not in esp-rom-elfs {ROM_RELEASE}", roms[k].0);
    }
    Ok(())
}

pub fn engine_file_name() -> &'static str {
    if cfg!(windows) { "esp32sim.exe" } else { "esp32sim" }
}

/// The directory cyd-lab.exe runs from.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// The engine shipped next to cyd-lab (the release zip layout), if any. A debug build in the
/// monorepo also takes the engine built at the repository root (`../target/{release,debug}`).
pub fn bundled_engine() -> Option<PathBuf> {
    let p = exe_dir()?.join(engine_file_name());
    if p.is_file() {
        return Some(p);
    }
    if cfg!(debug_assertions) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target");
        return ["release", "debug"].iter().map(|d| root.join(d).join(engine_file_name())).find(|p| p.is_file());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        let mut gz = b.into_inner().unwrap();
        gz.flush().unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn extracts_and_verifies() {
        let t = tempfile::tempdir().unwrap();
        let rom = b"fake rom";
        let sha = sha256_hex(rom);
        let tb = tarball(&[("esp-rom-elfs/README", b"x"), ("esp-rom-elfs/a_rom.elf", rom)]);
        extract_roms(t.path(), &tb, &[("a_rom.elf", &sha)]).unwrap();
        assert_eq!(std::fs::read(t.path().join("a_rom.elf")).unwrap(), rom);
        assert!(extract_roms(t.path(), &tb, &[("a_rom.elf", "00")]).unwrap_err().to_string().contains("mismatch"));
        assert!(extract_roms(t.path(), &tb, &[("b_rom.elf", &sha)]).unwrap_err().to_string().contains("not in"));
    }

    #[test]
    fn rejects_wrong_tarball() {
        let t = tempfile::tempdir().unwrap();
        assert!(install_from_tarball(t.path(), b"nope").unwrap_err().to_string().contains("SHA-256"));
        assert!(!roms_ok(t.path()));
    }
}
