//! Mounting and unmounting through udisks, which lets an unprivileged user
//! do both for removable media.

use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::devices::mountpoint_of;

fn udisksctl(args: &[&str]) -> Result<String> {
    let out = Command::new("udisksctl")
        .args(args)
        .arg("--no-user-interaction")
        .output()
        .context("cannot run udisksctl (is udisks2 installed?)")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        bail!("{}", if stderr.is_empty() { stdout } else { stderr })
    }
}

/// Mount `dev` if needed; returns the mount point and whether we mounted it.
pub fn ensure_mounted(dev: &str) -> Result<(PathBuf, bool)> {
    if let Some(m) = mountpoint_of(dev) {
        return Ok((m, false));
    }
    match udisksctl(&["mount", "-b", dev]) {
        // "Mounted /dev/sdc1 at /run/media/user/LABEL" (older: trailing '.')
        Ok(msg) => {
            if let Some(m) = mountpoint_of(dev) {
                return Ok((m, true));
            }
            if let Some((_, at)) = msg.split_once(" at ") {
                return Ok((PathBuf::from(at.trim_end_matches('.')), true));
            }
            bail!("mounted {dev} but cannot tell where: {msg}")
        }
        Err(e) => {
            // Raced with the desktop's automounter.
            if let Some(m) = mountpoint_of(dev) {
                return Ok((m, false));
            }
            Err(e.context(format!("cannot mount {dev}")))
        }
    }
}

/// Unmount `dev`, retrying for a while if something still holds it open.
pub fn unmount(dev: &str) -> Result<()> {
    let mut last = None;
    for attempt in 0..6 {
        if mountpoint_of(dev).is_none() {
            return Ok(());
        }
        match udisksctl(&["unmount", "-b", dev]) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
        sleep(Duration::from_millis(500 * (attempt + 1)));
    }
    if mountpoint_of(dev).is_none() {
        return Ok(());
    }
    Err(last.unwrap().context(format!("cannot unmount {dev}")))
}
