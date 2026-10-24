//! A real Windows COM port per emulated board: a com0com virtual null-modem pair
//! (`COM20 <-> CNCB20`) plus hub4com relaying `CNCB20` to the engine's RFC 2217 port,
//! exactly what `com2tcp-rfc2217.bat \\.\CNCB20 127.0.0.1 PORT` from the hub4com
//! package runs. Tools that only take a COM name (Arduino IDE, `esphome run`, Windows
//! flashers) then open `COM20`, and DTR/RTS still reach the auto-reset circuit
//! (com0com shows the tool's RTS/DTR to CNCB20 as CTS/DSR; hub4com maps them back).
//!
//! The planning (paths, arguments, `setupc list` parsing) is plain code, unit tested
//! everywhere. Detection, `setupc list` and the elevated pair creation only exist on
//! Windows and are **compile-checked only (x86_64-pc-windows-gnu), not run yet**.
//! The relay process itself is started and stopped by [`crate::engine::ProcessBackend`]
//! with the engine (in the Job object on Windows); that part is OS-neutral and tested
//! on Linux with a stand-in program.

// Off Windows only the tests and the relay plumbing use this module.
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use std::path::{Path, PathBuf};

/// A helper process to run next to the engine while the device is powered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// For the console: what it does.
    pub label: String,
}

/// The hidden end of the pair for `COMn`: `CNCBn`.
pub fn peer(com: &str) -> String {
    let n = com.trim().trim_start_matches(|c: char| c.is_ascii_alphabetic());
    format!("CNCB{n}")
}

/// `setupc.exe` arguments that create the pair (run as administrator, in com0com's folder).
pub fn setupc_install_args(com: &str) -> Vec<String> {
    vec!["install".into(), format!("PortName={com}"), format!("PortName={}", peer(com))]
}

/// The command line to show users who prefer to do it by hand.
pub fn setupc_command(com: &str) -> String {
    format!("setupc.exe {}", setupc_install_args(com).join(" "))
}

/// hub4com arguments equal to `com2tcp-rfc2217.bat \\.\CNCBn 127.0.0.1 PORT` (client
/// mode), with `*` so hub4com keeps reconnecting when the engine restarts.
pub fn hub4com_args(peer: &str, tcp_port: u16) -> Vec<String> {
    [
        "--create-filter=escparse,com,parse",
        "--create-filter=pinmap,com,pinmap:--rts=cts --dtr=dsr",
        "--create-filter=linectl,com,lc:--br=local --lc=local",
        "--add-filters=0:com",
        "--create-filter=telnet,tcp,telnet:--comport=client",
        "--create-filter=pinmap,tcp,pinmap:--rts=cts --dtr=dsr --break=break",
        "--create-filter=linectl,tcp,lc:--br=remote --lc=remote",
        "--add-filters=1:tcp",
        "--octs=off",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([format!(r"\\.\{peer}"), "--use-driver=tcp".into(), format!("*127.0.0.1:{tcp_port}")])
    .collect()
}

/// What was found of com0com / hub4com.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Com0com {
    pub setupc: Option<PathBuf>,
    pub hub4com: Option<PathBuf>,
}

/// Look for `setupc.exe` and `hub4com.exe` in these folders (and a `hub4com*`
/// subfolder of each, where the zip usually unpacks).
pub fn detect_in(dirs: &[PathBuf]) -> Com0com {
    let mut found = Com0com::default();
    let find = |dir: &Path, name: &str| -> Option<PathBuf> {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with("hub4com"))).map(|d| d.join(name)).find(|p| p.is_file())
    };
    for d in dirs {
        if found.setupc.is_none() {
            found.setupc = find(d, "setupc.exe");
        }
        if found.hub4com.is_none() {
            found.hub4com = find(d, "hub4com.exe");
        }
    }
    found
}

/// Where com0com and hub4com usually live: `CYD_LAB_COM0COM_DIR`, Program Files
/// (x86 and native) `\com0com`, then `PATH`.
#[cfg_attr(test, allow(dead_code))]
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("CYD_LAB_COM0COM_DIR") {
        dirs.push(PathBuf::from(d));
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(PathBuf::from(pf).join("com0com"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

/// Does `setupc list` output (lines like `       CNCA0 PortName=COM20` /
/// `       CNCB0 PortName=CNCB20`) contain both ends for `com`?
pub fn pair_listed(listing: &str, com: &str) -> bool {
    let has = |name: &str| {
        listing.lines().any(|l| {
            l.split_whitespace().nth(1).and_then(|kv| kv.strip_prefix("PortName=")).map(|v| v.split(',').next().unwrap_or("").eq_ignore_ascii_case(name)).unwrap_or(false)
        })
    };
    has(com) && has(&peer(com))
}

/// The relay for `com` on `tcp_port`, or a message saying what is missing.
/// `listing` is `setupc list` output (None when it could not be run).
pub fn plan(com: &str, tcp_port: u16, found: &Com0com, listing: Option<&str>) -> Result<Relay, String> {
    let Some(setupc) = &found.setupc else {
        return Err(format!(
            "{com}: com0com is not installed. Install the signed com0com driver (com0com 3.0.0.0 signed, from sourceforge.net/projects/com0com), then press \"Create {com} pair\" or run as administrator in its folder: {}",
            setupc_command(com)
        ));
    };
    let Some(hub4com) = &found.hub4com else {
        return Err(format!(
            "{com}: hub4com.exe not found. Unzip hub4com (sourceforge.net/projects/com0com, hub4com-2.1.0.0) into {} or a folder on PATH, or set CYD_LAB_COM0COM_DIR",
            setupc.parent().map(|p| p.display().to_string()).unwrap_or_default()
        ));
    };
    match listing {
        Some(l) if !pair_listed(l, com) => {
            return Err(format!("{com}: no com0com pair {com} <-> {}. Press \"Create {com} pair\" (asks for administrator) or run in {}: {}", peer(com), setupc.parent().map(|p| p.display().to_string()).unwrap_or_default(), setupc_command(com)))
        }
        _ => {}
    }
    Ok(Relay { program: hub4com.clone(), args: hub4com_args(&peer(com), tcp_port), label: format!("{com} <-> {} <-> rfc2217://127.0.0.1:{tcp_port} (hub4com)", peer(com)) })
}

/// `setupc list`, run in com0com's folder. Windows only. UNTESTED at runtime.
#[cfg(windows)]
pub fn list_pairs(setupc: &Path) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(setupc).arg("list").current_dir(setupc.parent()?).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create the pair with an elevation prompt (UAC) via PowerShell `Start-Process -Verb RunAs`
/// and wait for it. Windows only. UNTESTED at runtime.
#[cfg(windows)]
pub fn create_pair_elevated(setupc: &Path, com: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let dir = setupc.parent().unwrap_or(Path::new("."));
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let script = format!(
        "$p = Start-Process -FilePath {} -ArgumentList {} -WorkingDirectory {} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        q(&setupc.display().to_string()),
        q(&setupc_install_args(com).join(" ")),
        q(&dir.display().to_string())
    );
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).creation_flags(CREATE_NO_WINDOW).status()?;
    if st.success() { Ok(()) } else { Err(std::io::Error::other(format!("setupc exited with {st} (cancelled the administrator prompt?)"))) }
}

/// Plan the relay for one device at power-on. Windows only (elsewhere there is no COM port).
#[cfg(windows)]
pub fn plan_for(com: &str, tcp_port: u16) -> Result<Relay, String> {
    let found = detect_in(&search_dirs());
    let listing = found.setupc.as_deref().and_then(list_pairs);
    plan(com, tcp_port, &found, listing.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_commands() {
        assert_eq!(peer("COM20"), "CNCB20");
        assert_eq!(setupc_command("COM20"), "setupc.exe install PortName=COM20 PortName=CNCB20");
        let a = hub4com_args("CNCB20", 22170);
        assert_eq!(a[a.len() - 3..], [r"\\.\CNCB20".to_string(), "--use-driver=tcp".into(), "*127.0.0.1:22170".into()]);
        assert!(a.contains(&"--create-filter=telnet,tcp,telnet:--comport=client".to_string()));
    }

    #[test]
    fn setupc_listing() {
        let l = "       CNCA0 PortName=COM20,EmuBR=yes\r\n       CNCB0 PortName=CNCB20\r\n       CNCA1 PortName=-\r\n       CNCB1 PortName=-\r\n";
        assert!(pair_listed(l, "COM20"));
        assert!(pair_listed(l, "com20"));
        assert!(!pair_listed(l, "COM21"));
        assert!(!pair_listed(l, "COM2"));
    }

    #[test]
    fn detection_and_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let c0c = tmp.path().join("com0com");
        std::fs::create_dir_all(c0c.join("hub4com-2.1.0.0")).unwrap();
        let dirs = vec![tmp.path().join("nope"), c0c.clone()];
        assert_eq!(detect_in(&dirs), Com0com::default());
        let e = plan("COM20", 22170, &detect_in(&dirs), None).unwrap_err();
        assert!(e.contains("not installed") && e.contains("setupc.exe install PortName=COM20 PortName=CNCB20"), "{e}");
        std::fs::write(c0c.join("setupc.exe"), b"").unwrap();
        assert!(plan("COM20", 22170, &detect_in(&dirs), None).unwrap_err().contains("hub4com.exe not found"));
        std::fs::write(c0c.join("hub4com-2.1.0.0").join("hub4com.exe"), b"").unwrap();
        let found = detect_in(&dirs);
        assert!(found.hub4com.as_ref().unwrap().ends_with("hub4com-2.1.0.0/hub4com.exe"));
        assert!(plan("COM20", 22170, &found, Some("CNCA0 PortName=COM5\n")).unwrap_err().contains("no com0com pair"));
        let r = plan("COM20", 22170, &found, Some(" CNCA0 PortName=COM20\n CNCB0 PortName=CNCB20\n")).unwrap();
        assert_eq!(r.program, found.hub4com.unwrap());
        assert!(r.label.contains("COM20 <-> CNCB20"));
    }
}
