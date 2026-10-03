//! Application state: the list of cards on screen, and key handling.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::devices::{self, Device};
use crate::import::{self, Job, Phase};
use crate::paths::{Calibration, Paths};
use crate::ui;

/// How long a finished import of a card that has been taken out stays on
/// screen.
const LINGER: Duration = Duration::from_secs(15);
const QUIT_CONFIRM: Duration = Duration::from_secs(5);

pub struct Entry {
    pub key: String,
    /// Last known state of the device (kept after it is removed).
    pub device: Device,
    pub present: bool,
    pub job: Option<Arc<Job>>,
}

impl Entry {
    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.is_finished())
    }
}

pub struct App {
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub paths: Paths,
    pub watcher_error: Option<String>,
    pub missing_tools: Vec<&'static str>,
    pub quit_armed: Option<Instant>,
    pub quitting: bool,
    should_quit: bool,
    cal: Arc<Mutex<Calibration>>,
    devices: Receiver<Result<Vec<Device>, String>>,
}

fn in_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

impl App {
    pub fn new(paths: Paths) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            if tx
                .send(devices::list().map_err(|e| format!("{e:#}")))
                .is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        });
        let missing_tools = ["ffmpeg", "ffprobe", "udisksctl", "lsblk"]
            .into_iter()
            .filter(|t| !in_path(t))
            .collect();
        App {
            entries: Vec::new(),
            selected: 0,
            cal: Arc::new(Mutex::new(Calibration::load(&paths))),
            paths,
            watcher_error: None,
            missing_tools,
            quit_armed: None,
            quitting: false,
            should_quit: false,
            devices: rx,
        }
    }

    pub fn busy_count(&self) -> usize {
        self.entries.iter().filter(|e| e.busy()).count()
    }

    fn merge(&mut self, devices: Vec<Device>) {
        let selected_key = self.entries.get(self.selected).map(|e| e.key.clone());
        for e in &mut self.entries {
            e.present = false;
        }
        for d in devices {
            match self.entries.iter_mut().find(|e| e.key == d.key) {
                Some(e) => {
                    e.present = true;
                    e.device = d;
                }
                None => self.entries.push(Entry {
                    key: d.key.clone(),
                    device: d,
                    present: true,
                    job: None,
                }),
            }
        }
        self.entries.retain(|e| {
            if e.present {
                return true;
            }
            let Some(job) = &e.job else { return false };
            let s = job.snapshot();
            match s.phase {
                // Errors stay until dismissed.
                Phase::Failed | Phase::Cancelled => true,
                Phase::Done => s.finished_at.is_some_and(|t| t.elapsed() < LINGER),
                // Still stitching after the card was taken out.
                _ => true,
            }
        });
        if let Some(key) = selected_key {
            if let Some(i) = self.entries.iter().position(|e| e.key == key) {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn start_selected(&mut self) {
        let Some(e) = self.entries.get_mut(self.selected) else {
            return;
        };
        if !e.present || e.busy() || self.quitting {
            return;
        }
        e.job = Some(import::start(
            e.device.clone(),
            self.paths.clone(),
            self.cal.clone(),
        ));
    }

    fn dismiss_selected(&mut self) {
        let Some(e) = self.entries.get_mut(self.selected) else {
            return;
        };
        if e.busy() {
            return;
        }
        if e.present {
            e.job = None;
        } else {
            self.entries.remove(self.selected);
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        }
    }

    fn request_quit(&mut self) {
        if self.busy_count() == 0 {
            self.should_quit = true;
        } else if self.quit_armed.is_some_and(|t| t.elapsed() < QUIT_CONFIRM) {
            self.quitting = true;
            for e in &self.entries {
                if let Some(j) = &e.job {
                    j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        } else {
            self.quit_armed = Some(Instant::now());
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.entries.len().saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.entries.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.start_selected(),
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => self.dismiss_selected(),
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.request_quit(),
            _ => {}
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            while let Ok(msg) = self.devices.try_recv() {
                match msg {
                    Ok(devs) => {
                        self.watcher_error = None;
                        self.merge(devs);
                    }
                    Err(e) => self.watcher_error = Some(e),
                }
            }
            if self.quit_armed.is_some_and(|t| t.elapsed() >= QUIT_CONFIRM) {
                self.quit_armed = None;
            }
            if self.quitting && self.busy_count() == 0 {
                self.should_quit = true;
            }
            if self.should_quit {
                return Ok(());
            }
            terminal.draw(|f| ui::draw(f, &self))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.on_key(k.code, k.modifiers);
                    }
                }
            }
        }
    }
}
