//! Progress reporting for `--full` scans: a terminal bar or NDJSON events,
//! both on **stderr** — stdout stays the pure report stream `docs/SCHEMA.md`
//! promises. The default fast path never reports (it finishes in
//! milliseconds); `main` resolves every mode to `Off` unless `--full` is set.
//!
//! One `Progress` per file. The long phases each report `done_bytes` against a
//! known `total_bytes` (the mmap length or a precomputed chunk-byte sum):
//! `Phase::Index` for a demux-time whole-file walk, `Phase::Scan` for the
//! sampler. All tick sites are single-threaded (demux loops, streaming
//! windows, the batch boundaries *between* rayon collects), so state lives in
//! `Cell`s — the sink is deliberately not `Sync`; never hand it into a
//! `par_iter` closure.
//!
//! `update` is byte-gated first and clock-gated second: the common case is a
//! single `u64` compare, so per-cluster / per-OBU ticking costs nothing, and
//! `Instant::now()` runs at most once per gate step.

use std::cell::Cell;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::model::SCHEMA_VERSION;
use crate::render::Palette;

/// How progress is delivered. Resolved once in `main` (never `Bar`/`Json`
/// without `--full`); `Off` must stay near-zero-cost since every tick site
/// runs on the default path too.
#[derive(Copy, Clone)]
pub enum Mode {
    Off,
    /// A `Scanning: <name>` header line per file, then **one** `\r`-rewritten
    /// stderr bar line for the whole file, no matter how many internal phases
    /// run — the phases are an implementation detail the bar never surfaces
    /// (no label either; the header already says what's happening). The bar
    /// fraction is blended across phases (`bar_fraction`) and is monotonic by
    /// construction, so the percent can never reset mid-file — a bar that
    /// restarts at 0% reads as a loop/hang, a real user report; never
    /// reintroduce one. Only a failed file's in-progress bar line is erased
    /// (the header stays as context above the diagnostic). `color` carries
    /// the active theme's palette when stderr takes ANSI.
    Bar { color: Option<&'static Palette> },
    /// One compact JSON object per stderr line (see `docs/SCHEMA.md`,
    /// "Progress events").
    Json,
}

/// The two long phases a `--full` run can spend real time in.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Demux-time whole-file walk (MKV cluster indexing, raw HEVC/AV1 splits,
    /// the TS SPS rescue).
    Index,
    /// The sampler's access-unit scan.
    Scan,
}

impl Phase {
    /// Machine name for JSON events.
    fn name(self) -> &'static str {
        match self {
            Phase::Index => "index",
            Phase::Scan => "scan",
        }
    }
}

/// Minimum time between bar redraws.
const BAR_INTERVAL: Duration = Duration::from_millis(100);
/// Minimum time between JSON progress lines.
const JSON_INTERVAL: Duration = Duration::from_millis(250);
/// Floor for the byte-gate step (see `gate_step`).
const MIN_STEP: u64 = 1 << 20;

/// Byte-gate step: at most ~200 clock checks per phase, never finer than
/// `MIN_STEP` so tiny totals don't gate on every call.
fn gate_step(total: u64) -> u64 {
    (total / 200).max(MIN_STEP)
}

/// Per-file progress sink. Interior mutability (`Cell`) because tick sites
/// hold only `&self` through long call chains; single-threaded by design.
pub struct Progress {
    mode: Mode,
    /// Full display path, for JSON events (matches the report's `file`).
    file: String,
    /// Bare file name, shown whole on the header line.
    name: String,
    file_index: usize,
    file_count: usize,
    /// File start — JSON `elapsed_ms` shares the report field's semantics.
    started: Instant,
    phase: Cell<Option<Phase>>,
    total: Cell<u64>,
    done: Cell<u64>,
    phase_started: Cell<Instant>,
    next_emit: Cell<u64>,
    last_emit: Cell<Instant>,
    /// A 100% line was already emitted for the current phase.
    total_emitted: Cell<bool>,
    /// Display width of the bar line currently on screen (0 = none).
    bar_width: Cell<usize>,
    /// The per-file `Scanning: <name>` header line was printed (bar mode
    /// only) — also `main`'s signal that progress actually drew something,
    /// so its end-of-run screen clear fires only when there is a bar to
    /// clear.
    header_drawn: Cell<bool>,
    /// An `Index` phase ran for this file: `bar_fraction` maps it to the
    /// bar's first half and the following scan to the second.
    saw_index: Cell<bool>,
    /// The file completed (`finish`): the bar closes at 100% even when the
    /// last phase legitimately ended short (an index walk may).
    finished: Cell<bool>,
    /// The terminal cursor is hidden (DECTCEM) while the bar rewrites — a
    /// blinking cursor at the line's end reads as jitter. Restored by
    /// `persist_bar`/`clear_bar`, so every exit path (success *and* the
    /// `Drop` on error) shows it again. ANSI-gated: only set when the bar
    /// has a palette.
    cursor_hidden: Cell<bool>,
}

impl Progress {
    /// No-op sink for tests, sidecars, and inner calls that must not
    /// double-report.
    pub fn off() -> Self {
        Self::new(Mode::Off, Path::new(""), 0, 0)
    }

    pub fn new(mode: Mode, path: &Path, file_index: usize, file_count: usize) -> Self {
        let now = Instant::now();
        Progress {
            mode,
            file: path.display().to_string(),
            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            file_index,
            file_count,
            started: now,
            phase: Cell::new(None),
            total: Cell::new(0),
            done: Cell::new(0),
            phase_started: Cell::new(now),
            next_emit: Cell::new(0),
            last_emit: Cell::new(now),
            total_emitted: Cell::new(false),
            bar_width: Cell::new(0),
            header_drawn: Cell::new(false),
            saw_index: Cell::new(false),
            finished: Cell::new(false),
            cursor_hidden: Cell::new(false),
        }
    }

    /// Whether a bar-mode header (and thus at least one bar line) hit the
    /// terminal for this file.
    pub fn bar_shown(&self) -> bool {
        self.header_drawn.get()
    }

    /// Start (or switch to) a phase with a known byte denominator. Emits the
    /// 0% line so a slow first window still shows immediate feedback.
    pub fn begin(&self, phase: Phase, total_bytes: u64) {
        if matches!(self.mode, Mode::Off) {
            return;
        }
        // The file's header line prints once, above the first phase's bar,
        // with a blank line between them so the bar has room to breathe.
        if let Mode::Bar { color } = self.mode {
            if !self.header_drawn.replace(true) {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(
                    err,
                    "{}\n",
                    format_header(&self.name, self.file_index, self.file_count, color)
                );
                let _ = err.flush();
            }
        }
        // The bar line is per-file, not per-phase: a new phase keeps rewriting
        // the same line, its progress blended forward by `bar_fraction`.
        if phase == Phase::Index {
            self.saw_index.set(true);
        }
        self.phase.set(Some(phase));
        self.total.set(total_bytes);
        self.done.set(0);
        self.next_emit.set(gate_step(total_bytes));
        self.total_emitted.set(false);
        let now = Instant::now();
        self.phase_started.set(now);
        self.last_emit.set(now);
        self.emit(0, total_bytes);
    }

    /// Record the absolute byte position within the current phase. Clamped
    /// monotonic; throttled (byte gate, then clock gate). The 100% line always
    /// lands, once.
    pub fn update(&self, done_bytes: u64) {
        if matches!(self.mode, Mode::Off) || self.phase.get().is_none() {
            return;
        }
        let total = self.total.get();
        let done = done_bytes.min(total).max(self.done.get());
        self.done.set(done);
        // Reaching `total` bypasses the byte gate — the 100% line must land
        // even when the final position falls short of the next gate step.
        if done < self.next_emit.get() && done < total {
            return;
        }
        if done >= total {
            if self.total_emitted.replace(true) {
                return;
            }
        } else {
            // Skip this gate step entirely if the clock hasn't moved enough —
            // bounds `Instant::now()` calls to one per step.
            self.next_emit.set(done.saturating_add(gate_step(total)));
            let interval = match self.mode {
                Mode::Bar { .. } => BAR_INTERVAL,
                _ => JSON_INTERVAL,
            };
            let now = Instant::now();
            if now.duration_since(self.last_emit.get()) < interval {
                return;
            }
            self.last_emit.set(now);
        }
        self.emit(done, total);
    }

    /// File finished successfully: close the bar at 100% and keep its line /
    /// emit the JSON `done` event. Error paths skip this and rely on `Drop`,
    /// which erases the in-progress line instead.
    pub fn finish(&self) {
        self.finished.set(true);
        match self.mode {
            Mode::Off => {}
            Mode::Bar { .. } => {
                // The scan phase already closed at 100%; the override matters
                // when the last phase legitimately ended short of its total.
                self.done.set(self.total.get());
                self.persist_bar();
            }
            Mode::Json => {
                let line = serde_json::to_string(&Event {
                    event: "done",
                    hdrprobe_schema_version: SCHEMA_VERSION,
                    file: &self.file,
                    file_index: self.file_index,
                    file_count: self.file_count,
                    phase: None,
                    bytes_done: None,
                    bytes_total: None,
                    percent: None,
                    elapsed_ms: self.started.elapsed().as_secs_f64() * 1000.0,
                })
                .unwrap();
                eprintln!("{line}");
            }
        }
    }

    /// The whole-file fraction the bar shows, blended across phases: an
    /// `Index` walk fills the first half, the scan that follows it the
    /// second, and a scan with no preceding index owns the whole bar (the
    /// common case — MKV/MP4/TS are single-phase). Monotonic by construction:
    /// a phase switch can only move the needle forward, so the percent never
    /// resets mid-file. `finish` pins it to 1.0 (an index walk may
    /// legitimately end short of its total).
    fn bar_fraction(&self) -> f64 {
        if self.finished.get() {
            return 1.0;
        }
        let total = self.total.get();
        let frac = if total == 0 { 1.0 } else { self.done.get() as f64 / total as f64 };
        match self.phase.get() {
            Some(Phase::Index) => frac * 0.5,
            Some(Phase::Scan) if self.saw_index.get() => 0.5 + frac * 0.5,
            _ => frac,
        }
    }

    fn emit(&self, done: u64, total: u64) {
        let Some(phase) = self.phase.get() else { return };
        match self.mode {
            Mode::Off => {}
            Mode::Bar { color } => {
                let (line, width) = format_line(
                    self.bar_fraction(),
                    done,
                    total,
                    self.phase_started.get().elapsed(),
                    color,
                );
                // Rewrite in place, padding over any leftover from a longer
                // previous line — works without ANSI erase sequences. The
                // cursor hides for the bar's lifetime (colored terminals
                // only); its blink at the line's end reads as jitter.
                let pad = self.bar_width.get().saturating_sub(width);
                let mut err = std::io::stderr().lock();
                if color.is_some() && !self.cursor_hidden.replace(true) {
                    let _ = write!(err, "\x1b[?25l");
                }
                let _ = write!(err, "\r{line}{:pad$}", "", pad = pad);
                let _ = err.flush();
                self.bar_width.set(width.max(self.bar_width.get()));
            }
            Mode::Json => {
                let line = serde_json::to_string(&Event {
                    event: "progress",
                    hdrprobe_schema_version: SCHEMA_VERSION,
                    file: &self.file,
                    file_index: self.file_index,
                    file_count: self.file_count,
                    phase: Some(phase.name()),
                    bytes_done: Some(done),
                    bytes_total: Some(total),
                    percent: Some(percent(done, total)),
                    elapsed_ms: self.started.elapsed().as_secs_f64() * 1000.0,
                })
                .unwrap();
                eprintln!("{line}");
            }
        }
    }

    /// Freeze the on-screen bar line: redraw it at its final recorded state,
    /// then move past it so it stays on screen (idempotent; no-op for other
    /// modes or when nothing was drawn).
    fn persist_bar(&self) {
        if !matches!(self.mode, Mode::Bar { .. }) || self.bar_width.get() == 0 {
            return;
        }
        // The last throttled frame may be stale; refresh before freezing.
        self.emit(self.done.get(), self.total.get());
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err);
        if self.cursor_hidden.replace(false) {
            let _ = write!(err, "\x1b[?25h");
        }
        let _ = err.flush();
        self.bar_width.set(0);
    }

    /// Blank out an on-screen bar line (idempotent; no-op for other modes).
    fn clear_bar(&self) {
        let width = self.bar_width.replace(0);
        if width > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{:width$}\r", "", width = width);
            if self.cursor_hidden.replace(false) {
                let _ = write!(err, "\x1b[?25h");
            }
            let _ = err.flush();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // Error paths must not leave a stale bar line under the diagnostic.
        self.clear_bar();
    }
}

/// One stderr NDJSON line. `progress` events fill the phase/byte fields;
/// `done` events omit them. Deliberately not part of `model::Report`, so the
/// report schema (and its golden shape test) is untouched.
#[derive(serde::Serialize)]
struct Event<'a> {
    event: &'static str,
    hdrprobe_schema_version: &'static str,
    file: &'a str,
    file_index: usize,
    file_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_done: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    percent: Option<f64>,
    elapsed_ms: f64,
}

/// Completion percentage, one decimal. An empty phase is complete.
fn percent(done: u64, total: u64) -> f64 {
    if total == 0 {
        return 100.0;
    }
    (done as f64 / total as f64 * 1000.0).round() / 10.0
}

/// Bar glyph cells.
const BAR_CELLS: usize = 32;

/// Render the per-file header line printed once above the first phase's bar:
/// `Scanning: movie.m2ts  [2/7]` (`[k/N]` only for multi-file runs). The name
/// is printed whole — a long title wraps rather than losing its tail. Never
/// `\r`-rewritten, so no display width is tracked for it.
fn format_header(
    name: &str,
    file_index: usize,
    file_count: usize,
    color: Option<&'static Palette>,
) -> String {
    let paint = |code: &str, text: &str| -> String {
        match color {
            Some(_) if !code.is_empty() => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_string(),
        }
    };
    let (bright, label, faint) = match color {
        Some(p) => (p.bright, p.label, p.faint),
        None => ("", "", ""),
    };
    let mut line = String::new();
    line.push_str(&paint(label, "Scanning:"));
    line.push(' ');
    line.push_str(&paint(bright, name));
    if file_count > 1 {
        line.push_str("  ");
        line.push_str(&paint(faint, &format!("[{file_index}/{file_count}]")));
    }
    line
}

/// Render the file's single bar line; returns the string and its display
/// width (the ANSI codes are zero-width). Pure, so it's testable without a
/// terminal. Shape (drawn under the file's header line, no label of its own
/// — the header already says what's happening):
/// `█████████████▓──────────────────  42%  118 MB/s  ETA 0:41`
/// The fill is two-tone: solid bright cells, plus one mid-tone `▓` at the
/// leading edge when the fraction lands in a cell's back half — sub-cell
/// motion between full cells.
/// `frac` is the whole-file blended fraction (drives the glyphs and the
/// percent); `done`/`total`/`phase_elapsed` are the *current phase's* bytes
/// and clock, so rate and ETA describe the work actually in flight. Fits an
/// 80-column terminal at every field's widest.
fn format_line(
    frac: f64,
    done: u64,
    total: u64,
    phase_elapsed: Duration,
    color: Option<&'static Palette>,
) -> (String, usize) {
    let paint = |code: &str, text: &str| -> String {
        match color {
            Some(_) if !code.is_empty() => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_string(),
        }
    };
    let (bright, value, faint) = match color {
        Some(p) => (p.bright, p.value, p.faint),
        None => ("", "", ""),
    };

    let cells = frac * BAR_CELLS as f64;
    let filled = (cells.floor() as usize).min(BAR_CELLS);
    let half = filled < BAR_CELLS && cells - filled as f64 >= 0.5;
    let empty = BAR_CELLS - filled - usize::from(half);
    let pct = ((frac * 100.0).floor() as u32).min(100);

    let mut line = String::new();
    let mut width = 0usize;
    let push = |code: &str, text: &str, out: &mut String, w: &mut usize| {
        out.push_str(&paint(code, text));
        *w += text.chars().count();
    };

    push(bright, &"█".repeat(filled), &mut line, &mut width);
    if half {
        push(value, "▓", &mut line, &mut width);
    }
    push(faint, &"─".repeat(empty), &mut line, &mut width);
    push("", "  ", &mut line, &mut width);
    push(value, &format!("{pct:>3}%"), &mut line, &mut width);

    // Rate and ETA need a little history to mean anything.
    let secs = phase_elapsed.as_secs_f64();
    if secs >= 0.2 && done > 0 {
        let rate = done as f64 / secs;
        push("", "  ", &mut line, &mut width);
        push(value, &format!("{:.0} MB/s", rate / 1e6), &mut line, &mut width);
        if done < total && rate > 0.0 {
            let eta = ((total - done) as f64 / rate).ceil() as u64;
            push("", "  ", &mut line, &mut width);
            push(faint, &format!("ETA {}", format_eta(eta)), &mut line, &mut width);
        }
    }

    (line, width)
}

/// `m:ss`, or `h:mm:ss` from an hour up.
fn format_eta(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_step_scales_with_a_floor() {
        assert_eq!(gate_step(0), MIN_STEP);
        assert_eq!(gate_step(100 << 20), MIN_STEP); // 100 MiB / 200 < 1 MiB
        assert_eq!(gate_step(2000 << 20), 10 << 20); // 2000 MiB / 200
    }

    #[test]
    fn percent_is_one_decimal_and_empty_total_is_complete() {
        assert_eq!(percent(0, 0), 100.0);
        assert_eq!(percent(0, 400), 0.0);
        assert_eq!(percent(1, 3), 33.3);
        assert_eq!(percent(400, 400), 100.0);
    }

    #[test]
    fn eta_formats_minutes_and_hours() {
        assert_eq!(format_eta(0), "0:00");
        assert_eq!(format_eta(41), "0:41");
        assert_eq!(format_eta(3599), "59:59");
        assert_eq!(format_eta(3600), "1:00:00");
        assert_eq!(format_eta(3661), "1:01:01");
    }

    #[test]
    fn header_line_plain_golden() {
        // Single file: no [k/N]; multi-file runs carry the counter. The name
        // is never truncated, however long.
        assert_eq!(format_header("movie.m2ts", 1, 1, None), "Scanning: movie.m2ts");
        assert_eq!(format_header("movie.mkv", 2, 7, None), "Scanning: movie.mkv  [2/7]");
        let long = "Indiana Jones and the Raiders of the Lost Ark (1981) - 4K.m2ts";
        assert_eq!(format_header(long, 1, 1, None), format!("Scanning: {long}"));
    }

    #[test]
    fn header_line_colored_wraps_plain_text() {
        let palette: &'static Palette = crate::render::Theme::Green.palette();
        let line = format_header("movie.mkv", 2, 7, Some(palette));
        assert!(line.contains("\x1b["));
        assert!(line.contains("Scanning:"));
        assert!(line.contains("movie.mkv"));
        assert!(line.contains("[2/7]"));
    }

    #[test]
    fn bar_line_plain_golden() {
        // 42% of 1000 bytes: no color, too little history for rate/ETA.
        // 0.42 * 32 = 13.44 cells: 13 solid, the .44 remainder below the
        // half-cell threshold.
        let (line, width) = format_line(0.42, 420, 1000, Duration::from_secs(0), None);
        assert_eq!(line, "█████████████───────────────────   42%");
        assert_eq!(width, line.chars().count());
    }

    #[test]
    fn bar_line_half_cell_edge() {
        // 13.5 / 32 cells: 13 solid plus the mid-tone half-cell.
        let (line, width) = format_line(13.5 / 32.0, 422, 1000, Duration::from_secs(0), None);
        assert_eq!(line, "█████████████▓──────────────────   42%");
        assert_eq!(width, line.chars().count());
    }

    #[test]
    fn bar_line_with_rate_and_eta() {
        // 512 MiB of 1 GiB in 2s => ~268 MB/s, ETA 2s.
        let (line, width) = format_line(0.5, 512 << 20, 1 << 30, Duration::from_secs(2), None);
        assert_eq!(line, "████████████████────────────────   50%  268 MB/s  ETA 0:02");
        assert_eq!(width, line.chars().count());
    }

    #[test]
    fn bar_line_colored_has_zero_width_codes() {
        let palette: &'static Palette = crate::render::Theme::Green.palette();
        let (line, width) = format_line(1.0, 1000, 1000, Duration::from_secs(0), Some(palette));
        // Same display width as the plain version, ANSI codes present.
        let (plain, plain_width) = format_line(1.0, 1000, 1000, Duration::from_secs(0), None);
        assert_eq!(width, plain_width);
        assert!(line.contains("\x1b["));
        assert!(line.len() > plain.len());
    }

    /// The single bar spans the whole file: an index phase fills the first
    /// half, the scan the second, and the fraction never moves backward at
    /// the phase switch. `finish` pins it to 1.0.
    #[test]
    fn bar_fraction_blends_phases_monotonically() {
        // Json mode writes to stderr — captured by the test harness, harmless.
        let p = Progress::new(Mode::Json, Path::new("x.hevc"), 1, 1);
        p.begin(Phase::Index, 100);
        p.update(50);
        assert_eq!(p.bar_fraction(), 0.25);
        p.update(100);
        assert_eq!(p.bar_fraction(), 0.5);
        p.begin(Phase::Scan, 200); // switch lands exactly where index left off
        assert_eq!(p.bar_fraction(), 0.5);
        p.update(100);
        assert_eq!(p.bar_fraction(), 0.75);
        p.finish();
        assert_eq!(p.bar_fraction(), 1.0);

        // No index phase: the scan owns the whole bar.
        let p = Progress::new(Mode::Json, Path::new("x.mkv"), 1, 1);
        p.begin(Phase::Scan, 100);
        p.update(40);
        assert_eq!(p.bar_fraction(), 0.4);
    }

    #[test]
    fn progress_event_serialization_is_pinned() {
        let e = Event {
            event: "progress",
            hdrprobe_schema_version: "1.0",
            file: "D:\\m\\movie.m2ts",
            file_index: 2,
            file_count: 7,
            phase: Some("scan"),
            bytes_done: Some(123),
            bytes_total: Some(456),
            percent: Some(27.0),
            elapsed_ms: 841.2,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"progress","hdrprobe_schema_version":"1.0","file":"D:\\m\\movie.m2ts","file_index":2,"file_count":7,"phase":"scan","bytes_done":123,"bytes_total":456,"percent":27.0,"elapsed_ms":841.2}"#
        );
    }

    #[test]
    fn done_event_omits_phase_fields() {
        let e = Event {
            event: "done",
            hdrprobe_schema_version: "1.0",
            file: "a.mkv",
            file_index: 1,
            file_count: 1,
            phase: None,
            bytes_done: None,
            bytes_total: None,
            percent: None,
            elapsed_ms: 9312.5,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"done","hdrprobe_schema_version":"1.0","file":"a.mkv","file_index":1,"file_count":1,"elapsed_ms":9312.5}"#
        );
    }

    #[test]
    fn off_sink_ignores_everything() {
        let p = Progress::off();
        p.begin(Phase::Scan, 100);
        p.update(50);
        p.update(100);
        p.finish();
        // Off mode never records phase state.
        assert!(p.phase.get().is_none());
    }

    /// The byte gate advances in `gate_step` increments and the 100% line is
    /// emitted exactly once (`total_emitted` latches).
    #[test]
    fn update_gates_by_bytes_and_latches_total() {
        // Json mode writes to stderr — captured by the test harness, harmless.
        let p = Progress::new(Mode::Json, Path::new("x.ts"), 1, 1, );
        let total = 400 << 20; // step = 2 MiB
        p.begin(Phase::Scan, total);
        assert_eq!(p.next_emit.get(), 2 << 20);
        p.update(1 << 20); // below the gate: no state change
        assert_eq!(p.next_emit.get(), 2 << 20);
        p.update(3 << 20); // past the gate: advances one step from `done`
        assert_eq!(p.next_emit.get(), (3 << 20) + (2 << 20));
        p.update(2 << 20); // regression clamped monotonic
        assert_eq!(p.done.get(), 3 << 20);
        p.update(total + 5); // clamped to total, latches
        assert_eq!(p.done.get(), total);
        assert!(p.total_emitted.get());
        p.update(total); // second 100% is swallowed
        assert!(p.total_emitted.get());
    }
}
