//! "Set up Arduino IDE": install the cyd-lab board into the user's sketchbook
//! (`<sketchbook>/hardware/cydlab/esp32/`), no Boards Manager URL needed.
//!
//! The board borrows Espressif's esp32:esp32 core by reference, so `platform.txt` is
//! generated here from the *installed* esp32 core (its properties that use
//! `{runtime.platform.path}` are repeated with the esp32 folder, see
//! `arduino/gen_platform.py`). Discovery and monitor are referenced by path
//! (`pluggable_discovery.cydlab.pattern`, `pluggable_monitor.pattern.cydlab`), which
//! works for a sketchbook platform without a package index (tested with arduino-cli 1.5).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const BOARDS_TXT: &str = include_str!("../arduino/cydlab/esp32/boards.txt");
pub const FQBN: &str = "cydlab:esp32:cyd2432s028r";
pub const BOARD_NAME: &str = "ESP32-2432S028R CYD (cyd-lab, emulated)";

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArduinoDirs {
    pub sketchbook: PathBuf,
    pub data: PathBuf,
}

/// `directories.user` / `directories.data` from an arduino-cli.yaml (just enough YAML).
fn yaml_dirs(text: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let (mut user, mut data, mut inside) = (None, None, false);
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            inside = line.trim_end() == "directories:";
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((k, v)) = line.trim().split_once(':') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if v.is_empty() {
                continue;
            }
            match k.trim() {
                "user" => user = Some(PathBuf::from(v)),
                "data" => data = Some(PathBuf::from(v)),
                _ => {}
            }
        }
    }
    (user, data)
}

impl ArduinoDirs {
    /// Arduino IDE 2's folders: its arduino-cli.yaml if it has one, else the defaults.
    pub fn discover() -> Option<ArduinoDirs> {
        let base = directories::BaseDirs::new()?;
        let home = base.home_dir().to_path_buf();
        let docs = directories::UserDirs::new().and_then(|u| u.document_dir().map(Path::to_path_buf)).unwrap_or_else(|| home.join("Documents"));
        let mut sketchbook = if cfg!(any(windows, target_os = "macos")) { docs.join("Arduino") } else { home.join("Arduino") };
        let mut data = if cfg!(windows) {
            base.data_local_dir().join("Arduino15")
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Arduino15")
        } else {
            home.join(".arduino15")
        };
        for cfg in [home.join(".arduinoIDE").join("arduino-cli.yaml"), data.join("arduino-cli.yaml")] {
            if let Ok(t) = std::fs::read_to_string(&cfg) {
                let (u, d) = yaml_dirs(&t);
                if let Some(u) = u {
                    sketchbook = u;
                }
                if let Some(d) = d {
                    data = d;
                }
                break;
            }
        }
        Some(ArduinoDirs { sketchbook, data })
    }

    pub fn platform_dir(&self) -> PathBuf {
        self.sketchbook.join("hardware").join("cydlab").join("esp32")
    }
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0)).collect()
}

/// The newest installed esp32:esp32 core: (version, platform folder).
pub fn esp32_core(data: &Path) -> Option<(String, PathBuf)> {
    let root = data.join("packages").join("esp32").join("hardware").join("esp32");
    let mut best: Option<(String, PathBuf)> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let v = e.file_name().to_string_lossy().into_owned();
        if !e.path().join("platform.txt").is_file() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| version_key(&v) > version_key(b)) {
            best = Some((v, e.path()));
        }
    }
    best
}

/// platform.txt for the sketchbook install, from esp32's platform.txt.
pub fn platform_txt(esp32_platform: &str, esp32_version: &str, windows: bool) -> String {
    let sep = if windows { "\\" } else { "/" };
    let ext = if windows { ".exe" } else { "" };
    let mut out = format!(
        "# cyd-lab Arduino platform (installed by cyd-lab's \"Set up Arduino IDE\"): emulated ESP32\n\
         # \"Cheap Yellow Display\" boards over the network. No core of its own: the board builds with\n\
         # Espressif's esp32:esp32 core by reference and adds cyd-lab's port discovery and serial\n\
         # monitor, so Arduino IDE 2 uploads and monitors with no COM driver.\n\
         # GENERATED from esp32:esp32 {esp32_version}'s platform.txt: press \"Set up Arduino IDE\" again\n\
         # after updating the esp32 core.\n\n\
         name=cyd-lab (emulated CYD boards)\nversion={}\n\n\
         pluggable_discovery.required.0=builtin:serial-discovery\n\
         pluggable_discovery.required.1=builtin:mdns-discovery\n\
         pluggable_discovery.cydlab.pattern=\"{{runtime.platform.path}}{sep}tools{sep}cyd-lab-discovery{ext}\"\n\
         pluggable_monitor.pattern.cydlab=\"{{runtime.platform.path}}{sep}tools{sep}cyd-lab-monitor{ext}\"\n\n\
         # The esp32:esp32 platform folder (build.core.path is <it>/cores/esp32).\n\
         cydlab.esp32.path={{build.core.path}}/../..\n\
         cydlab.esp32.path.windows={{build.core.path}}\\..\\..\n\n\
         # esp32:esp32 properties that use {{runtime.platform.path}}, pointed back at esp32's folder:\n",
        env!("CARGO_PKG_VERSION")
    );
    for line in esp32_platform.lines() {
        let l = line.trim_end_matches('\r');
        if l.trim_start().starts_with('#') || !l.contains('=') || !l.contains("{runtime.platform.path}") {
            continue;
        }
        out.push_str(&l.replace("{runtime.platform.path}", "{cydlab.esp32.path}"));
        out.push('\n');
    }
    out
}

/// Where the bundled discovery/monitor executables are: `arduino/tools/` next to
/// cyd-lab, or next to cyd-lab itself (a cargo build).
pub fn find_tools(exe_dir: &Path) -> Option<PathBuf> {
    [exe_dir.join("arduino").join("tools"), exe_dir.to_path_buf()]
        .into_iter()
        .find(|d| d.join(exe("cyd-lab-discovery")).is_file() && d.join(exe("cyd-lab-monitor")).is_file())
}

/// What happened, for the result window.
#[derive(Debug, Clone)]
pub struct Report {
    pub ok: bool,
    pub lines: Vec<String>,
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("cydlab-tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        bail!("replacing {}: {e} (is Arduino IDE open? Close it and press the button again)", path.display());
    }
    Ok(())
}

/// Install (or refresh) the board. `running_json` is what discovery should read.
pub fn install(dirs: &ArduinoDirs, tools: &Path, running_json: &Path) -> Result<Report> {
    let Some((ver, core)) = esp32_core(&dirs.data) else {
        return Ok(Report {
            ok: false,
            lines: vec![
                format!("The ESP32 core for Arduino is not installed (looked in {}).", dirs.data.join("packages").join("esp32").display()),
                "One step: open Arduino IDE 2 > Tools > Board > Boards Manager…, search \"esp32\", install \"esp32 by Espressif Systems\". Then press \"Set up Arduino IDE\" again.".into(),
            ],
        });
    };
    let esp32_txt = std::fs::read_to_string(core.join("platform.txt")).with_context(|| format!("reading {}", core.join("platform.txt").display()))?;
    let dir = dirs.platform_dir();
    std::fs::create_dir_all(dir.join("tools")).with_context(|| format!("creating {}", dir.display()))?;
    write_atomic(&dir.join("boards.txt"), BOARDS_TXT.as_bytes())?;
    write_atomic(&dir.join("platform.txt"), platform_txt(&esp32_txt, &ver, cfg!(windows)).as_bytes())?;
    for t in ["cyd-lab-discovery", "cyd-lab-monitor"] {
        let src = tools.join(exe(t));
        let data = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        let dst = dir.join("tools").join(exe(t));
        if std::fs::read(&dst).ok().as_deref() != Some(&data[..]) {
            write_atomic(&dst, &data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    write_atomic(&dir.join("tools").join("running-path.txt"), running_json.to_string_lossy().as_bytes())?;
    Ok(Report {
        ok: true,
        lines: vec![
            format!("Installed into {} (uses esp32 core {ver}).", dir.display()),
            "In Arduino IDE 2 (restart it if it is open):".into(),
            format!("  Tools → Board → cyd-lab (emulated CYD boards) → {BOARD_NAME}"),
            "  Tools → Port → network ports → \"<device name> (cyd-lab)\"  (the device must be powered on)".into(),
            "  Upload as usual; the Serial Monitor works too.".into(),
            format!("arduino-cli compile -u -b {FQBN} -p rfc2217://127.0.0.1:<port> <sketch>"),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml() {
        let t = "board_manager:\n  additional_urls: []\ndirectories:\n  data: C:\\Users\\g\\AppData\\Local\\Arduino15\n  user: \"D:\\Sketches\"\nother:\n  user: nope\n";
        let (u, d) = yaml_dirs(t);
        assert_eq!(u, Some(PathBuf::from("D:\\Sketches")));
        assert_eq!(d, Some(PathBuf::from("C:\\Users\\g\\AppData\\Local\\Arduino15")));
    }

    #[test]
    fn generates_platform_and_installs() {
        let t = tempfile::tempdir().unwrap();
        let core = t.path().join("data/packages/esp32/hardware/esp32");
        for v in ["3.0.7", "3.3.12", "3.10.0-rc1"] {
            std::fs::create_dir_all(core.join(v)).unwrap();
        }
        std::fs::write(core.join("3.3.12/platform.txt"), "# c {runtime.platform.path}\nname=x\ntools.gen.cmd=python3 \"{runtime.platform.path}/tools/g.py\"\r\n").unwrap();
        std::fs::write(core.join("3.0.7/platform.txt"), "").unwrap();
        let dirs = ArduinoDirs { sketchbook: t.path().join("sb"), data: t.path().join("data") };
        let tools = t.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        // no 3.10 platform.txt: 3.3.12 is the newest usable
        assert_eq!(esp32_core(&dirs.data).unwrap().0, "3.3.12");
        let r = install(&dirs, &tools, Path::new("/x/running.json"));
        assert!(r.is_err()); // tools missing
        std::fs::write(tools.join(exe("cyd-lab-discovery")), b"d").unwrap();
        std::fs::write(tools.join(exe("cyd-lab-monitor")), b"m").unwrap();
        assert_eq!(find_tools(&t.path().join("nope")), None);
        let r = install(&dirs, &tools, Path::new("/x/running.json")).unwrap();
        assert!(r.ok, "{:?}", r.lines);
        let p = std::fs::read_to_string(dirs.platform_dir().join("platform.txt")).unwrap();
        assert!(p.contains("tools.gen.cmd=python3 \"{cydlab.esp32.path}/tools/g.py\"\n"));
        assert!(p.contains("pluggable_discovery.cydlab.pattern="));
        assert!(!p.contains("# c {"));
        assert_eq!(std::fs::read_to_string(dirs.platform_dir().join("tools/running-path.txt")).unwrap(), "/x/running.json");
        assert!(std::fs::read_to_string(dirs.platform_dir().join("boards.txt")).unwrap().contains("cyd2432s028r.name"));
    }

    #[test]
    fn missing_core_says_the_one_step() {
        let t = tempfile::tempdir().unwrap();
        let dirs = ArduinoDirs { sketchbook: t.path().join("sb"), data: t.path().join("data") };
        let r = install(&dirs, t.path(), Path::new("r.json")).unwrap();
        assert!(!r.ok);
        assert!(r.lines[1].contains("esp32 by Espressif Systems"));
    }
}
