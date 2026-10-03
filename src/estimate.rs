//! Progress and ETA for one import.
//!
//! An import is a fixed sequence of stages, each with an amount of work in
//! its own unit (bytes, or seconds of video for the black filler) and a rate.
//! The rate starts from the calibration and is pulled towards what the stage
//! actually achieves while it runs, so the time left is
//!
//! ```text
//! sum over stages of (work left / rate)
//! ```
//!
//! and the progress fraction is the share of estimated time already spent.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::paths::Calibration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageKind {
    /// Copying photos and videos off the card (bytes).
    Copy,
    /// Decoding every video once to find damaged files (bytes).
    Check,
    /// Encoding black filler for damaged files (seconds of video).
    Black,
    /// Stream-copy concatenation of the segments (bytes).
    Join,
    /// Decoding outputs that contain black filler once more (bytes).
    Verify,
}

impl StageKind {
    pub const ALL: [StageKind; 5] = [
        StageKind::Copy,
        StageKind::Check,
        StageKind::Black,
        StageKind::Join,
        StageKind::Verify,
    ];

    fn index(self) -> usize {
        self as usize
    }

    pub fn key(self) -> &'static str {
        match self {
            StageKind::Copy => "copy",
            StageKind::Check => "check",
            StageKind::Black => "black",
            StageKind::Join => "join",
            StageKind::Verify => "verify",
        }
    }

    /// Measured on the reference machine; see `Calibration`.
    pub fn default_rate(self) -> f64 {
        match self {
            StageKind::Copy => 40e6,
            StageKind::Check => 100e6,
            StageKind::Black => 4.0,
            StageKind::Join => 130e6,
            StageKind::Verify => 100e6,
        }
    }

    /// Whether the unit is bytes (so the rate can be shown as MB/s).
    pub fn is_bytes(self) -> bool {
        self != StageKind::Black
    }
}

/// How many seconds of the calibrated rate a measurement has to outweigh.
const PRIOR_SECONDS: f64 = 8.0;
/// Window for the instantaneous transfer rate shown in the UI.
const RATE_WINDOW: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
struct Stage {
    total: f64,
    done: f64,
    /// Work done while this stage was the running one (for its rate).
    measured: f64,
    prior: f64,
    active: Duration,
}

impl Stage {
    fn rate(&self) -> f64 {
        let t = self.active.as_secs_f64();
        (self.measured + self.prior * PRIOR_SECONDS) / (t + PRIOR_SECONDS)
    }
}

#[derive(Clone, Debug)]
pub struct Estimator {
    stages: Vec<Stage>,
    current: Option<StageKind>,
    since: Instant,
    window: VecDeque<(Instant, f64)>,
    shown: f64,
}

impl Estimator {
    pub fn new(cal: &Calibration) -> Self {
        Estimator {
            stages: StageKind::ALL
                .iter()
                .map(|&k| Stage {
                    total: 0.0,
                    done: 0.0,
                    measured: 0.0,
                    prior: cal.rate(k),
                    active: Duration::ZERO,
                })
                .collect(),
            current: None,
            since: Instant::now(),
            window: VecDeque::new(),
            shown: 0.0,
        }
    }

    fn account(&mut self) {
        let now = Instant::now();
        if let Some(k) = self.current {
            self.stages[k.index()].active += now - self.since;
        }
        self.since = now;
    }

    /// Make `kind` the stage that is running now (time is charged to it).
    pub fn enter(&mut self, kind: Option<StageKind>) {
        self.account();
        if self.current != kind {
            self.window.clear();
        }
        self.current = kind;
    }

    pub fn current(&self) -> Option<StageKind> {
        self.current
    }

    pub fn set_total(&mut self, kind: StageKind, total: f64) {
        let s = &mut self.stages[kind.index()];
        s.total = total.max(0.0);
        s.done = s.done.min(s.total);
    }

    pub fn total(&self, kind: StageKind) -> f64 {
        self.stages[kind.index()].total
    }

    pub fn done(&self, kind: StageKind) -> f64 {
        self.stages[kind.index()].done
    }

    pub fn advance(&mut self, kind: StageKind, amount: f64) {
        let s = &mut self.stages[kind.index()];
        s.done += amount;
        if Some(kind) == self.current {
            s.measured += amount;
            let now = Instant::now();
            self.window.push_back((now, amount));
            while let Some(&(t, _)) = self.window.front() {
                if now - t > RATE_WINDOW {
                    self.window.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Mark a stage finished (also used when it turns out to be smaller).
    pub fn finish(&mut self, kind: StageKind) {
        let s = &mut self.stages[kind.index()];
        s.total = s.done;
    }

    fn rate(&self, kind: StageKind) -> f64 {
        let mut s = self.stages[kind.index()].clone();
        if Some(kind) == self.current {
            s.active += self.since.elapsed();
        }
        s.rate()
    }

    /// Estimated seconds until the whole import is finished.
    pub fn remaining_secs(&self) -> f64 {
        StageKind::ALL
            .iter()
            .map(|&k| {
                let s = &self.stages[k.index()];
                (s.total - s.done).max(0.0) / self.rate(k)
            })
            .sum()
    }

    /// Share of the estimated total time already done, never going backwards.
    pub fn fraction(&mut self) -> f64 {
        let (mut done, mut total) = (0.0, 0.0);
        for &k in &StageKind::ALL {
            let s = &self.stages[k.index()];
            let r = self.rate(k);
            done += s.done.min(s.total) / r;
            total += s.total / r;
        }
        let f = if total > 0.0 { done / total } else { 0.0 };
        self.shown = self.shown.max(f.clamp(0.0, 1.0));
        self.shown
    }

    pub fn complete(&mut self) {
        self.shown = 1.0;
    }

    /// Recent throughput of the running stage (units per second).
    pub fn current_rate(&self) -> Option<(StageKind, f64)> {
        let k = self.current?;
        let first = self.window.front()?.0;
        let span = first.elapsed().as_secs_f64().max(0.5);
        let sum: f64 = self.window.iter().map(|&(_, a)| a).sum();
        Some((k, sum / span))
    }

    /// Rates of the stages that ran long enough to be worth remembering.
    pub fn measured(&self) -> Vec<(StageKind, f64)> {
        StageKind::ALL
            .iter()
            .filter_map(|&k| {
                let s = &self.stages[k.index()];
                let t = s.active.as_secs_f64();
                (t > 5.0 && s.measured > 0.0).then(|| (k, s.measured / t))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_uses_calibrated_rates() {
        let cal = Calibration::default();
        let mut e = Estimator::new(&cal);
        e.set_total(StageKind::Copy, 400e6); // 10 s at 40 MB/s
        e.set_total(StageKind::Check, 1000e6); // 10 s at 100 MB/s
        assert!((e.remaining_secs() - 20.0).abs() < 1e-6);
        e.advance(StageKind::Copy, 400e6);
        let f = e.fraction();
        assert!((f - 0.5).abs() < 1e-6, "{f}");
    }

    #[test]
    fn fraction_never_decreases() {
        let mut e = Estimator::new(&Calibration::default());
        e.set_total(StageKind::Copy, 100.0);
        e.advance(StageKind::Copy, 50.0);
        let a = e.fraction();
        e.set_total(StageKind::Join, 1e12);
        assert!(e.fraction() >= a);
    }
}
