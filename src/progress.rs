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
    /// One stderr line per phase, `\r`-rewritten while the phase runs. A
    /// completed phase's line **persists** at its final state and the next
    /// phase starts a fresh line — consecutive phases read as steps, never as
    /// a bar that restarted (an MKV `--full` is indexing *then* scanning; a
    /// second 0% on the same line looks like a hang). Only a failed file's
    /// in-progress line is erased, so the diagnostic prints clean. `color`
    /// carries the active theme's palette when stderr takes ANSI.
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
    /// Human label for the bar.
    fn label(self) -> &'static str {
        match self {
            Phase::Index => "indexing",
            Phase::Scan => "scanning",
        }
    }
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
    /// Bare file name, truncated for the bar.
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
        }
    }

    /// Start (or switch to) a phase with a known byte denominator. Emits the
    /// 0% line so a slow first window still shows immediate feedback.
    pub fn begin(&self, phase: Phase, total_bytes: u64) {
        if matches!(self.mode, Mode::Off) {
            return;
        }
        // A previous phase's bar line stays on screen at its final state; the
        // new phase draws on a fresh line (see `Mode::Bar`).
        self.persist_bar();
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

    /// File finished successfully: keep the final bar line / emit the JSON
    /// `done` event. Error paths skip this and rely on `Drop`, which erases
    /// the in-progress line instead.
    pub fn finish(&self) {
        match self.mode {
            Mode::Off => {}
            Mode::Bar { .. } => self.persist_bar(),
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

    fn emit(&self, done: u64, total: u64) {
        let Some(phase) = self.phase.get() else { return };
        match self.mode {
            Mode::Off => {}
            Mode::Bar { color } => {
                let (line, width) = format_line(
                    phase,
                    &self.name,
                    done,
                    total,
                    self.phase_started.get().elapsed(),
                    self.file_index,
                    self.file_count,
                    color,
                );
                // Rewrite in place, padding over any leftover from a longer
                // previous line — works without ANSI erase sequences.
                let pad = self.bar_width.get().saturating_sub(width);
                let mut err = std::io::stderr().lock();
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

    /// Freeze the on-screen bar line: redraw it at the phase's final recorded
    /// state, then move past it so it stays as a step record (idempotent;
    /// no-op for other modes or when nothing was drawn).
    fn persist_bar(&self) {
        if !matches!(self.mode, Mode::Bar { .. }) || self.bar_width.get() == 0 {
            return;
        }
        // The last throttled frame may be stale; refresh before freezing.
        self.emit(self.done.get(), self.total.get());
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err);
        let _ = err.flush();
        self.bar_width.set(0);
    }

    /// Blank out an on-screen bar line (idempotent; no-op for other modes).
    fn clear_bar(&self) {
        let width = self.bar_width.replace(0);
        if width > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{:width$}\r", "", width = width);
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
const BAR_CELLS: usize = 14;
/// Longest file name shown before truncation.
const NAME_MAX: usize = 24;

/// Render one bar line; returns the string and its display width (the ANSI
/// codes are zero-width). Pure, so it's testable without a terminal. Shape:
/// `scanning  movie.m2ts  ██████────────  42%  118 MB/s  ETA 0:41  [2/7]`
/// (rate/ETA appear once the phase has enough history; `[k/N]` only for
/// multi-file runs). Fits an 80-column terminal at every field's widest.
#[allow(clippy::too_many_arguments)]
fn format_line(
    phase: Phase,
    name: &str,
    done: u64,
    total: u64,
    phase_elapsed: Duration,
    file_index: usize,
    file_count: usize,
    color: Option<&'static Palette>,
) -> (String, usize) {
    let paint = |code: &str, text: &str| -> String {
        match color {
            Some(_) if !code.is_empty() => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_string(),
        }
    };
    let (bright, value, label, faint) = match color {
        Some(p) => (p.bright, p.value, p.label, p.faint),
        None => ("", "", "", ""),
    };

    let frac = if total == 0 { 1.0 } else { done as f64 / total as f64 };
    let filled = (frac * BAR_CELLS as f64).floor() as usize;
    let filled = filled.min(BAR_CELLS);
    let pct = (frac * 100.0).floor() as u32;

    let shown = truncate_name(name, NAME_MAX);
    let mut line = String::new();
    let mut width = 0usize;
    let push = |code: &str, text: &str, out: &mut String, w: &mut usize| {
        out.push_str(&paint(code, text));
        *w += text.chars().count();
    };

    push(label, phase.label(), &mut line, &mut width);
    push("", "  ", &mut line, &mut width);
    push(label, &shown, &mut line, &mut width);
    push("", "  ", &mut line, &mut width);
    push(bright, &"█".repeat(filled), &mut line, &mut width);
    push(faint, &"─".repeat(BAR_CELLS - filled), &mut line, &mut width);
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

    if file_count > 1 {
        push("", "  ", &mut line, &mut width);
        push(faint, &format!("[{file_index}/{file_count}]"), &mut line, &mut width);
    }

    (line, width)
}

/// Keep a file name within `max` display characters, marking the cut with an
/// ellipsis.
fn truncate_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let head: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
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
    fn names_truncate_with_an_ellipsis() {
        assert_eq!(truncate_name("movie.m2ts", 24), "movie.m2ts");
        let long = "a_very_long_remux_file_name_indeed.m2ts";
        let cut = truncate_name(long, 24);
        assert_eq!(cut.chars().count(), 24);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn bar_line_plain_golden() {
        // 42% of 1000 bytes, 500 MB/s pace, single file: no color, no [k/N].
        let (line, width) = format_line(
            Phase::Scan,
            "movie.m2ts",
            420,
            1000,
            Duration::from_secs(0), // too little history for rate/ETA
            1,
            1,
            None,
        );
        assert_eq!(line, "scanning  movie.m2ts  █████─────────   42%");
        assert_eq!(width, line.chars().count());
    }

    #[test]
    fn bar_line_with_rate_eta_and_file_counter() {
        // 512 MiB of 1 GiB in 2s => ~268 MB/s, ETA 2s.
        let (line, width) = format_line(
            Phase::Index,
            "movie.mkv",
            512 << 20,
            1 << 30,
            Duration::from_secs(2),
            2,
            7,
            None,
        );
        assert_eq!(line, "indexing  movie.mkv  ███████───────   50%  268 MB/s  ETA 0:02  [2/7]");
        assert_eq!(width, line.chars().count());
    }

    #[test]
    fn bar_line_colored_has_zero_width_codes() {
        let palette: &'static Palette = crate::render::Theme::Green.palette();
        let (line, width) =
            format_line(Phase::Scan, "m.ts", 1000, 1000, Duration::from_secs(0), 1, 1, Some(palette));
        // Same display width as the plain version, ANSI codes present.
        let (plain, plain_width) =
            format_line(Phase::Scan, "m.ts", 1000, 1000, Duration::from_secs(0), 1, 1, None);
        assert_eq!(width, plain_width);
        assert!(line.contains("\x1b["));
        assert!(line.len() > plain.len());
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
