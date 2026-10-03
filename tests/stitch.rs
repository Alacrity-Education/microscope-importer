//! End-to-end test of the stitcher on small synthetic camera segments.
//! Skips itself when ffmpeg is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use microscope_importer::estimate::StageKind;
use microscope_importer::stitch::{self, Progress};

fn have_ffmpeg() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|t| {
        Command::new(t)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A camera-like segment: H.264 + AAC mono, `secs` long.
fn segment(dir: &Path, name: &str, secs: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=s=320x240:r=30:d={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=f=440:r=16000:d={secs}"))
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-bf",
            "0",
            "-c:a",
            "aac",
            "-ac",
            "1",
            "-shortest",
        ])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

/// Overwrite a stretch in the middle of the media data with garbage.
fn corrupt(path: &Path) {
    let mut data = fs::read(path).unwrap();
    let mid = data.len() / 2;
    for b in &mut data[mid..mid + 4096] {
        *b = 0xFF;
    }
    fs::write(path, data).unwrap();
}

#[derive(Default)]
struct Recorder {
    advanced: Mutex<Vec<(StageKind, f64)>>,
}

impl Progress for Recorder {
    fn stage(&self, _: Option<StageKind>) {}
    fn set_total(&self, _: StageKind, _: f64) {}
    fn advance(&self, kind: StageKind, amount: f64) {
        self.advanced.lock().unwrap().push((kind, amount));
    }
    fn detail(&self, _: String) {}
}

fn duration(path: &Path) -> f64 {
    stitch::probe(path).unwrap().duration
}

fn decode_errors(path: &Path) -> String {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn joins_segments_and_replaces_damaged_ones_with_black() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed - skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let originals = tmp.path().join(".originalframes/card");
    let out = tmp.path().join("videos");
    fs::create_dir_all(&originals).unwrap();

    // Recording 1: 60 + 60 + 10 s, the middle segment damaged.
    // Recording 2: a lone short clip 30 s after recording 1 ends.
    let files = vec![
        segment(&originals, "20260101100000_000001A.MP4", 60),
        segment(&originals, "20260101100100_000002A.MP4", 60),
        segment(&originals, "20260101100200_000003A.MP4", 10),
        segment(&originals, "20260101100240_000004A.MP4", 5),
    ];
    corrupt(&files[1]);

    let progress = Recorder::default();
    let report =
        stitch::stitch(&files, &originals, &out, &AtomicBool::new(false), &progress).unwrap();

    assert_eq!(
        report.damaged,
        vec!["20260101100100_000002A.MP4".to_owned()]
    );
    assert!(report.unreadable.is_empty());
    let names: Vec<String> = report
        .outputs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "2026-01-01_10-00-00_000001A-000003A.mp4",
            "2026-01-01_10-02-40_000004A.mp4"
        ]
    );
    let joined = &report.outputs[0];
    assert!(
        (duration(joined) - 130.0).abs() < 0.5,
        "{}",
        duration(joined)
    );
    assert_eq!(decode_errors(joined), "", "joined file must decode cleanly");

    // The damaged segment sits unchanged next to the recordings.
    assert_eq!(report.as_is, vec![out.join("20260101100100_000002A.MP4")]);
    assert_eq!(
        fs::read(&report.as_is[0]).unwrap(),
        fs::read(&files[1]).unwrap()
    );

    // The lone clip is a hard link to the original, not a copy.
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::metadata(&report.outputs[1]).unwrap().ino(),
        fs::metadata(&files[3]).unwrap().ino()
    );

    // Ledgers: everything processed, the damaged file recorded.
    let processed = fs::read_to_string(originals.join(stitch::PROCESSED_LEDGER)).unwrap();
    assert_eq!(processed.lines().count(), 4);
    let damaged = fs::read_to_string(originals.join(stitch::DAMAGED_LEDGER)).unwrap();
    assert_eq!(damaged.trim(), "20260101100100_000002A.MP4");

    // Progress was reported for every stage that had work.
    let advanced = progress.advanced.lock().unwrap();
    for kind in [
        StageKind::Check,
        StageKind::Black,
        StageKind::Join,
        StageKind::Verify,
    ] {
        let sum: f64 = advanced
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, a)| a)
            .sum();
        assert!(sum > 0.0, "no progress for {kind:?}");
    }

    // No scratch files left behind.
    let leftovers: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn clean_segments_join_as_a_stream_copy() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed - skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let originals = tmp.path().join("orig");
    let out = tmp.path().join("videos");
    fs::create_dir_all(&originals).unwrap();
    let files = vec![
        segment(&originals, "20260101100000_000001B.MP4", 60),
        segment(&originals, "20260101100100_000002B.MP4", 20),
    ];
    let report = stitch::stitch(
        &files,
        &originals,
        &out,
        &AtomicBool::new(false),
        &Recorder::default(),
    )
    .unwrap();
    assert_eq!(report.outputs.len(), 1);
    assert!(report.damaged.is_empty());
    let joined = &report.outputs[0];
    assert!((duration(joined) - 80.0).abs() < 0.5);
    // Same packets: the size is the sum of the inputs give or take headers.
    let inputs: u64 = files.iter().map(|f| fs::metadata(f).unwrap().len()).sum();
    let size = fs::metadata(joined).unwrap().len();
    assert!(size.abs_diff(inputs) < inputs / 50, "{size} vs {inputs}");
}
