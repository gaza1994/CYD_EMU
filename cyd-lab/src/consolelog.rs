//! Each device's console (UART0 and cyd-lab's notes) written to `console.log` in its
//! folder, so a long or lost session can be read back or attached to a bug report.
//! Rolls over at [`MAX_BYTES`]: the full file becomes `console.log.old` (one backup).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "console.log";
pub const OLD_NAME: &str = "console.log.old";
/// About 2 MB per file.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub struct ConsoleLog {
    path: PathBuf,
    max: u64,
    file: Option<File>,
    size: u64,
}

impl ConsoleLog {
    pub fn new(dir: &Path) -> ConsoleLog {
        ConsoleLog::with_limit(dir, MAX_BYTES)
    }

    pub fn with_limit(dir: &Path, max: u64) -> ConsoleLog {
        ConsoleLog { path: dir.join(FILE_NAME), max, file: None, size: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. Errors (folder gone, disk full) are ignored: the on-screen console
    /// is what matters, and the next line tries again.
    pub fn line(&mut self, line: &str) {
        let n = line.len() as u64 + 1;
        if self.file.is_some() && self.size > 0 && self.size + n > self.max {
            self.file = None;
            let old = self.path.with_file_name(OLD_NAME);
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&self.path, &old);
        }
        if self.file.is_none() {
            let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) else { return };
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
            if self.size > 0 && self.size + n > self.max {
                return self.line(line);
            }
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(format!("{line}\n").as_bytes()).is_ok() {
                self.size += n;
            } else {
                self.file = None;
            }
        }
    }

    /// Close the file (before the device folder is deleted; Windows can't remove a folder
    /// with an open file in it). The next line reopens it.
    pub fn close(&mut self) {
        self.file = None;
    }
}

/// "2026-10-25 01:02:03 UTC" for a Unix time (no time-zone database needed).
pub fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // civil_from_days: the standard days-since-epoch to proleptic Gregorian date conversion.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem / 60 % 60, rem % 60)
}

pub fn now_stamp() -> String {
    utc_stamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_rolls_over_with_one_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ConsoleLog::with_limit(tmp.path(), 100);
        for k in 0..10 {
            log.line(&format!("line {k:02} ........"));
        }
        let cur = fs::read_to_string(tmp.path().join(FILE_NAME)).unwrap();
        let old = fs::read_to_string(tmp.path().join(OLD_NAME)).unwrap();
        assert!(cur.len() <= 100 && old.len() <= 100, "{} {}", cur.len(), old.len());
        assert!(cur.ends_with("line 09 ........\n"));
        assert!(old.lines().last().unwrap() < cur.lines().next().unwrap());
        // Another session appends to the same file, rolling over when it would pass the limit.
        log.close();
        let mut again = ConsoleLog::with_limit(tmp.path(), 100);
        again.line("next session");
        let cur2 = fs::read_to_string(tmp.path().join(FILE_NAME)).unwrap();
        assert!(cur2.ends_with("next session\n") && cur2.len() <= 100);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
        // Closed: the folder can be removed (Windows needs every handle closed).
        again.close();
        fs::remove_dir_all(tmp.path()).unwrap();
    }

    #[test]
    fn utc_stamps() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_stamp(1_792_890_000), "2026-10-25 01:00:00 UTC");
    }
}
