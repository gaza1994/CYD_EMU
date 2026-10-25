//! Opening web pages, folders and files in the system's default handler.
//!
//! eframe is built without its `links` feature, so `ctx.open_url` (and `ui.hyperlink`) do
//! nothing on the desktop. Everything cyd-lab opens goes through here instead: ShellExecuteW
//! on Windows (no `cmd /c start`, so `&` and `%` in a URL need no quoting), `open` on macOS,
//! `xdg-open` elsewhere.

use std::path::Path;

/// A page published on this PC: `http://HOST:PORT/PATH`. `host` is where the forward
/// listens from this PC's point of view (127.0.0.1, or the LAN address), never the
/// firmware's own address on the emulated network (10.0.2.15, 192.168.4.1, ...).
pub fn page_url(host: &str, port: u16, path: &str) -> String {
    let host = host.trim();
    let host = if host.is_empty() || host == "0.0.0.0" || host == "localhost" { "127.0.0.1" } else { host };
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    let path = path.trim();
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    format!("http://{host}:{port}{path}")
}

/// The page behind a published port, opened from this PC: always the loopback address
/// (a forward bound to 0.0.0.0 answers there too), never the guest's 10.0.2.15. `None` for
/// ports that are not web pages (ESPHome API, OTA, MQTT, telnet).
pub fn forward_page_url(host_port: u16, guest_port: u16) -> Option<String> {
    match guest_port {
        6053 | 3232 | 3233 | 8266 | 1883 | 8883 | 22 | 23 => None,
        443 | 8443 => Some(format!("https://127.0.0.1:{host_port}/")),
        _ => Some(page_url("127.0.0.1", host_port, "/")),
    }
}

/// Only web addresses: no `file:`, `javascript:` or program names from firmware output.
fn web_url_ok(url: &str) -> bool {
    let u = url.trim();
    (u.starts_with("http://") || u.starts_with("https://")) && !u.chars().any(|c| c.is_control() || c == ' ' || c == '"')
}

/// Opens a web address in the default browser.
pub fn url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if !web_url_ok(url) {
        return Err(format!("not a web address: {url}"));
    }
    shell_open(url).map_err(|e| format!("could not open {url}: {e}"))
}

/// Opens a folder in the file manager (or a file in its default program).
pub fn path(p: &Path) -> Result<(), String> {
    if !p.exists() {
        return Err(format!("{} does not exist", p.display()));
    }
    shell_open(&p.to_string_lossy()).map_err(|e| format!("could not open {}: {e}", p.display()))
}

#[cfg(windows)]
fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open"), wide(target));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    // Anything above 32 is success; below is an error code.
    let code = r as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(match code {
            2 | 3 => "not found".to_string(),
            5 => "access denied".to_string(),
            31 => "no program is set to open it (default browser / file association)".to_string(),
            c => format!("ShellExecute error {c}"),
        })
    }
}

#[cfg(not(windows))]
fn shell_open(target: &str) -> Result<(), String> {
    let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // Only a failure to start counts: the handler runs on in the background.
    std::process::Command::new(prog)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{prog}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_urls() {
        assert_eq!(page_url("127.0.0.1", 18080, ""), "http://127.0.0.1:18080/");
        assert_eq!(page_url("localhost", 8080, "/"), "http://127.0.0.1:8080/");
        assert_eq!(page_url("0.0.0.0", 8080, "index.html"), "http://127.0.0.1:8080/index.html");
        assert_eq!(page_url("10.100.1.5", 80, "/a?b=1&c=2"), "http://10.100.1.5:80/a?b=1&c=2");
        assert_eq!(page_url("::1", 8080, "/"), "http://[::1]:8080/");
    }

    #[test]
    fn forward_pages() {
        assert_eq!(forward_page_url(18080, 80).as_deref(), Some("http://127.0.0.1:18080/"));
        assert_eq!(forward_page_url(8443, 443).as_deref(), Some("https://127.0.0.1:8443/"));
        assert_eq!(forward_page_url(16054, 6053), None);
        assert_eq!(forward_page_url(3232, 3232), None);
        assert!(!forward_page_url(18080, 80).unwrap().contains("10.0.2."));
    }

    #[test]
    fn only_web_addresses() {
        assert!(web_url_ok("http://127.0.0.1:18080/"));
        assert!(web_url_ok("https://github.com/x?a=1&b=2"));
        assert!(!web_url_ok("file:///C:/Windows"));
        assert!(!web_url_ok("calc.exe"));
        assert!(!web_url_ok("http://a\" & calc"));
        assert!(url("javascript:alert(1)").is_err());
    }
}
