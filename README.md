# microscope-importer

Helper tool to import footage automatically from Adonstar microscopes.

![last commit](https://img.shields.io/github/last-commit/Alacrity-Education/microscope-importer?style=flat-square) ![last release](https://img.shields.io/github/v/release/Alacrity-Education/microscope-importer?style=flat-square) ![language](https://img.shields.io/github/languages/top/Alacrity-Education/microscope-importer?style=flat-square) [![blazingly fast](https://blazingly.fast/api/badge.svg?repo=Alacrity-Education%2Fmicroscope-importer)](https://blazingly.fast) ![license](https://img.shields.io/github/license/Alacrity-Education/microscope-importer?style=flat-square) ![status](https://img.shields.io/badge/status-working-green?style=flat-square) ![repo size](https://img.shields.io/github/repo-size/Alacrity-Education/microscope-importer?style=flat-square) [![CI](https://img.shields.io/github/actions/workflow/status/Alacrity-Education/microscope-importer/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/Alacrity-Education/microscope-importer/actions/workflows/ci.yml)

## Introduction

Our microscopes record to an SD card, and they do not record a session as one
video: the camera cuts a new file every 1, 2, 3, 5, 10, 15 or 20 minutes. A
morning at the bench is a few hundred files named after the camera clock,
mixed with the photos taken along the way, in a folder layout that differs
from one model to the next. Getting that onto a computer as watchable
recordings meant copying everything by hand, working out which files belong
together and joining them, without losing quality and without mixing up two
sessions.

microscope-importer does all of it. It is a terminal application that lists
every SD card attached to the computer, mounted or not. Pick a card and press
Enter: the photos go to `~/Pictures/Microscope`, the videos are copied off, the
card is unmounted so you can take it out, and the segments are joined back into
whole recordings in `~/Videos/Microscope` as a lossless stream copy. Several
cards can be imported at the same time, each in its own box with a progress
bar, transfer rate and time estimate.

## Demo

Two cards imported at once; the one on top has already been taken out of the
reader while its videos are still being stitched:

```
 🔬 Microscope Importer   photos ~/Pictures/Microscope   videos ~/Videos/Microscope   2 imports running
 Insert SD cards at any time - each one gets its own box below.

╭   sdc1  “MICROSCOPE”  31.26 GB  exfat  Mass Storage Device  [removed] ────────────────────────────────╮
│ Removed, Stitching videos...  Black filler for damaged 20281013211244_000034A.MP4                     │
│ ███████████████████████████████████████████████████▉       55.3%                                      │
│                                                                                                       │
│ ⚙ encoding    ETA 41s    total 2.99 GB    2.03 GB left to stitch                                      │
│ photos 4/4   videos 11/11                                                                             │
╰───────────────────────────────────────────────────────────────────────────────────────────────────────╯
┏ ▶ sdd1  “CARD2”  31.90 GB  vfat  Generic SD/MMC  [not mounted] ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃  ✔ Can remove SD card   Stitching videos…  Checking videos 4/6                                        ┃
┃ ████████████████████████████████████████▋                    32.2%                                    ┃
┃                                                                                                       ┃
┃ ⚙ 59.3 MB/s    ETA 25s    total 1.56 GB    1.56 GB left to stitch                                     ┃
┃ photos 1/1   videos 6/6                                                                               ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛

  ↑↓  select    Enter  import    x  dismiss    q  quit
```

## Features

- Find every SD card as soon as it is inserted, mounted or not, and mount it
  when the import starts. Disks that carry the running system are never
  offered.
- Import several cards at the same time, each with its own progress bar,
  percentage, transfer rate, time left, total size and data left.
- Search the card recursively, whatever its folder layout, for photos
  (`jpg`, `jpeg`, `png`) and videos (`mp4`). Photos are copied first.
- Unmount the card as soon as everything is copied: **Can remove SD card**
  shows while the videos are still being stitched. A card taken out at that
  point stays on screen as **Removed, Stitching videos...** until it is done.
- Join the camera's 1, 2, 3, 5, 10, 15 or 20 minute segments back into whole
  recordings, losslessly: the output holds the camera's own packets.
- Never interleave two sessions, even when the camera clock was reset at a
  power cycle: segments only join when the file counter goes up by one.
- Decode every video once and replace a damaged segment by black video with a
  note naming the file, so the recording keeps its length and the rest of it
  plays. The damaged file is placed next to the recording, unchanged.
- Keep every original in a hidden folder, never copy a file twice (checksums
  decide on a name collision), never
  stitch a video twice, never delete anything from the card.
- Estimate honestly: the progress bar and the ETA cover copying *and*
  stitching, with per-stage speeds that are measured while the import runs
  and remembered for the next one.

## Getting started

Install the release package for your distribution. On Arch (x86_64):

    curl -fsSL -o /tmp/microscope-importer.pkg.tar.zst "$(curl -fsSL https://api.github.com/repos/Alacrity-Education/microscope-importer/releases/latest | grep -o 'https://[^"]*\.pkg\.tar\.zst' | head -1)" && sudo pacman -U /tmp/microscope-importer.pkg.tar.zst

On Debian 13 and Ubuntu 24.04 or newer (amd64):

    curl -fsSL -o /tmp/microscope-importer.deb "$(curl -fsSL https://api.github.com/repos/Alacrity-Education/microscope-importer/releases/latest | grep -o 'https://[^"]*_amd64\.deb' | head -1)" && sudo apt install /tmp/microscope-importer.deb

The package pulls in what the program drives at run time: `ffmpeg` (with
libx264 and drawtext, as both distributions ship it), `udisks2` for mounting
and unmounting as a normal user and `util-linux` for `lsblk`. The font of the
note on black filler (DejaVu Sans Bold, [licence](assets/DejaVuSans-LICENSE.txt))
is compiled in. Linux only.

Or build from source, with cargo and rustc 1.85 or newer:

    cargo build --release       # target/release/microscope-importer
    cargo install --path .      # or a microscope-importer on the PATH

First run:

1. Run `microscope-importer` in a terminal (or start *Microscope Importer*
   from the application menu).
2. Insert the card. It appears in its own box.
3. Select it with `↑`/`↓` and press `Enter`.
4. When the box says **Can remove SD card**, take the card out, insert the
   next one and press `Enter` on it too.
5. The recordings are in `~/Videos/Microscope`, the photos in
   `~/Pictures/Microscope`.

## How it works

### An import

1. **Mount** the card through udisks if it is not mounted yet.
2. **Scan** it recursively for photos and videos. Hidden entries are skipped,
   which also skips macOS `._` files and `.Trashes`.
3. **Copy the photos** to `~/Pictures/Microscope`.
4. **Copy the videos** to `~/Videos/Microscope/.originalframes/<card>/`, where
   `<card>` is the card's filesystem UUID.
5. **Unmount** the card.
6. **Check** every video by decoding it once with ffmpeg.
7. **Stitch** the recordings into `~/Videos/Microscope`.

Every file is copied through a hidden `.part` name, flushed to disk and only
then given its real name, and it keeps the card's modification time. When a
file of the same name is already in the destination, checksums (BLAKE3)
decide: the same file is skipped, a different one is imported as
`IMG_0001_1.JPG` (then `_2`, `_3`...). Files of different sizes are known to
differ without hashing. Before anything is copied the free space on the
destination is checked.

### Stitching

The microscope names its files `YYYYMMDDhhmmss_NNNNNNX.MP4`: start time, file
counter, channel letter. Two files are one recording when

* the earlier one has the full segment length (1, 2, 3, 5, 10, 15 or 20
  minutes, give or take 1.5 s, detected per recording),
* the counter goes up by exactly one and the channel letter matches,
* the later one starts at most 2 s after the earlier one ends, and
* both have the same codec, resolution, frame rate and audio format.

A recording is joined with ffmpeg's concat demuxer as a stream copy. The
result is checked before it gets its name: its duration has to match the sum
of its segments, and so does its frame count. Recordings of a single file are
hard links to the original, so they take no extra space. Videos that do not
follow the camera's naming are imported under their own name.

### Damaged files

A segment that decodes with errors (an SD card write error, a card pulled
while recording) is replaced in the recording by black video and silence of
the same length, encoded with the camera's resolution, frame rate, pixel
format and colour settings so it splices into the stream copy, and showing

> Source file - check original footage
> 20281013211244_000034A.MP4

A recording that contains filler is decoded once more as a whole before it
is accepted. The damaged file itself is placed unchanged next to the
recordings, so whatever is still playable in it is at hand, and it is listed
in `.originalframes/<card>/damaged.txt`. A file ffprobe cannot read at all is
left out of the recording (which splits there) and placed next to the
recordings the same way.

### Progress and time estimate

The progress bar and the ETA cover the whole import. Each stage has its own
speed: copying off the card, decoding for the check, encoding black filler,
joining, and verifying recordings that contain filler. Each stage starts from
a calibrated rate and is pulled towards the rate it actually achieves while it
runs; the time left is the work left in every stage divided by its rate. The
rates measured by every import are remembered in
`~/.local/state/microscope-importer/calibration`, so the estimate fits the
machine and the card reader after a few imports.

The defaults were measured on a 24-core desktop with 4K60 H.264 footage at
41 Mbit/s on a hard disk:

| stage | default rate |
| --- | --- |
| copy off the card | 40 MB/s |
| check (full decode) | 100 MB/s |
| black filler (4K, x264 veryfast) | 4 s of video per second |
| join (stream copy) | 130 MB/s |
| verify (full decode) | 100 MB/s |

## TUI controls

One screen: a box per card, the selected one with a thick cyan border.

| key | action |
| --- | --- |
| `↑`/`↓`, `k`/`j`, `Home`/`End` | select a card |
| `Enter` / `space` | import the selected card (again, after it finished) |
| `x` / `Delete` | clear a finished import, dismiss an error of a removed card |
| `q` / `Esc` / `Ctrl-C` | quit; while imports run it asks once more, then cancels them and removes their partial files |

| status | meaning |
| --- | --- |
| `● Ready` | press Enter to import |
| `⇣ Copying photos 3/12`, `⇣ Copying videos 37/157` | copying off the card |
| `✔ Can remove SD card  Stitching videos…` | the card is unmounted, stitching runs from the copies |
| `Removed, Stitching videos...` | the card was taken out; the box stays until stitching is done |
| `✔ Done` | finished; a removed card's box disappears after 15 s |
| `⚠ Unmount failed - eject manually` | something held the card open |
| `✘ …` | the import failed or was cancelled; the message says why |

## Output files

| path | content |
| --- | --- |
| `~/Pictures/Microscope/*` | the photos |
| `~/Videos/Microscope/2028-10-13_21-10-44_000032A-000038A.mp4` | a recording: start time, first and last segment |
| `~/Videos/Microscope/2028-10-13_02-20-27_000009A.mp4` | a recording of one segment (hard link) |
| `~/Videos/Microscope/20281013211244_000034A.MP4` | a damaged segment, unchanged |
| `~/Videos/Microscope/.originalframes/<card>/*.MP4` | the originals as they came off the card |
| `~/Videos/Microscope/.originalframes/<card>/processed.txt` | videos already stitched; they are not stitched again |
| `~/Videos/Microscope/.originalframes/<card>/damaged.txt` | damaged and unreadable videos |
| `~/.local/state/microscope-importer/import.log` | what every import did |
| `~/.local/state/microscope-importer/calibration` | the learned stage rates |

## Configuration

| variable | purpose |
| --- | --- |
| `MICROSCOPE_IMPORTER_VIDEOS` | videos destination (default `~/Videos/Microscope`) |
| `MICROSCOPE_IMPORTER_PICTURES` | photos destination (default `~/Pictures/Microscope`) |
| `MICROSCOPE_IMPORTER_LOOP=1` | also list loop devices, to test with card images |

To try it without a card:

    truncate -s 4G card.img && mkfs.exfat -L TEST card.img
    udisksctl loop-setup -f card.img            # mount it, copy footage on, unmount
    MICROSCOPE_IMPORTER_LOOP=1 MICROSCOPE_IMPORTER_VIDEOS=/tmp/v MICROSCOPE_IMPORTER_PICTURES=/tmp/p microscope-importer
    udisksctl loop-delete -b /dev/loopN         # "takes the card out"

This project is built **entirely** with AI agents. It serves to automate a time-consuming process that we would have to do manually. Being a low-risk project (the originals are always kept, and the card is never written to), we have taken the liberty to benchmark LLM's capabilities to write helper tools with a given specification and very minimal technical guidance.
Rust was chosen as a language whose compiler helps verify what the LLM does without many repetitive write-test-debug cycles.

## Contributing

Issues and pull requests go to
[GitHub](https://github.com/Alacrity-Education/microscope-importer).

`cargo test --release` runs the unit tests and an end-to-end stitching test
that generates small clips with ffmpeg (it skips itself without ffmpeg).

Releases are tagged `vX.Y.Z` on a commit whose `Cargo.toml` carries that
version, and the release workflow builds the Arch and Debian packages and
attaches them to the release.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
