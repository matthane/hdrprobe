//! hdrprobe — fast HDR / Dolby Vision metadata inspector.

mod av1;
mod avc;
mod bdiso;
mod bits;
mod container;
mod dv;
mod hdr;
mod hevc;
mod model;
mod prefetch;
mod progress;
mod prores;
mod render;
mod sample;
mod shell;
mod sidecar;
mod vp9;

use std::fs::File;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use memmap2::Mmap;

use crate::model::{Hdr10Plus, Report};
use crate::render::{RenderOpts, Theme};

#[derive(Parser, Debug)]
#[command(name = "hdrprobe", version, about = "Fast HDR / HDR10+ / Dolby Vision metadata inspector")]
struct Cli {
    /// Input file(s) or directory(ies); '-' probes a stream head read from stdin.
    #[arg(required_unless_present_any = ["install_shell", "uninstall_shell"])]
    files: Vec<PathBuf>,

    /// Output JSON instead of text (array for multiple files).
    #[arg(short, long)]
    json: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Exhaustive per-frame scan (drops the sub-2s guarantee).
    #[arg(short, long)]
    full: bool,

    /// Container DV config only — skip RPU parsing.
    #[arg(long)]
    no_rpu: bool,

    /// Number of seek points to sample.
    #[arg(short, long, default_value_t = 16)]
    samples: usize,

    /// Comma list of sections to show: general,hdr,dv,hdr10plus.
    #[arg(long)]
    sections: Option<String>,

    /// Colour output: auto, always, never.
    #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,

    /// Colour theme for coloured output.
    #[arg(long, value_enum, env = "HDRPROBE_THEME", default_value_t = Theme::Paper)]
    theme: Theme,

    /// Progress reporting for --full scans (the fast path finishes in
    /// milliseconds and never reports): auto shows a bar when stderr is a
    /// terminal, json emits one machine-readable event per stderr line.
    #[arg(long, value_enum, default_value_t = ProgressWhen::Auto)]
    progress: ProgressWhen,

    /// One-line summary per file.
    #[arg(short, long)]
    quiet: bool,

    /// Descend into directory arguments.
    #[arg(short, long)]
    recursive: bool,

    /// Number of parallel worker threads.
    #[arg(long)]
    threads: Option<usize>,

    /// Write output to a file instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Register a right-click "hdrprobe" context-menu submenu with Fast and Full entries for supported files and folders (Windows).
    #[arg(long)]
    install_shell: bool,

    /// Remove the right-click context-menu submenu (Windows).
    #[arg(long)]
    uninstall_shell: bool,

    /// The console window exists solely for this run — set by the shell verb's
    /// fresh window, never for a shared interactive terminal. Currently inert:
    /// it once let the end-of-run screen clear purge scrollback, but reports
    /// now stream per file and nothing clears the screen. Still accepted (and
    /// still emitted by `shell.rs`) because registered verb command strings in
    /// user registries pass it — removing the flag would break every existing
    /// install's right-click verbs.
    #[arg(long, hide = true)]
    #[allow(dead_code)]
    own_console: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
    Ndjson,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ProgressWhen {
    Auto,
    Bar,
    Json,
    Off,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Shell integration is an action-and-exit path: register/remove the Explorer
    // context-menu verb, then return without touching the file pipeline. Its
    // confirmation renders in the report's own styling (masthead + section rule
    // + kv rows), gated by the same --color policy against stdout.
    if cli.install_shell || cli.uninstall_shell {
        let color = match cli.color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => supports_color::on(supports_color::Stream::Stdout).is_some(),
        };
        if color {
            print!("{}", render::render_banner(cli.theme));
        }
        // Same width probe as the report path: the section rule stretches to
        // the live terminal, pipes keep the fixed fallback.
        let wrap_width = terminal_width();
        let res = if cli.install_shell {
            shell::install(color, cli.theme, wrap_width)
        } else {
            shell::uninstall(color, cli.theme, wrap_width)
        };
        return match res {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        };
    }

    // Third-party parsers (libdovi / hdr10plus) can panic on malformed input.
    // We isolate those with `catch_unwind` (see `dv::rpu::guard`) and handle
    // them as `None`, so keep the default hook quiet for the expected ones
    // while still surfacing genuine bugs from our own code.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !dv::rpu::panic_silenced() {
            default_hook(info);
        }
    }));

    if let Some(n) = cli.threads {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }

    // `-` (stdin) can carry at most one stream per invocation.
    if cli.files.iter().filter(|f| f.as_os_str() == "-").count() > 1 {
        eprintln!("error: '-' (stdin) may be given at most once");
        return ExitCode::from(1);
    }

    let paths = match collect_paths(&cli.files, cli.recursive) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    if paths.is_empty() {
        eprintln!("error: no input files found");
        return ExitCode::from(1);
    }

    let format = if cli.json { Format::Json } else { cli.format };
    let use_color = match cli.color {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            cli.output.is_none()
                && format == Format::Text
                && supports_color::on(supports_color::Stream::Stdout).is_some()
        }
    };

    // Progress is `--full`-only (the fast path is over in milliseconds) and
    // lives entirely on stderr — stdout stays the pure report stream. Under
    // `auto` the bar needs an interactive stderr; the bar's colour follows the
    // same --color policy as the report, checked against stderr's own
    // capability.
    let progress_mode = if !cli.full {
        progress::Mode::Off
    } else {
        let bar_color = match cli.color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => supports_color::on(supports_color::Stream::Stderr).is_some(),
        };
        let bar = progress::Mode::Bar { color: bar_color.then(|| cli.theme.palette()) };
        match cli.progress {
            ProgressWhen::Auto if std::io::stderr().is_terminal() => bar,
            ProgressWhen::Auto => progress::Mode::Off,
            ProgressWhen::Bar => bar,
            ProgressWhen::Json => progress::Mode::Json,
            ProgressWhen::Off => progress::Mode::Off,
        }
    };

    // Long value lines reflow to the terminal width — interactive text
    // reports only. Piped/redirected stdout and `--output` files have no
    // terminal (the probes below fail on non-console handles), and the
    // JSON/NDJSON/quiet machine paths never wrap, so every consumed byte
    // stream keeps its exact historical shape.
    let wrap_width = if cli.output.is_none() && format == Format::Text && !cli.quiet {
        terminal_width()
    } else {
        None
    };

    let mut out_buf = String::new();
    let mut json_reports: Vec<serde_json::Value> = Vec::new();
    let mut had_error = false;
    // Full text reports already emitted/buffered (drives the between-reports
    // divider; a buffer-emptiness check would miscount when the masthead is
    // buffered for `--output`).
    let mut text_reports = 0usize;

    // Each report goes out the moment its file finishes, so a long multi-file
    // `--full` scan shows results as they're ready instead of after the last
    // file. Only pretty JSON must wait for the end (one array), and
    // `--output` keeps its single atomic file write. Byte-neutral: the
    // streamed bytes are exactly what the end-of-run dump used to print.
    let stream_reports = cli.output.is_none() && format != Format::Json;

    // The masthead prints once per run, only on the colored interactive text
    // path — quiet, JSON/NDJSON, and piped output stay machine-clean. It goes
    // out immediately (not into the report buffer) so a long `--full` scan
    // shows it above the stderr progress bar, not after scanning finishes;
    // with `--output` it stays buffered so it lands in the file.
    let show_banner = use_color && format == Format::Text && !cli.quiet;
    let banner_eager = show_banner && cli.output.is_none();
    if show_banner {
        let banner = render::render_banner(cli.theme);
        if banner_eager {
            print!("{banner}");
            let _ = std::io::stdout().flush();
        } else {
            out_buf.push_str(&banner);
        }
    }

    for (i, path) in paths.iter().enumerate() {
        let progress = progress::Progress::new(progress_mode, path, i + 1, paths.len());
        let result = if path.as_os_str() == "-" {
            process_stdin(&cli, &progress)
        } else {
            process_file(path, &cli, &progress)
        };
        match result {
            Ok(report) => {
                // On the decorated interactive path the finished file's
                // header + bar are erased so its streamed report prints in
                // their place — the screen accumulates clean reports with
                // the live bar always at the bottom. Everywhere else the
                // bar persists above the report (or JSON emits `done`).
                if banner_eager {
                    progress.finish_erased();
                } else {
                    progress.finish();
                }
                let mut piece = String::new();
                match format {
                    Format::Text => {
                        if cli.quiet {
                            piece.push_str(&render::render_quiet(&report));
                            piece.push('\n');
                        } else {
                            let opts =
                                render_opts(&cli, use_color, wrap_width, i + 1, paths.len());
                            // Rule between consecutive reports only — never
                            // before the first or after the last, so a
                            // single-report run's output is unchanged.
                            if text_reports > 0 {
                                piece.push_str(&render::render_divider(&opts));
                            }
                            text_reports += 1;
                            piece.push_str(&render::render(&report, &opts));
                            piece.push('\n');
                        }
                    }
                    Format::Json => json_reports.push(serde_json::to_value(&report).unwrap()),
                    Format::Ndjson => {
                        piece.push_str(&serde_json::to_string(&report).unwrap());
                        piece.push('\n');
                    }
                }
                if stream_reports {
                    print!("{piece}");
                    let _ = std::io::stdout().flush();
                } else {
                    out_buf.push_str(&piece);
                }
            }
            Err(e) => {
                // Drop erases any live bar line so the diagnostic prints
                // clean (the header stays as context above it).
                drop(progress);
                had_error = true;
                eprintln!("error: {}: {:#}", path.display(), e);
            }
        }
    }

    if format == Format::Json {
        let v = if json_reports.len() == 1 && paths.len() == 1 {
            json_reports.into_iter().next().unwrap()
        } else {
            serde_json::Value::Array(json_reports)
        };
        out_buf = serde_json::to_string_pretty(&v).unwrap();
        out_buf.push('\n');
    }

    // There is no end-of-run screen clear: reports stream as files finish,
    // so a clear here would wipe output the user is already reading. The
    // decorated interactive path stays clean anyway — `finish_erased` above
    // removes each file's progress display before its report prints.
    if let Err(e) = write_output(&cli.output, &out_buf) {
        eprintln!("error: writing output: {e}");
        return ExitCode::from(1);
    }

    if had_error {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

fn render_opts(
    cli: &Cli,
    color: bool,
    wrap_width: Option<usize>,
    file_index: usize,
    file_count: usize,
) -> RenderOpts {
    let (mut g, mut h, mut d, mut hp) = (true, true, true, true);
    if let Some(list) = &cli.sections {
        g = false;
        h = false;
        d = false;
        hp = false;
        for s in list.split(',') {
            match s.trim() {
                "general" => g = true,
                "hdr" => h = true,
                "dv" => d = true,
                "hdr10plus" => hp = true,
                _ => {}
            }
        }
    }
    RenderOpts {
        color,
        theme: cli.theme,
        wrap_width,
        file_index,
        file_count,
        show_general: g,
        show_hdr: h,
        show_dv: d,
        show_hdr10plus: hp,
    }
}

/// Visible column count of the terminal on stdout, `None` when stdout isn't
/// one (pipes, redirects, files) — which is what gates value-line reflow, so
/// no separate is-a-terminal check is needed. Probed once per run: a resize
/// mid-run reflows from the next run.
#[cfg(windows)]
fn terminal_width() -> Option<usize> {
    use std::os::windows::io::AsRawHandle as _;
    console_width(std::io::stdout().as_raw_handle())
}

/// Stderr counterpart, sizing the progress header's wrapped-row count for
/// `progress::Progress::finish_erased`. Probed per header print, not per
/// run — the bar redraws track a resize, so the erase should too.
#[cfg(windows)]
fn stderr_terminal_width() -> Option<usize> {
    use std::os::windows::io::AsRawHandle as _;
    console_width(std::io::stderr().as_raw_handle())
}

#[cfg(windows)]
fn console_width(handle: std::os::windows::io::RawHandle) -> Option<usize> {
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        size: Coord,
        cursor_position: Coord,
        attributes: u16,
        window: SmallRect,
        maximum_window_size: Coord,
    }
    extern "system" {
        fn GetConsoleScreenBufferInfo(
            handle: *mut core::ffi::c_void,
            info: *mut ConsoleScreenBufferInfo,
        ) -> i32;
    }
    let mut info = ConsoleScreenBufferInfo {
        size: Coord { x: 0, y: 0 },
        cursor_position: Coord { x: 0, y: 0 },
        attributes: 0,
        window: SmallRect { left: 0, top: 0, right: 0, bottom: 0 },
        maximum_window_size: Coord { x: 0, y: 0 },
    };
    let ok = unsafe { GetConsoleScreenBufferInfo(handle, &mut info) };
    if ok == 0 {
        return None;
    }
    // The visible window, not the (often much taller/wider) screen buffer.
    let width = i32::from(info.window.right) - i32::from(info.window.left) + 1;
    (width > 0).then_some(width as usize)
}

/// Unix counterpart: the TIOCGWINSZ window size of stdout's tty.
#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    tty_width(libc::STDOUT_FILENO)
}

/// Stderr counterpart, sizing the progress header's wrapped-row count for
/// `progress::Progress::finish_erased`.
#[cfg(unix)]
fn stderr_terminal_width() -> Option<usize> {
    tty_width(libc::STDERR_FILENO)
}

#[cfg(unix)]
fn tty_width(fd: libc::c_int) -> Option<usize> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then(|| usize::from(ws.ws_col))
}

/// Platforms with neither probe don't reflow — the unwrapped line is always
/// correct output, just longer than the window.
#[cfg(not(any(windows, unix)))]
fn terminal_width() -> Option<usize> {
    None
}

/// No probe: the progress header is assumed to occupy a single row.
#[cfg(not(any(windows, unix)))]
fn stderr_terminal_width() -> Option<usize> {
    None
}

fn process_file(path: &Path, cli: &Cli, progress: &progress::Progress) -> Result<Report> {
    // Metadata sidecars (raw RPU, DV XML, HDR10+ JSON) carry no picture data and
    // skip the whole video pipeline. `None` means "not a sidecar" — fall through.
    if let Some(report) = sidecar::try_process(path).with_context(|| format!("parsing {}", path.display()))? {
        return Ok(report);
    }

    let started = Instant::now();
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    // SAFETY: file is read-only inspected; we accept the usual mmap caveat that
    // external truncation during the run is UB. Acceptable for a CLI inspector.
    let mmap = unsafe { Mmap::map(&file) }.with_context(|| format!("mmapping {}", path.display()))?;

    // On a network filesystem (SMB/NFS) the mmap parse would fault the metadata
    // region in as many synchronous round-trips; warm it with one pipelined read
    // first. No-op on local volumes; never changes what we parse.
    let remote = prefetch::is_remote(&file);
    let warmed_head = prefetch::warm_metadata(remote, &file, path, &mmap);

    // A Blu-ray ISO is probed through its BDMV main feature: locate the
    // playlist-selected clip's contiguous byte range, then run the ordinary
    // TS/M2TS pipeline over that *subslice*, so every slice-relative
    // mechanism (head/tail windows, streaming positions, bitrate
    // denominators, progress) is correct by construction. Extension-gated: a
    // UDF image under another name takes the ordinary demux path below.
    let is_iso = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"));
    let feature = if is_iso && bdiso::is_udf_iso(&mmap) {
        let f = bdiso::locate_main_feature(&mmap, remote.then_some(&file))
            .context("locating the BDMV main feature")?;
        // The clip's TS head/tail windows, translated to its range in the
        // image: the ISO counterpart of `warm_metadata`'s TS branch.
        prefetch::warm_ts_windows(remote, &file, f.clip_start, f.clip_len);
        Some(f)
    } else {
        None
    };
    let data: &[u8] = match &feature {
        Some(f) => &mmap[f.clip_start as usize..(f.clip_start + f.clip_len) as usize],
        None => &mmap,
    };

    // `--full` on a genuinely remote volume: the whole-file walks tailgate a
    // bounded look-ahead warm (`prefetch::Frontier`), so the file crosses the
    // wire once, linearly, instead of thousands of scattered page-fault
    // round-trips. Off everywhere else — local `--full` and the default path
    // are unchanged. `warm_metadata` above still covers the tail extents (TS
    // last-PCR, MKV `Tags`, `mfra`) a front-first stream reaches last. For an
    // ISO the frontier is based at the clip: walk positions are subslice-
    // relative and the reads must land at `clip_start + pos` in the image.
    let frontier = if cli.full && prefetch::is_remote_strict(&file, path) {
        match &feature {
            Some(f) => prefetch::Frontier::new_at(&file, f.clip_start, f.clip_len),
            None => prefetch::Frontier::new(&file, size),
        }
    } else {
        prefetch::Frontier::off()
    };

    let demux = match &feature {
        // The main feature is M2TS by construction (the locator's sync-lock
        // gate); extension dispatch would misroute the `.iso` name.
        Some(_) => container::ts::demux(data, cli.full, progress, &frontier)
            .context("demuxing the BDMV main-feature clip")?,
        None => {
            container::demux(path, data, cli.full, progress, &frontier).context("demux failed")?
        }
    };

    // The sampled access units are scattered across the whole file (worst for
    // MP4, whose sample index spans a multi-GB mdat), so warm exactly the
    // ranges the sampler will fault. Default path only: `--full` reads every
    // chunk and `--no-rpu` reads none.
    if !cli.full && !cli.no_rpu {
        prefetch::warm_sample_chunks(remote, &file, &demux, cli.samples, warmed_head);
    }

    Ok(assemble_report(
        path.display().to_string(),
        size,
        data,
        &demux,
        feature,
        false,
        cli,
        progress,
        &frontier,
        started,
    ))
}

/// `hdrprobe -`: probe a bounded head of stdin. The buffer feeds the same
/// sniff-dispatched slice pipeline a file probe runs (no extension ⇒
/// `container::demux` dispatches by magic bytes); everything that needs a real
/// file — the sidecar gate, mmap, prefetch, the Blu-ray ISO branch, the
/// `--full` frontier — is skipped. A stream that ended within the head budget
/// is complete and reports exactly like a file probe; one that exceeded it is
/// truncated: the report says so (`input_truncated`, `size_bytes` = bytes
/// probed) and prefix-derived facts are withheld.
fn process_stdin(cli: &Cli, progress: &progress::Progress) -> Result<Report> {
    if cli.full {
        bail!("--full cannot scan stdin (a pipe has no seekable whole); pass a file path");
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("stdin is a terminal; pipe stream data in or pass a file path");
    }

    let started = Instant::now();
    let (buf, truncated) = read_stdin_head(stdin.lock()).context("reading stdin")?;
    if buf.is_empty() {
        bail!("no data on stdin");
    }

    let frontier = prefetch::Frontier::off();
    let mut demux = container::demux(Path::new("-"), &buf, cli.full, progress, &frontier)
        .context("demux failed")?;
    if truncated {
        suppress_prefix_derived_facts(&mut demux);
    }

    Ok(assemble_report(
        "-".to_string(),
        buf.len() as u64,
        &buf,
        &demux,
        None,
        truncated,
        cli,
        progress,
        &frontier,
        started,
    ))
}

/// Truncation honesty for `hdrprobe -`: drop facts whose derivation spans the
/// payload rather than a declared header — over a prefix they'd be
/// valid-looking wrong numbers. TS duration is the head-to-tail PCR delta,
/// and a prefix's "tail" is just the cut point. Bitrates survive only for
/// MP4/MOV, whose stsz/trun table sums are exact regardless of truncation;
/// MKV's summed block index can't distinguish a cleanly-cut prefix from a
/// complete walk, and every `overall` rate divides the prefix's byte count.
/// MP4 `mvhd` / MKV Segment-Info durations are declared header facts and
/// stand. Keyed on the container label — never thread a truncation flag into
/// the backends.
fn suppress_prefix_derived_facts(demux: &mut container::Demux) {
    if demux.container.starts_with("MPEG-2 TS") {
        demux.duration_secs = None;
    }
    let mp4 = matches!(demux.container, "MP4 (ISOBMFF)" | "QuickTime (MOV)");
    for t in &mut demux.tracks {
        t.bitrate = t.bitrate.filter(|b| mp4 && b.scope == model::BitrateScope::VideoStream);
    }
}

/// Sniff block read from stdin before choosing the head budget: enough for
/// every `container::sniff_demux` magic check (the TS sync-lock needs under
/// 1 KiB) with generous slack.
const STDIN_SNIFF_BYTES: usize = 64 << 10; // 64 KiB

/// Head budget for non-TS stdin input. A stream that sniffs as TS/M2TS gets
/// `ts::HEAD_SCAN_BYTES` (24 MiB) instead — the same first-IDR coupling as
/// the file path, since TS metadata rides the in-band SPS ~a GOP in. MKV/MP4
/// declare their metadata up front and raw streams bound their head walks at
/// 8 MiB, so 16 MiB is comfortable slack for everything else.
const STDIN_HEAD_BYTES: usize = 16 << 20; // 16 MiB

/// Head budget for a sniffed stdin block: how many bytes of the stream are
/// worth reading before parsing begins.
fn stdin_budget(head: &[u8]) -> usize {
    if container::sniffs_as_ts(head) {
        container::ts::HEAD_SCAN_BYTES as usize
    } else {
        STDIN_HEAD_BYTES
    }
}

/// Read a bounded head from `r`: a sniff block first, then up to the sniffed
/// format's budget plus one byte — reading past the budget is what makes
/// truncation detectable (`true` ⇒ the stream held more; the extra byte is
/// dropped). EOF at or under the budget means the input is complete.
/// Generic over the reader and budget so tests drive it with `Cursor` and
/// tiny budgets.
fn read_bounded_head(
    mut r: impl std::io::Read,
    sniff_bytes: usize,
    budget_for: impl FnOnce(&[u8]) -> usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.by_ref().take(sniff_bytes as u64).read_to_end(&mut buf)?;
    let budget = budget_for(&buf);
    // A short sniff read means EOF already arrived; only a full block can
    // have more bytes behind it.
    if buf.len() >= sniff_bytes {
        let remaining = (budget + 1).saturating_sub(buf.len());
        r.take(remaining as u64).read_to_end(&mut buf)?;
    }
    let truncated = buf.len() > budget;
    if truncated {
        buf.truncate(budget);
    }
    Ok((buf, truncated))
}

/// The stdin head read: sniff block, then the format-aware budget.
fn read_stdin_head(r: impl std::io::Read) -> std::io::Result<(Vec<u8>, bool)> {
    read_bounded_head(r, STDIN_SNIFF_BYTES, stdin_budget)
}

/// Sample the demuxed stream and assemble the final `Report` — the shared
/// back half of `process_file` and `process_stdin`. `truncated` marks a
/// stdin head cut short by its budget: the report carries it verbatim and
/// the scan-derived duration fallback is withheld (a prefix's frame count is
/// not the stream's). File probes always pass `false`.
#[allow(clippy::too_many_arguments)]
fn assemble_report(
    file: String,
    size_bytes: u64,
    data: &[u8],
    demux: &container::Demux,
    feature: Option<bdiso::MainFeature>,
    truncated: bool,
    cli: &Cli,
    progress: &progress::Progress,
    frontier: &prefetch::Frontier,
    started: Instant,
) -> Report {
    let opts = sample::Options { samples: cli.samples, full: cli.full, no_rpu: cli.no_rpu };
    let scan = sample::scan(demux, data, &opts, progress, frontier);

    // Raw AV1 `--full`: duration (frames ÷ fps) exists only after the fused
    // walk counted the frames, so it lands here instead of demux.
    let duration_secs = demux.duration_secs.or_else(|| {
        (!truncated).then(|| scan.tracks.iter().find_map(|t| t.duration_secs)).flatten()
    });

    let mut video_tracks = Vec::with_capacity(demux.tracks.len());
    for (track, scan) in demux.tracks.iter().zip(scan.tracks) {
        let is_av1 = matches!(track.codec, container::Codec::Av1);
        let mut dv = scan
            .dv
            .finalize(track.width, track.height, track.dv_config.as_ref(), cli.full, is_av1, track.dv_dual_track)
            .or_else(|| track.dv_config.as_ref().map(|c| dv::levels::container_only(c, track.dv_dual_track)));

        // The reported frame rate. The `--full` streaming walks recover what
        // their demux's bounded pass no longer can: raw IVF's whole-stream
        // average rate (`scan.fps`), and for MKV without a DefaultDuration
        // the exact frame count feeding the count ÷ duration fallback the
        // demux's complete index used to compute — same inputs, same values.
        let fps = track.fps.or(scan.fps).or_else(|| match (scan.frame_count, duration_secs) {
            (Some(n), Some(d)) if n > 0 && d > 0.0 => Some(n as f64 / d),
            _ => None,
        });

        // The two grade-vs-base-layer verdicts (FEL brightness expansion,
        // mastering primaries mismatch) are only decidable here on the video
        // path: both need the base layer's own declared mastering display
        // (container MDCV or ST.2086 SEI), which a metadata sidecar doesn't
        // have. Gated on *this track's own* mastering/SEI — an independent
        // sibling track can never lend the DV track its display, or vice versa.
        if let Some(dv) = dv.as_mut() {
            let bl_mastering = track.mastering.as_ref().or(scan.sei.mastering.as_ref());
            dv::levels::flag_fel_brightness_expansion(dv, bl_mastering.map(|m| m.max_luminance));
            dv::levels::flag_mastering_primaries_mismatch(
                dv,
                bl_mastering.and_then(|m| m.primaries.as_deref()),
            );
            // The level derivation is video-path-only for the same reason:
            // it needs the track's real coded dimensions and the *reported*
            // frame rate — a metadata sidecar has neither (assumed canvas,
            // authoring-declared rate).
            dv::levels::fill_derived_level(dv, track.width, track.height, fps);
        }

        let hdr10plus = scan.sei.hdr10plus.map(|info| Hdr10Plus {
            application_version: info.application_version,
            num_windows: info.num_windows,
            profile: (info.profile != 0).then_some(info.profile as char),
            target_max_luminance: (info.target_max_luminance > 0)
                .then_some(info.target_max_luminance),
        });

        // HDR Vivid: the MP4 `cuvv` box is the container's declaration and
        // wins the version (its bitmap's highest bit — a multi-version stream
        // declares them all, where a sampled SEI shows one); the SEI supplies
        // the data-set type and target set and is the sole source everywhere
        // else. Either alone is presence. The published versions are all X.0.
        let hdr_vivid = match (track.cuvv_version_map, scan.sei.hdr_vivid.as_ref()) {
            (Some(map), sei) => Some(model::HdrVivid {
                version: format!("{}.0", 16 - map.leading_zeros()),
                system_start_code: sei.map(|s| s.system_start_code),
                target_max_luminances: sei
                    .map(|s| hdr::pq_targets_to_nits(&s.target_pq))
                    .unwrap_or_default(),
                // Like dolby_vision.sampled: false under --no-rpu (a box-only
                // detection sampled nothing) and under --full.
                sampled: !cli.full && sei.is_some(),
            }),
            (None, Some(s)) => Some(model::HdrVivid {
                version: format!("{}.0", s.version),
                system_start_code: Some(s.system_start_code),
                target_max_luminances: hdr::pq_targets_to_nits(&s.target_pq),
                sampled: !cli.full,
            }),
            (None, None) => None,
        };

        let sl_hdr = scan.sei.sl_hdr.as_ref().map(|sl| model::SlHdr {
            mode: sl.mode,
            spec_version: format!("{}.{}", sl.spec_major, sl.spec_minor),
            // Values past 1 are reserved in TS 103 433: name neither, never guess.
            payload_mode: match sl.payload_mode {
                Some(0) => Some("parameter-based".to_string()),
                Some(1) => Some("table-based".to_string()),
                _ => None,
            },
            // An unrecognized CICP code drops the name, never a guess; the
            // luminance still reports.
            target_primaries: sl
                .target_primaries
                .and_then(|c| container::cicp_primaries(c as u16))
                .map(str::to_string),
            target_max_luminance: sl.target_max_nits.map(u32::from),
            source_mastering: sl.source_mastering.clone(),
        });

        let hdr = Some(hdr::assemble(track, dv.as_ref(), &scan.sei));

        // Reflect the HLG/PQ alt-transfer SEI override in the displayed colour line.
        let mut color = track.color.clone();
        if let Some(pt) = scan.sei.preferred_transfer {
            if let Some(t) = container::cicp_transfer(pt as u16) {
                color.transfer = Some(t.to_string());
            }
        }

        video_tracks.push(model::VideoTrack {
            track_number: track.track_number,
            program: track.program,
            default: track.default_flag,
            codec: track.codec.label(),
            codec_profile: track.codec_profile.clone(),
            width: if track.width > 0 { Some(track.width) } else { None },
            height: if track.height > 0 { Some(track.height) } else { None },
            fps,
            // A container-known rate wins (MKV statistics tags); the `--full`
            // streaming walks (TS ES bytes, MKV block bytes) fill the gap with
            // the exact per-track sum their demux could no longer compute —
            // the same value the old whole-stream paths produced (`Some(0)`
            // es_bytes ⇒ `None`, as before).
            bitrate: track.bitrate.or_else(|| {
                scan.es_bytes
                    .and_then(|bytes| model::Bitrate::video_stream(bytes, duration_secs))
            }),
            bit_depth: track.bit_depth,
            chroma: track.chroma.clone(),
            stereo: track.stereo.clone(),
            color,
            hdr,
            dolby_vision: dv,
            hdr10plus,
            sl_hdr,
            hdr_vivid,
        });
    }

    // The ISO report describes the probed clip (duration, bitrate, tracks)
    // under the ISO's own name and size; the `Main feature` line carries the
    // selected playlist/clip and the playlist's edit duration.
    let container = match &feature {
        Some(_) => "Blu-ray ISO (BDMV)".to_string(),
        None => demux.container.to_string(),
    };
    let bd_iso = feature.map(|f| model::BdIso {
        playlist: f.playlist,
        playlist_duration_secs: f.playlist_duration_secs,
        clip: f.clip,
        clip_index: f.clip_index,
        clip_count: f.clip_count,
    });

    Report {
        hdrprobe_schema_version: model::SCHEMA_VERSION,
        file,
        size_bytes,
        input_truncated: truncated,
        container,
        bd_iso,
        format_version: None,
        duration_secs,
        video_tracks,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
}

const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts", "mts", "hevc", "h265", "265", "ivf", "obu",
    "iso",
];

fn collect_paths(inputs: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            collect_dir(input, recursive, &mut out)?;
        } else {
            out.push(input.clone());
        }
    }
    Ok(out)
}

fn collect_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if recursive {
                collect_dir(&path, recursive, out)?;
            }
        } else if is_video(&path) || sidecar::is_sidecar_candidate(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn write_output(output: &Option<PathBuf>, buf: &str) -> Result<()> {
    match output {
        Some(p) => {
            let mut f = File::create(p)?;
            f.write_all(buf.as_bytes())?;
        }
        None => {
            print!("{buf}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Drive the bounded reader with a fixed injected budget so the tests
    /// don't depend on the sniff classifier. Returns the cursor's final
    /// position too, guarding the "never drains past budget + 1" contract.
    fn read_with_budget(data: &[u8], sniff: usize, budget: usize) -> (Vec<u8>, bool, u64) {
        let mut cur = Cursor::new(data);
        let (buf, truncated) = read_bounded_head(&mut cur, sniff, |_| budget).unwrap();
        (buf, truncated, cur.position())
    }

    #[test]
    fn bounded_head_reads_complete_streams_whole() {
        // EOF below the budget: complete, everything returned.
        let (buf, truncated, _) = read_with_budget(&[7u8; 10], 4, 100);
        assert_eq!(buf, [7u8; 10]);
        assert!(!truncated);
        // EOF exactly at the budget: still complete.
        let (buf, truncated, _) = read_with_budget(&[7u8; 100], 4, 100);
        assert_eq!(buf.len(), 100);
        assert!(!truncated);
        // EOF inside the sniff block: complete without a second read.
        let (buf, truncated, _) = read_with_budget(&[7u8; 3], 4, 100);
        assert_eq!(buf.len(), 3);
        assert!(!truncated);
    }

    #[test]
    fn bounded_head_detects_and_bounds_truncation() {
        // One byte past the budget: truncated, trimmed back to the budget.
        let (buf, truncated, _) = read_with_budget(&[7u8; 101], 4, 100);
        assert_eq!(buf.len(), 100);
        assert!(truncated);
        // Far past the budget: the reader stops at budget + 1 — the bound
        // that lets a pipe writer stop instead of being drained.
        let (buf, truncated, pos) = read_with_budget(&[7u8; 10_000], 4, 100);
        assert_eq!(buf.len(), 100);
        assert!(truncated);
        assert_eq!(pos, 101);
    }

    #[test]
    fn stdin_budget_couples_ts_to_its_head_scan() {
        // A head that sniffs as TS/M2TS gets the same window the file path
        // reads (`ts::HEAD_SCAN_BYTES`); everything else gets the flat
        // stdin budget.
        let mut ts = vec![0u8; 4 * 188 + 1];
        for k in 0..5 {
            ts[k * 188] = 0x47;
        }
        assert_eq!(stdin_budget(&ts), container::ts::HEAD_SCAN_BYTES as usize);
        assert_eq!(stdin_budget(&[0u8; 1024]), STDIN_HEAD_BYTES);
        assert_eq!(stdin_budget(&[]), STDIN_HEAD_BYTES);
    }

    fn demux_with(
        container: &'static str,
        duration: Option<f64>,
        bitrate: Option<model::Bitrate>,
    ) -> container::Demux {
        let mut track =
            container::TrackDemux::new(container::Codec::Hevc, container::NalFormat::AnnexB);
        track.bitrate = bitrate;
        container::Demux::single(container, duration, track)
    }

    #[test]
    fn truncation_suppresses_span_derived_facts_only() {
        use model::{Bitrate, BitrateScope};
        let overall = Some(Bitrate { bits_per_sec: 1.0, scope: BitrateScope::Overall });
        let stream = Some(Bitrate::video_stream_bps(1.0));

        // TS: the PCR-span duration and the overall rate are prefix-derived.
        let mut d = demux_with("MPEG-2 TS (M2TS/BDAV)", Some(2.0), overall);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, None);
        assert!(d.tracks[0].bitrate.is_none());

        // MKV: the declared Segment-Info duration stands; a summed-index
        // rate can't prove completeness over a prefix and is dropped.
        let mut d = demux_with("Matroska", Some(3600.0), stream);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, Some(3600.0));
        assert!(d.tracks[0].bitrate.is_none());

        // MP4: the mvhd duration and the exact stsz/trun table rate stand.
        let mut d = demux_with("MP4 (ISOBMFF)", Some(3600.0), stream);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, Some(3600.0));
        assert!(d.tracks[0].bitrate.is_some());
    }
}
