//! One import: mount the card, copy photos, copy videos into the hidden
//! originals folder, unmount, then stitch. Runs on its own thread and
//! publishes its state for the UI.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::devices::Device;
use crate::estimate::{Estimator, StageKind};
use crate::paths::{Calibration, Paths};
use crate::stitch::{self, Cancelled, Progress, PROCESSED_LEDGER};
use crate::udisks;
use crate::util::{self, gb, log};

const PHOTO_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4"];
const COPY_BUFFER: usize = 4 << 20;
/// Space kept free on the destination disk.
const SPACE_MARGIN: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Mounting,
    Scanning,
    CopyingPhotos,
    CopyingVideos,
    /// Deleting the imported files from the card.
    Clearing,
    Unmounting,
    Stitching,
    Done,
    Failed,
    Cancelled,
}

impl Phase {
    pub fn is_finished(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Cancelled)
    }

    /// The card is no longer needed once this phase is reached.
    pub fn card_released(self) -> bool {
        matches!(self, Phase::Stitching | Phase::Done)
    }
}

/// Everything the UI shows about an import.
#[derive(Clone, Debug)]
pub struct JobState {
    pub phase: Phase,
    pub detail: String,
    pub est: Estimator,
    /// Bytes to copy off the card (after skipping what was already there).
    pub copy_total: u64,
    pub copied: u64,
    pub photos: (usize, usize),
    pub videos: (usize, usize),
    /// Files found on the card that had been imported before.
    pub skipped: usize,
    pub unmounted: bool,
    pub unmount_error: Option<String>,
    /// Files deleted from the card after they were imported.
    pub cleared: usize,
    pub clear_error: Option<String>,
    pub error: Option<String>,
    pub recordings: usize,
    pub damaged: Vec<String>,
    pub unreadable: Vec<String>,
    pub finished_at: Option<Instant>,
}

pub struct Job {
    pub device: Device,
    pub state: Mutex<JobState>,
    pub cancel: AtomicBool,
}

impl Job {
    pub fn snapshot(&self) -> JobState {
        self.state.lock().unwrap().clone()
    }

    /// Snapshot plus the overall progress fraction. The fraction is taken on
    /// the job's own estimator so that it never goes backwards on screen.
    pub fn view(&self) -> (JobState, f64) {
        let mut s = self.state.lock().unwrap();
        let fraction = s.est.fraction();
        (s.clone(), fraction)
    }

    pub fn is_finished(&self) -> bool {
        self.state.lock().unwrap().phase.is_finished()
    }

    fn update(&self, f: impl FnOnce(&mut JobState)) {
        f(&mut self.state.lock().unwrap());
    }

    fn set_phase(&self, phase: Phase, detail: impl Into<String>) {
        let detail = detail.into();
        self.update(|s| {
            s.phase = phase;
            s.detail = detail;
        });
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }
}

impl Progress for Job {
    fn stage(&self, kind: Option<StageKind>) {
        self.update(|s| s.est.enter(kind));
    }
    fn set_total(&self, kind: StageKind, total: f64) {
        self.update(|s| s.est.set_total(kind, total));
    }
    fn advance(&self, kind: StageKind, amount: f64) {
        self.update(|s| s.est.advance(kind, amount));
    }
    fn detail(&self, text: String) {
        self.update(|s| s.detail = text);
    }
}

/// Start importing `device` on a new thread.
pub fn start(device: Device, paths: Paths, cal: Arc<Mutex<Calibration>>) -> Arc<Job> {
    let est = Estimator::new(&cal.lock().unwrap());
    let job = Arc::new(Job {
        device,
        cancel: AtomicBool::new(false),
        state: Mutex::new(JobState {
            phase: Phase::Mounting,
            detail: String::new(),
            est,
            copy_total: 0,
            copied: 0,
            photos: (0, 0),
            videos: (0, 0),
            skipped: 0,
            unmounted: false,
            unmount_error: None,
            cleared: 0,
            clear_error: None,
            error: None,
            recordings: 0,
            damaged: Vec::new(),
            unreadable: Vec::new(),
            finished_at: None,
        }),
    });
    let worker = job.clone();
    std::thread::spawn(move || {
        let result = run(&worker, &paths);
        let measured = worker.state.lock().unwrap().est.measured();
        cal.lock().unwrap().learn(&measured, &paths);
        worker.update(|s| {
            s.est.enter(None);
            s.finished_at = Some(Instant::now());
            match result {
                Ok(()) => {
                    s.phase = Phase::Done;
                    s.est.complete();
                }
                Err(e) if e.is::<Cancelled>() => s.phase = Phase::Cancelled,
                Err(e) => {
                    s.phase = Phase::Failed;
                    s.error = Some(format!("{e:#}"));
                }
            }
        });
        let s = worker.snapshot();
        log(format!(
            "{}: finished {:?}{}",
            worker.device.dev,
            s.phase,
            s.error.map(|e| format!(": {e}")).unwrap_or_default()
        ));
    });
    job
}

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// All photos and videos on the card, in path order. Hidden entries (".*",
/// including macOS "._" resource forks and ".Trashes") are skipped, as are
/// symbolic links.
fn scan(root: &Path, job: &Job) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (mut photos, mut videos) = (Vec::new(), Vec::new());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        job.check_cancel()?;
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if dir == root => {
                return Err(e).with_context(|| format!("cannot read {}", root.display()))
            }
            Err(e) => {
                log(format!("skipping {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                if has_extension(&path, PHOTO_EXTENSIONS) {
                    photos.push(path);
                } else if has_extension(&path, VIDEO_EXTENSIONS) {
                    videos.push(path);
                }
            }
        }
    }
    photos.sort();
    videos.sort();
    Ok((photos, videos))
}

/// One photo or video found on the card.
struct Source {
    src: PathBuf,
    name: String,
    size: u64,
}

fn sources(paths: &[PathBuf]) -> Result<Vec<Source>> {
    paths
        .iter()
        .map(|src| {
            let meta =
                fs::metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
            Ok(Source {
                src: src.clone(),
                name: src.file_name().unwrap().to_string_lossy().into_owned(),
                size: meta.len(),
            })
        })
        .collect()
}

/// `name`, then `stem_1.ext`, `stem_2.ext`... for a file whose name is
/// taken by a different file.
fn candidate(name: &str, n: usize) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{}_{n}{}", &name[..i], &name[i..]),
        _ => format!("{name}_{n}"),
    }
}

/// BLAKE3 of a file. `on_bytes` is told about every chunk read, so hashing
/// a file on the card shows up as transfer progress.
fn checksum(
    path: &Path,
    job: &Job,
    buf: &mut [u8],
    mut on_bytes: impl FnMut(u64),
) -> Result<blake3::Hash> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    loop {
        job.check_cancel()?;
        let n = f
            .read(buf)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        on_bytes(n as u64);
    }
    Ok(hasher.finalize())
}

/// Space a set of files will take in `dest`, not counting files whose name
/// is already there with the same size (most likely the same file - the
/// checksum decides at copy time).
fn bytes_needed(sources: &[Source], dest: &Path) -> u64 {
    sources
        .iter()
        .filter(|s| fs::metadata(dest.join(&s.name)).map_or(true, |m| m.len() != s.size))
        .map(|s| s.size)
        .sum()
}

/// What happened to one file.
enum Outcome {
    Copied(PathBuf),
    /// An identical file (same checksum) was already there.
    Duplicate(PathBuf),
}

/// Bring one file into `dest`. When the name is taken, the checksums decide:
/// the same file is skipped, a different one gets `_1`, `_2`... appended.
fn import_file(job: &Job, item: &Source, dest: &Path, buf: &mut [u8]) -> Result<Outcome> {
    let mut source_hash = None;
    for n in 0.. {
        let dst = dest.join(candidate(&item.name, n));
        let Ok(meta) = fs::metadata(&dst) else {
            copy_file(job, item, &dst, buf)?;
            return Ok(Outcome::Copied(dst));
        };
        if meta.len() != item.size {
            continue; // different size, certainly a different file
        }
        let ours = match source_hash {
            Some(h) => h,
            None => {
                // Reading the card counts as transfer; if this turns out to
                // be a new file, the copy below reads it once more.
                let h = checksum(&item.src, job, buf, |n| {
                    job.update(|s| {
                        s.copied += n;
                        s.est.advance(StageKind::Copy, n as f64);
                    })
                })?;
                source_hash = Some(h);
                h
            }
        };
        if checksum(&dst, job, buf, |_| {})? == ours {
            return Ok(Outcome::Duplicate(dst));
        }
    }
    unreachable!()
}

fn check_space(needs: &[(&Path, u64)]) -> Result<()> {
    let mut per_fs: Vec<(u64, u64, u64, &Path)> = Vec::new(); // fsid, free, need, path
    for &(path, bytes) in needs {
        let (free, fsid) = util::free_space(path)
            .with_context(|| format!("cannot query free space on {}", path.display()))?;
        match per_fs.iter_mut().find(|e| e.0 == fsid) {
            Some(e) => e.2 += bytes,
            None => per_fs.push((fsid, free, bytes, path)),
        }
    }
    for (_, free, need, path) in per_fs {
        if need + SPACE_MARGIN > free {
            bail!(
                "not enough space on {}: {} needed, {} free",
                path.display(),
                gb(need + SPACE_MARGIN),
                gb(free)
            );
        }
    }
    Ok(())
}

/// Copy one file through a hidden ".part" name and keep the card's
/// modification time. The data is hashed on the way; once it is flushed to
/// disk it is dropped from the page cache and read back, and only a copy
/// whose checksum matches gets its real name - the card's file may be
/// deleted afterwards.
fn copy_file(job: &Job, item: &Source, dst: &Path, buf: &mut [u8]) -> Result<()> {
    let dir = dst.parent().unwrap();
    let part = dir.join(format!(
        ".{}.part",
        dst.file_name().unwrap().to_string_lossy()
    ));
    let result = (|| -> Result<()> {
        let mut src = File::open(&item.src)
            .with_context(|| format!("cannot open {} (card removed?)", item.src.display()))?;
        let mut out =
            File::create(&part).with_context(|| format!("cannot create {}", part.display()))?;
        let mut copied = 0u64;
        let mut hasher = blake3::Hasher::new();
        loop {
            job.check_cancel()?;
            let n = src
                .read(buf)
                .with_context(|| format!("cannot read {} (card removed?)", item.src.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .with_context(|| format!("cannot write {}", part.display()))?;
            copied += n as u64;
            job.update(|s| {
                s.copied += n as u64;
                s.est.advance(StageKind::Copy, n as f64);
            });
        }
        if copied != item.size {
            bail!("{} changed size while copying", item.src.display());
        }
        if let Ok(t) = fs::metadata(&item.src).and_then(|m| m.modified()) {
            let _ = out.set_modified(t);
        }
        out.sync_all()?;
        drop_cache(&out);
        drop(out);
        if checksum(&part, job, buf, |_| {})? != hasher.finalize() {
            bail!("verification of {} failed: the copy differs", dst.display());
        }
        fs::rename(&part, dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// Ask the kernel to forget the cached pages of a file that was just
/// flushed, so reading it back reads what is on the disk.
fn drop_cache(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// Delete imported files from the card. Returns how many were deleted and
/// the first error, if any (a write-protected card is not a failed import).
fn clear_card(job: &Job, files: &[PathBuf]) -> (usize, Option<String>) {
    let mut deleted = 0;
    let mut first_error = None;
    for (i, f) in files.iter().enumerate() {
        job.update(|s| s.detail = format!("{}/{}", i + 1, files.len()));
        match fs::remove_file(f) {
            Ok(()) => deleted += 1,
            Err(e) => {
                log(format!("cannot delete {}: {e}", f.display()));
                // Short enough for the card's box: "Read-only file system".
                let reason = e.to_string();
                let reason = reason.split(" (os error").next().unwrap_or("").to_owned();
                first_error.get_or_insert(reason);
            }
        }
    }
    (deleted, first_error)
}

fn run(job: &Job, paths: &Paths) -> Result<()> {
    let dev = &job.device;
    log(format!("{}: import started ({})", dev.dev, dev.model));

    job.set_phase(Phase::Mounting, format!("Mounting {}", dev.dev));
    let (mount, _) = udisks::ensure_mounted(&dev.dev)?;
    job.check_cancel()?;

    job.set_phase(Phase::Scanning, format!("Scanning {}", mount.display()));
    let (photos, videos) = scan(&mount, job)?;
    log(format!(
        "{}: {} photos, {} videos on {}",
        dev.dev,
        photos.len(),
        videos.len(),
        mount.display()
    ));

    let originals = paths.originals().join(dev.card_id());
    for dir in [&paths.pictures, &paths.videos, &originals] {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let photos = sources(&photos)?;
    let videos = sources(&videos)?;
    let photo_bytes: u64 = photos.iter().map(|p| p.size).sum();
    let video_bytes: u64 = videos.iter().map(|p| p.size).sum();
    // Joined recordings are new files next to the originals they come from.
    let new_videos = bytes_needed(&videos, &originals);
    check_space(&[
        (&paths.pictures, bytes_needed(&photos, &paths.pictures)),
        (&paths.videos, 2 * new_videos),
    ])?;

    job.update(|s| {
        // Every file is read off the card once: copied, or hashed when a
        // file of the same name and size is already there.
        s.copy_total = photo_bytes + video_bytes;
        s.photos = (photos.len(), 0);
        s.videos = (videos.len(), 0);
        s.est
            .set_total(StageKind::Copy, (photo_bytes + video_bytes) as f64);
        // First guess for stitching: the new videos are checked and joined.
        s.est.set_total(StageKind::Check, new_videos as f64);
        s.est.set_total(StageKind::Join, new_videos as f64);
        s.est.enter(Some(StageKind::Copy));
    });

    let mut buf = vec![0u8; COPY_BUFFER];
    let (mut copied, mut skipped) = (0, 0);
    let mut video_files = Vec::new();
    // Card files whose content is now verified to be in the library.
    let mut imported = Vec::new();
    for (phase, items, dest, is_photo) in [
        (Phase::CopyingPhotos, &photos, &paths.pictures, true),
        (Phase::CopyingVideos, &videos, &originals, false),
    ] {
        for (i, item) in items.iter().enumerate() {
            job.set_phase(phase, item.name.clone());
            let done_before = job.snapshot().copied;
            let path = match import_file(job, item, dest, &mut buf)? {
                Outcome::Copied(p) => {
                    copied += 1;
                    p
                }
                Outcome::Duplicate(p) => {
                    skipped += 1;
                    p
                }
            };
            job.update(|s| {
                // Keep the byte count exact whatever was read for this file.
                let delta = (done_before + item.size) as f64 - s.copied as f64;
                s.copied = done_before + item.size;
                s.est.advance(StageKind::Copy, delta);
                s.skipped = skipped;
                if is_photo {
                    s.photos.1 = i + 1;
                } else {
                    s.videos.1 = i + 1;
                }
            });
            imported.push(item.src.clone());
            if !is_photo {
                video_files.push(path);
            }
        }
    }
    job.update(|s| s.est.finish(StageKind::Copy));
    log(format!(
        "{}: {copied} files copied ({}), {skipped} already imported",
        dev.dev,
        gb(photo_bytes + video_bytes)
    ));

    // Videos stitched in an earlier import of this card are not redone.
    let processed: HashSet<String> = util::read_lines(&originals.join(PROCESSED_LEDGER))
        .into_iter()
        .collect();
    let to_stitch: Vec<PathBuf> = video_files
        .into_iter()
        .filter(|p| !processed.contains(&*p.file_name().unwrap().to_string_lossy()))
        .collect();
    let to_stitch_bytes: u64 = to_stitch
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    job.update(|s| {
        s.est.set_total(StageKind::Check, to_stitch_bytes as f64);
        s.est.set_total(StageKind::Join, to_stitch_bytes as f64);
    });

    // Everything is safely in the library: clear the card for the next
    // session. Only the imported files go; anything else on it stays.
    job.set_phase(Phase::Clearing, "");
    job.update(|s| s.est.enter(None));
    let (cleared, clear_error) = clear_card(job, &imported);
    log(format!(
        "{}: deleted {cleared} of {} imported files from the card",
        dev.dev,
        imported.len()
    ));
    job.update(|s| {
        s.cleared = cleared;
        s.clear_error = clear_error;
    });

    // The card is not needed any more.
    job.set_phase(Phase::Unmounting, format!("Unmounting {}", dev.dev));
    job.update(|s| s.est.enter(None));
    match udisks::unmount(&dev.dev) {
        Ok(()) => job.update(|s| s.unmounted = true),
        Err(e) => {
            log(format!("{}: {e:#}", dev.dev));
            job.update(|s| s.unmount_error = Some(format!("{e:#}")));
        }
    }

    job.set_phase(Phase::Stitching, "Stitching videos");
    let report = stitch::stitch(&to_stitch, &originals, &paths.videos, &job.cancel, job)?;
    job.update(|s| {
        s.recordings = report.outputs.len();
        s.damaged = report.damaged;
        s.unreadable = report.unreadable;
        s.detail = String::new();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            device: Device {
                key: String::new(),
                dev: "/dev/null".into(),
                name: "test".into(),
                label: None,
                uuid: None,
                fstype: "vfat".into(),
                size: 0,
                mountpoint: None,
                model: String::new(),
            },
            cancel: AtomicBool::new(false),
            state: Mutex::new(JobState {
                phase: Phase::CopyingPhotos,
                detail: String::new(),
                est: Estimator::new(&Calibration::default()),
                copy_total: 0,
                copied: 0,
                photos: (0, 0),
                videos: (0, 0),
                skipped: 0,
                unmounted: false,
                unmount_error: None,
                cleared: 0,
                clear_error: None,
                error: None,
                recordings: 0,
                damaged: Vec::new(),
                unreadable: Vec::new(),
                finished_at: None,
            }),
        }
    }

    fn source(dir: &Path, name: &str, data: &[u8]) -> Source {
        let src = dir.join(name);
        fs::write(&src, data).unwrap();
        sources(&[src]).unwrap().remove(0)
    }

    #[test]
    fn name_collisions_are_decided_by_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        fs::create_dir_all(&card).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let job = job();
        let mut buf = vec![0u8; 1024];

        // New name: copied.
        let a = source(&card, "IMG_0001.JPG", b"first photo");
        let Outcome::Copied(p) = import_file(&job, &a, &dest, &mut buf).unwrap() else {
            panic!("expected a copy")
        };
        assert_eq!(p, dest.join("IMG_0001.JPG"));

        // Same name, same content: skipped.
        assert!(matches!(
            import_file(&job, &a, &dest, &mut buf).unwrap(),
            Outcome::Duplicate(p) if p == dest.join("IMG_0001.JPG")
        ));

        // Same name and size, different content: _1.
        let other = tmp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let b = source(&other, "IMG_0001.JPG", b"other photo");
        assert_eq!(a.size, b.size);
        let Outcome::Copied(p) = import_file(&job, &b, &dest, &mut buf).unwrap() else {
            panic!("expected a copy")
        };
        assert_eq!(p, dest.join("IMG_0001_1.JPG"));
        assert_eq!(fs::read(&p).unwrap(), b"other photo");

        // ...and importing it again finds it under _1.
        assert!(matches!(
            import_file(&job, &b, &dest, &mut buf).unwrap(),
            Outcome::Duplicate(p) if p == dest.join("IMG_0001_1.JPG")
        ));

        // Different size: _2, without hashing.
        let c = source(&card, "IMG_0001.JPG", b"a much longer third photo");
        let Outcome::Copied(p) = import_file(&job, &c, &dest, &mut buf).unwrap() else {
            panic!("expected a copy")
        };
        assert_eq!(p, dest.join("IMG_0001_2.JPG"));
    }

    #[test]
    fn clearing_deletes_only_the_imported_files() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path();
        for f in ["a.JPG", "b.MP4", "keep.txt"] {
            fs::write(card.join(f), b"x").unwrap();
        }
        let job = job();
        let files = vec![
            card.join("a.JPG"),
            card.join("b.MP4"),
            card.join("gone.MP4"),
        ];
        let (deleted, error) = clear_card(&job, &files);
        assert_eq!(deleted, 2);
        assert_eq!(error.as_deref(), Some("No such file or directory"));
        assert!(!card.join("a.JPG").exists());
        assert!(card.join("keep.txt").exists());
    }

    #[test]
    fn candidates() {
        assert_eq!(candidate("a.MP4", 0), "a.MP4");
        assert_eq!(candidate("a.MP4", 1), "a_1.MP4");
        assert_eq!(candidate("noext", 2), "noext_2");
    }
}
