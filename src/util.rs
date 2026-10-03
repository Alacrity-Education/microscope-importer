//! Small helpers shared by the importer and the UI: number formatting, the
//! log file, free-space queries and collision-free file names.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const GB: f64 = 1e9;
pub const MB: f64 = 1e6;

/// "12.34 GB" with two decimals, the unit the TUI uses throughout.
pub fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / GB)
}

/// "1h 02m", "12m 03s", "45s".
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--".into();
    }
    let s = secs.round() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm), so no date crate is needed for the camera's file names.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Current UTC time as "2026-10-03 05:29:01Z", for the log.
pub fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86400));
    let t = secs.rem_euclid(86400);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}Z",
        t / 3600,
        (t / 60) % 60,
        t % 60
    )
}

static LOG: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Direct `log()` output to this file (created on first write).
pub fn init_log(path: PathBuf) {
    *LOG.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(path);
}

/// Append one line to the log file. Failing to log is never fatal.
pub fn log(msg: impl AsRef<str>) {
    let Some(lock) = LOG.get() else { return };
    let Ok(guard) = lock.lock() else { return };
    let Some(path) = guard.as_ref() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {}", now_stamp(), msg.as_ref());
    }
}

/// Bytes available to an unprivileged user on the filesystem holding `path`,
/// and that filesystem's id (so two paths on the same disk can be summed).
pub fn free_space(path: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_bavail as u64 * st.f_frsize as u64, st.f_fsid as u64))
}

/// `dir/name`, or `dir/stem_1.ext`, `dir/stem_2.ext`... if that is taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem}_{n}{ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

/// Read a list of names (one per line) - used for the per-card ledgers.
pub fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in [-1000, 0, 1, 365, 19000, 21000, 25000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
    }

    #[test]
    fn formatting() {
        assert_eq!(duration(45.2), "45s");
        assert_eq!(duration(723.0), "12m 03s");
        assert_eq!(duration(3720.0), "1h 02m");
        assert_eq!(gb(1_500_000_000), "1.50 GB");
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.mp4"), dir.path().join("a.mp4"));
        fs::write(dir.path().join("a.mp4"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.mp4"), dir.path().join("a_1.mp4"));
    }
}
