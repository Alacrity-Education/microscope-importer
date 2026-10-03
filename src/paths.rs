//! Where things go, and the persisted speed calibration used for the ETA.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::estimate::StageKind;

/// Hidden folder under the videos directory that receives the camera's
/// segment files before they are stitched. One subfolder per SD card.
pub const ORIGINALS_DIR: &str = ".originalframes";

#[derive(Clone, Debug)]
pub struct Paths {
    /// ~/Videos/Microscope - stitched recordings.
    pub videos: PathBuf,
    /// ~/Pictures/Microscope - photos.
    pub pictures: PathBuf,
    /// ~/.local/state/microscope-importer - log and calibration.
    pub state: PathBuf,
}

impl Paths {
    /// Default locations; MICROSCOPE_IMPORTER_VIDEOS / _PICTURES override the
    /// two destinations (used for testing without touching the real library).
    pub fn from_env() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/state"))
            .join("microscope-importer");
        let videos = std::env::var_os("MICROSCOPE_IMPORTER_VIDEOS")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Videos/Microscope"));
        let pictures = std::env::var_os("MICROSCOPE_IMPORTER_PICTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Pictures/Microscope"));
        Ok(Paths {
            videos,
            pictures,
            state,
        })
    }

    pub fn originals(&self) -> PathBuf {
        self.videos.join(ORIGINALS_DIR)
    }

    pub fn log_file(&self) -> PathBuf {
        self.state.join("import.log")
    }

    fn calibration_file(&self) -> PathBuf {
        self.state.join("calibration")
    }

    /// Show a path relative to $HOME as "~/...".
    pub fn pretty(path: &std::path::Path) -> String {
        if let Some(home) = std::env::var_os("HOME") {
            if let Ok(rest) = path.strip_prefix(&home) {
                return format!("~/{}", rest.display());
            }
        }
        path.display().to_string()
    }
}

/// Throughput of every stage of an import, in stage units per second (bytes
/// per second, except for the black filler which is seconds of video encoded
/// per second). The defaults were measured on a 24-core desktop with 4K60
/// H.264 footage at 41 Mbit/s; every finished import refines them.
#[derive(Clone, Debug)]
pub struct Calibration {
    rates: BTreeMap<&'static str, f64>,
}

impl Default for Calibration {
    fn default() -> Self {
        let rates = StageKind::ALL
            .iter()
            .map(|k| (k.key(), k.default_rate()))
            .collect();
        Calibration { rates }
    }
}

impl Calibration {
    pub fn rate(&self, kind: StageKind) -> f64 {
        self.rates[kind.key()]
    }

    pub fn load(paths: &Paths) -> Self {
        let mut cal = Calibration::default();
        if let Ok(text) = fs::read_to_string(paths.calibration_file()) {
            for line in text.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                    if let Some(kind) = StageKind::ALL.iter().find(|k| k.key() == key) {
                        if let Ok(v) = value.parse::<f64>() {
                            if v.is_finite() && v > 0.0 {
                                cal.rates.insert(kind.key(), v);
                            }
                        }
                    }
                }
            }
        }
        cal
    }

    /// Blend freshly measured rates into the stored ones and save.
    pub fn learn(&mut self, measured: &[(StageKind, f64)], paths: &Paths) {
        for &(kind, rate) in measured {
            if rate.is_finite() && rate > 0.0 {
                let old = self.rate(kind);
                self.rates.insert(kind.key(), 0.5 * old + 0.5 * rate);
            }
        }
        let text: String = self
            .rates
            .iter()
            .map(|(k, v)| format!("{k} {v:.1}\n"))
            .collect();
        let _ = fs::create_dir_all(&paths.state);
        let tmp = paths.calibration_file().with_extension("tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(tmp, paths.calibration_file());
        }
    }
}
