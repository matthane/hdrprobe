//! hdrprobe — fast HDR / Dolby Vision metadata inspector.

mod av1;
mod avc;
mod bits;
mod container;
mod dv;
mod hdr;
mod hevc;
mod model;
mod prefetch;
mod progress;
mod render;
mod sample;
mod shell;
mod sidecar;

use std::fs::File;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use memmap2::Mmap;

use crate::model::{General, Hdr10Plus, Report};
use crate::render::{RenderOpts, Theme};

#[derive(Parser, Debug)]
#[command(name = "hdrprobe", version, about = "Fast HDR / Dolby Vision metadata inspector")]
struct Cli {
    /// Input file(s) or directory(ies).
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

    /// Register a right-click "hdrprobe" context-menu submenu with Fast and Full entries (Windows).
    #[arg(long)]
    install_shell: bool,

    /// Remove the right-click context-menu submenu (Windows).
    #[arg(long)]
    uninstall_shell: bool,
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
    // context-menu verb, then return without touching the file pipeline.
    if cli.install_shell || cli.uninstall_shell {
        let res = if cli.install_shell { shell::install() } else { shell::uninstall() };
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

    let mut out_buf = String::new();
    let mut json_reports: Vec<serde_json::Value> = Vec::new();
    let mut had_error = false;

    // The masthead prints once per run, only on the colored interactive text
    // path — quiet, JSON/NDJSON, and piped output stay machine-clean.
    if use_color && format == Format::Text && !cli.quiet {
        out_buf.push_str(&render::render_banner(cli.theme));
    }

    for (i, path) in paths.iter().enumerate() {
        let progress = progress::Progress::new(progress_mode, path, i + 1, paths.len());
        match process_file(path, &cli, &progress) {
            Ok(report) => {
                progress.finish();
                match format {
                    Format::Text => {
                        if cli.quiet {
                            out_buf.push_str(&render::render_quiet(&report));
                            out_buf.push('\n');
                        } else {
                            let opts = render_opts(&cli, use_color);
                            out_buf.push_str(&render::render(&report, &opts));
                            out_buf.push('\n');
                        }
                    }
                    Format::Json => json_reports.push(serde_json::to_value(&report).unwrap()),
                    Format::Ndjson => {
                        out_buf.push_str(&serde_json::to_string(&report).unwrap());
                        out_buf.push('\n');
                    }
                }
            }
            Err(e) => {
                // Drop erases any live bar line so the diagnostic prints clean.
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

fn render_opts(cli: &Cli, color: bool) -> RenderOpts {
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
        show_general: g,
        show_hdr: h,
        show_dv: d,
        show_hdr10plus: hp,
    }
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

    // `--full` on a genuinely remote volume: the whole-file walks tailgate a
    // bounded look-ahead warm (`prefetch::Frontier`), so the file crosses the
    // wire once, linearly, instead of thousands of scattered page-fault
    // round-trips. Off everywhere else — local `--full` and the default path
    // are unchanged. `warm_metadata` above still covers the tail extents (TS
    // last-PCR, MKV `Tags`, `mfra`) a front-first stream reaches last.
    let frontier = if cli.full && prefetch::is_remote_strict(&file, path) {
        prefetch::Frontier::new(&file, size)
    } else {
        prefetch::Frontier::off()
    };

    let demux =
        container::demux(path, &mmap, cli.full, progress, &frontier).context("demux failed")?;

    // The sampled access units are scattered across the whole file (worst for
    // MP4, whose sample index spans a multi-GB mdat), so warm exactly the
    // ranges the sampler will fault. Default path only: `--full` reads every
    // chunk and `--no-rpu` reads none.
    if !cli.full && !cli.no_rpu {
        prefetch::warm_sample_chunks(remote, &file, &demux, cli.samples, warmed_head);
    }

    let opts = sample::Options { samples: cli.samples, full: cli.full, no_rpu: cli.no_rpu };
    let scan = sample::scan(&demux, &mmap, &opts, progress, &frontier);

    let is_av1 = matches!(demux.codec, container::Codec::Av1);
    let mut dv = scan
        .dv
        .finalize(demux.width, demux.height, demux.dv_config.as_ref(), cli.full, is_av1, demux.dv_dual_track)
        .or_else(|| demux.dv_config.as_ref().map(|c| dv::levels::container_only(c, demux.dv_dual_track)));

    // FEL brightness expansion is only decidable here on the video path: it
    // needs the base layer's own declared mastering display (container MDCV or
    // ST.2086 SEI), which a metadata sidecar doesn't have.
    if let Some(dv) = dv.as_mut() {
        let bl_max = demux
            .mastering
            .as_ref()
            .or(scan.sei.mastering.as_ref())
            .map(|m| m.max_luminance);
        dv::levels::flag_fel_brightness_expansion(dv, bl_max);
    }

    let hdr10plus = scan.sei.hdr10plus.map(|info| Hdr10Plus {
        application_version: info.application_version,
        num_windows: info.num_windows,
        profile: (info.profile != 0).then_some(info.profile as char),
        target_max_luminance: (info.target_max_luminance > 0).then_some(info.target_max_luminance),
    });

    let hdr = Some(hdr::assemble(&demux, dv.as_ref(), &scan.sei));

    // Reflect the HLG/PQ alt-transfer SEI override in the displayed colour line.
    let mut color = demux.color.clone();
    if let Some(pt) = scan.sei.preferred_transfer {
        if let Some(t) = container::cicp_transfer(pt as u16) {
            color.transfer = Some(t.to_string());
        }
    }

    let general = General {
        container: demux.container.to_string(),
        codec: demux.codec.label(),
        codec_profile: demux.codec_profile.clone(),
        format_version: None,
        width: if demux.width > 0 { Some(demux.width) } else { None },
        height: if demux.height > 0 { Some(demux.height) } else { None },
        // MKV `--full` without a DefaultDuration: the exact frame count exists
        // only after the streaming walk, so the count ÷ duration fallback the
        // demux's complete index used to compute lands here instead — same
        // inputs, same value.
        fps: demux.fps.or_else(|| match (scan.frame_count, demux.duration_secs) {
            (Some(n), Some(d)) if n > 0 && d > 0.0 => Some(n as f64 / d),
            _ => None,
        }),
        duration_secs: demux.duration_secs,
        // A container-known rate wins (MKV statistics tags); the `--full`
        // streaming walks (TS ES bytes, MKV block bytes) fill the gap with the
        // exact sum their demux could no longer compute — the same value the
        // old whole-stream paths produced (`Some(0)` es_bytes ⇒ `None`, as
        // before).
        bitrate: demux.bitrate.or_else(|| {
            scan.es_bytes.and_then(|bytes| model::Bitrate::video_stream(bytes, demux.duration_secs))
        }),
        bit_depth: demux.bit_depth,
        chroma: demux.chroma.clone(),
        stereo: demux.stereo.clone(),
        color,
    };

    Ok(Report {
        hdrprobe_schema_version: model::SCHEMA_VERSION,
        file: path.display().to_string(),
        size_bytes: size,
        general,
        hdr,
        dolby_vision: dv,
        hdr10plus,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

const VIDEO_EXTS: &[&str] =
    &["mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts", "mts", "hevc", "h265", "265", "ivf", "obu"];

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
