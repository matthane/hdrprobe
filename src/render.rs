//! Sectioned, aligned terminal rendering of a `Report`.

use std::fmt::Write;

use crate::model::{BitrateScope, ColorInfo, DolbyVision, Report, VideoTrack};

pub struct RenderOpts {
    pub color: bool,
    pub theme: Theme,
    /// Visible column count of the output terminal, when stdout is one.
    /// Value lines longer than this reflow at their part separators with
    /// continuations indented to the value column (see `wrap_line`). `None`
    /// (pipes, `--output` files, JSON/quiet paths) disables reflow, so every
    /// machine-consumed byte stream is unchanged.
    pub wrap_width: Option<usize>,
    /// 1-based position of this report's file in the run, and the run's file
    /// total — the report header echoes the progress header's `[k/N]` counter.
    /// Multi-file runs only: with `file_count <= 1` no counter renders, so
    /// single-file output is unchanged.
    pub file_index: usize,
    pub file_count: usize,
    pub show_general: bool,
    pub show_hdr: bool,
    pub show_dv: bool,
    pub show_hdr10plus: bool,
    pub show_sl_hdr: bool,
    pub show_hdr_vivid: bool,
}

/// A colour theme picks the ink for the fixed four-role hierarchy (see
/// `Colorizer`); which role each element gets never varies by theme, so a
/// new theme is four tuned colours, not a layout decision. Themes only touch
/// coloured output — plain text is byte-identical under every theme.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Theme {
    /// Green phosphor (P1 CRT).
    Green,
    /// Amber phosphor (P3 CRT).
    Amber,
    /// Warm red.
    Red,
    /// Cyan-blue phosphor.
    Ice,
    /// Violet-purple.
    Purple,
    /// Neutral paper-white — the default.
    Paper,
    /// Bold/dim intensity only, no colours of its own: inherits the
    /// terminal's scheme, and works on terminals without truecolor.
    Mono,
}

impl Theme {
    pub(crate) fn palette(self) -> &'static Palette {
        match self {
            Theme::Green => &GREEN,
            Theme::Amber => &AMBER,
            Theme::Red => &RED,
            Theme::Ice => &ICE,
            Theme::Purple => &PURPLE,
            Theme::Paper => &PAPER,
            Theme::Mono => &MONO,
        }
    }
}

const LABEL_W: usize = 16;

/// Visible column where values start: the 2-space gutter, the padded label,
/// and the 2-space gap. Continuation lines of a reflowed row indent to here
/// so wrapped values stay one aligned block under their first line. A
/// track-group body shifts the whole geometry right by `Colorizer::indent`.
const VALUE_COL: usize = 2 + LABEL_W + 2;

/// Extra left margin for everything inside a multi-track track group: its
/// section rules indent by this much (right edges stay flush with the
/// full-width track rule) and its kv rows by this much on top of the 2-space
/// gutter, so which HDR/DV sections belong to which track reads at a glance.
/// Single-track reports never use it — their layout is byte-pinned.
const TRACK_INDENT: usize = 2;

/// Below this terminal width reflow stops helping — values would wrap into a
/// sliver next to the label column — so narrower terminals get the unwrapped
/// line and the terminal's own hard wrap.
const MIN_WRAP_WIDTH: usize = VALUE_COL + 20;

/// Fallback width of a colored section rule ("── NAME ───…": marks, spaces,
/// name, fill) and the between-reports divider, used when no terminal width
/// was probed (pipes, `--output`, platforms without a probe). On a live
/// terminal both rules stretch to `RenderOpts::wrap_width` instead — full
/// bleed, no cap — so they track the window like value reflow does. Unlike
/// reflow there is no `MIN_WRAP_WIDTH` bow-out: a shrunk rule is still a
/// rule, and shrinking beats the hard wrap a fixed 64 columns would take in
/// a narrower window.
const RULE_W: usize = 64;

/// The interactive-terminal masthead: a two-row half-block "HDRPROBE" in the
/// theme palette (bright top row, mid bottom — a CRT falloff), with the
/// crate version faint alongside. Decoration only: `main` prints it once per
/// run, and only for colored text output (never quiet/JSON/pipes), so
/// machine consumers and logs never see it.
pub fn render_banner(theme: Theme) -> String {
    let c = Colorizer { on: true, palette: theme.palette(), wrap: None, indent: 0, word_wrap: false };
    let mut s = String::new();
    let _ = writeln!(s, "{}", c.bright("█ █ █▀▄ █▀█ █▀█ █▀█ █▀█ █▄▄ █▀▀"));
    let _ = writeln!(
        s,
        "{}  {}",
        c.value("█▀█ █▄▀ █▀▄ █▀▀ █▀▄ █▄█ █▄█ ██▄"),
        c.faint(concat!("v", env!("CARGO_PKG_VERSION")))
    );
    s.push('\n');
    s
}

/// An auxiliary confirmation (the shell install/uninstall messages) in the
/// report's own geometry: one section rule plus aligned kv rows, inked by the
/// same four-role hierarchy, with the report's terminal-width behaviour —
/// the rule stretches to `wrap` and long values reflow with continuations
/// indented to the value column. Reflow here is word-wrap
/// (`Colorizer::word_wrap`): the registered command strings carry no part
/// separators, so the report's separator-only rule would drop them to the
/// terminal's column-0 hard wrap. Plain (uncolored) output keeps the
/// identical layout minus the ink, exactly like the report body.
///
/// Its only production caller is the `cfg(windows)` shell verb, so the
/// function is compiled out elsewhere (`test` keeps the unit tests building
/// on every platform) — without the gate, non-Windows CI fails `-Dwarnings`
/// on dead code.
#[cfg(any(windows, test))]
pub fn render_notice(
    title: &str,
    rows: &[(&str, String)],
    color: bool,
    theme: Theme,
    wrap: Option<usize>,
) -> String {
    let c = Colorizer { on: color, palette: theme.palette(), wrap, indent: 0, word_wrap: true };
    let mut s = String::new();
    let _ = writeln!(s, "{}", c.section(title));
    for (label, value) in rows {
        kv(&mut s, &c, label, value);
    }
    s
}

pub fn render(r: &Report, o: &RenderOpts) -> String {
    let mut s = String::new();
    let c = Colorizer { on: o.color, palette: o.theme.palette(), wrap: o.wrap_width, indent: 0, word_wrap: false };
    let mut notes = Footnotes::default();

    // Each section carries one bright headline for quick glancing (General's
    // Video, HDR's Format, DV's and HDR10+'s Profile) — video inputs only. A
    // metadata sidecar (no codec, always single-track) surfaces too few lines
    // for a single bold value to read as anything but odd, so its report stays
    // headline-free.
    let multi = r.video_tracks.len() > 1;
    let sidecar = !multi && r.video_tracks[0].codec.is_empty();

    s.push_str(&report_header(&r.file, r.size_bytes, r.input_truncated, o, &c));
    s.push('\n');

    if !multi {
        // Single track — the overwhelming majority — keeps the historical
        // layout exactly: one General section interleaving the file-level and
        // track-level rows, then the track's HDR/DV/HDR10+ sections.
        let t = &r.video_tracks[0];
        if o.show_general {
            let _ = writeln!(s, "{}", c.section("General"));
            kv(&mut s, &c, "Container", &r.container);
            // Blu-ray ISO probes: which playlist/clip the report describes,
            // with the playlist's own edit duration (the Duration line below
            // stays the probed clip's transport-clock duration).
            if let Some(iso) = &r.bd_iso {
                kv(&mut s, &c, "Main feature", &bd_iso_line(iso));
            }
            // Sidecar schema version (a DV XML's root `version` attribute); video
            // inputs never carry one, so the line only appears for sidecars.
            if let Some(v) = &r.format_version {
                kv(&mut s, &c, "Schema version", v);
            }
            if let Some(d) = r.duration_secs {
                kv(&mut s, &c, "Duration", &human_duration(d));
            }
            // Video files show fps in the Video line; a metadata-only sidecar (no
            // codec) has no Video line, so it surfaces its frame rate on its own.
            if sidecar {
                if let Some(fps) = t.fps {
                    kv(&mut s, &c, "Frame rate", &format!("{:.3} fps", fps));
                }
            }
            track_general_rows(&mut s, &c, t, sidecar);
            s.push('\n');
        }
        track_sections(&mut s, &c, t, sidecar, o, &mut notes);
    } else {
        // Multi-track: the General section keeps only the file-level facts,
        // then each track renders under its own rule — the same Bitrate/Video/
        // Color rows followed by its HDR/DV/HDR10+ sections.
        if o.show_general {
            let _ = writeln!(s, "{}", c.section("General"));
            kv(&mut s, &c, "Container", &r.container);
            if let Some(iso) = &r.bd_iso {
                kv(&mut s, &c, "Main feature", &bd_iso_line(iso));
            }
            if let Some(v) = &r.format_version {
                kv(&mut s, &c, "Schema version", v);
            }
            if let Some(d) = r.duration_secs {
                kv(&mut s, &c, "Duration", &human_duration(d));
            }
            s.push('\n');
        }
        // Everything belonging to a track renders through an indented
        // Colorizer: its section rules shift right (right edges flush) and
        // its rows deepen, so the full-width track rules read as the outer
        // level at a glance. The footnote foot keeps the base geometry.
        let ct = Colorizer { indent: TRACK_INDENT, ..c };
        for (i, t) in r.video_tracks.iter().enumerate() {
            // The track rule is structure, not a section: it always prints so
            // the per-track groups stay attributable under any --sections set.
            let _ = writeln!(s, "{}", c.section(&track_title(i, t, r)));
            if o.show_general {
                track_general_rows(&mut s, &ct, t, false);
            }
            s.push('\n');
            track_sections(&mut s, &ct, t, false, o, &mut notes);
        }
    }

    // Footnotes collected from marked labels render once at the report's
    // foot, so per-line caveats never clutter the values they qualify (two
    // sampled tracks share one mark — `Footnotes` dedupes identical texts).
    // The elapsed time is JSON-only (`elapsed_ms`); the text report doesn't
    // show it.
    for (mark, text) in notes.lines() {
        let _ = writeln!(s, "{}", c.faint(&format!("{mark} {text}")));
    }

    s
}

/// The rule title of one track in a multi-track report: 1-based position,
/// plus the identity that distinguishes it — the TS program for a
/// multi-program mux, and a "Default" tag when the container flags exactly
/// this track as default (all-default is Matroska's default value and would
/// be noise).
fn track_title(i: usize, t: &VideoTrack, r: &Report) -> String {
    let mut title = format!("Track {}", i + 1);
    if let Some(p) = t.program {
        let _ = write!(title, " · Program {p}");
    }
    if t.default == Some(true) && r.video_tracks.iter().any(|o| o.default == Some(false)) {
        title.push_str(" · Default");
    }
    title
}

/// The track-level rows of the General section (Bitrate, Video, Color) —
/// shared verbatim between the single-track layout (inside the combined
/// General section) and each multi-track track group.
fn track_general_rows(s: &mut String, c: &Colorizer, t: &VideoTrack, sidecar: bool) {
    if let Some(br) = &t.bitrate {
        let scope = match br.scope {
            BitrateScope::VideoStream => "video stream",
            BitrateScope::Overall => "overall",
        };
        kv_styled(
            s,
            c,
            "Bitrate",
            &format!("{}{}", c.value(&human_bitrate(br.bits_per_sec)), c.tag(scope)),
        );
    }
    let video = video_line(t);
    if !video.is_empty() && !sidecar {
        kv_styled(s, c, "Video", &c.bright(&video));
    }
    let color = color_line(t);
    if !color.is_empty() {
        kv(s, c, "Color", &color);
    }
}

/// One track's HDR / Dolby Vision / HDR10+ sections.
fn track_sections(
    s: &mut String,
    c: &Colorizer,
    t: &VideoTrack,
    sidecar: bool,
    o: &RenderOpts,
    notes: &mut Footnotes,
) {
    if o.show_hdr {
        if let Some(hdr) = &t.hdr {
            let _ = writeln!(s, "{}", c.section("HDR"));
            kv_styled(s, c, "Format", &c.bright(&hdr.format));
            if let Some(m) = &hdr.mastering {
                // Gamut first, luminance after: "DCI-P3 D65 · max 1000  min 0.0001 cd/m²".
                let prim = m.primaries.as_ref().map(|p| format!("{p} · ")).unwrap_or_default();
                kv(
                    s,
                    c,
                    "Mastering",
                    &format!("{}max {}  min {} cd/m²", prim, fmt_num(m.max_luminance), fmt_num(m.min_luminance)),
                );
            }
            if let Some(cl) = &hdr.content_light {
                let flag = if cl.zeroed { format!("  {}", c.warn("zeroed")) } else { String::new() };
                let light = c.value(&format!("MaxCLL {} · MaxFALL {}", cl.max_cll, cl.max_fall));
                kv_styled(s, c, "Content light", &format!("{}{}", light, flag));
            }
            s.push('\n');
        }
    }

    // SL-HDR and HDR Vivid get their own sections like Dolby Vision and
    // HDR10+ — each section exists only when the metadata was found, and
    // each has its own --sections name (slhdr / hdrvivid) so the dynamic
    // formats filter symmetrically with dv and hdr10plus.
    if o.show_sl_hdr {
        if let Some(sl) = &t.sl_hdr {
            let _ = writeln!(s, "{}", c.section("SL-HDR"));
            // The variant headline, bright like the HDR10+ Profile row. The
            // Format line carries the same numbered name — the industry
            // treats SL-HDR1/2/3 as sibling formats (ETSI part titles, DVB
            // component codes, MediaInfo's HDR_Format), so the digit stays a
            // format fact, not plumbing.
            kv_styled(s, c, "Type", &c.bright(&format!("SL-HDR{}", sl.mode)));
            kv(s, c, "Version", &format!("v{}", sl.spec_version));
            if let Some(pm) = &sl.payload_mode {
                kv(s, c, "Payload mode", pm);
            }
            // The presentation target the adaptation metadata is tuned toward
            // (corpus-verified title-stable), e.g. the 100-nit BT.2020 SDR
            // rendition an SL-HDR2 receiver derives.
            match (&sl.target_primaries, sl.target_max_luminance) {
                (Some(p), Some(n)) => kv(s, c, "Target", &format!("{} · {} nits", p, n)),
                (Some(p), None) => kv(s, c, "Target", p),
                (None, Some(n)) => kv(s, c, "Target", &format!("{} nits", n)),
                (None, None) => {}
            }
            s.push('\n');
        }
    }
    if o.show_hdr_vivid {
        if let Some(hv) = &t.hdr_vivid {
            let _ = writeln!(s, "{}", c.section("HDR Vivid"));
            kv(s, c, "Version", &format!("v{}", hv.version));
            // The authored display anchors the per-frame curves are computed
            // toward — a distinct set like the DV trim targets, with the same
            // sampled caveat (its own footnote text: frames, not RPUs).
            if !hv.target_max_luminances.is_empty() {
                let mark = if hv.sampled { notes.mark(SAMPLED_FRAMES_NOTE) } else { "" };
                let label =
                    if hv.target_max_luminances.len() == 1 { "Target" } else { "Targets" };
                let vals = hv
                    .target_max_luminances
                    .iter()
                    .map(|n| format!("{n} nits"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                kv(s, c, &format!("{label}{mark}"), &vals);
            }
            s.push('\n');
        }
    }

    if o.show_dv {
        if let Some(dv) = &t.dolby_vision {
            let _ = writeln!(s, "{}", c.section("Dolby Vision"));

            if let Some(census) = &dv.census {
                // Census stats lead the section (consistent across all input
                // types). This line is census-gated, and the census only exists
                // on a full scan (sidecars are always full; video needs --full),
                // so an RPU count here is never a sample — no "[full scan]" tag.
                kv(s, c, "RPU count", &dv.rpu_count.to_string());
                kv(s, c, "Scene cuts", &census.scene_cuts.to_string());
            }
            // Shot-based vs frame-by-frame authoring, decided from adjacent
            // frames' DM payloads — only exhaustive stream-order reads (a
            // `--full` scan or a DV sidecar) produce it, so like the census
            // lines it never reflects a sample. The text keeps the verdict;
            // the JSON carries the pair counts behind it.
            if let Some(cad) = &dv.metadata_cadence {
                kv(s, c, "Metadata cadence", &cad.cadence);
            }
            if let Some(structure) = &dv.structure {
                kv(s, c, "Structure", structure);
            }

            // The BL/EL/RPU carriage booleans are serialized in the JSON report
            // (part of the documented schema) but omitted from this text report:
            // the profile and MEL/FEL tag already convey the layer structure, and
            // the per-track BL flag reads as misleading on dual-track P7.
            //
            // The profile describes a *stream* (its codec and base layer), not
            // bare metadata: an RPU is profile-agnostic (dovi_tool's blanket
            // "8" for extracted RPUs is remux convention, not a definition)
            // and a DV XML's GenerateProfile is an authoring target. So the
            // line is skipped for sidecars; the JSON keeps `profile` and
            // `profile_compat_assumed` (that flag fires only on these inputs,
            // so its old "[compat assumed]" tag no longer renders anywhere).
            if !sidecar {
                // A dual-layer-authored RPU riding an EL-less carriage: the
                // usual product of a custom transcode that injected a UHD-BD
                // P7 RPU without converting it — which is also what makes
                // muxers that fingerprint the RPU write odd profile digits
                // (mkvmerge's AV1 "10.6"), so the chip rides the line whose
                // number it explains. The chip stays short; the JSON spells
                // out `unconverted_dual_layer_rpu`.
                let unconverted = if dv.unconverted_dual_layer_rpu {
                    format!("  {}", c.warn("Unconverted RPU"))
                } else {
                    String::new()
                };
                let profile = c.bright(&dv_profile_display(dv, t));
                kv_styled(s, c, "Profile", &format!("{profile}{unconverted}"));
            }

            // The DV level only defines the codec bit-rate envelope; it says
            // nothing useful at a glance, so it's kept on the model but not
            // rendered here.
            if let Some(cm) = &dv.cm_version {
                // Only the content-mapping version: "present" is implied by the
                // section header, and the EL type (MEL/FEL) is already on the
                // Profile line. `cm_version` is stored as "CM v2.9"/"CM v4.0";
                // drop the redundant "CM " since the label spells it out.
                let ver = cm.strip_prefix("CM ").unwrap_or(cm);
                kv(s, c, "Content mapping", ver);
            }

            // The reconstructed ("VDR") signal depth from the RPU header —
            // model-gated to FEL, the one case where a real residual composes
            // beyond the base layer: 12-bit on Profile 7, 14 on Profile 4. The
            // "10-bit BL" half is a fact of every parsed RPU (libdovi validates
            // the header's BL/EL depths to exactly 10), not a guess. Rendered
            // for FEL sidecars too: unlike the profile, these are values the
            // metadata itself carries.
            if let Some(bits) = dv.reconstructed_bit_depth {
                kv(s, c, "Reconstruction", &format!("{bits}-bit (10-bit BL + FEL residual)"));
            }
            // The DV grade's mastering display comes from the DM data header
            // (source_min/max_pq), not a metadata level — so it renders with the
            // header-derived lines above, ahead of the L2..L11 level lines. The
            // gamut is level-carried (the header has no primaries): L9 in an
            // RPU, the Level-0 global `<MasteringDisplay>` in a DV XML — so it
            // rides along with its provenance tagged, like the trim targets'.
            if let Some(md) = &dv.mastering_display {
                let prim = md
                    .primaries
                    .as_ref()
                    .map(|p| {
                        let tag = md
                            .primaries_level
                            .map(|l| format!("{} ", c.prov(&format!("L{l}"))))
                            .unwrap_or_default();
                        format!("{} {}{}", c.value(p), tag, c.value("· "))
                    })
                    .unwrap_or_default();
                // The grade out-brights the base layer's declared mastering: a
                // FEL whose residual likely carries highlights the BL lacks, so
                // stripping the EL (e.g. a P7 -> P8 conversion) would lose them.
                let expansion = dv
                    .fel_brightness_expansion
                    .map(|_| format!("  {}", c.warn("FEL brightness expansion")))
                    .unwrap_or_default();
                // The L9 gamut at the front of this line disagrees with the
                // base layer's own declared mastering primaries (the HDR
                // section's Mastering line) — usually re-encode drift, e.g. a
                // BT.2020-claiming MDCV left over a P3-D65 grade. Both values
                // already render on their own lines; the badge saves the
                // cross-check.
                let mismatch = dv
                    .mastering_primaries_mismatch
                    .as_ref()
                    .map(|_| format!("  {}", c.warn("MDP mismatch")))
                    .unwrap_or_default();
                let lum = c.value(&format!(
                    "max {}  min {} cd/m²",
                    fmt_num(md.max_luminance),
                    fmt_num(md.min_luminance)
                ));
                kv_styled(s, c, "Mastering", &format!("{}{}{}{}", prim, lum, mismatch, expansion));
            }
            if !dv.trim_targets.is_empty() {
                // The target set is a union over the RPUs actually read, and the
                // L8 half is per-shot in real titles (a BD original whose head
                // shots carry only the 100-nit L8 while later scenes add 600),
                // so a sampled union may be incomplete — footnoted like L5. A
                // full scan is complete, so it carries no caveat mark. (The
                // L10-defined custom targets folded into the L8 set are
                // title-global and complete from any sample, but the set can
                // still be missing preset-target trims, so the mark stays
                // set-level.)
                let mark = if dv.sampled { notes.mark(SAMPLED_NOTE) } else { "" };
                let list = dv
                    .trim_targets
                    .iter()
                    .map(|t| {
                        let tag = t.levels.iter().map(|l| format!("L{l}")).collect::<Vec<_>>().join("/");
                        format!("{} {}", c.value(&format!("{} nits", t.nits)), c.prov(&tag))
                    })
                    .collect::<Vec<_>>()
                    .join(&c.value(", "));
                kv_styled(s, c, &format!("Trim targets{mark}"), &list);
            }
            if !dv.l5_active_areas.is_empty() {
                // The set of distinct active areas is shown inline (joined by
                // " + ") rather than one line per area: offsets are the raw L5
                // signal, the active area is derived. The sampled/assumed-canvas
                // caveat describes the whole set, so both L5 labels share the
                // same footnote mark; a full scan carries no mark.
                let mark = match dv.l5_assumed_canvas {
                    Some([w, h]) => notes.mark(&format!(
                        "assumes a {w}×{h} canvas; DV sidecars carry no resolution"
                    )),
                    None if dv.sampled => notes.mark(SAMPLED_NOTE),
                    None => "",
                };
                let offsets = dv
                    .l5_active_areas
                    .iter()
                    .map(|a| format!("L{} R{} T{} B{}", a.left, a.right, a.top, a.bottom))
                    .collect::<Vec<_>>()
                    .join(" + ");
                // More than one distinct active area means the aspect ratio
                // changes across the title — worth flagging as special.
                let variable = if dv.l5_active_areas.len() > 1 {
                    format!("  {}", c.warn("variable"))
                } else {
                    String::new()
                };
                kv_styled(s, c, &format!("L5 offsets{mark}"), &format!("{}{}", c.value(&offsets), variable));
                let areas = dv
                    .l5_active_areas
                    .iter()
                    .filter(|a| a.width > 0 && a.height > 0)
                    .map(|a| format!("{}×{}  ({})", a.width, a.height, aspect(a.width, a.height)))
                    .collect::<Vec<_>>()
                    .join(" + ");
                if !areas.is_empty() {
                    kv(s, c, &format!("L5 active area{mark}"), &areas);
                }
            }
            // L6's CLL fields exist to feed HDR10 signaling (CTA-861.3), which
            // only an HDR10-compatible base consumes — compat id 1, or 6 (the
            // UHD Blu-ray HDR10 base). On every other base (IPT-PQ-c2 compat 0,
            // HLG compat 4, SDR compat 2) they're a zeroed placeholder or, if
            // filled, inert for playback — corpus 8.4/10.4 titles carry the same
            // zeroed L6 as P5. Keep the line out of the text report; the JSON
            // still carries `l6` verbatim (the mastering half is real either
            // way). Without a compat id the profile label's minor digit is the
            // convention default, so gate on the major: P7/P8 default to an
            // HDR10 base (7.6/8.1), while P4 (SDR) and a bare P5 (IPT) don't.
            let hdr10_base = match dv.bl_compatibility_id {
                Some(id) => id == 1 || id == 6,
                None => dv.profile.starts_with('7') || dv.profile.starts_with('8'),
            };
            if let Some(l6) = dv.l6.as_ref().filter(|_| hdr10_base) {
                let flag = if l6.zeroed { format!("  {}", c.warn("zeroed")) } else { String::new() };
                let light = c.value(&format!("MaxCLL {} · MaxFALL {}", l6.max_cll, l6.max_fall));
                kv_styled(s, c, "L6 content light", &format!("{}{}", light, flag));
            }
            // L9 folds into the Mastering line above when recognized; a
            // standalone line remains only when it couldn't ride there (no
            // mastering display in the DM header, or an unmatched custom gamut).
            let l9_on_mastering =
                dv.mastering_display.as_ref().is_some_and(|m| m.primaries.is_some());
            if !l9_on_mastering {
                if let Some(l9) = &dv.l9_mastering {
                    kv(s, c, "L9 mastering", l9);
                }
            }
            if let Some(l11) = &dv.l11_content {
                let wp = match &dv.l11_white_point {
                    Some(wp) => format!(" · white point {wp}"),
                    None => String::new(),
                };
                let rm = match dv.l11_reference_mode {
                    Some(true) => " · reference mode",
                    _ => "",
                };
                kv(s, c, "L11 APO", &format!("{}{}{}", l11, wp, rm));
            }
            if let Some(census) = &dv.census {
                let levels = census
                    .level_presence
                    .iter()
                    .map(|lp| {
                        if lp.rpus_with == dv.rpu_count {
                            format!("L{}", lp.level)
                        } else {
                            format!("L{} ({})", lp.level, lp.rpus_with)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if !levels.is_empty() {
                    kv(s, c, "Levels present", &levels);
                }
            }
            s.push('\n');
        }
    }

    // The section exists only when HDR10+ metadata was found, like Dolby Vision.
    if o.show_hdr10plus {
        if let Some(hp) = &t.hdr10plus {
            let _ = writeln!(s, "{}", c.section("HDR10+"));
            // The section's bright headline, mirroring the DV Profile line
            // (and like it, plain for an HDR10+ JSON sidecar).
            if let Some(p) = hp.profile {
                let styled = if sidecar { c.value(&p.to_string()) } else { c.bright(&p.to_string()) };
                kv_styled(s, c, "Profile", &styled);
            }
            kv(s, c, "Application", &format!("v{}", hp.application_version));
            kv(s, c, "Windows", &hp.num_windows.to_string());
            if let Some(n) = hp.target_max_luminance {
                kv(s, c, "Target", &format!("{} nits", n));
            }
            s.push('\n');
        }
    }
}

/// The one caveat most reports carry: the default pipeline reads a spread of
/// RPUs, so RPU-derived sets (trim targets, L5 areas) may be incomplete.
const SAMPLED_NOTE: &str = "sampled from a spread of RPUs; --full reads every one";

/// The same caveat for SEI-derived sets (the HDR Vivid target anchors), whose
/// carrier is the frame's SEI rather than an RPU.
const SAMPLED_FRAMES_NOTE: &str = "sampled from a spread of frames; --full reads every one";

/// Caveat footnotes referenced from row labels ("Trim targets*") and spelled
/// out once at the report's foot. Only caveats register — a full scan needs no
/// excuse, so it gets no mark and the marker's absence reads as completeness.
/// Registering the same text twice reuses the first marker, so the trim and L5
/// lines of a sampled report share one asterisk.
#[derive(Default)]
struct Footnotes {
    notes: Vec<String>,
}

impl Footnotes {
    const MARKS: [&'static str; 3] = ["*", "†", "‡"];

    fn mark(&mut self, text: &str) -> &'static str {
        let i = match self.notes.iter().position(|n| n == text) {
            Some(i) => i,
            None => {
                self.notes.push(text.to_string());
                self.notes.len() - 1
            }
        };
        Self::MARKS[i.min(Self::MARKS.len() - 1)]
    }

    fn lines(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.notes
            .iter()
            .enumerate()
            .map(|(i, n)| (Self::MARKS[i.min(Self::MARKS.len() - 1)], n.as_str()))
    }
}

/// The bare file name of a report's `file` path, for the report header.
/// Falls back to the full string when a name can't be split off (e.g. a
/// path ending in `..`) — never an empty header.
fn file_name(path: &str) -> &str {
    std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path)
}

/// The per-report header line. Colored: phosphor banner glyph, faint size.
/// Plain: the classic "name  (size)" shape, unchanged for pipes and logs.
/// Both show the bare file name (matching the `--full` scanning header); the
/// full path stays on the JSON report's `file` field, where machine consumers
/// need it. A multi-file run appends the progress header's `[k/N]` counter
/// (faint colored, bracketed plain) so each report says where it sits in the
/// run; the counter is position-in-run, so a failed file leaves an honest gap
/// in the sequence rather than renumbering the survivors. A truncated stdin
/// probe marks the size with a trailing `+` — the bytes probed, with more
/// behind them.
fn report_header(
    file: &str,
    size_bytes: u64,
    truncated: bool,
    o: &RenderOpts,
    c: &Colorizer,
) -> String {
    let name = file_name(file);
    let size = format!("{}{}", human_size(size_bytes), if truncated { "+" } else { "" });
    let counter = if o.file_count > 1 {
        format!("  {}", c.faint(&format!("[{}/{}]", o.file_index, o.file_count)))
    } else {
        String::new()
    };
    if c.on {
        format!("{} {}{}\n", c.bright(&format!("▮ {}", name)), c.faint(&size), counter)
    } else {
        format!("{}  ({}){}\n", name, size, counter)
    }
}

/// Divider between consecutive reports of a multi-file text run: a full-width
/// *heavy* rule (`━`, vs the section rules' light `─` — same width, thicker
/// stroke, so a report boundary reads differently from a section boundary),
/// bright when colored (the section rules stay faint — weight and ink both
/// separate the two). Sized like the section rules: the probed terminal
/// width when there is one, `RULE_W` otherwise — so piped text keeps its
/// exact historical divider. `main` inserts it between rendered reports only
/// — never before the first or after the last — so a single-file run and
/// every machine path (quiet, JSON, NDJSON) are unchanged.
pub fn render_divider(o: &RenderOpts) -> String {
    let c = Colorizer { on: o.color, palette: o.theme.palette(), wrap: o.wrap_width, indent: 0, word_wrap: false };
    format!("{}\n\n", c.bright(&"━".repeat(c.rule_width())))
}

/// One-line summary for `--quiet` — one line *per video track*: a
/// single-track file keeps the exact historical shape, a multi-track file
/// tags each line `[k/N]` so per-track facts stay unambiguous and
/// grep/awk-friendly.
pub fn render_quiet(r: &Report) -> String {
    let n = r.video_tracks.len();
    r.video_tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut parts = Vec::new();
            if let Some(dv) = &t.dolby_vision {
                parts.push(format!("DV {}", dv_profile_display(dv, t)));
            }
            if let Some(hdr) = &t.hdr {
                parts.push(hdr.format.clone());
            } else if t.hdr10plus.is_some() {
                parts.push("HDR10+".to_string());
            } else if t.dolby_vision.is_none() {
                parts.push("SDR".to_string());
            }
            if let (Some(w), Some(h)) = (t.width, t.height) {
                parts.push(format!("{}×{}", w, h));
            }
            if n == 1 {
                format!("{}  {}", r.file, parts.join(" · "))
            } else {
                format!("{} [{}/{}]  {}", r.file, i + 1, n, parts.join(" · "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn kv(s: &mut String, c: &Colorizer, label: &str, value: &str) {
    kv_styled(s, c, label, &c.value(value));
}

/// Like `kv`, but the value already carries its own styling (mixed value/tag/
/// warning segments), so it isn't re-wrapped — ANSI codes don't nest, and a
/// wrapper around an inner reset would drop the colour mid-line.
fn kv_styled(s: &mut String, c: &Colorizer, label: &str, value: &str) {
    // Char count, not byte length: a footnote marker on the label (†, ‡) is
    // multi-byte but single-column, and byte-based padding would misalign it.
    let pad = " ".repeat(LABEL_W.saturating_sub(label.chars().count()));
    let line = format!("{}{}{}  {}", " ".repeat(2 + c.indent), c.label(label), pad, value);
    // The value column (and with it the whole reflow geometry) shifts with
    // the track-group indent, so the bow-out floor shifts the same amount.
    let value_col = VALUE_COL + c.indent;
    match c.wrap {
        Some(w) if w >= MIN_WRAP_WIDTH + c.indent => {
            for l in wrap_line(&line, w, value_col, c.word_wrap) {
                let _ = writeln!(s, "{l}");
            }
        }
        _ => {
            let _ = writeln!(s, "{line}");
        }
    }
}

/// One visible character of a styled line, tagged with the SGR parameter
/// string active where it appears (`""` = unstyled). Decomposing the line
/// this way makes reflow pure text layout: escape codes are zero-width by
/// construction and a break inside a styled span re-opens the span on the
/// continuation line for free when the cells are re-serialized.
struct Cell<'a> {
    style: &'a str,
    ch: char,
}

/// Decompose a rendered line into per-character cells. Only the escape shape
/// this renderer itself emits (`\x1b[<params>m`, non-nesting, `0` = reset) is
/// recognized — the input is always our own `Colorizer` output.
fn parse_cells(line: &str) -> Vec<Cell<'_>> {
    let mut cells = Vec::new();
    let mut style = "";
    let mut it = line.char_indices().peekable();
    while let Some((i, ch)) = it.next() {
        if ch == '\x1b' && matches!(it.peek(), Some((_, '['))) {
            it.next();
            let start = i + 2;
            let mut end = start;
            for (j, c2) in it.by_ref() {
                if c2 == 'm' {
                    end = j;
                    break;
                }
            }
            let params = &line[start..end];
            style = if params == "0" { "" } else { params };
            continue;
        }
        cells.push(Cell { style, ch });
    }
    cells
}

/// Re-serialize cells to a styled string, grouping runs of one style into
/// `\x1b[..m…\x1b[0m` spans (the same open+reset shape `Colorizer::wrap`
/// emits). `indent` prepends the `value_col` margin for continuation lines.
fn render_cells(cells: &[Cell<'_>], indent: bool, value_col: usize) -> String {
    let mut out = String::new();
    if indent {
        out.push_str(&" ".repeat(value_col));
    }
    let mut i = 0;
    while i < cells.len() {
        let style = cells[i].style;
        let mut j = i + 1;
        while j < cells.len() && cells[j].style == style {
            j += 1;
        }
        if !style.is_empty() {
            let _ = write!(out, "\x1b[{style}m");
        }
        out.extend(cells[i..j].iter().map(|c| c.ch));
        if !style.is_empty() {
            out.push_str("\x1b[0m");
        }
        i = j;
    }
    out
}

/// Break opportunities of a kv line, as `(end, resume)` cell indices: the
/// line may end at `end` (exclusive) and continue at `resume`, dropping the
/// separator whitespace between them. Breaks land only after a part
/// separator — the ` · ` / `, ` / ` + ` joins the value builders use (the
/// separator stays at line end, signalling continuation) — or at an unstyled
/// double space (the gap before a warning chip or between value halves).
/// Never inside a part, and never before the value column, so the label is
/// untouchable. Single spaces are not candidates, which also keeps warning
/// chips (styled spaces inside inverse video) whole. With `words` set (the
/// notice path — no chips there) every space run additionally breaks before
/// its first space, so separator-less values word-wrap to the value column.
fn break_candidates(cells: &[Cell<'_>], value_col: usize, words: bool) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let n = cells.len();
    for i in value_col..n {
        let next_space = i + 1 < n && cells[i + 1].ch == ' ';
        let sep = match cells[i].ch {
            '·' | ',' => next_space,
            '+' => next_space && cells[i - 1].ch == ' ',
            ' ' => {
                (words && cells[i - 1].ch != ' ')
                    || (next_space && cells[i].style.is_empty() && cells[i + 1].style.is_empty())
            }
            _ => false,
        };
        if !sep {
            continue;
        }
        // A double space ends the line *before* the gap; a separator glyph
        // stays on the line. A separator's padding spaces are consumed even
        // when styled (they sit inside the separator's own span), but a
        // double-space break stops at the first styled space — that space
        // belongs to what follows (a warning chip's inverse-video lead pad),
        // not to the gap.
        let gap = cells[i].ch == ' ';
        let end = if gap { i } else { i + 1 };
        let mut resume = i + 1;
        while resume < n
            && cells[resume].ch == ' '
            && (!gap || words || cells[resume].style.is_empty())
        {
            resume += 1;
        }
        if resume < n {
            v.push((end, resume));
        }
    }
    v
}

/// Greedy reflow of one rendered kv line to `width` visible columns:
/// continuation lines indent to `value_col` (the row's own value column —
/// shifted right inside a track group) and re-open the style active at the
/// break. Char count is the width measure — every glyph this report emits is
/// single-column (the same assumption the label padding makes). A line that
/// already fits is returned byte-identical; a part too long for any break
/// overflows to the next separator (the terminal's hard wrap takes it from
/// there) rather than breaking mid-part. `words` widens the candidates to
/// word boundaries (`break_candidates`).
fn wrap_line(line: &str, width: usize, value_col: usize, words: bool) -> Vec<String> {
    let cells = parse_cells(line);
    if cells.len() <= width {
        return vec![line.to_string()];
    }
    let candidates = break_candidates(&cells, value_col, words);
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let indent = !out.is_empty();
        let budget = if indent { width - value_col } else { width };
        if cells.len() - start <= budget {
            out.push(render_cells(&cells[start..], indent, value_col));
            break;
        }
        let fit = candidates.iter().rev().find(|(end, _)| *end > start && end - start <= budget);
        let chosen = fit.or_else(|| candidates.iter().find(|(end, _)| *end > start));
        match chosen {
            Some(&(end, resume)) => {
                out.push(render_cells(&cells[start..end], indent, value_col));
                start = resume;
            }
            None => {
                out.push(render_cells(&cells[start..], indent, value_col));
                break;
            }
        }
    }
    out
}

fn video_line(g: &VideoTrack) -> String {
    let mut parts = Vec::new();
    let codec = match &g.codec_profile {
        Some(p) => format!("{} ({})", g.codec, p),
        None => g.codec.clone(),
    };
    if !codec.is_empty() {
        parts.push(codec);
    }
    if let (Some(w), Some(h)) = (g.width, g.height) {
        parts.push(format!("{}×{}", w, h));
    }
    if let Some(f) = g.fps {
        parts.push(format!("{:.3} fps", f));
    }
    let mut depth = String::new();
    if let Some(b) = g.bit_depth {
        depth = format!("{}-bit", b);
    }
    if let Some(ch) = &g.chroma {
        if depth.is_empty() {
            depth = ch.clone();
        } else {
            depth = format!("{} {}", depth, ch);
        }
    }
    if !depth.is_empty() {
        parts.push(depth);
    }
    if let Some(s) = &g.stereo {
        parts.push(s.clone());
    }
    parts.join(" · ")
}

fn color_line(t: &VideoTrack) -> String {
    // The profile-defined colour inferences inside apply only to video inputs: a
    // metadata-only sidecar (no codec — the same signal that suppresses the Video
    // line) has no base layer whose colour they could describe.
    build_color_line(
        &t.color,
        t.dolby_vision.as_ref().map(|dv| dv.profile.as_str()),
        !t.codec.is_empty(),
    )
}

fn build_color_line(cc: &ColorInfo, dv_profile: Option<&str>, has_video: bool) -> String {
    let mut parts = Vec::new();

    // Dolby Vision Profile 5 is spec-locked to Dolby's IPT-PQ-c2 colour space over
    // BT.2020 primaries / PQ / full range — that's definitional, not signalled. The
    // colour space can't be expressed in CICP, so the SPS carries "unspecified"
    // (2/2/2) and only the range survives, leaving a bare "full". Any CICP a P5
    // stream did happen to carry would be noise, so state the fixed profile colour.
    // Match by prefix: the label carries the compat minor when a dvcC supplied one
    // ("5.0"), but a raw elementary stream has no dvcC and labels bare ("5").
    let is_p5 = has_video && dv_profile.is_some_and(|p| p.starts_with('5'));
    // Profile 4's base layer is defined as Rec.709 SDR (VUI 0,1,1,1,0). Older P4
    // muxes omit the colour description entirely (colour_description_present_flag=0),
    // so the SPS yields no primaries/transfer at all — like the P5 case, state the
    // profile-defined base colour rather than leave it blank. A P4 stream that *does*
    // signal a colour description keeps its own values (this only fills the gap).
    let is_p4 = has_video && dv_profile.is_some_and(|p| p.starts_with('4'));
    let p4_colour_absent = is_p4 && cc.primaries.is_none() && cc.transfer.is_none();
    if is_p5 {
        // P5 is the case that must *not* collapse: its encoding (PQ) genuinely
        // differs from its colour space (IPT-PQ-c2 over BT.2020), so all three show.
        parts.push("IPT-PQ-c2".to_string());
        parts.push("BT.2020".to_string());
        parts.push("PQ (SMPTE ST 2084)".to_string());
    } else {
        // Dolby's IPT-PQ-c2 (CICP matrix 15) is the one matrix coefficient worth
        // naming: it identifies the colour space of Profile 20 (MV-HEVC) DV, which —
        // unlike P5 — signals valid primaries/transfer/range in its colr box.
        if cc.matrix.as_deref() == Some("IPT-PQ-c2") {
            parts.push("IPT-PQ-c2".to_string());
        }
        // Colour space (primaries) and encoding (transfer). For Profile 4 with no
        // signalled colour description, both are the profile-defined Rec.709.
        let primaries = if p4_colour_absent { Some("BT.709") } else { cc.primaries.as_deref() };
        let transfer = if p4_colour_absent { Some("BT.709") } else { cc.transfer.as_deref() };
        // When the colour space and encoding carry the same name (Rec.709 SDR: a
        // BT.709 gamut with a BT.709 transfer), collapse the pair to one label
        // instead of printing "BT.709 · BT.709". Distinct pairs (e.g. BT.2020 + PQ)
        // both show.
        match (primaries, transfer) {
            (Some(p), Some(t)) if p == t => parts.push(p.to_string()),
            _ => {
                if let Some(p) = primaries {
                    parts.push(p.to_string());
                }
                if let Some(t) = transfer {
                    parts.push(t.to_string());
                }
            }
        }
    }
    if let Some(m) = &cc.range {
        parts.push(m.clone());
    } else if p4_colour_absent {
        parts.push("limited".to_string());
    }
    parts.join(" · ")
}

/// The Dolby Vision Profile value as displayed (the text report's Profile line
/// and the `--quiet` summary): the model's label, except that a bare number —
/// a raw elementary stream, where no dvcC/dvvC *exists* to declare the
/// compatibility minor — is completed when the digit is certain: "5" from the
/// profile's definition (compat 0 is the only value P5 admits), "10" from the
/// base layer's signalled CICP when that signal picks the digit airtight
/// (`infer_p10_compat`). Video inputs only: a metadata sidecar has no base
/// layer to read. Display-only opinion by design: the JSON `profile` /
/// `bl_compatibility_id` / `compatibility` keep exactly what the mux declares
/// (the bare number / null), so machine consumers get the raw facts and draw
/// their own inferences.
fn dv_profile_display(dv: &DolbyVision, track: &VideoTrack) -> String {
    // A bare label implies no compat id was declared anywhere — a declared or
    // XML-supplied id would already have rendered the minor digit.
    if !track.codec.is_empty() {
        // Profile 5 admits *only* compat 0 (IPT-PQ-c2, no cross-compatible
        // base — Dolby's P&L spec), so a bare "5" (a raw ES with no dvcC)
        // completes definitionally, no base-layer signal needed — the same
        // definition `build_color_line` already states for its Color line.
        if dv.profile == "5" {
            return "5.0".to_string();
        }
        if dv.profile == "10" {
            if let Some(id) = infer_p10_compat(&track.color) {
                return format!("10.{id}");
            }
        }
    }
    dv.profile.clone()
}

/// The compatibility minor for a bare Profile 10, deduced by elimination from
/// the base layer's signalled CICP. Profile 10 admits compat ids {0, 1, 2, 4}
/// (IPT / HDR10 / SDR / HLG bases), so an explicit base-layer colour signal
/// leaves exactly one candidate — but only an *explicit* one:
///
/// - The IPT-PQ-c2 matrix (CICP 15) is Dolby's own colour system → 0.
/// - An explicit SDR gamma transfer → 2. No matrix tag needed to exclude IPT:
///   IPT-PQ-c2 is PQ-encoded by definition.
/// - PQ → 1 and HLG → 4 additionally require BT.2020 primaries *and* an
///   explicit non-IPT matrix: an IPT base is itself PQ-encoded and its
///   signalling convention (inherited from Profile 5) leaves CICP
///   unspecified, so PQ over an absent matrix is not airtight evidence of an
///   HDR10 base — it could be a 10.0 stream tagging only its EOTF.
///
/// Anything less explicit returns `None` and the label stays bare. Matching
/// on the closed label strings from `container::cicp_*` keeps this in one
/// value space with the rest of the renderer.
fn infer_p10_compat(cc: &ColorInfo) -> Option<u8> {
    let matrix = cc.matrix.as_deref();
    if matrix == Some("IPT-PQ-c2") {
        return Some(0);
    }
    let transfer = cc.transfer.as_deref()?;
    // The SDR gamma transfers `container::cicp_transfer` can name (BT.2020
    // 10/12-bit are the wide-gamut SDR curves, same OETF family as BT.709).
    if matches!(transfer, "BT.709" | "BT.601" | "BT.2020 (10-bit)" | "BT.2020 (12-bit)") {
        return Some(2);
    }
    if cc.primaries.as_deref() != Some("BT.2020") || matrix.is_none() {
        return None;
    }
    match transfer {
        "PQ (SMPTE ST 2084)" => Some(1),
        "HLG (ARIB STD-B67)" => Some(4),
        _ => None,
    }
}

fn fmt_num(v: f64) -> String {
    if v == v.trunc() {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

fn aspect(w: u32, h: u32) -> String {
    if h == 0 {
        return "?".to_string();
    }
    format!("{:.2}:1", w as f64 / h as f64)
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[i])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

fn human_bitrate(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.2} Mb/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.0} kb/s", bps / 1_000.0)
    } else {
        format!("{:.0} b/s", bps)
    }
}

/// The Main feature line of a Blu-ray ISO report:
/// `00800.mpls (2:14:05) · clip 1/1: 00055.m2ts`.
fn bd_iso_line(iso: &crate::model::BdIso) -> String {
    format!(
        "{} ({}) · clip {}/{}: {}",
        iso.playlist,
        human_duration(iso.playlist_duration_secs),
        iso.clip_index,
        iso.clip_count,
        iso.clip
    )
}

fn human_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}h {:02}m {:02}s", h, m, s)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Phosphor-CRT palette: one hue at four intensities. Brightness is the
/// visual hierarchy — bright for headline facts (file name, HDR format, DV
/// profile, section names), mid for ordinary values, low for labels, faint
/// for tags and rules — and warnings invert the video, which a single-hue
/// scheme makes unmissable. Each field is the SGR parameter string for one
/// role; the values are hand-tuned per theme (the ramp is perceptual, not a
/// linear scale), with `warn` carrying the bright ink behind inverse video.
pub(crate) struct Palette {
    pub(crate) bright: &'static str,
    pub(crate) value: &'static str,
    pub(crate) label: &'static str,
    pub(crate) faint: &'static str,
    warn: &'static str,
}

const GREEN: Palette = Palette {
    bright: "1;38;2;120;255;160",
    value: "38;2;80;210;120",
    label: "38;2;55;140;85",
    faint: "38;2;38;95;60",
    warn: "7;38;2;120;255;160",
};

const AMBER: Palette = Palette {
    bright: "1;38;2;255;205;120",
    value: "38;2;225;165;70",
    label: "38;2;160;115;50",
    faint: "38;2;110;80;35",
    warn: "7;38;2;255;205;120",
};

const RED: Palette = Palette {
    bright: "1;38;2;255;150;125",
    value: "38;2;230;105;85",
    label: "38;2;165;75;62",
    faint: "38;2;115;52;45",
    warn: "7;38;2;255;150;125",
};

const ICE: Palette = Palette {
    bright: "1;38;2;140;230;255",
    value: "38;2;90;190;225",
    label: "38;2;60;130;160",
    faint: "38;2;40;90;115",
    warn: "7;38;2;140;230;255",
};

const PURPLE: Palette = Palette {
    bright: "1;38;2;215;165;255",
    value: "38;2;180;125;230",
    label: "38;2;128;88;165",
    faint: "38;2;88;62;115",
    warn: "7;38;2;215;165;255",
};

const PAPER: Palette = Palette {
    bright: "1;38;2;240;240;235",
    value: "38;2;200;200;195",
    label: "38;2;140;140;135",
    faint: "38;2;100;100;95",
    warn: "7;38;2;240;240;235",
};

/// No colour of its own — bold/dim against the terminal's scheme. `value` is
/// deliberately empty (unstyled default foreground): `wrap` passes an empty
/// code through untouched.
const MONO: Palette = Palette {
    bright: "1",
    value: "",
    label: "2",
    faint: "2",
    warn: "7",
};

/// Applies the four-role hierarchy from one theme's `Palette`. The layout
/// (banner, footnotes, line structure) is shared between modes; with colour
/// off the intensities vanish, so the tag/provenance/warning helpers fall
/// back to the bracket conventions (`[tag]`, `(warning)`) that carry the
/// same semantics in plain text.
struct Colorizer {
    on: bool,
    palette: &'static Palette,
    /// Terminal width for value-line reflow and rule sizing
    /// (`RenderOpts::wrap_width`). Carried here because every `kv` call site
    /// already threads the Colorizer; the banner and report header — the
    /// decoration paths with nothing to size — pass `None`.
    wrap: Option<usize>,
    /// Left margin added to kv rows and section rules — `TRACK_INDENT` for a
    /// multi-track track body, 0 everywhere else. Carried here (like `wrap`)
    /// so the shared row/section builders shift without signature churn; the
    /// value column and reflow geometry follow it.
    indent: usize,
    /// Reflow kv values at any word boundary, not only at part separators —
    /// the notice path's mode: its registered command strings carry no
    /// `·`/`,`/` + ` joins, and without word breaks they'd fall through to
    /// the terminal's column-0 hard wrap instead of the value column. Report
    /// rows keep this false: their parts are semantic units that never split
    /// internally.
    word_wrap: bool,
}

impl Colorizer {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on && !code.is_empty() {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }
    /// Headline values.
    fn bright(&self, t: &str) -> String {
        self.wrap(self.palette.bright, t)
    }
    /// Ordinary values.
    fn value(&self, t: &str) -> String {
        self.wrap(self.palette.value, t)
    }
    /// Row labels.
    fn label(&self, t: &str) -> String {
        self.wrap(self.palette.label, t)
    }
    /// Faintest level: tags, rules, the timing footer.
    fn faint(&self, t: &str) -> String {
        self.wrap(self.palette.faint, t)
    }
    /// A whole-line qualifier tag (the bitrate scope: [video stream],
    /// [overall]) — the
    /// sampling caveats use `Footnotes` instead. Carries its own leading
    /// spacing: coloured it hangs off the value as a faint " tag" (a dim
    /// qualifier, not a separate element, so no `·` — matching `prov`),
    /// plain it keeps the classic three-space "[tag]".
    fn tag(&self, t: &str) -> String {
        if self.on {
            self.faint(&format!(" {t}"))
        } else {
            format!("   [{t}]")
        }
    }
    /// A per-value provenance tag (L2, L9): faint beside its value when
    /// coloured, bracketed when plain.
    fn prov(&self, t: &str) -> String {
        if self.on {
            self.faint(t)
        } else {
            format!("[{t}]")
        }
    }
    /// A warning: inverse-video uppercase chip when coloured, the classic
    /// parenthesis when plain.
    fn warn(&self, t: &str) -> String {
        if self.on {
            format!("\x1b[{}m {} \x1b[0m", self.palette.warn, t.to_uppercase())
        } else {
            format!("({t})")
        }
    }
    /// Width for section rules and the report divider: the probed terminal
    /// width when present, `RULE_W` for pipes and files — the same gate that
    /// keeps value reflow byte-neutral off-terminal, but with no minimum:
    /// rules shrink safely where reflow would bow out.
    fn rule_width(&self) -> usize {
        self.wrap.unwrap_or(RULE_W)
    }
    /// Section header: an uppercase ruled line when coloured, the bare name
    /// when plain. Inside a track group (`indent` > 0) the rule shifts right
    /// and leads with a `└─` branch instead of `──` — an L hanging off the
    /// track rule above, marking the section as that track's child — while
    /// its fill shortens so the right edge stays flush with the full-width
    /// track rule. The plain name indents the same columns (plain mode has
    /// no rule glyphs, so the branch is colour-only styling).
    fn section(&self, name: &str) -> String {
        let margin = " ".repeat(self.indent);
        if self.on {
            let up = name.to_uppercase();
            let lead = if self.indent > 0 { "└─" } else { "──" };
            // margin + lead + " " + name + " " + fill = rule_width() columns.
            let fill = "─".repeat(
                self.rule_width()
                    .saturating_sub(self.indent + 4)
                    .saturating_sub(up.chars().count()),
            );
            format!("{}{} {} {}", margin, self.faint(lead), self.bright(&up), self.faint(&fill))
        } else {
            format!("{margin}{name}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range_only() -> ColorInfo {
        ColorInfo { range: Some("full".to_string()), ..Default::default() }
    }

    /// Visible column count of a styled line (escape codes are zero-width).
    fn visible_len(line: &str) -> usize {
        parse_cells(line).len()
    }

    /// The auxiliary notice shares the report's row geometry: plain output is
    /// the bare section name plus gutter-and-label-column kv rows, no ANSI.
    #[test]
    fn notice_plain_uses_report_row_geometry() {
        let rows = [("Registered", "31 file types + folders".to_string())];
        assert_eq!(
            render_notice("Shell integration", &rows, false, Theme::Paper, None),
            "Shell integration\n  Registered        31 file types + folders\n"
        );
    }

    /// Colored, the notice gets the report's ruled section header and the
    /// label/value ink roles from the active palette, and the rule stretches
    /// to the probed terminal width like a report section.
    #[test]
    fn notice_colored_rules_and_inks_like_a_section() {
        let rows = [("Removed", "1 file type".to_string())];
        let s = render_notice("Shell integration", &rows, true, Theme::Green, Some(90));
        assert!(s.contains("SHELL INTEGRATION"), "section name uppercases under color: {s}");
        assert!(s.contains(&format!("\x1b[{}m", GREEN.label)), "label ink: {s}");
        assert!(s.contains(&format!("\x1b[{}m", GREEN.value)), "value ink: {s}");
        let rule = s.lines().next().unwrap();
        assert_eq!(visible_len(rule), 90, "rule fills the probed width: {rule}");
    }

    /// Notice values word-wrap at any word boundary (not only the report's
    /// part separators — a registered command string has none), every
    /// continuation indented to the value column (never the terminal's
    /// column-0 hard wrap) and no visible line over the probed width.
    #[test]
    fn notice_word_wraps_commands_to_the_value_column() {
        let sep = [("Registered", "context-menu submenu · 18 file types + folders".to_string())];
        let s = render_notice("Shell integration", &sep, false, Theme::Paper, Some(50));
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[1], "  Registered        context-menu submenu · 18 file");
        assert_eq!(lines[2], "                    types + folders");

        let cmd = r#"cmd /c "mode con: cols=110 lines=45 & cls & "C:\Programs\hdrprobe.exe" --own-console "%1" & pause""#;
        let rows = [("Fast runs", cmd.to_string())];
        let s = render_notice("Shell integration", &rows, false, Theme::Paper, Some(50));
        let lines: Vec<&str> = s.lines().skip(1).collect();
        assert!(lines.len() > 1, "the command must reflow: {s}");
        for (i, l) in lines.iter().enumerate() {
            assert!(l.chars().count() <= 50, "line over width: {l}");
            if i > 0 {
                assert!(l.starts_with(&" ".repeat(VALUE_COL)), "continuation off-column: {l}");
            }
        }
        // Word breaks drop exactly one space each, so rejoining the pieces
        // with single spaces restores the registered command verbatim.
        let mut parts = vec![lines[0].chars().skip(VALUE_COL).collect::<String>()];
        parts.extend(lines[1..].iter().map(|l| l.trim_start().to_string()));
        assert_eq!(parts.join(" "), cmd);
    }

    /// P5's colour is definitional (never signalled), so the line must state it
    /// whether the profile labels with a compat minor ("5.0", from a container
    /// dvcC) or bare ("5", a raw elementary stream with no dvcC).
    #[test]
    fn p5_states_definitional_colour_for_both_label_shapes() {
        for label in ["5.0", "5"] {
            assert_eq!(
                build_color_line(&range_only(), Some(label), true),
                "IPT-PQ-c2 · BT.2020 · PQ (SMPTE ST 2084) · full",
                "profile label {label}"
            );
        }
    }

    /// A metadata-only sidecar has no base layer, so the profile-defined colour
    /// inferences (P5 IPT-PQ-c2, P4 Rec.709 SDR) must never fire for one.
    #[test]
    fn sidecar_gets_no_profile_defined_colour() {
        assert_eq!(build_color_line(&ColorInfo::default(), Some("5"), false), "");
        assert_eq!(build_color_line(&ColorInfo::default(), Some("4.2"), false), "");
    }

    /// Profile 20 signals real CICP in its colr box; the matrix name is prepended
    /// to the signalled values rather than substituted for them.
    #[test]
    fn p20_keeps_signalled_cicp() {
        let cc = ColorInfo {
            primaries: Some("BT.2020".to_string()),
            transfer: Some("PQ (SMPTE ST 2084)".to_string()),
            matrix: Some("IPT-PQ-c2".to_string()),
            range: Some("full".to_string()),
        };
        assert_eq!(
            build_color_line(&cc, Some("20.0"), true),
            "IPT-PQ-c2 · BT.2020 · PQ (SMPTE ST 2084) · full"
        );
    }

    /// A P4 mux with no signalled colour description states the profile-defined
    /// Rec.709 SDR base, collapsed to one label plus the default limited range.
    #[test]
    fn p4_fills_absent_colour_description() {
        assert_eq!(
            build_color_line(&ColorInfo::default(), Some("4.2 (FEL)"), true),
            "BT.709 · limited"
        );
    }

    fn cc(primaries: Option<&str>, transfer: Option<&str>, matrix: Option<&str>) -> ColorInfo {
        ColorInfo {
            primaries: primaries.map(str::to_string),
            transfer: transfer.map(str::to_string),
            matrix: matrix.map(str::to_string),
            range: Some("limited".to_string()),
        }
    }

    /// A fully explicit CICP picks the Profile 10 compat digit by elimination:
    /// PQ over a real BT.2020 matrix can only be an HDR10 base (1), HLG an HLG
    /// base (4), an SDR gamma transfer an SDR base (2), and the IPT-PQ-c2
    /// matrix Dolby's own colour system (0).
    #[test]
    fn p10_compat_from_explicit_cicp() {
        let pq = cc(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("BT.2020 NCL"));
        assert_eq!(infer_p10_compat(&pq), Some(1));
        let hlg = cc(Some("BT.2020"), Some("HLG (ARIB STD-B67)"), Some("BT.2020 NCL"));
        assert_eq!(infer_p10_compat(&hlg), Some(4));
        let sdr = cc(Some("BT.709"), Some("BT.709"), None);
        assert_eq!(infer_p10_compat(&sdr), Some(2));
        let ipt = cc(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("IPT-PQ-c2"));
        assert_eq!(infer_p10_compat(&ipt), Some(0));
    }

    /// PQ without an explicit matrix is *not* airtight — an IPT (10.0) base is
    /// itself PQ-encoded and conventionally leaves CICP unspecified — and an
    /// empty colour block (a mux signalling nothing, or a sidecar) infers
    /// nothing at all.
    #[test]
    fn p10_compat_declines_ambiguous_cicp() {
        let pq_no_matrix = cc(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), None);
        assert_eq!(infer_p10_compat(&pq_no_matrix), None);
        let pq_no_primaries = cc(None, Some("PQ (SMPTE ST 2084)"), Some("BT.2020 NCL"));
        assert_eq!(infer_p10_compat(&pq_no_primaries), None);
        assert_eq!(infer_p10_compat(&ColorInfo::default()), None);
    }

    fn opts(color: bool, file_index: usize, file_count: usize) -> RenderOpts {
        RenderOpts {
            color,
            theme: Theme::Paper,
            wrap_width: None,
            file_index,
            file_count,
            show_general: true,
            show_hdr: true,
            show_dv: true,
            show_hdr10plus: true,
            show_sl_hdr: true,
            show_hdr_vivid: true,
        }
    }

    /// The header's `[k/N]` counter renders only for multi-file runs; a
    /// single-file report keeps its exact historical header shape.
    #[test]
    fn header_counter_multi_file_only() {
        let c = Colorizer { on: false, palette: Theme::Paper.palette(), wrap: None, indent: 0, word_wrap: false };
        let single = report_header("m/movie.mkv", 1024, false, &opts(false, 1, 1), &c);
        assert_eq!(single, "movie.mkv  (1.00 KiB)\n");
        let multi = report_header("m/movie.mkv", 1024, false, &opts(false, 2, 7), &c);
        assert_eq!(multi, "movie.mkv  (1.00 KiB)  [2/7]\n");
    }

    /// A truncated stdin probe marks the header size with a trailing `+`
    /// (bytes probed, more behind them); complete inputs keep the exact
    /// historical shape.
    #[test]
    fn header_marks_truncated_stdin_size() {
        let c = Colorizer { on: false, palette: Theme::Paper.palette(), wrap: None, indent: 0, word_wrap: false };
        let truncated = report_header("-", 16 << 20, true, &opts(false, 1, 1), &c);
        assert_eq!(truncated, "-  (16.00 MiB+)\n");
    }

    /// A plain line that fits the width comes back byte-identical in one
    /// piece — reflow never rewrites what it doesn't have to.
    #[test]
    fn wrap_line_noop_when_it_fits() {
        let line = "  Video             HEVC (Main 10) · 3840×2160 · 23.976 fps";
        assert_eq!(wrap_line(line, 80, VALUE_COL, false), vec![line.to_string()]);
    }

    /// An overlong plain value breaks after a ` · ` separator (the trailing
    /// dot signals continuation), the separator's space is consumed, and the
    /// continuation indents to the value column.
    #[test]
    fn wrap_line_breaks_at_separators_with_hanging_indent() {
        let line = "  Video             HEVC (Multiview Main 10, High tier @ L5) · 3840×2160 · 24.000 fps · 10-bit 4:2:0 · Stereoscopic 3D (2 views)";
        let wrapped = wrap_line(line, 64, VALUE_COL, false);
        assert_eq!(
            wrapped,
            vec![
                "  Video             HEVC (Multiview Main 10, High tier @ L5) ·".to_string(),
                format!("{}3840×2160 · 24.000 fps · 10-bit 4:2:0 ·", " ".repeat(VALUE_COL)),
                format!("{}Stereoscopic 3D (2 views)", " ".repeat(VALUE_COL)),
            ]
        );
        for l in &wrapped {
            assert!(l.chars().count() <= 64, "line over width: {l:?}");
        }
    }

    /// A break inside a styled span closes the span at the line end and
    /// re-opens the same style on the continuation, so colour never bleeds
    /// across the wrap and never drops mid-value.
    #[test]
    fn wrap_line_reopens_style_across_the_break() {
        let c = Colorizer { on: true, palette: Theme::Green.palette(), wrap: None, indent: 0, word_wrap: false };
        let value = c.bright("HEVC (Multiview Main 10, High tier @ L5) · 3840×2160 · 24.000 fps");
        let line = format!("  {}{}  {}", c.label("Video"), " ".repeat(LABEL_W - 5), value);
        let wrapped = wrap_line(&line, 64, VALUE_COL, false);
        assert_eq!(wrapped.len(), 2);
        let bright = format!("\x1b[{}m", Theme::Green.palette().bright);
        // Every line's styling is self-contained: opens re-emitted, reset last.
        assert!(wrapped[0].ends_with("\x1b[0m"));
        assert!(wrapped[1].starts_with(&format!("{}{}", " ".repeat(VALUE_COL), bright)));
        assert!(wrapped[1].ends_with("\x1b[0m"));
        // Visible text survives the round trip exactly: the continuation
        // indent stands in for the one separator space the break consumed.
        let visible: String = parse_cells(&wrapped.join("")).iter().map(|c| c.ch).collect();
        let flat: String = parse_cells(&line).iter().map(|c| c.ch).collect();
        assert_eq!(visible.replace(&" ".repeat(VALUE_COL), " "), flat);
    }

    /// The unstyled double space before a warning chip is a break point: the
    /// chip moves whole to the continuation line, never split internally
    /// (its interior spaces are styled, so they are not candidates).
    #[test]
    fn wrap_line_moves_warning_chips_whole() {
        let c = Colorizer { on: true, palette: Theme::Green.palette(), wrap: None, indent: 0, word_wrap: false };
        let line = format!(
            "  {}{}  {}{}  {}",
            c.label("Mastering"),
            " ".repeat(LABEL_W - 9),
            c.value("BT.2020"),
            c.value(" · max 4000  min 0.0001 cd/m²"),
            c.warn("FEL brightness expansion")
        );
        let wrapped = wrap_line(&line, 60, VALUE_COL, false);
        assert_eq!(wrapped.len(), 2);
        let cells = parse_cells(&wrapped[1]);
        let chip: String = cells.iter().map(|c| c.ch).collect();
        // The chip's own inverse-video lead pad survives the break: the first
        // cell past the indent is a *styled* space, part of the chip.
        assert_eq!(chip, format!("{} FEL BRIGHTNESS EXPANSION ", " ".repeat(VALUE_COL)));
        let lead = &cells[VALUE_COL];
        assert!(lead.ch == ' ' && !lead.style.is_empty());
    }

    /// The rendered report reflows only when `wrap_width` is set: the same
    /// report with `None` is untouched, so pipes and files never wrap.
    #[test]
    fn report_wraps_only_with_a_width() {
        let r = Report {
            hdrprobe_schema_version: crate::model::SCHEMA_VERSION,
            file: "movie.mp4".to_string(),
            size_bytes: 0,
            input_truncated: false,
            container: "MP4 (ISOBMFF)".to_string(),
            bd_iso: None,
            format_version: None,
            duration_secs: None,
            video_tracks: vec![VideoTrack {
                track_number: None,
                program: None,
                default: None,
                codec: "HEVC".to_string(),
                codec_profile: Some("Multiview Main 10, High tier @ L5".to_string()),
                width: Some(3840),
                height: Some(2160),
                fps: Some(24.0),
                bitrate: None,
                bit_depth: Some(10),
                chroma: Some("4:2:0".to_string()),
                stereo: Some("Stereoscopic 3D (2 views)".to_string()),
                color: ColorInfo::default(),
                hdr: None,
                dolby_vision: None,
                hdr10plus: None,
                sl_hdr: None,
                hdr_vivid: None,
            }],
            elapsed_ms: 0.0,
        };
        let plain = render(&r, &opts(false, 1, 1));
        let video = plain.lines().find(|l| l.trim_start().starts_with("Video")).unwrap();
        assert!(video.chars().count() > 64);
        let mut o = opts(false, 1, 1);
        o.wrap_width = Some(64);
        let wrapped = render(&r, &o);
        assert!(wrapped.lines().all(|l| l.chars().count() <= 64));
        // Below the useful floor, reflow bows out entirely.
        o.wrap_width = Some(30);
        assert_eq!(render(&r, &o), plain);
    }

    /// A bare video track for report-shape tests.
    fn test_track(codec: &str, w: u32, default: Option<bool>) -> VideoTrack {
        VideoTrack {
            track_number: None,
            program: None,
            default,
            codec: codec.to_string(),
            codec_profile: None,
            width: Some(w),
            height: Some(w * 9 / 16),
            fps: None,
            bitrate: None,
            bit_depth: None,
            chroma: None,
            stereo: None,
            color: ColorInfo::default(),
            hdr: Some(crate::model::Hdr {
                format: "SDR".to_string(),
                mastering: None,
                content_light: None,
            }),
            dolby_vision: None,
            hdr10plus: None,
            sl_hdr: None,
            hdr_vivid: None,
        }
    }

    fn test_report(tracks: Vec<VideoTrack>) -> Report {
        Report {
            hdrprobe_schema_version: crate::model::SCHEMA_VERSION,
            file: "show.mkv".to_string(),
            size_bytes: 0,
            input_truncated: false,
            container: "Matroska".to_string(),
            bd_iso: None,
            format_version: None,
            duration_secs: Some(60.0),
            video_tracks: tracks,
            elapsed_ms: 0.0,
        }
    }

    /// Multi-track reports render one rule-titled group per track with the
    /// sections repeated inside; single-track reports keep the historical
    /// layout with no track rule at all.
    #[test]
    fn multi_track_report_renders_per_track_groups() {
        let single = render(&test_report(vec![test_track("HEVC", 3840, None)]), &opts(false, 1, 1));
        assert!(!single.contains("Track 1"), "single-track output must not grow a track rule");
        assert_eq!(single.matches("HDR").count(), 1);

        let two = test_report(vec![
            test_track("HEVC", 3840, Some(true)),
            test_track("HEVC", 1920, Some(false)),
        ]);
        let s = render(&two, &opts(false, 1, 1));
        // Plain mode uses bare section names, so the track titles appear bare
        // — at column 0, like the file-level General.
        assert!(s.contains("\nTrack 1 · Default\n"), "default-flagged track is tagged:\n{s}");
        assert!(s.contains("\nTrack 2\n"));
        assert!(!s.contains("Track 2 · Default"));
        assert_eq!(s.matches("Format").count(), 2, "each track repeats its sections");
        // File-level facts print once, in the General section.
        assert_eq!(s.matches("Container").count(), 1);
        assert_eq!(s.matches("Duration").count(), 1);
        // Video rows are per track.
        assert!(s.contains("3840×2160") && s.contains("1920×1080"));
        // Everything inside a track group indents by TRACK_INDENT: section
        // names shift off column 0 and rows deepen past the 2-space gutter,
        // so group membership reads at a glance. File-level rows keep the
        // base gutter, and the single-track layout is untouched.
        assert!(s.contains("\n  HDR\n"), "track section names indent:\n{s}");
        assert!(!s.contains("\nHDR\n"));
        assert!(s.contains("\n    Video "), "track rows deepen:\n{s}");
        assert!(s.contains("\n  Container "), "file-level rows keep the base gutter:\n{s}");
        assert!(single.contains("\nHDR\n") && single.contains("\n  Video "));
    }

    /// A multi-program track title carries the program instead of a default tag.
    #[test]
    fn track_title_carries_program_number() {
        let mut t = test_track("HEVC", 1920, None);
        t.program = Some(28);
        let r = test_report(vec![test_track("HEVC", 3840, None), t]);
        let s = render(&r, &opts(false, 1, 1));
        assert!(s.contains("Track 2 · Program 28"), "{s}");
    }

    /// Quiet output: one line per track, tagged only when there are several.
    #[test]
    fn quiet_lines_per_track() {
        let one = render_quiet(&test_report(vec![test_track("HEVC", 3840, None)]));
        assert_eq!(one, "show.mkv  SDR · 3840×2160");
        let two = render_quiet(&test_report(vec![
            test_track("HEVC", 3840, None),
            test_track("HEVC", 1920, None),
        ]));
        assert_eq!(two, "show.mkv [1/2]  SDR · 3840×2160\nshow.mkv [2/2]  SDR · 1920×1080");
    }

    /// The between-reports divider is a full-width heavy rule (`━`, distinct
    /// from the section rules' light `─` but the same columns), bare when
    /// plain and bright-wrapped when colored, followed by one blank line
    /// before the next report's header. With no probed width (pipes, files)
    /// it keeps its exact historical RULE_W columns.
    #[test]
    fn divider_matches_section_rule_width() {
        let rule = "━".repeat(RULE_W);
        assert_eq!(render_divider(&opts(false, 2, 7)), format!("{rule}\n\n"));
        let colored = render_divider(&opts(true, 2, 7));
        assert!(colored.contains(&rule));
        assert!(colored.starts_with('\x1b') && colored.ends_with("\n\n"));
    }

    /// On a live terminal the rules follow the probed width — wider and
    /// narrower than RULE_W both — while `None` keeps the fallback. Unlike
    /// value reflow there is no MIN_WRAP_WIDTH floor: a narrow window gets a
    /// shrunk rule, not a hard-wrapped 64-column one.
    #[test]
    fn rules_follow_probed_terminal_width() {
        for w in [30, 100] {
            let mut o = opts(false, 2, 7);
            o.wrap_width = Some(w);
            assert_eq!(render_divider(&o), format!("{}\n\n", "━".repeat(w)));
            let c = Colorizer {
                on: true,
                palette: Theme::Green.palette(),
                wrap: Some(w),
                indent: 0,
                word_wrap: false,
            };
            let rule = c.section("General");
            assert_eq!(parse_cells(&rule).len(), w, "section rule not {w} columns");
        }
        // No probe: the section rule keeps the RULE_W fallback.
        let c = Colorizer { on: true, palette: Theme::Green.palette(), wrap: None, indent: 0, word_wrap: false };
        let rule = c.section("General");
        assert_eq!(parse_cells(&rule).len(), RULE_W);
    }

    /// A track-body section rule shifts right by the group indent, leads
    /// with the `└─` branch marking it a child of the track rule, and its
    /// fill shortens to match, so the right edge stays flush with the
    /// full-width track rule above it — probed width and fallback alike.
    /// Base-geometry rules keep the plain `──` lead.
    #[test]
    fn indented_section_rule_stays_flush_right() {
        for wrap in [Some(100), None] {
            let c =
                Colorizer {
                    on: true,
                    palette: Theme::Green.palette(),
                    wrap,
                    indent: TRACK_INDENT,
                    word_wrap: false,
                };
            let rule = c.section("HDR");
            let cells = parse_cells(&rule);
            assert_eq!(cells.len(), wrap.unwrap_or(RULE_W), "right edge not flush");
            let visible: String = cells.iter().map(|c| c.ch).collect();
            assert!(visible.starts_with("  └─ HDR "), "rule not a branch: {visible:?}");
        }
        let c = Colorizer { on: true, palette: Theme::Green.palette(), wrap: None, indent: 0, word_wrap: false };
        let rule = c.section("General");
        let visible: String = parse_cells(&rule).iter().map(|c| c.ch).collect();
        assert!(visible.starts_with("── GENERAL "), "base rule grew a branch: {visible:?}");
    }
}
