//! Sectioned, aligned terminal rendering of a `Report`.

use std::fmt::Write;

use crate::model::{BitrateScope, ColorInfo, Report};

pub struct RenderOpts {
    pub color: bool,
    pub show_general: bool,
    pub show_hdr: bool,
    pub show_dv: bool,
    pub show_hdr10plus: bool,
}

const LABEL_W: usize = 16;

pub fn render(r: &Report, o: &RenderOpts) -> String {
    let mut s = String::new();
    let c = Colorizer { on: o.color };
    let mut notes = Footnotes::default();

    // Colored: phosphor banner glyph, faint size. Plain: the classic
    // "name  (size)" shape, unchanged for pipes and logs.
    if c.on {
        let _ = writeln!(
            s,
            "{} {}",
            c.bright(&format!("▮ {}", r.file)),
            c.faint(&format!("· {}", human_size(r.size_bytes)))
        );
    } else {
        let _ = writeln!(s, "{}  ({})", r.file, human_size(r.size_bytes));
    }
    s.push('\n');

    if o.show_general {
        let _ = writeln!(s, "{}", c.section("General"));
        kv(&mut s, &c, "Container", &r.general.container);
        // Sidecar schema version (a DV XML's root `version` attribute); video
        // inputs never carry one, so the line only appears for sidecars.
        if let Some(v) = &r.general.format_version {
            kv(&mut s, &c, "Schema version", v);
        }
        if let Some(d) = r.general.duration_secs {
            kv(&mut s, &c, "Duration", &human_duration(d));
        }
        // Video files show fps in the Video line; a metadata-only sidecar (no
        // codec) has no Video line, so it surfaces its frame rate on its own.
        if r.general.codec.is_empty() {
            if let Some(fps) = r.general.fps {
                kv(&mut s, &c, "Frame rate", &format!("{:.3} fps", fps));
            }
        }
        if let Some(br) = &r.general.bitrate {
            let scope = match br.scope {
                BitrateScope::VideoStream => "video stream",
                BitrateScope::Overall => "overall",
            };
            kv_styled(
                &mut s,
                &c,
                "Bitrate",
                &format!("{}{}", c.value(&human_bitrate(br.bits_per_sec)), c.tag(scope)),
            );
        }
        let video = video_line(r);
        if !video.is_empty() && !r.general.codec.is_empty() {
            kv(&mut s, &c, "Video", &video);
        }
        let color = color_line(r);
        if !color.is_empty() {
            kv(&mut s, &c, "Color", &color);
        }
        s.push('\n');
    }

    if o.show_hdr {
        if let Some(hdr) = &r.hdr {
            let _ = writeln!(s, "{}", c.section("HDR"));
            kv_styled(&mut s, &c, "Format", &c.bright(&hdr.format));
            if let Some(m) = &hdr.mastering {
                // Gamut first, luminance after: "DCI-P3 D65 · max 1000  min 0.0001 cd/m²".
                let prim = m.primaries.as_ref().map(|p| format!("{p} · ")).unwrap_or_default();
                kv(
                    &mut s,
                    &c,
                    "Mastering",
                    &format!("{}max {}  min {} cd/m²", prim, fmt_num(m.max_luminance), fmt_num(m.min_luminance)),
                );
            }
            if let Some(cl) = &hdr.content_light {
                let flag = if cl.zeroed { format!("  {}", c.warn("zeroed")) } else { String::new() };
                let light = c.value(&format!("MaxCLL {} · MaxFALL {}", cl.max_cll, cl.max_fall));
                kv_styled(&mut s, &c, "Content light", &format!("{}{}", light, flag));
            }
            s.push('\n');
        }
    }

    if o.show_dv {
        if let Some(dv) = &r.dolby_vision {
            let _ = writeln!(s, "{}", c.section("Dolby Vision"));

            if let Some(census) = &dv.census {
                // Census stats lead the section (consistent across all input
                // types). This line is census-gated, and the census only exists
                // on a full scan (sidecars are always full; video needs --full),
                // so an RPU count here is never a sample — no "[full scan]" tag.
                kv(&mut s, &c, "RPU count", &dv.rpu_count.to_string());
                kv(&mut s, &c, "Scene cuts", &census.scene_cuts.to_string());
            }
            if let Some(structure) = &dv.structure {
                kv(&mut s, &c, "Structure", structure);
            }

            // The BL/EL/RPU carriage booleans are still collected on the model
            // (for a future backend schema) but omitted from this report: the
            // profile and MEL/FEL tag already convey the layer structure, and
            // the per-track BL flag reads as misleading on dual-track P7.
            let profile = if dv.profile_compat_assumed {
                format!("{}{}", c.bright(&dv.profile), c.tag("compat assumed"))
            } else {
                c.bright(&dv.profile)
            };
            kv_styled(&mut s, &c, "Profile", &profile);

            // The DV level only defines the codec bit-rate envelope; it says
            // nothing useful at a glance, so it's kept on the model but not
            // rendered here.
            if let Some(cm) = &dv.cm_version {
                // Only the content-mapping version: "present" is implied by the
                // section header, and the EL type (MEL/FEL) is already on the
                // Profile line. `cm_version` is stored as "CM v2.9"/"CM v4.0";
                // drop the redundant "CM " since the label spells it out.
                let ver = cm.strip_prefix("CM ").unwrap_or(cm);
                kv(&mut s, &c, "Content mapping", ver);
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
                let lum = c.value(&format!(
                    "max {}  min {} cd/m²",
                    fmt_num(md.max_luminance),
                    fmt_num(md.min_luminance)
                ));
                kv_styled(&mut s, &c, "Mastering", &format!("{}{}{}", prim, lum, expansion));
            }
            if !dv.trim_targets.is_empty() {
                // The target set is a union over the RPUs actually read, and the
                // L8 half is per-shot in real titles (a BD original whose head
                // shots carry only the 100-nit L8 while later scenes add 600),
                // so a sampled union may be incomplete — footnoted like L5. A
                // full scan is complete, so it carries no caveat mark.
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
                kv_styled(&mut s, &c, &format!("Trim targets{mark}"), &list);
            }
            if !dv.l5_active_areas.is_empty() {
                // The set of distinct active areas is shown inline (joined by
                // " + ") rather than one line per area: offsets are the raw L5
                // signal, the active area is derived. The sampled/assumed-canvas
                // caveat describes the whole set, so it rides the label as one
                // footnote mark; a full scan carries no mark.
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
                kv_styled(&mut s, &c, &format!("L5 offsets{mark}"), &format!("{}{}", c.value(&offsets), variable));
                let areas = dv
                    .l5_active_areas
                    .iter()
                    .filter(|a| a.width > 0 && a.height > 0)
                    .map(|a| format!("{}×{}  ({})", a.width, a.height, aspect(a.width, a.height)))
                    .collect::<Vec<_>>()
                    .join(" + ");
                if !areas.is_empty() {
                    kv(&mut s, &c, "L5 active area", &areas);
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
                kv_styled(&mut s, &c, "L6 content light", &format!("{}{}", light, flag));
            }
            // L9 folds into the Mastering line above when recognized; a
            // standalone line remains only when it couldn't ride there (no
            // mastering display in the DM header, or an unmatched custom gamut).
            let l9_on_mastering =
                dv.mastering_display.as_ref().is_some_and(|m| m.primaries.is_some());
            if !l9_on_mastering {
                if let Some(l9) = &dv.l9_mastering {
                    kv(&mut s, &c, "L9 mastering", l9);
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
                kv(&mut s, &c, "L11 APO", &format!("{}{}{}", l11, wp, rm));
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
                    kv(&mut s, &c, "Levels present", &levels);
                }
            }
            s.push('\n');
        }
    }

    // The section exists only when HDR10+ metadata was found, like Dolby Vision.
    if o.show_hdr10plus {
        if let Some(hp) = &r.hdr10plus {
            let _ = writeln!(s, "{}", c.section("HDR10+"));
            if let Some(p) = hp.profile {
                kv(&mut s, &c, "Profile", &p.to_string());
            }
            kv(&mut s, &c, "Application", &format!("v{}", hp.application_version));
            kv(&mut s, &c, "Windows", &hp.num_windows.to_string());
            if let Some(n) = hp.target_max_luminance {
                kv(&mut s, &c, "Target", &format!("{} nits", n));
            }
            s.push('\n');
        }
    }

    // Footnotes collected from marked labels render once at the report's
    // foot, so per-line caveats never clutter the values they qualify. The
    // elapsed time is JSON-only (`elapsed_ms`); the text report doesn't
    // show it.
    for (mark, text) in notes.lines() {
        let _ = writeln!(s, "{}", c.faint(&format!("{mark} {text}")));
    }

    s
}

/// The one caveat most reports carry: the default pipeline reads a spread of
/// RPUs, so RPU-derived sets (trim targets, L5 areas) may be incomplete.
const SAMPLED_NOTE: &str = "sampled from a spread of RPUs; --full reads every one";

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

/// One-line summary for `--quiet`.
pub fn render_quiet(r: &Report) -> String {
    let mut parts = Vec::new();
    if let Some(dv) = &r.dolby_vision {
        parts.push(format!("DV {}", dv.profile));
    }
    if let Some(hdr) = &r.hdr {
        parts.push(hdr.format.clone());
    } else if r.hdr10plus.is_some() {
        parts.push("HDR10+".to_string());
    } else if r.dolby_vision.is_none() {
        parts.push("SDR".to_string());
    }
    if let (Some(w), Some(h)) = (r.general.width, r.general.height) {
        parts.push(format!("{}×{}", w, h));
    }
    format!("{}  {}", r.file, parts.join(" · "))
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
    let _ = writeln!(s, "  {}{}  {}", c.label(label), pad, value);
}

fn video_line(r: &Report) -> String {
    let g = &r.general;
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

fn color_line(r: &Report) -> String {
    // The profile-defined colour inferences inside apply only to video inputs: a
    // metadata-only sidecar (no codec — the same signal that suppresses the Video
    // line) has no base layer whose colour they could describe.
    build_color_line(
        &r.general.color,
        r.dolby_vision.as_ref().map(|dv| dv.profile.as_str()),
        !r.general.codec.is_empty(),
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

/// Phosphor-CRT palette: one green hue at four intensities. Brightness is the
/// visual hierarchy — bright for headline facts (file name, HDR format, DV
/// profile, section names), mid for ordinary values, low for labels, faint
/// for tags and rules — and warnings invert the video, which a single-hue
/// scheme makes unmissable. The layout (banner, footnotes, line structure) is
/// shared between modes; with colour off the intensities vanish, so the
/// tag/provenance/warning helpers fall back to the bracket conventions
/// (`[tag]`, `(warning)`) that carry the same semantics in plain text.
struct Colorizer {
    on: bool,
}

impl Colorizer {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }
    /// Headline values.
    fn bright(&self, t: &str) -> String {
        self.wrap("1;38;2;120;255;160", t)
    }
    /// Ordinary values.
    fn value(&self, t: &str) -> String {
        self.wrap("38;2;80;210;120", t)
    }
    /// Row labels.
    fn label(&self, t: &str) -> String {
        self.wrap("38;2;55;140;85", t)
    }
    /// Faintest level: tags, rules, the timing footer.
    fn faint(&self, t: &str) -> String {
        self.wrap("38;2;38;95;60", t)
    }
    /// A whole-line qualifier tag ([video stream], [compat assumed]) — the
    /// sampling caveats use `Footnotes` instead. Carries its own leading
    /// separator: coloured it hangs off the value as a faint " · tag", plain
    /// it keeps the classic three-space "[tag]".
    fn tag(&self, t: &str) -> String {
        if self.on {
            self.faint(&format!(" · {t}"))
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
            format!("\x1b[7;38;2;120;255;160m {} \x1b[0m", t.to_uppercase())
        } else {
            format!("({t})")
        }
    }
    /// Section header: an uppercase ruled line when coloured, the bare name
    /// when plain.
    fn section(&self, name: &str) -> String {
        if self.on {
            let up = name.to_uppercase();
            let fill = "─".repeat(60usize.saturating_sub(up.chars().count()));
            format!("{} {} {}", self.faint("──"), self.bright(&up), self.faint(&fill))
        } else {
            name.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range_only() -> ColorInfo {
        ColorInfo { range: Some("full".to_string()), ..Default::default() }
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
}
