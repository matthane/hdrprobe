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
mod mjpeg;
mod mpeg2;
mod mpeg4part2;
mod prefetch;
mod progress;
mod prores;
mod render;
mod sample;
mod shell;
mod sidecar;
mod theora;
mod vc1;
mod vp9;

use std::fs::File;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

    /// Comma list of sections to show: general,hdr,dv,hdr10plus,slhdr,hdrvivid.
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

    /// Include per-file error objects in the machine output (--json / --format
    /// ndjson): a failed file contributes {"file", "error"} beside the reports,
    /// so a scanner learns which files failed without parsing stderr. Off by
    /// default; text output and exit codes are unchanged either way.
    #[arg(long)]
    errors: bool,

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
    // Not `Cli::parse()`: clap's own exit path uses code 2 for a malformed
    // command line, which the exit-code contract (SCHEMA.md "Exit codes")
    // reserves for unreadable *input* — a usage error is 1, like the tool's
    // own usage checks below. `--help`/`--version` stay clap's success exit.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                e.exit()
            }
            let _ = e.print();
            return ExitCode::from(1);
        }
    };

    // Shell integration is an action-and-exit path: register/remove the Explorer
    // context-menu verb, then return without touching the file pipeline. Its
    // confirmation renders in the report's own styling (masthead + section rule
    // + kv rows), gated by the same --color policy against stdout.
    if cli.install_shell || cli.uninstall_shell {
        let color = resolve_color(
            cli.color,
            supports_color::on(supports_color::Stream::Stdout).is_some(),
            ansi_stdout,
        );
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
    let use_color = resolve_color(
        cli.color,
        cli.output.is_none()
            && format == Format::Text
            && supports_color::on(supports_color::Stream::Stdout).is_some(),
        ansi_stdout,
    );

    // Progress is `--full`-only (the fast path is over in milliseconds) and
    // lives entirely on stderr — stdout stays the pure report stream. Under
    // `auto` the bar needs an interactive stderr; the bar's colour follows the
    // same --color policy as the report, checked against stderr's own
    // capability.
    let progress_mode = if !cli.full {
        progress::Mode::Off
    } else {
        let bar_color = resolve_color(
            cli.color,
            supports_color::on(supports_color::Stream::Stderr).is_some(),
            ansi_stderr,
        );
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
            write_stdout(&banner);
        } else {
            out_buf.push_str(&banner);
        }
    }

    let mut stdout_gone = false;
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
                    if !write_stdout(&piece) {
                        // The consumer stopped reading (`| head`). Nothing left
                        // to say, and continuing would scan files whose reports
                        // no one will see.
                        stdout_gone = true;
                        break;
                    }
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
                // Under --errors the failure also joins the machine stream as
                // an error object (see SCHEMA.md "Error objects"), so an
                // NDJSON consumer learns which files failed without parsing
                // stderr. The stderr line above stays either way, and text
                // output is untouched.
                if cli.errors && format != Format::Text {
                    let err = model::ErrorReport {
                        hdrprobe_schema_version: model::SCHEMA_VERSION,
                        file: path.display().to_string(),
                        error: format!("{e:#}"),
                    };
                    match format {
                        Format::Json => {
                            json_reports.push(serde_json::to_value(&err).unwrap())
                        }
                        Format::Ndjson => {
                            let mut piece = serde_json::to_string(&err).unwrap();
                            piece.push('\n');
                            if stream_reports {
                                if !write_stdout(&piece) {
                                    stdout_gone = true;
                                    break;
                                }
                            } else {
                                out_buf.push_str(&piece);
                            }
                        }
                        Format::Text => unreachable!("gated above"),
                    }
                }
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
    if !stdout_gone {
        if let Err(e) = write_output(&cli.output, &out_buf) {
            eprintln!("error: writing output: {e}");
            return ExitCode::from(1);
        }
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
    let (mut sl, mut hv) = (true, true);
    if let Some(list) = &cli.sections {
        g = false;
        h = false;
        d = false;
        hp = false;
        sl = false;
        hv = false;
        for s in list.split(',') {
            match s.trim() {
                "general" => g = true,
                "hdr" => h = true,
                "dv" => d = true,
                "hdr10plus" => hp = true,
                // Hyphenated spellings accepted since unknown tokens are
                // silently ignored — a near-miss shouldn't hide a section.
                "slhdr" | "sl-hdr" => sl = true,
                "hdrvivid" | "hdr-vivid" => hv = true,
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
        show_sl_hdr: sl,
        show_hdr_vivid: hv,
    }
}

/// Resolve the `--color` policy for one stream. `detected` is the capability
/// probe (`supports-color` plus the caller's own stream-shape gates) and
/// `enable` puts the stream in a state where escape sequences actually
/// render, reporting whether that succeeded — see [`ansi_stdout`].
///
/// The rule lives here rather than at the three call sites because they used
/// to carry three copies of it, and the `always` arm is the one a fourth copy
/// would get wrong: forcing colour is a statement of *intent*, not a claim
/// that the console is already in the right mode, so it still calls `enable`
/// and then ignores the answer. Ignoring it is what keeps `--color always`
/// emitting codes into a pipe or a file, where nothing can be enabled.
fn resolve_color(when: ColorWhen, detected: bool, enable: impl FnOnce() -> bool) -> bool {
    match when {
        ColorWhen::Always => {
            enable();
            true
        }
        ColorWhen::Never => false,
        ColorWhen::Auto => detected && enable(),
    }
}

/// Windows' `ENABLE_VIRTUAL_TERMINAL_PROCESSING`. With this bit clear a console
/// screen buffer *stores* an escape sequence as text instead of acting on it.
#[cfg(windows)]
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

/// Whether escapes written to stdout will render, enabling Windows'
/// virtual-terminal processing as a side effect. Always true off Windows,
/// where a terminal needs no permission to interpret its own escapes.
///
/// A console process inherits VT processing **off** — measured as mode `0x3`
/// under conhost and under the `--install-shell` verb's own `cmd /c` window —
/// so before this existed every colour code hdrprobe wrote landed on screen as
/// literal `←[38;2;…m` text (issue #12: 82 of them in one default report).
/// Windows Terminal's ConPTY hands the child `0x7` instead, which is why the
/// identical binary renders correctly there and why this survived a release
/// cycle: it worked in exactly the terminal developers use. `supports-color`
/// is no guard — its Windows arm assumes every terminal since Windows 10 1511
/// handles ANSI, which is true only once *some* process has asked, and nothing
/// here was asking.
///
/// Enabling only ORs the VT bit. The documented precondition
/// `ENABLE_PROCESSED_OUTPUT` rides every inherited mode observed (`0x3` keeps
/// it), and forcing it would override a parent that deliberately put the
/// console in raw mode.
#[cfg(windows)]
fn ansi_stdout() -> bool {
    use std::os::windows::io::AsRawHandle as _;
    ansi_capable(std::io::stdout().as_raw_handle())
}

/// Stderr counterpart, gating the `--full` progress bar's colour.
#[cfg(windows)]
fn ansi_stderr() -> bool {
    use std::os::windows::io::AsRawHandle as _;
    ansi_capable(std::io::stderr().as_raw_handle())
}

#[cfg(not(windows))]
fn ansi_stdout() -> bool {
    true
}

#[cfg(not(windows))]
fn ansi_stderr() -> bool {
    true
}

#[cfg(windows)]
fn ansi_capable(handle: std::os::windows::io::RawHandle) -> bool {
    extern "system" {
        fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: u32) -> i32;
    }
    let mut raw = 0u32;
    let mode =
        (unsafe { GetConsoleMode(handle, &mut raw) } != 0).then_some(raw);
    let set_ok = match mode {
        Some(m) if m & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0 => {
            unsafe { SetConsoleMode(handle, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 }
        }
        _ => false,
    };
    vt_verdict(mode, set_ok)
}

/// The colour verdict for a handle, from the two Win32 outcomes: `mode` is
/// `None` when `GetConsoleMode` failed, `set_ok` whether the enable took.
/// Split out so the policy is pinned by tests that need no console.
///
/// The asymmetry is the whole point. Colour is vetoed **only** for a genuine
/// console that provably refuses VT (one pinned to "Use legacy console", or
/// Windows 8 and older) — there, plain text beats a screen of raw escapes. A
/// handle that is not a console at all keeps the caller's decision untouched:
/// a mintty/MSYS pty is a *pipe* that `IsTerminal` correctly vouches for and
/// `GetConsoleMode` correctly rejects, so vetoing on that failure would strip
/// colour from Git Bash in order to fix conhost.
#[cfg(windows)]
fn vt_verdict(mode: Option<u32>, set_ok: bool) -> bool {
    match mode {
        None => true,
        Some(m) => m & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 || set_ok,
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

    // A video disc ISO is probed through its main feature: locate the
    // feature's contiguous byte range (the playlist-selected clip on a
    // Blu-ray, the byte-largest title VOB set on a DVD), then run the
    // ordinary pipeline over that *subslice*, so every slice-relative
    // mechanism (head/tail windows, streaming positions, bitrate
    // denominators, progress) is correct by construction. Extension-gated: a
    // UDF image under another name takes the ordinary demux path below.
    let is_iso = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"));
    let feature = if is_iso && bdiso::is_udf_iso(&mmap) {
        let f = bdiso::locate_feature(&mmap, remote.then_some(&file))
            .context("locating the disc's main feature")?;
        // The feature's head/tail windows, translated to its range in the
        // image: the ISO counterpart of `warm_metadata`'s TS and PS branches.
        let (start, len) = f.clip_range();
        match &f {
            bdiso::DiscFeature::Bd(_) => prefetch::warm_ts_windows(remote, &file, start, len),
            bdiso::DiscFeature::Dvd(_) => prefetch::warm_ps_windows(remote, &file, start, len),
        }
        Some(f)
    } else {
        None
    };
    let data: &[u8] = match &feature {
        Some(f) => {
            let (start, len) = f.clip_range();
            &mmap[start as usize..(start + len) as usize]
        }
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
            Some(f) => {
                let (start, len) = f.clip_range();
                prefetch::Frontier::new_at(&file, start, len)
            }
            None => prefetch::Frontier::new(&file, size),
        }
    } else {
        prefetch::Frontier::off()
    };

    let demux = match &feature {
        // A BDMV feature is M2TS by construction (the locator's sync-lock
        // gate) and a DVD feature is an MPEG program stream by the format's
        // definition; extension dispatch would misroute the `.iso` name.
        Some(bdiso::DiscFeature::Bd(_)) => container::ts::demux(data, cli.full, progress, &frontier)
            .context("demuxing the BDMV main-feature clip")?,
        Some(bdiso::DiscFeature::Dvd(_)) => {
            container::ps::demux(data).context("demuxing the DVD main-feature title set")?
        }
        None => {
            container::demux(path, data, cli.full, progress, &frontier).context("demux failed")?
        }
    };
    let mut demux = demux;

    // A DVD ISO's duration authority is the IFO's declared runtime — the
    // MKV/MP4 declared-duration convention, because unlike a bare program
    // stream a DVD *does* declare one, and the PS backend's measured PTS span
    // is structurally unreliable here: a cell or layer-break PTS reset
    // between the head and tail windows is invisible to both (the documented
    // concatenation limit), and the real dual-layer reference pressing
    // measured 33 minutes of a declared 109-minute feature exactly that way.
    // The overall bitrate divides by the duration, so it moves with it.
    // No parsed IFO leaves the measured span standing, with its limits.
    if let Some(bdiso::DiscFeature::Dvd(f)) = &feature {
        if let Some(declared) = f.title_duration_secs {
            demux.duration_secs = Some(declared);
            if let [track] = demux.tracks.as_mut_slice() {
                track.bitrate = model::Bitrate::overall(data.len() as u64, Some(declared));
            }
        }
    }

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
    // A program stream's duration is the video PTS span from a head window to a
    // tail window, so over a prefix the "tail" is just the cut point — the same
    // reasoning as the TS head-to-tail PCR delta beside it.
    //
    // Ogg's is the same shape once more: it records no duration field, so the
    // number comes from the last granule position a bounded tail window holds,
    // and a prefix's "tail" is the cut point rather than the end of the stream.
    use container::ps::{MPEG1_SYSTEM_LABEL, MPEG2_PROGRAM_LABEL, PES_ONLY_LABEL};
    // Raw DV's duration is the payload length ÷ the frame size, so over a
    // stdin prefix it would describe the buffered bytes rather than the file.
    if demux.container.starts_with("MPEG-2 TS")
        || matches!(
            demux.container,
            MPEG2_PROGRAM_LABEL
                | MPEG1_SYSTEM_LABEL
                | PES_ONLY_LABEL
                | container::ogg::CONTAINER_LABEL
                | container::dif::CONTAINER_LABEL
        )
    {
        demux.duration_secs = None;
    }
    // Two containers keep a video-stream rate over a prefix: MP4/MOV, whose
    // stsz/trun table sums are exact regardless of truncation, and RealMedia,
    // whose MDPR rate is a header *declaration* — both are facts the buffered
    // head carries whole, unlike MKV's summed block index.
    let declared_rate = matches!(
        demux.container,
        "MP4 (ISOBMFF)" | "QuickTime (MOV)" | container::rm::CONTAINER_LABEL
    );
    for t in &mut demux.tracks {
        t.bitrate =
            t.bitrate.filter(|b| declared_rate && b.scope == model::BitrateScope::VideoStream);
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
    feature: Option<bdiso::DiscFeature>,
    truncated: bool,
    cli: &Cli,
    progress: &progress::Progress,
    frontier: &prefetch::Frontier,
) -> Report {
    let opts = sample::Options { samples: cli.samples, full: cli.full, no_rpu: cli.no_rpu };
    let scan = sample::scan(demux, data, &opts, progress, frontier);

    // `--full` is a promise that every access unit was read, and the report
    // keeps its sampled footnote off on that basis. A backend whose chunk index
    // covers only a bounded head window cannot keep that promise however many
    // of its chunks the scan visits, so the marks stay on for it.
    let complete_scan = cli.full && !demux.bounded_index;

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
            .finalize(track.width, track.height, track.dv_config.as_ref(), complete_scan, is_av1, track.dv_dual_track)
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

        // Aspect: a format signals the pixel ratio *or* the display ratio,
        // and the missing one is arithmetic against the coded size — never a
        // guess, so both floats exist exactly when a rational was signalled
        // (plus the coded size where the derivation needs it).
        let aspect: Option<(f64, f64)> = match (track.pixel_aspect, track.display_aspect) {
            (Some((pn, pd)), Some((dn, dd))) if pn > 0 && pd > 0 && dn > 0 && dd > 0 => {
                Some((f64::from(pn) / f64::from(pd), f64::from(dn) / f64::from(dd)))
            }
            (Some((pn, pd)), None)
                if pn > 0 && pd > 0 && track.width > 0 && track.height > 0 =>
            {
                let par = f64::from(pn) / f64::from(pd);
                Some((par, par * f64::from(track.width) / f64::from(track.height)))
            }
            (None, Some((dn, dd)))
                if dn > 0 && dd > 0 && track.width > 0 && track.height > 0 =>
            {
                let dar = f64::from(dn) / f64::from(dd);
                Some((dar * f64::from(track.height) / f64::from(track.width), dar))
            }
            _ => None,
        };
        // The same signalled rationals, exact: presence mirrors the floats
        // (both come from the one match above — when `aspect` is Some every
        // input below is nonzero, so `reduced` cannot refuse). The floats are
        // untouched; a derived rational and its float can differ in the last
        // binary digit, which is why the doc ties them loosely.
        let (par_rational, dar_rational) = if aspect.is_some() {
            match (track.pixel_aspect, track.display_aspect) {
                (Some((pn, pd)), Some((dn, dd))) => (
                    model::Rational::reduced(pn.into(), pd.into()),
                    model::Rational::reduced(dn.into(), dd.into()),
                ),
                (Some((pn, pd)), None) => (
                    model::Rational::reduced(pn.into(), pd.into()),
                    model::Rational::reduced(
                        u64::from(pn) * u64::from(track.width),
                        u64::from(pd) * u64::from(track.height),
                    ),
                ),
                (None, Some((dn, dd))) => (
                    model::Rational::reduced(
                        u64::from(dn) * u64::from(track.height),
                        u64::from(dd) * u64::from(track.width),
                    ),
                    model::Rational::reduced(dn.into(), dd.into()),
                ),
                (None, None) => (None, None),
            }
        } else {
            (None, None)
        };

        // The base layer's *effective* colour: what the container or coded
        // stream signalled, with the HLG/PQ alt-transfer SEI override applied.
        // Built here, before the Dolby Vision post-passes, because the
        // compatibility-id inference reads it — see `fill_inferred_compat`.
        let mut color = track.color.clone();
        let mut color_source = track.color_source;
        if let Some(pt) = scan.sei.preferred_transfer {
            if let Some(t) = container::cicp_transfer(pt as u16) {
                color.transfer = Some(t.to_string());
                color_source.transfer = Some(model::ColorSource::Sei);
            }
        }

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
            // Likewise the last rung of compatibility-id resolution: deducing
            // the id from the base layer's colour needs a base layer. It reads
            // the *effective* colour, with the alt-transfer SEI already applied,
            // because that SEI is what distinguishes the two streams Dolby's
            // table separates: a transfer characteristic of 14 is CCID 4 only
            // "when used with the alternative_transfer_characteristic SEI
            // message ... with the preferred_transfer_function set to 18". A
            // bare 14 with no such SEI is an ordinary SDR wide-gamut curve and
            // must resolve nothing.
            dv::levels::fill_inferred_compat(dv, &color);
            // And the base-layer transfer fact that rides the resolved id.
            dv::levels::flag_pq_reshaping(dv);
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
        // Coverage mirrors dolby_vision's: `none` when no frame was read at
        // all (--no-rpu, where only the cuvv declaration can detect), `full`
        // for a complete scan, else `sampled` — including a box-only default
        // run whose sample spread found no SEI (frames *were* read, and a
        // mid-title-only SEI could sit outside them).
        let vivid_coverage = if cli.no_rpu {
            model::Coverage::None
        } else if complete_scan {
            model::Coverage::Full
        } else {
            model::Coverage::Sampled
        };
        let hdr_vivid = match (track.cuvv_version_map, scan.sei.hdr_vivid.as_ref()) {
            (Some(map), sei) => Some(model::HdrVivid {
                version: format!("{}.0", 16 - map.leading_zeros()),
                system_start_code: sei.map(|s| s.system_start_code),
                target_max_luminances: sei
                    .map(|s| hdr::pq_targets_to_nits(&s.target_pq))
                    .unwrap_or_default(),
                coverage: vivid_coverage,
            }),
            (None, Some(s)) => Some(model::HdrVivid {
                version: format!("{}.0", s.version),
                system_start_code: Some(s.system_start_code),
                target_max_luminances: hdr::pq_targets_to_nits(&s.target_pq),
                coverage: vivid_coverage,
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
            source_mastering_display: sl.source_mastering.clone(),
        });

        let hdr = Some(hdr::assemble(track, dv.as_ref(), &scan.sei));

        // Last: the base-layer colour a Dolby Vision profile and compatibility
        // id define outright, for the fields nothing signalled. Video path only
        // — a metadata sidecar has no base layer to describe. After
        // `hdr::assemble`, which reads the demuxed colour: nothing derived here
        // may feed back into classification.
        if let Some(dv) = dv.as_ref() {
            dv::levels::fill_derived_color(&mut color, &mut color_source, dv);
        }

        video_tracks.push(model::VideoTrack {
            track_number: track.track_number,
            program: track.program,
            default: track.default_flag,
            codec: Some(track.codec.label()),
            codec_id: track.codec_id.clone(),
            codec_profile: track.codec_profile.clone(),
            width: if track.width > 0 { Some(track.width) } else { None },
            height: if track.height > 0 { Some(track.height) } else { None },
            fps,
            fps_rational: track
                .fps_rational
                .and_then(|(n, d)| model::Rational::reduced(n, d)),
            duration_secs: track.duration_secs,
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
            pixel_aspect_ratio: aspect.map(|a| a.0),
            pixel_aspect_ratio_rational: par_rational,
            display_aspect_ratio: aspect.map(|a| a.1),
            display_aspect_ratio_rational: dar_rational,
            scan_type: track.scan_type.map(str::to_string),
            stereo: track.stereo.clone(),
            color,
            color_source,
            hdr,
            dolby_vision: dv,
            hdr10plus,
            sl_hdr,
            hdr_vivid,
        });
    }

    // The ISO report describes the probed feature (duration, bitrate, tracks)
    // under the ISO's own name and size; the `Main feature` line carries what
    // was selected and its own declared duration (a Blu-ray playlist's edit
    // duration, a DVD title set's IFO runtime).
    let container = match &feature {
        Some(bdiso::DiscFeature::Bd(_)) => "Blu-ray ISO (BDMV)".to_string(),
        Some(bdiso::DiscFeature::Dvd(_)) => "DVD-Video ISO (VIDEO_TS)".to_string(),
        None => demux.container.to_string(),
    };
    let (mut bd_iso, mut dvd_iso) = (None, None);
    match feature {
        Some(bdiso::DiscFeature::Bd(f)) => {
            bd_iso = Some(model::BdIso {
                playlist: f.playlist,
                playlist_duration_secs: f.playlist_duration_secs,
                clip: f.clip,
                clip_index: f.clip_index,
                clip_count: f.clip_count,
            });
        }
        Some(bdiso::DiscFeature::Dvd(f)) => {
            dvd_iso = Some(model::DvdIso {
                vts: f.vts,
                vob_count: f.vob_count,
                title_duration_secs: f.title_duration_secs,
            });
        }
        None => {}
    }

    Report {
        hdrprobe_schema_version: model::SCHEMA_VERSION,
        file,
        size_bytes,
        // A stdin stream cut by the head budget, or a *file* whose container
        // declares more bytes than it holds (`Demux::declared_short`) — the
        // flag names why an AVI/ASF/FLV prefix's numbers cannot all describe
        // one file. Only the stdin case feeds the prefix-suppression table
        // (`suppress_prefix_derived_facts`); the backends behind
        // `declared_short` already withheld their own prefix-invalid facts.
        input_truncated: truncated || demux.declared_short,
        container,
        bd_iso,
        dvd_iso,
        format_version: None,
        duration_secs,
        video_tracks,
    }
}

/// Extensions a directory scan picks up. Every extension `container::demux`
/// dispatches on belongs here or the format is unreachable in bulk — probing one
/// file by name would work while `hdrprobe rips/` silently skipped it.
///
/// `.bin` is the deliberate omission: the raw-HEVC dispatch accepts it, but it
/// is far too generic a name to claim in a directory of mixed files. `.wma`,
/// `.ogg` and `.oga` are omitted for the sibling reason — all three dispatch
/// when named, because each can carry video, but each is overwhelmingly an
/// audio extension, and a scanned music library would print an error per track.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts", "mts", "hevc", "h265", "265", "ivf", "obu",
    "iso", "mpg", "mpeg", "vob", "m2p", "evo", "m2v", "m1v", "mpv", "avi", "wmv", "asf", "flv",
    "ogv", "dv", "dif", "rm", "rmvb",
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
            write_stdout(buf);
        }
    }
    Ok(())
}

/// Write a piece of the report stream to stdout, treating a closed pipe as the
/// consumer having read its fill rather than as a failure.
///
/// `hdrprobe … | head` and `| less` (quit early) are ordinary use, and the
/// `print!` macro *panics* on the write error they produce — printing a Rust
/// backtrace over the user's terminal and exiting 101, a code outside this
/// tool's contract entirely (0 ok, 1 usage, 2 unreadable). This is the same
/// convention the stdin path already documents from the other end of the pipe:
/// when the far side stops, that is a success signal, not an error.
///
/// Returns `false` once stdout is gone, so callers stop writing rather than
/// repeating the failure once per remaining file. A genuine write error (a full
/// disk on a redirect) still reports itself and stops the stream.
fn write_stdout(buf: &str) -> bool {
    let mut out = std::io::stdout().lock();
    match out.write_all(buf.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => false,
        Err(e) => {
            eprintln!("error: writing output: {e}");
            false
        }
    }
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
    fn color_policy_asks_the_console_before_emitting_escapes() {
        use std::cell::Cell;
        let asked = Cell::new(0usize);
        let refuse = || {
            asked.set(asked.get() + 1);
            false
        };
        let accept = || {
            asked.set(asked.get() + 1);
            true
        };

        // `auto` over a console that refuses virtual-terminal processing
        // prints plain text. Emitting the codes anyway is what issue #12 saw.
        assert!(!resolve_color(ColorWhen::Auto, true, refuse));
        assert!(resolve_color(ColorWhen::Auto, true, accept));
        // A stream the caller already ruled out (piped, `--output`, JSON)
        // short-circuits, so no console state is touched for machine output.
        assert!(!resolve_color(ColorWhen::Auto, false, accept));
        assert_eq!(asked.get(), 2);

        // `always` forces colour whatever the console says — that is what
        // makes `--color always > file` keep its codes — but it still asks,
        // because a forced run in a conhost window needs the enable too.
        assert!(resolve_color(ColorWhen::Always, false, refuse));
        assert_eq!(asked.get(), 3);

        // `never` short-circuits ahead of the probe.
        assert!(!resolve_color(ColorWhen::Never, true, accept));
        assert_eq!(asked.get(), 3);
    }

    /// `cargo test` captures stdout onto a pipe, so this runs the
    /// not-a-console arm on Windows and the unconditional `true` elsewhere.
    /// The guarantee it pins is the Git Bash one: a handle that isn't a
    /// Windows console must never have its colour vetoed.
    #[test]
    fn ansi_probe_never_vetoes_a_handle_that_is_not_a_console() {
        assert!(ansi_stdout());
        assert!(ansi_stderr());
    }

    #[cfg(windows)]
    #[test]
    fn vt_verdict_vetoes_only_a_console_that_refuses() {
        // Not a console: a pipe, a redirect, or a mintty/MSYS pty that
        // `IsTerminal` vouches for. Vetoing here would strip colour from Git
        // Bash to fix conhost.
        assert!(vt_verdict(None, false));
        // Already on — Windows Terminal's ConPTY hands the child 0x7.
        assert!(vt_verdict(Some(0x7), false));
        // Off, and the enable took: conhost's 0x3, the issue-12 case.
        assert!(vt_verdict(Some(0x3), true));
        // Off, and the enable failed: a console pinned to "Use legacy
        // console", or Windows 8 and older. Plain beats raw escapes.
        assert!(!vt_verdict(Some(0x3), false));
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
        use model::{Bitrate, BitrateScope, BitrateSource};
        let overall = Some(Bitrate {
            bits_per_sec: 1.0,
            scope: BitrateScope::Overall,
            source: BitrateSource::Measured,
        });
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

        // Ogg: the duration is the last granule position a tail window holds,
        // so over a prefix it describes the cut point rather than the stream.
        let mut d = demux_with(container::ogg::CONTAINER_LABEL, Some(7200.0), overall);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, None);
        assert!(d.tracks[0].bitrate.is_none());

        // RealMedia: the duration and the stream rate are both header
        // declarations the buffered prefix carries whole, so both stand —
        // matching the file-probe path, where a declared-short .rm keeps them.
        let mut d = demux_with(container::rm::CONTAINER_LABEL, Some(7286.037), stream);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, Some(7286.037));
        assert!(d.tracks[0].bitrate.is_some(), "a declared rate survives a prefix");

        // And the label match is a constant pattern, not a catch-all binding:
        // a container not in the list keeps its declared duration.
        let mut d = demux_with("AVI (RIFF)", Some(7200.0), stream);
        suppress_prefix_derived_facts(&mut d);
        assert_eq!(d.duration_secs, Some(7200.0), "only listed labels suppress");
    }
}
