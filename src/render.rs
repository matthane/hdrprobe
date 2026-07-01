//! Sectioned, aligned terminal rendering of a `Report`.

use std::fmt::Write;

use crate::model::{BitrateScope, Report};

pub struct RenderOpts {
    pub color: bool,
    pub show_general: bool,
    pub show_hdr: bool,
    pub show_dv: bool,
    pub show_hdr10plus: bool,
    pub show_timing: bool,
}

const LABEL_W: usize = 15;

pub fn render(r: &Report, o: &RenderOpts) -> String {
    let mut s = String::new();
    let c = Colorizer { on: o.color };

    let _ = writeln!(s, "{}  ({})", c.bold(&r.file), human_size(r.size_bytes));
    s.push('\n');

    if o.show_general {
        let _ = writeln!(s, "{}", c.header("General"));
        kv(&mut s, &c, "Container", &r.general.container);
        if let Some(d) = r.general.duration_secs {
            kv(&mut s, &c, "Duration", &human_duration(d));
        }
        if let Some(br) = &r.general.bitrate {
            let scope = match br.scope {
                BitrateScope::VideoStream => "video stream",
                BitrateScope::Overall => "overall",
            };
            let tag = c.dim(&format!("({})", scope));
            kv(&mut s, &c, "Bitrate", &format!("{}   {}", human_bitrate(br.bits_per_sec), tag));
        }
        let video = video_line(r);
        if !video.is_empty() {
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
            let _ = writeln!(s, "{}", c.header("HDR"));
            kv(&mut s, &c, "Format", &hdr.format);
            if let Some(m) = &hdr.mastering {
                let prim = m
                    .primaries
                    .as_ref()
                    .map(|p| format!("   {}", c.dim(&format!("[{}]", p))))
                    .unwrap_or_default();
                kv(
                    &mut s,
                    &c,
                    "Mastering",
                    &format!("max {}  min {} cd/m²{}", fmt_num(m.max_luminance), fmt_num(m.min_luminance), prim),
                );
            }
            if let Some(cl) = &hdr.content_light {
                kv(&mut s, &c, "Content light", &format!("MaxCLL {} · MaxFALL {}", cl.max_cll, cl.max_fall));
            }
            s.push('\n');
        }
    }

    if o.show_dv {
        if let Some(dv) = &r.dolby_vision {
            let _ = writeln!(s, "{}", c.header("Dolby Vision"));

            if let Some(structure) = &dv.structure {
                kv(&mut s, &c, "Structure", structure);
            }

            let mut carriage = Vec::new();
            if dv.bl_present {
                carriage.push("BL");
            }
            if dv.el_present {
                carriage.push("EL");
            }
            if dv.rpu_present {
                carriage.push("RPU");
            }
            // Carriage only; BL cross-compatibility (HDR10/SDR/HLG) is already
            // shown as a "(fallback)" tag on the HDR format line, so it's not
            // repeated here.
            let mut prof = c.value(&dv.profile).to_string();
            let _ = write!(prof, "   ({})", carriage.join("+"));
            kv(&mut s, &c, "Profile", &prof);

            if let Some(l) = dv.level {
                let envelope = match dv_level_tier_mbps(l) {
                    Some((main, high)) => format!(
                        "{}   {}",
                        l,
                        c.dim(&format!("(max bit rate: {main} Mbps Main tier / {high} Mbps High tier)"))
                    ),
                    None => l.to_string(),
                };
                kv(&mut s, &c, "Level", &envelope);
            }
            if let Some(cm) = &dv.cm_version {
                let elt = dv.el_type.as_ref().map(|e| format!(" · {}", e)).unwrap_or_default();
                // The L254 block is only present in CM v4.0; don't tag v2.9 with it.
                let tag = if cm == "CM v4.0" { format!("   {}", c.dim("[L254]")) } else { String::new() };
                kv(&mut s, &c, "RPU / DM", &format!("present · {}{}{}", cm, elt, tag));
            }
            for area in &dv.l5_active_areas {
                let offsets = format!("L{} R{} T{} B{}", area.left, area.right, area.top, area.bottom);
                let dims = if area.width > 0 && area.height > 0 {
                    format!(
                        "{}×{}  ({})  ·  {}",
                        area.width,
                        area.height,
                        aspect(area.width, area.height),
                        c.dim(&offsets)
                    )
                } else {
                    format!("offsets {}", offsets)
                };
                let tag = match dv.l5_assumed_canvas {
                    Some([w, h]) => format!("[assumes {}×{} canvas]", w, h),
                    None if dv.sampled => "[sampled]".to_string(),
                    None => "[full scan]".to_string(),
                };
                kv(&mut s, &c, "L5 active area", &format!("{}   {}", dims, c.dim(&tag)));
            }
            if let Some(l6) = &dv.l6_fallback {
                let flag = if l6.zeroed { format!("  {}", c.warn("(zeroed!)")) } else { String::new() };
                kv(&mut s, &c, "L6 fallback", &format!("MaxCLL {} · MaxFALL {}{}", l6.max_cll, l6.max_fall, flag));
            }
            if let Some(l9) = &dv.l9_mastering {
                kv(&mut s, &c, "L9 mastering", l9);
            }
            if let Some(l11) = &dv.l11_content {
                let rm = match dv.l11_reference_mode {
                    Some(true) => " · reference mode",
                    _ => "",
                };
                kv(&mut s, &c, "L11 content", &format!("{}{}", l11, rm));
            }
            if !dv.trim_targets_nits.is_empty() {
                let list = dv.trim_targets_nits.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
                kv(&mut s, &c, "Trim targets", &format!("{} nit   {}", list, c.dim("[L2/L8]")));
            }
            if let Some(census) = &dv.census {
                kv(&mut s, &c, "RPU count", &format!("{}   {}", dv.rpu_count, c.dim("[full scan]")));
                kv(&mut s, &c, "Scene cuts", &census.scene_cuts.to_string());
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

    // Only surface HDR10+ when it's present, like the Dolby Vision section.
    if o.show_hdr10plus && r.hdr10plus.present {
        let _ = writeln!(s, "{}", c.header("HDR10+"));
        if let Some(p) = r.hdr10plus.profile {
            kv(&mut s, &c, "Profile", &p.to_string());
        }
        if let Some(v) = r.hdr10plus.application_version {
            kv(&mut s, &c, "Application", &format!("v{}", v));
        }
        if let Some(w) = r.hdr10plus.num_windows {
            kv(&mut s, &c, "Windows", &w.to_string());
        }
        if let Some(n) = r.hdr10plus.target_max_luminance {
            kv(&mut s, &c, "Target", &format!("{} nits", n));
        }
        s.push('\n');
    }

    if o.show_timing {
        let _ = writeln!(s, "{} {}", c.accent("⚡"), c.dim(&format!("{:.0} ms", r.elapsed_ms)));
    }

    s
}

/// One-line summary for `--quiet`.
pub fn render_quiet(r: &Report) -> String {
    let mut parts = Vec::new();
    if let Some(dv) = &r.dolby_vision {
        parts.push(format!("DV {}", dv.profile));
    }
    if let Some(hdr) = &r.hdr {
        parts.push(hdr.format.clone());
    } else if r.hdr10plus.present {
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
    let pad = " ".repeat(LABEL_W.saturating_sub(label.len()));
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
    parts.join(" · ")
}

fn color_line(r: &Report) -> String {
    let cc = &r.general.color;
    let mut parts = Vec::new();

    // Dolby Vision Profile 5 is spec-locked to Dolby's IPT-PQ-c2 colour space over
    // BT.2020 primaries / PQ / full range — that's definitional, not signalled. The
    // colour space can't be expressed in CICP, so the SPS carries "unspecified"
    // (2/2/2) and only the range survives, leaving a bare "full". Any CICP a P5
    // stream did happen to carry would be noise, so state the fixed profile colour.
    let is_p5 = r.dolby_vision.as_ref().is_some_and(|dv| dv.profile == "5");
    if is_p5 {
        parts.push("IPT-PQ-c2".to_string());
        parts.push("BT.2020".to_string());
        parts.push("PQ (SMPTE ST 2084)".to_string());
    } else {
        if let Some(p) = &cc.primaries {
            parts.push(p.clone());
        }
        if let Some(t) = &cc.transfer {
            parts.push(t.clone());
        }
    }
    if let Some(m) = &cc.range {
        parts.push(m.clone());
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

/// Max Main / High tier bit rates (Mbps) per Dolby Vision level, from DV spec Table 4.
/// The envelope makes the bare level number meaningful; resolution/fps already appear
/// in the video section, so only the tier bit-rate caps are surfaced here.
fn dv_level_tier_mbps(level: u8) -> Option<(u16, u16)> {
    Some(match level {
        1..=2 => (20, 50),
        3..=5 => (20, 70),
        6..=7 => (25, 130),
        8..=9 => (40, 130),
        10..=11 => (60, 240),
        12 => (120, 480),
        13 => (240, 800),
        _ => return None,
    })
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
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn header(&self, t: &str) -> String {
        self.wrap("1;36", t)
    }
    fn label(&self, t: &str) -> String {
        self.wrap("0", t)
    }
    fn value(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn warn(&self, t: &str) -> String {
        self.wrap("33", t)
    }
    fn accent(&self, t: &str) -> String {
        self.wrap("33", t)
    }
}
