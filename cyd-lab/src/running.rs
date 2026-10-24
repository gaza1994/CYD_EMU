//! `running.json`: which cyd-lab devices are powered on and where their serial ports are.
//!
//! cyd-lab rewrites `<data dir>/running.json` whenever a device starts or stops (and
//! removes it on exit). The Arduino pluggable discovery (`cyd-lab-discovery`) reads it,
//! so Arduino IDE 2 / arduino-cli can list the emulated boards as network ports with no
//! COM driver. Shared by the app and the helper binaries (included via `#[path]`), so
//! it uses only std, serde and serde_json.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "running.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunningFile {
    pub version: u32,
    /// Process id of the cyd-lab that wrote this file (stale-file check).
    pub pid: u32,
    pub devices: Vec<RunningDevice>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunningDevice {
    pub id: String,
    pub name: String,
    pub profile: String,
    /// The engine's RFC 2217 `--serial-tcp` port on 127.0.0.1.
    pub serial_port: u16,
}

impl RunningDevice {
    pub fn address(&self) -> String {
        format!("rfc2217://127.0.0.1:{}", self.serial_port)
    }
    pub fn label(&self) -> String {
        format!("{} (cyd-lab)", self.name)
    }
}

/// cyd-lab's data directory: `CYD_LAB_HOME`, else `%APPDATA%\cyd-lab` / `~/.local/share/cyd-lab`.
pub fn data_dir() -> Option<PathBuf> {
    match std::env::var_os("CYD_LAB_HOME") {
        Some(p) => Some(PathBuf::from(p)),
        None => directories::BaseDirs::new().map(|b| b.data_dir().join("cyd-lab")),
    }
}

impl RunningFile {
    pub fn read(path: &Path) -> Option<RunningFile> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Atomic replace (write a temp file, then rename), so readers never see half a file.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// The devices, or none if the cyd-lab that wrote the file is gone (crashed).
    pub fn live_devices(&self) -> Vec<RunningDevice> {
        if self.pid != 0 && !pid_alive(self.pid) {
            return Vec::new();
        }
        self.devices.clone()
    }
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // Signal 0: existence check only. EPERM still means "exists".
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_stale_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let dev = RunningDevice { id: "cyd-1".into(), name: "Kitchen CYD".into(), profile: "cyd-2432s028r".into(), serial_port: 22170 };
        let f = RunningFile { version: 1, pid: std::process::id(), devices: vec![dev.clone()] };
        f.write(&path).unwrap();
        let back = RunningFile::read(&path).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.live_devices(), vec![dev.clone()]);
        assert_eq!(dev.address(), "rfc2217://127.0.0.1:22170");
        assert_eq!(dev.label(), "Kitchen CYD (cyd-lab)");
        // A writer that is gone: its devices are not running any more.
        let stale = RunningFile { pid: u32::MAX - 7, ..f };
        assert!(stale.live_devices().is_empty());
    }
}
