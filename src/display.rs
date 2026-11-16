use crate::paths::Outcome;
use anyhow::Result;
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    sync::Mutex,
    time::{Duration, Instant},
};

struct Transfer {
    name: String,
    bytes: u64,
    total: Option<u64>,
    started: Instant,
}
struct State {
    active: BTreeMap<usize, Transfer>,
    next: usize,
    downloaded: usize,
    existing: usize,
    missing: usize,
    failed: usize,
    duplicates: usize,
    rows: usize,
    last_draw: Instant,
}

pub struct Display {
    tty: bool,
    state: Mutex<State>,
}

impl Display {
    pub fn new(progress: bool) -> Self {
        Self {
            tty: progress && io::stderr().is_terminal(),
            state: Mutex::new(State {
                active: BTreeMap::new(),
                next: 0,
                downloaded: 0,
                existing: 0,
                missing: 0,
                failed: 0,
                duplicates: 0,
                rows: 0,
                last_draw: Instant::now(),
            }),
        }
    }

    pub fn start(&self, name: String) -> usize {
        let mut state = self.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        state.active.insert(
            id,
            Transfer {
                name,
                bytes: 0,
                total: None,
                started: Instant::now(),
            },
        );
        if self.tty && state.last_draw.elapsed() >= Duration::from_millis(100) {
            self.draw(&mut state, None);
        }
        id
    }

    pub fn progress(&self, id: usize, bytes: u64, total: Option<u64>) {
        let mut state = self.state.lock().unwrap();
        if let Some(item) = state.active.get_mut(&id) {
            item.bytes = bytes;
            item.total = total;
        }
        if self.tty && state.last_draw.elapsed() >= Duration::from_millis(100) {
            self.draw(&mut state, None);
        }
    }

    pub fn duplicate(&self) {
        self.state.lock().unwrap().duplicates += 1;
    }

    pub fn invalid(&self, error: String) {
        let mut state = self.state.lock().unwrap();
        state.failed += 1;
        self.draw(&mut state, Some(format!("{:<7} {}", "INVALID", error)));
    }

    pub fn finish(&self, id: usize, outcome: Result<Outcome>) {
        let mut state = self.state.lock().unwrap();
        let item = state.active.remove(&id).unwrap();
        let message = match outcome {
            Ok(Outcome::Downloaded) => {
                state.downloaded += 1;
                self.completion("DONE", &item.name, &bytes(item.bytes as f64))
            }
            Ok(Outcome::Existing) => {
                state.existing += 1;
                self.completion("EXISTS", &item.name, "already on disk")
            }
            Ok(Outcome::Missing) => {
                state.missing += 1;
                self.completion("MISSING", &item.name, "HTTP 404")
            }
            Err(error) => {
                state.failed += 1;
                self.completion("FAILED", &item.name, &format!("{error:#}"))
            }
        };
        self.draw(&mut state, Some(message));
    }

    pub fn summary(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        self.clear(&mut state);
        eprintln!(
            "Finished: {} downloaded, {} existing, {} missing, {} failed, {} duplicates skipped",
            state.downloaded, state.existing, state.missing, state.failed, state.duplicates
        );
        state.failed
    }

    fn completion(&self, status: &str, name: &str, detail: &str) -> String {
        // Permanent messages keep full names and a fixed details column.
        if name.chars().count() > 40 {
            format!("{status:<7} {name}  {detail}")
        } else {
            format!("{status:<7} {name:<40} {detail}")
        }
    }

    fn clear(&self, state: &mut State) {
        let mut out = io::stderr().lock();
        for _ in 0..state.rows {
            let _ = write!(out, "\x1b[1A\r\x1b[2K");
        }
        state.rows = 0;
    }

    fn draw(&self, state: &mut State, message: Option<String>) {
        self.clear(state);
        let mut out = io::stderr().lock();
        if let Some(message) = message {
            // URL input and error text must never inject terminal control sequences.
            let clean: String = message.chars().filter(|c| !c.is_control()).collect();
            let _ = writeln!(out, "{clean}");
        }
        if !self.tty {
            return;
        }
        let width = terminal_width();
        let height = std::env::var("LINES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(24)
            .max(4);
        let summary = format!(
            "Downloads: {} active | {} done | {} existing | {} missing | {} failed",
            state.active.len(),
            state.downloaded,
            state.existing,
            state.missing,
            state.failed
        );
        let _ = writeln!(out, "{}", clip(&summary, width - 1));
        let name_width = filename_width(width);
        let header = transfer_row(
            "STATUS", "FILE", name_width, "PROGRESS", "RECEIVED", "TOTAL", "SPEED",
        );
        let _ = writeln!(out, "{}", clip(&header, width - 1));
        state.rows = 2;
        let visible = height.saturating_sub(5);
        for item in state.active.values().take(visible) {
            let ratio = item
                .total
                .filter(|n| *n > 0)
                .map(|n| (item.bytes as f64 / n as f64).min(1.0));
            let filled = (ratio.unwrap_or(0.0) * 10.0) as usize;
            let bar = format!("{}{}", "=".repeat(filled), "-".repeat(10 - filled));
            let total = item
                .total
                .map(|n| bytes(n as f64))
                .unwrap_or_else(|| "?".into());
            let speed = bytes(item.bytes as f64 / item.started.elapsed().as_secs_f64().max(0.001));
            let line = transfer_row(
                "ACTIVE",
                &item.name,
                name_width,
                &format!("[{bar}]"),
                &bytes(item.bytes as f64),
                &total,
                &format!("{speed}/s"),
            );
            let _ = writeln!(out, "{}", clip(&line, width - 1));
            state.rows += 1;
        }
        if state.active.len() > visible {
            let _ = writeln!(
                out,
                "{}",
                clip(
                    &format!("... {} more active downloads", state.active.len() - visible),
                    width - 1
                )
            );
            state.rows += 1;
        }
        let _ = out.flush();
        state.last_draw = Instant::now();
    }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80usize)
        .clamp(20, 240)
}

fn filename_width(width: usize) -> usize {
    width.saturating_sub(56).max(8)
}

fn abbreviated(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    // Preserve the archive date and extension when a symbol is long.
    let tail: String = text
        .chars()
        .rev()
        .take(width.saturating_sub(3))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

fn transfer_row(
    status: &str,
    name: &str,
    name_width: usize,
    progress: &str,
    received: &str,
    total: &str,
    speed: &str,
) -> String {
    let name = abbreviated(name, name_width);
    format!("{status:<7} {name:<name_width$} {progress:^12} {received:>9} / {total:>9} {speed:>11}")
}

fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}
fn bytes(mut value: f64) -> String {
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 || unit == "GiB" {
            return format!("{value:.1}{unit}");
        }
        value /= 1024.0;
    }
    unreachable!()
}
