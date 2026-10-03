//! Drawing: one box per SD card, stacked, the selected one highlighted.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, Paragraph};
use ratatui::Frame;

use crate::app::{App, Entry};
use crate::estimate::StageKind;
use crate::import::{JobState, Phase};
use crate::paths::Paths;
use crate::util::{self, gb, MB};

const CARD_HEIGHT: u16 = 7;

pub fn draw(f: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, header, app);
    draw_cards(f, body, app);
    draw_footer(f, footer, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut title = vec![
        Span::styled(
            " 🔬 Microscope Importer ",
            Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw("  "),
        Span::styled("photos ", Style::new().dim()),
        Span::styled(
            Paths::pretty(&app.paths.pictures),
            Style::new().fg(Color::Green),
        ),
        Span::styled("   videos ", Style::new().dim()),
        Span::styled(
            Paths::pretty(&app.paths.videos),
            Style::new().fg(Color::Magenta),
        ),
    ];
    let busy = app.busy_count();
    if busy > 0 {
        title.push(Span::styled(
            format!(
                "   {busy} import{} running",
                if busy == 1 { "" } else { "s" }
            ),
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    let mut lines = vec![Line::from(title)];
    if !app.missing_tools.is_empty() {
        lines.push(Line::styled(
            format!(
                " Missing programs: {} - install them first",
                app.missing_tools.join(", ")
            ),
            Style::new().fg(Color::Red).bold(),
        ));
    } else if let Some(e) = &app.watcher_error {
        lines.push(Line::styled(
            format!(" Cannot list devices: {e}"),
            Style::new().fg(Color::Red),
        ));
    } else {
        lines.push(Line::styled(
            " Insert SD cards at any time - each one gets its own box below.",
            Style::new().dim(),
        ));
    }
    f.render_widget(
        Paragraph::new(lines),
        area.inner(ratatui::layout::Margin::new(0, 0)),
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let key =
        |k: &'static str| Span::styled(k, Style::new().fg(Color::Black).bg(Color::Gray).bold());
    let line = if app.quitting {
        Line::styled(
            " Cancelling imports and cleaning up partial files…",
            Style::new().fg(Color::Red).bold(),
        )
    } else if app.quit_armed.is_some() {
        Line::from(vec![
            Span::styled(
                " Imports are running! ",
                Style::new().fg(Color::White).bg(Color::Red).bold(),
            ),
            Span::styled(" Press ", Style::new().fg(Color::Red)),
            key(" q "),
            Span::styled(
                " again to cancel them and quit.",
                Style::new().fg(Color::Red),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw(" "),
            key(" ↑↓ "),
            Span::raw(" select   "),
            key(" Enter "),
            Span::raw(" import   "),
            key(" x "),
            Span::raw(" dismiss   "),
            key(" q "),
            Span::raw(" quit"),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_cards(f: &mut Frame, area: Rect, app: &App) {
    if app.entries.is_empty() {
        let [_, mid, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Fill(1),
        ])
        .areas(area);
        let text = vec![
            Line::styled("No SD card found", Style::new().fg(Color::Yellow).bold()),
            Line::styled(
                "Insert a card into a reader - mounted or not, it shows up here.",
                Style::new().dim(),
            ),
        ];
        f.render_widget(Paragraph::new(text).alignment(Alignment::Center), mid);
        return;
    }
    let fits = (area.height / CARD_HEIGHT).max(1) as usize;
    let first = app
        .selected
        .saturating_sub(fits - 1)
        .min(app.entries.len().saturating_sub(fits));
    for (slot, (i, entry)) in app
        .entries
        .iter()
        .enumerate()
        .skip(first)
        .take(fits)
        .enumerate()
    {
        let rect = Rect {
            y: area.y + slot as u16 * CARD_HEIGHT,
            height: CARD_HEIGHT.min(area.height),
            ..area
        };
        draw_card(f, rect, entry, i == app.selected);
    }
    if app.entries.len() > fits {
        let more = format!(" {}/{} ", app.selected + 1, app.entries.len());
        let r = Rect {
            x: area.right().saturating_sub(more.len() as u16 + 1),
            y: area.bottom().saturating_sub(1),
            width: more.len() as u16,
            height: 1,
        };
        f.render_widget(Paragraph::new(more).style(Style::new().fg(Color::Cyan)), r);
    }
}

fn card_title(e: &Entry, selected: bool) -> Line<'static> {
    let d = &e.device;
    let mut spans = vec![
        Span::styled(
            if selected { " ▶ " } else { "   " },
            Style::new().fg(Color::Cyan).bold(),
        ),
        Span::styled(d.name.clone(), Style::new().fg(Color::White).bold()),
    ];
    if let Some(label) = &d.label {
        spans.push(Span::styled(
            format!("  “{label}”"),
            Style::new().fg(Color::Yellow),
        ));
    }
    spans.push(Span::styled(
        format!("  {}  {}", gb(d.size), d.fstype),
        Style::new().fg(Color::Gray),
    ));
    if !d.model.is_empty() {
        spans.push(Span::styled(format!("  {}", d.model), Style::new().dim()));
    }
    let (state, color) = if !e.present {
        ("removed", Color::Yellow)
    } else if d.mountpoint.is_some() {
        ("mounted", Color::Green)
    } else {
        ("not mounted", Color::Gray)
    };
    spans.push(Span::raw("  "));
    spans.push(Span::styled(format!("[{state}] "), Style::new().fg(color)));
    Line::from(spans)
}

struct View {
    status: Vec<Span<'static>>,
    color: Color,
    ratio: f64,
    stats: Line<'static>,
    info: Line<'static>,
}

fn stat(label: &'static str, value: String, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(label, Style::new().dim()),
        Span::styled(value, Style::new().fg(color).bold()),
        Span::raw("    "),
    ]
}

fn stats_line(s: &mut JobState) -> Line<'static> {
    let stage = s.est.current();
    let rate = match s.est.current_rate() {
        Some((k, r)) if k.is_bytes() => format!("{:.1} MB/s", r / MB),
        Some(_) => "encoding".into(),
        None => "--".into(),
    };
    let rate_label = if stage == Some(StageKind::Copy) {
        "⇣ "
    } else {
        "⚙ "
    };
    let eta = if s.phase.is_finished() {
        "--".into()
    } else {
        util::duration(s.est.remaining_secs())
    };
    let copying = matches!(
        s.phase,
        Phase::Mounting
            | Phase::Scanning
            | Phase::CopyingPhotos
            | Phase::CopyingVideos
            | Phase::Clearing
    );
    let (left, left_label) = if copying || s.phase == Phase::Done {
        (s.copy_total.saturating_sub(s.copied), " left to copy")
    } else {
        // Video data not yet written into a recording (re-read once more
        // while a recording with black filler is verified).
        let k = if stage == Some(StageKind::Verify) {
            StageKind::Verify
        } else {
            StageKind::Join
        };
        (
            (s.est.total(k) - s.est.done(k)).max(0.0) as u64,
            " left to stitch",
        )
    };
    let mut spans = Vec::new();
    spans.extend(stat(rate_label, rate, Color::Yellow));
    spans.extend(stat("ETA ", eta, Color::Cyan));
    spans.extend(stat("total ", gb(s.copy_total), Color::White));
    spans.push(Span::styled(
        gb(left),
        Style::new().fg(Color::Magenta).bold(),
    ));
    spans.push(Span::styled(left_label, Style::new().dim()));
    Line::from(spans)
}

fn view(e: &Entry) -> View {
    let Some(job) = &e.job else {
        let info = match &e.device.mountpoint {
            Some(m) => format!("Mounted at {}", m.display()),
            None if e.present => "Not mounted - it is mounted automatically for the import".into(),
            None => String::new(),
        };
        return View {
            status: vec![
                Span::styled("● Ready", Style::new().fg(Color::Green).bold()),
                Span::styled("  press Enter to import", Style::new().fg(Color::Gray)),
            ],
            color: Color::DarkGray,
            ratio: 0.0,
            stats: Line::styled("", Style::new()),
            info: Line::styled(info, Style::new().dim()),
        };
    };
    let (mut s, ratio) = job.view();
    let bold = |text: String, c: Color| Span::styled(text, Style::new().fg(c).bold());
    let dim = |text: String| Span::styled(text, Style::new().fg(Color::Gray));
    let (status, color) = match s.phase {
        Phase::Mounting => (
            vec![bold("◌ Mounting…".into(), Color::Yellow)],
            Color::Yellow,
        ),
        Phase::Scanning => (
            vec![bold(
                "◌ Looking for photos and videos…".into(),
                Color::Yellow,
            )],
            Color::Yellow,
        ),
        Phase::CopyingPhotos => (
            vec![
                bold(
                    format!("⇣ Copying photos {}/{}", s.photos.1 + 1, s.photos.0),
                    Color::Blue,
                ),
                dim(format!("  {}", s.detail)),
            ],
            Color::Blue,
        ),
        Phase::CopyingVideos => (
            vec![
                bold(
                    format!("⇣ Copying videos {}/{}", s.videos.1 + 1, s.videos.0),
                    Color::Cyan,
                ),
                dim(format!("  {}", s.detail)),
            ],
            Color::Cyan,
        ),
        Phase::Clearing => (
            vec![
                bold(
                    "🗑 Deleting imported files from the SD card…".into(),
                    Color::Yellow,
                ),
                dim(format!("  {}", s.detail)),
            ],
            Color::Yellow,
        ),
        Phase::Unmounting => (
            vec![bold("⏏ Unmounting…".into(), Color::Yellow)],
            Color::Yellow,
        ),
        Phase::Stitching => {
            let mut v = if !e.present {
                vec![bold("Removed, Stitching videos...".into(), Color::Yellow)]
            } else if s.unmounted {
                vec![
                    Span::styled(
                        " ✔ Can remove SD card ",
                        Style::new().fg(Color::Black).bg(Color::Green).bold(),
                    ),
                    bold("  Stitching videos…".into(), Color::Magenta),
                ]
            } else {
                vec![
                    bold("⚠ Unmount failed - eject manually".into(), Color::Red),
                    bold("  Stitching videos…".into(), Color::Magenta),
                ]
            };
            v.push(dim(format!("  {}", s.detail)));
            (v, Color::Magenta)
        }
        Phase::Done => {
            let mut v = vec![bold("✔ Done".into(), Color::Green)];
            if e.present {
                v.push(dim("  - SD card can be removed".into()));
            }
            (v, Color::Green)
        }
        Phase::Failed => (
            vec![bold(
                format!("✘ {}", s.error.clone().unwrap_or_else(|| "failed".into())),
                Color::Red,
            )],
            Color::Red,
        ),
        Phase::Cancelled => (vec![bold("✘ Cancelled".into(), Color::Red)], Color::Red),
    };

    let mut info = vec![
        Span::styled("photos ", Style::new().dim()),
        Span::styled(
            format!("{}/{}", s.photos.1, s.photos.0),
            Style::new().fg(Color::Blue),
        ),
        Span::styled("   videos ", Style::new().dim()),
        Span::styled(
            format!("{}/{}", s.videos.1, s.videos.0),
            Style::new().fg(Color::Cyan),
        ),
    ];
    if s.skipped > 0 {
        info.push(Span::styled(
            format!("   {} already imported", s.skipped),
            Style::new().dim(),
        ));
    }
    if let Some(e) = &s.clear_error {
        info.push(Span::styled(
            format!("   ⚠ card not cleared: {e}"),
            Style::new().fg(Color::Red),
        ));
    } else if s.cleared > 0 {
        info.push(Span::styled(
            format!("   🗑 {} deleted from card", s.cleared),
            Style::new().fg(Color::Yellow),
        ));
    }
    if s.phase == Phase::Done {
        info.push(Span::styled(
            format!(
                "   → {} recording{}",
                s.recordings,
                if s.recordings == 1 { "" } else { "s" }
            ),
            Style::new().fg(Color::Magenta),
        ));
    }
    if !s.damaged.is_empty() {
        info.push(Span::styled(
            format!("   {} damaged → black, copied as-is", s.damaged.len()),
            Style::new().fg(Color::Red),
        ));
    }
    if !s.unreadable.is_empty() {
        info.push(Span::styled(
            format!("   {} unreadable, copied as-is", s.unreadable.len()),
            Style::new().fg(Color::Red),
        ));
    }
    if s.phase.is_finished() && e.present {
        info.push(Span::styled("   x to clear", Style::new().dim()));
    } else if s.phase.is_finished() && s.phase != Phase::Done {
        info.push(Span::styled("   x to dismiss", Style::new().dim()));
    }
    View {
        stats: stats_line(&mut s),
        status,
        color,
        ratio,
        info: Line::from(info),
    }
}

fn draw_card(f: &mut Frame, area: Rect, e: &Entry, selected: bool) {
    let v = view(e);
    let border = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if e.present {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM)
    };
    let block = Block::bordered()
        .border_type(if selected {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(border)
        .title(card_title(e, selected));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1); 5]).split(inner);
    let pad = |r: Rect| Rect {
        x: r.x + 1,
        width: r.width.saturating_sub(2),
        ..r
    };
    f.render_widget(Paragraph::new(Line::from(v.status)), pad(rows[0]));
    let gauge = Gauge::default()
        .gauge_style(Style::new().fg(v.color).bg(Color::Rgb(40, 40, 48)))
        .ratio(v.ratio.clamp(0.0, 1.0))
        .use_unicode(true)
        .label(match e.job {
            Some(_) => Span::styled(
                format!("{:5.1}%", v.ratio * 100.0),
                Style::new().fg(Color::White).bold(),
            ),
            None => Span::raw(""),
        });
    f.render_widget(gauge, pad(rows[1]));
    f.render_widget(Paragraph::new(v.stats), pad(rows[3]));
    f.render_widget(Paragraph::new(v.info), pad(rows[4]));
}
