//! Stitching the camera's fixed-length segments back into recordings.
//!
//! The microscope writes `YYYYMMDDhhmmss_NNNNNNX.MP4` (start time, file
//! counter, channel letter) and cuts a new file every 1, 2, 3, 5, 10, 15 or 20
//! minutes. Two files belong to the same recording when
//!
//!   * the earlier one has the full segment length (detected per recording),
//!   * the counter goes up by exactly one and the channel letter matches,
//!   * the next one starts at most 2 s after the earlier one ends, and
//!   * both have identical stream parameters.
//!
//! The counter condition also protects against a camera whose clock was
//! reset: footage of two sessions can never interleave.
//!
//! Recordings are joined with ffmpeg's concat demuxer as a stream copy: the
//! output carries the camera's packets unchanged. Every input is decoded once
//! beforehand; a damaged file is replaced by black video and silence of the
//! same length, with a note naming the file, encoded to match the camera's
//! stream parameters so it splices in. The damaged file itself is placed
//! unchanged next to the recordings as well.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::estimate::StageKind;
use crate::util::{self, days_from_civil, log, unique_path};

/// Segment lengths the camera can be set to, in seconds.
pub const SEGMENT_LENGTHS: [f64; 7] = [60.0, 120.0, 180.0, 300.0, 600.0, 900.0, 1200.0];
/// How far a full segment's duration may be off its nominal length.
pub const SEGMENT_TOLERANCE: f64 = 1.5;
/// Largest gap between two files of one recording, in seconds.
pub const MAX_GAP: f64 = 2.0;
/// First line of the note shown over the black filler.
pub const BLACK_NOTE: &str = "Source file - check original footage";
/// Names of the per-card ledgers kept next to the originals.
pub const PROCESSED_LEDGER: &str = "processed.txt";
pub const DAMAGED_LEDGER: &str = "damaged.txt";
/// Parallel decode checks; each ffmpeg is multi-threaded already.
const CHECK_WORKERS: usize = 3;

/// Error used to unwind an import the user cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What a camera file name says about the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraName {
    /// The 14 digits as written.
    pub stamp: String,
    /// Seconds since the epoch, camera clock (time zone irrelevant).
    pub start: i64,
    pub counter: u32,
    pub channel: String,
}

/// Parse `YYYYMMDDhhmmss_NNNNNNX.mp4` (case-insensitive extension, the
/// channel letter is optional).
pub fn parse_name(name: &str) -> Option<CameraName> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".mp4")?;
    let (stamp, rest) = stem.split_once('_')?;
    if stamp.len() != 14 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits != 6 {
        return None;
    }
    let channel = &rest[6..];
    if channel.len() > 1 || !channel.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| stamp[r].parse::<u32>().unwrap();
    let (y, mo, d, h, mi, s) = (n(0..4), n(4..6), n(6..8), n(8..10), n(10..12), n(12..14));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let start = days_from_civil(y as i64, mo, d) * 86400 + (h * 3600 + mi * 60 + s) as i64;
    Some(CameraName {
        stamp: stamp.to_owned(),
        start,
        counter: rest[..6].parse().ok()?,
        channel: channel.to_ascii_uppercase(),
    })
}

/// The nominal segment length a duration corresponds to, if any.
pub fn segment_length(duration: f64) -> Option<f64> {
    SEGMENT_LENGTHS
        .iter()
        .copied()
        .find(|l| (duration - l).abs() <= SEGMENT_TOLERANCE)
}

/// What ffprobe says about a file.
#[derive(Clone, Debug)]
pub struct Probe {
    pub duration: f64,
    /// Codec parameters of all streams; files only join when they match.
    pub signature: String,
    pub video: Value,
    pub audio: Option<Value>,
    /// Number of video frames from the container index, if present.
    pub frames: Option<u64>,
}

pub fn probe(path: &Path) -> Option<Probe> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let info: Value = serde_json::from_slice(&out.stdout).ok()?;
    let duration: f64 = info["format"]["duration"].as_str()?.parse().ok()?;
    let streams = info["streams"].as_array()?;
    let video = streams.iter().find(|s| s["codec_type"] == "video")?.clone();
    let audio = streams.iter().find(|s| s["codec_type"] == "audio").cloned();
    let keys = [
        "codec_type",
        "codec_name",
        "profile",
        "width",
        "height",
        "pix_fmt",
        "r_frame_rate",
        "sample_rate",
        "channels",
    ];
    let signature = streams
        .iter()
        .map(|s| {
            keys.iter()
                .map(|k| s[*k].to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join(";");
    let frames = video["nb_frames"].as_str().and_then(|s| s.parse().ok());
    Some(Probe {
        duration,
        signature,
        video,
        audio,
        frames,
    })
}

/// One video file waiting to be stitched.
#[derive(Clone, Debug)]
pub struct Clip {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub cam: Option<CameraName>,
    pub probe: Option<Probe>,
    /// Decoding reported errors.
    pub bad: bool,
}

impl Clip {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Clip {
            size: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            cam: parse_name(&name),
            probe: None,
            bad: false,
            name,
            path,
        }
    }

    fn duration(&self) -> f64 {
        self.probe.as_ref().map_or(0.0, |p| p.duration)
    }

    fn end(&self) -> f64 {
        self.cam.as_ref().map_or(0.0, |c| c.start as f64) + self.duration()
    }
}

/// Split clips into recordings. Clips without a camera name or without
/// probe data stay on their own. Returns groups of indices into `clips`,
/// in chronological order.
pub fn group(clips: &[Clip]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..clips.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |c: &Clip| {
            c.cam
                .as_ref()
                .map(|n| (0, n.start, n.counter, c.name.clone()))
                .unwrap_or((1, 0, 0, c.name.clone()))
        };
        key(&clips[a]).cmp(&key(&clips[b]))
    });
    let mut groups: Vec<Vec<usize>> = Vec::new();
    // Per channel: index into `groups` of the recording that is still open.
    let mut open: HashMap<String, usize> = HashMap::new();
    for i in order {
        let clip = &clips[i];
        let (Some(cam), Some(probe)) = (&clip.cam, &clip.probe) else {
            groups.push(vec![i]);
            continue;
        };
        if let Some(&g) = open.get(&cam.channel) {
            let chain = &groups[g];
            let prev = &clips[*chain.last().unwrap()];
            let first = &clips[chain[0]];
            let seg = segment_length(prev.duration());
            let prev_cam = prev.cam.as_ref().unwrap();
            let gap = cam.start as f64 - prev.end();
            let joins = seg.is_some()
                && (chain.len() == 1 || seg == segment_length(first.duration()))
                && cam.counter == prev_cam.counter + 1
                && gap.abs() <= MAX_GAP
                && prev.probe.as_ref().map(|p| &p.signature) == Some(&probe.signature);
            if joins {
                groups[g].push(i);
                continue;
            }
        }
        open.insert(cam.channel.clone(), groups.len());
        groups.push(vec![i]);
    }
    groups
}

/// File name of a recording: `YYYY-MM-DD_hh-mm-ss_<first>-<last>.mp4`; a
/// file that is not a camera segment keeps its own name.
pub fn output_name(clips: &[&Clip]) -> String {
    let first = clips[0];
    let Some(cam) = &first.cam else {
        return first.name.clone();
    };
    let s = &cam.stamp;
    let mut name = format!(
        "{}-{}-{}_{}-{}-{}_{:06}{}",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[8..10],
        &s[10..12],
        &s[12..14],
        cam.counter,
        cam.channel
    );
    if clips.len() > 1 {
        if let Some(last) = &clips[clips.len() - 1].cam {
            name += &format!("-{:06}{}", last.counter, last.channel);
        }
    }
    name + ".mp4"
}

/// Receives progress from the stitcher.
pub trait Progress: Sync {
    fn stage(&self, kind: Option<StageKind>);
    fn set_total(&self, kind: StageKind, total: f64);
    fn advance(&self, kind: StageKind, amount: f64);
    fn detail(&self, text: String);
}

/// Outcome of stitching one card.
#[derive(Debug, Default)]
pub struct Report {
    pub outputs: Vec<PathBuf>,
    /// Decoded with errors; replaced by black filler.
    pub damaged: Vec<String>,
    /// Could not be used at all; left out of every recording.
    pub unreadable: Vec<String>,
    /// Damaged and unreadable files placed unchanged next to the
    /// recordings, so whatever is still playable in them is at hand.
    pub as_is: Vec<PathBuf>,
}

struct FfOutput {
    success: bool,
    stderr: String,
}

/// Run ffmpeg, reporting the output timestamp it has reached (seconds).
fn run_ffmpeg(
    args: &[OsString],
    cancel: &AtomicBool,
    mut on_time: impl FnMut(f64),
) -> Result<FfOutput> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-nostats",
            "-progress",
            "pipe:1",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run ffmpeg (is it installed?)")?;
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf.truncate(64 * 1024);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = err_reader.join();
            return Err(Cancelled.into());
        }
        let Ok(line) = line else { break };
        if let Some(us) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = us.trim().parse::<i64>() {
                on_time(us.max(0) as f64 / 1e6);
            }
        }
    }
    let status = child.wait()?;
    let stderr = err_reader.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(FfOutput {
        success: status.success(),
        stderr,
    })
}

/// Fully decode a file; any reported error means it is damaged.
fn decodes_cleanly(
    path: &Path,
    duration: f64,
    cancel: &AtomicBool,
    mut on_fraction: impl FnMut(f64),
) -> Result<bool> {
    let args: Vec<OsString> = vec![
        "-i".into(),
        path.into(),
        "-map".into(),
        "0".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    let out = run_ffmpeg(&args, cancel, |t| {
        on_fraction((t / duration.max(0.001)).clamp(0.0, 1.0))
    })?;
    if !out.stderr.trim().is_empty() {
        log(format!(
            "decode errors in {}: {}",
            path.display(),
            out.stderr.lines().take(3).collect::<Vec<_>>().join(" | ")
        ));
    }
    Ok(out.success && out.stderr.trim().is_empty())
}

/// Whether a damaged clip can be replaced by black filler that splices into
/// a stream copy (H.264 video, AAC or no audio).
fn black_supported(clip: &Clip) -> bool {
    let Some(p) = &clip.probe else { return false };
    p.video["codec_name"] == "h264" && p.audio.as_ref().is_none_or(|a| a["codec_name"] == "aac")
}

fn font_option() -> &'static str {
    static FONT: OnceLock<String> = OnceLock::new();
    FONT.get_or_init(|| {
        let found = Command::new("fc-match")
            .args(["-f", "%{file}", "sans:bold"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .filter(|f| Path::new(f).is_file());
        match found {
            Some(f) => format!("fontfile='{}'", f.replace('\'', "")),
            None => "font='Sans'".to_owned(),
        }
    })
}

/// Encode black video + silence matching `clip`, with the note over it.
fn make_black(
    clip: &Clip,
    target: &Path,
    scratch: &Path,
    cancel: &AtomicBool,
    mut on_secs: impl FnMut(f64),
) -> Result<()> {
    let p = clip.probe.as_ref().context("no stream information")?;
    let v = &p.video;
    let size = format!("{}x{}", v["width"], v["height"]);
    let fps = v["r_frame_rate"].as_str().unwrap_or("30/1");
    let (num, den) = fps.split_once('/').unwrap_or((fps, "1"));
    let gop = (num.parse::<f64>().unwrap_or(30.0) / den.parse::<f64>().unwrap_or(1.0))
        .round()
        .max(1.0);
    let timescale = v["time_base"]
        .as_str()
        .and_then(|t| t.split_once('/'))
        .map_or("90000", |(_, d)| d)
        .to_owned();

    let mut filters = Vec::new();
    for (i, line) in [BLACK_NOTE, clip.name.as_str()].iter().enumerate() {
        let textfile = scratch.join(format!("note{i}.txt"));
        fs::write(&textfile, line)?;
        let y = if i == 0 {
            "h/2-text_h*1.3"
        } else {
            "h/2+text_h*0.3"
        };
        filters.push(format!(
            "drawtext={}:textfile='{}':fontcolor=white:fontsize=h/18:x=(w-text_w)/2:y={y}",
            font_option(),
            textfile.display()
        ));
    }

    let mut args: Vec<OsString> = vec![
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("color=black:s={size}:r={fps}").into(),
    ];
    if let Some(a) = &p.audio {
        let channels = a["channels"].as_u64().unwrap_or(1);
        let layout = match channels {
            1 => "mono".to_owned(),
            2 => "stereo".to_owned(),
            n => format!("{n}c"),
        };
        let rate = a["sample_rate"].as_str().unwrap_or("48000");
        args.extend(
            [
                "-f",
                "lavfi",
                "-i",
                &format!("anullsrc=r={rate}:cl={layout}"),
            ]
            .map(OsString::from),
        );
    }
    args.extend(
        [
            "-filter_complex",
            &format!("[0:v]{}[v]", filters.join(",")),
            "-map",
            "[v]",
            "-t",
            &format!("{:.6}", p.duration),
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "stillimage",
            "-pix_fmt",
            v["pix_fmt"].as_str().unwrap_or("yuv420p"),
            "-bf",
            "0",
            "-g",
            &format!("{gop}"),
            // Every keyframe carries its own SPS/PPS, so the filler decodes
            // with its own parameter sets inside the camera's stream.
            "-x264-params",
            "repeat-headers=1",
            "-video_track_timescale",
            &timescale,
        ]
        .map(OsString::from),
    );
    let profile = v["profile"].as_str().unwrap_or("").to_ascii_lowercase();
    let profile = if profile.contains("baseline") {
        Some("baseline")
    } else if profile.starts_with("main") {
        Some("main")
    } else if profile.starts_with("high") {
        Some("high")
    } else {
        None
    };
    if let Some(profile) = profile {
        args.extend(["-profile:v", profile].map(OsString::from));
    }
    for (opt, key) in [
        ("-color_range", "color_range"),
        ("-colorspace", "color_space"),
        ("-color_primaries", "color_primaries"),
        ("-color_trc", "color_transfer"),
    ] {
        if let Some(val) = v[key].as_str().filter(|s| *s != "unknown") {
            args.extend([opt, val].map(OsString::from));
        }
    }
    if p.audio.is_some() {
        args.extend(["-map", "1:a", "-c:a", "aac", "-b:a", "64k"].map(OsString::from));
    }
    args.push(target.into());

    let mut last = 0.0;
    let out = run_ffmpeg(&args, cancel, |t| {
        let t = t.min(p.duration);
        on_secs(t - last);
        last = t;
    })?;
    on_secs(p.duration - last);
    if !out.success {
        bail!(
            "cannot encode black filler for {}: {}",
            clip.name,
            out.stderr.trim()
        );
    }
    Ok(())
}

/// Serialises the choice of output names between concurrent imports.
static NAME_LOCK: Mutex<()> = Mutex::new(());
static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Pick a free output name and claim it with an empty placeholder file.
fn reserve_output(dir: &Path, name: &str) -> Result<PathBuf> {
    let _guard = NAME_LOCK.lock().unwrap();
    let path = unique_path(dir, name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    Ok(path)
}

fn link_or_copy(src: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let _guard = NAME_LOCK.lock().unwrap();
    let target = unique_path(dir, name);
    if fs::hard_link(src, &target).is_err() {
        fs::copy(src, &target)
            .with_context(|| format!("cannot copy {} to {}", src.display(), target.display()))?;
    }
    Ok(target)
}

fn set_mtime_like(target: &Path, source: &Path) {
    if let Ok(t) = fs::metadata(source).and_then(|m| m.modified()) {
        if let Ok(f) = fs::OpenOptions::new().write(true).open(target) {
            let _ = f.set_modified(t);
        }
    }
}

/// Removes a scratch directory and partial output however we leave.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            if p.is_dir() {
                let _ = fs::remove_dir_all(p);
            } else {
                let _ = fs::remove_file(p);
            }
        }
    }
}

/// Join one recording (possibly with black filler) into `target`.
fn join(
    clips: &[&Clip],
    target: &Path,
    out_dir: &Path,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<()> {
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let scratch = out_dir.join(format!(".stitch-{}-{seq}", std::process::id()));
    fs::create_dir_all(&scratch)?;
    let tmp = out_dir.join(format!(
        ".{}.partial.mp4",
        target.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let _cleanup = Cleanup(vec![scratch.clone(), tmp.clone()]);

    let mut list = String::new();
    let mut frames: Option<u64> = Some(0);
    let mut has_black = false;
    for (i, clip) in clips.iter().enumerate() {
        let source = if clip.bad {
            has_black = true;
            progress.stage(Some(StageKind::Black));
            progress.detail(format!("Black filler for damaged {}", clip.name));
            let black = scratch.join(format!("black{i}.mp4"));
            make_black(clip, &black, &scratch, cancel, |s| {
                progress.advance(StageKind::Black, s)
            })?;
            black
        } else {
            frames = frames
                .zip(clip.probe.as_ref().and_then(|p| p.frames))
                .map(|(a, b)| a + b);
            clip.path.clone()
        };
        let escaped = source.to_string_lossy().replace('\'', r"'\''");
        list += &format!("file '{escaped}'\n");
    }
    let list_file = scratch.join("list.txt");
    fs::write(&list_file, list)?;

    let expected: f64 = clips.iter().map(|c| c.duration()).sum();
    let bytes: f64 = clips.iter().map(|c| c.size as f64).sum();
    progress.stage(Some(StageKind::Join));
    progress.detail(format!("Joining {} files", clips.len()));
    let args: Vec<OsString> = vec![
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_file.into(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-map_metadata".into(),
        "0".into(),
        tmp.clone().into(),
    ];
    let mut reported = 0.0;
    let out = run_ffmpeg(&args, cancel, |t| {
        let now = (t / expected.max(0.001)).clamp(0.0, 1.0) * bytes;
        progress.advance(StageKind::Join, now - reported);
        reported = now;
    })?;
    progress.advance(StageKind::Join, bytes - reported);
    if !out.success {
        bail!(
            "ffmpeg could not join {}: {}",
            clips[0].name,
            out.stderr.trim()
        );
    }

    // The stream copy must have kept every second and every frame.
    let result = probe(&tmp).context("joined file is unreadable")?;
    let tolerance = 1.0 + 0.02 * clips.len() as f64;
    if (result.duration - expected).abs() > tolerance {
        bail!(
            "joined file lasts {:.2}s, expected {:.2}s",
            result.duration,
            expected
        );
    }
    if has_black {
        // The filler brings its own parameter sets; decode the whole result
        // once to be sure the splice is clean.
        progress.stage(Some(StageKind::Verify));
        progress.detail("Verifying joined recording".into());
        let good: f64 = clips.iter().filter(|c| !c.bad).map(|c| c.size as f64).sum();
        let mut reported = 0.0;
        let ok = decodes_cleanly(&tmp, result.duration, cancel, |f| {
            progress.advance(StageKind::Verify, f * good - reported);
            reported = f * good;
        })?;
        progress.advance(StageKind::Verify, good - reported);
        if !ok {
            bail!("joined recording {} has decode errors", clips[0].name);
        }
    } else if let (Some(want), Some(got)) = (frames, result.frames) {
        if want != got {
            bail!("joined file has {got} frames, expected {want}");
        }
    }
    fs::rename(&tmp, target)?;
    Ok(())
}

/// Stitch the given segment files (already copied off the card into
/// `originals`) into recordings in `out_dir`. The per-card ledgers in
/// `originals` are updated as recordings are completed.
pub fn stitch(
    files: &[PathBuf],
    originals: &Path,
    out_dir: &Path,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<Report> {
    let mut report = Report::default();
    let mut clips: Vec<Clip> = files.iter().cloned().map(Clip::new).collect();

    progress.detail("Reading video headers".into());
    for clip in &mut clips {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        clip.probe = probe(&clip.path);
    }

    // Decode everything once, a few files at a time.
    progress.stage(Some(StageKind::Check));
    progress.detail(format!("Checking videos 0/{}", clips.len()));
    progress.set_total(StageKind::Check, clips.iter().map(|c| c.size as f64).sum());
    let next = AtomicUsize::new(0);
    let checked = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<bool>)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..CHECK_WORKERS.min(clips.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(clip) = clips.get(i) else { break };
                let size = clip.size as f64;
                let result = match &clip.probe {
                    None => Ok(false),
                    Some(p) => {
                        let mut reported = 0.0;
                        let r = decodes_cleanly(&clip.path, p.duration, cancel, |f| {
                            progress.advance(StageKind::Check, f * size - reported);
                            reported = f * size;
                        });
                        progress.advance(StageKind::Check, -reported);
                        r
                    }
                };
                progress.advance(StageKind::Check, size);
                let n = checked.fetch_add(1, Ordering::Relaxed) + 1;
                progress.detail(format!("Checking videos {n}/{}", clips.len()));
                let failed = result.is_err();
                results.lock().unwrap().push((i, result));
                if failed {
                    break;
                }
            });
        }
    });
    for (i, result) in results.into_inner().unwrap() {
        clips[i].bad = !result?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }

    // Files that cannot be replaced by filler are left out of every
    // recording; the counter gap they leave splits the recording there.
    let processed = originals.join(PROCESSED_LEDGER);
    let damaged_ledger = originals.join(DAMAGED_LEDGER);
    let mut usable = Vec::new();
    for clip in clips {
        if clip.probe.is_none() || (clip.bad && !black_supported(&clip)) {
            log(format!("unusable video {}", clip.path.display()));
            fs::create_dir_all(out_dir)?;
            report
                .as_is
                .push(link_or_copy(&clip.path, out_dir, &clip.name)?);
            util::append_line(&processed, &clip.name)?;
            util::append_line(&damaged_ledger, &clip.name)?;
            report.unreadable.push(clip.name);
        } else {
            usable.push(clip);
        }
    }
    let clips = usable;
    let groups = group(&clips);

    let needs_join = |g: &Vec<usize>| g.len() > 1 || clips[g[0]].bad;
    let join_total: f64 = groups
        .iter()
        .filter(|g| needs_join(g))
        .flatten()
        .map(|&i| clips[i].size as f64)
        .sum();
    let black_total: f64 = clips.iter().filter(|c| c.bad).map(|c| c.duration()).sum();
    let verify_total: f64 = groups
        .iter()
        .filter(|g| g.iter().any(|&i| clips[i].bad))
        .flatten()
        .filter(|&&i| !clips[i].bad)
        .map(|&i| clips[i].size as f64)
        .sum();
    progress.set_total(StageKind::Join, join_total);
    progress.set_total(StageKind::Black, black_total);
    progress.set_total(StageKind::Verify, verify_total);

    fs::create_dir_all(out_dir)?;
    for (n, g) in groups.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let members: Vec<&Clip> = g.iter().map(|&i| &clips[i]).collect();
        let name = output_name(&members);
        progress.detail(format!("Recording {}/{}: {name}", n + 1, groups.len()));
        let target = if needs_join(g) {
            let target = reserve_output(out_dir, &name)?;
            if let Err(e) = join(&members, &target, out_dir, cancel, progress) {
                let _ = fs::remove_file(&target);
                return Err(e);
            }
            target
        } else {
            link_or_copy(&members[0].path, out_dir, &name)?
        };
        set_mtime_like(&target, &members[members.len() - 1].path);
        log(format!(
            "wrote {} from {} file(s)",
            target.display(),
            members.len()
        ));
        for clip in &members {
            util::append_line(&processed, &clip.name)?;
            if clip.bad {
                // The recording has black in its place; the damaged file
                // itself goes next to it unchanged.
                report
                    .as_is
                    .push(link_or_copy(&clip.path, out_dir, &clip.name)?);
                util::append_line(&damaged_ledger, &clip.name)?;
                report.damaged.push(clip.name.clone());
            }
        }
        report.outputs.push(target);
    }
    progress.stage(None);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camera_names() {
        let c = parse_name("20281013021439_000003A.MP4").unwrap();
        assert_eq!(c.counter, 3);
        assert_eq!(c.channel, "A");
        assert_eq!(c.stamp, "20281013021439");
        let d = parse_name("20281013021539_000004a.mp4").unwrap();
        assert_eq!(d.start - c.start, 60);
        assert_eq!(d.channel, "A");
        assert!(parse_name("20281013021539_000004.mp4").is_some());
        assert!(parse_name("IMG_0001.MP4").is_none());
        assert!(parse_name("20281013021539_00004A.MP4").is_none());
        assert!(parse_name("20281313021539_000004A.MP4").is_none());
        assert!(parse_name("20281013021539_000004AB.MP4").is_none());
    }

    #[test]
    fn segment_lengths() {
        assert_eq!(segment_length(60.03), Some(60.0));
        assert_eq!(segment_length(1199.0), Some(1200.0));
        assert_eq!(segment_length(45.7), None);
        assert_eq!(segment_length(600.0), Some(600.0));
    }

    fn clip(name: &str, duration: f64) -> Clip {
        Clip {
            path: PathBuf::from(name),
            name: name.into(),
            size: 1,
            cam: parse_name(name),
            probe: Some(Probe {
                duration,
                signature: "h264".into(),
                video: Value::Null,
                audio: None,
                frames: None,
            }),
            bad: false,
        }
    }

    fn names(clips: &[Clip], groups: &[Vec<usize>]) -> Vec<Vec<String>> {
        groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|&i| clips[i].name[15..21].to_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn groups_like_the_prototype() {
        // From the real card: 3..8 is one recording (8 is the short tail),
        // 9 starts 22 s after 8 ends, 10-11 is a recording, 12 is alone.
        let clips = vec![
            clip("20281013021439_000003A.MP4", 60.0),
            clip("20281013021539_000004A.MP4", 60.0),
            clip("20281013021639_000005A.MP4", 60.03),
            clip("20281013021739_000006A.MP4", 60.0),
            clip("20281013021839_000007A.MP4", 60.03),
            clip("20281013021939_000008A.MP4", 25.7),
            clip("20281013022027_000009A.MP4", 2.33),
            clip("20281013022047_000010A.MP4", 60.0),
            clip("20281013022147_000011A.MP4", 45.7),
            clip("20281013022252_000012A.MP4", 25.68),
        ];
        let g = group(&clips);
        assert_eq!(
            names(&clips, &g),
            vec![
                vec!["000003", "000004", "000005", "000006", "000007", "000008"],
                vec!["000009"],
                vec!["000010", "000011"],
                vec!["000012"],
            ]
        );
    }

    #[test]
    fn handles_long_segments_and_one_second_jitter() {
        let clips = vec![
            clip("20260101100000_000001A.MP4", 1200.0),
            clip("20260101102001_000002A.MP4", 1200.0), // +1 s jitter
            clip("20260101104001_000003A.MP4", 300.0),
        ];
        assert_eq!(group(&clips).len(), 1);
    }

    #[test]
    fn does_not_join_across_counter_gaps_or_clock_resets() {
        let clips = vec![
            clip("20260101100000_000001A.MP4", 60.0),
            clip("20260101100100_000003A.MP4", 60.0), // 2 missing
            // Clock reset: same start time as file 1, next counter.
            clip("20260101100000_000004A.MP4", 60.0),
        ];
        assert_eq!(group(&clips).len(), 3);
    }

    #[test]
    fn does_not_mix_segment_lengths_or_formats() {
        let mut clips = vec![
            clip("20260101100000_000001A.MP4", 60.0),
            clip("20260101100100_000002A.MP4", 120.0),
            clip("20260101100300_000003A.MP4", 60.0),
        ];
        assert_eq!(group(&clips).len(), 2, "1-2 join, 3 starts a new one");
        clips[1].probe.as_mut().unwrap().signature = "hevc".into();
        assert_eq!(group(&clips).len(), 3);
    }

    #[test]
    fn names_outputs() {
        let a = clip("20281013021439_000003A.MP4", 60.0);
        let b = clip("20281013021539_000004A.MP4", 60.0);
        assert_eq!(output_name(&[&a]), "2028-10-13_02-14-39_000003A.mp4");
        assert_eq!(
            output_name(&[&a, &b]),
            "2028-10-13_02-14-39_000003A-000004A.mp4"
        );
        let other = clip("holiday.MP4", 10.0);
        assert_eq!(output_name(&[&other]), "holiday.MP4");
    }
}
