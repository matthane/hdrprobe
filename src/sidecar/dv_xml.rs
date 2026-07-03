//! Dolby Vision CM (v2.9 / v4.0) metadata XML. libdovi's `CmXmlParser` reads it
//! into a `GenerateConfig` — a base RPU plus a list of per-shot metadata (each
//! shot being a run of frames that share trims, with optional per-frame edits).
//!
//! libdovi's own `generate_rpu_list` expands that to one RPU *per frame* (a full
//! clone each), which for a feature-length title is hundreds of thousands of
//! allocations — and we'd then aggregate every one, only to collapse them back to
//! title-stable levels. Since the metadata is per *shot*, we instead build one
//! representative RPU per shot (base + shot blocks), plus one per frame edit, and
//! fold each into the aggregator weighted by its frame count. The census is
//! identical to the per-frame expansion (`replace_metadata_block` only ever
//! adds/replaces a level, never removes one, so an edited frame's level set is a
//! superset of its shot's; L5/trim are order-independent sets; L6/L9/L11/L254 are
//! title-constant) at a fraction of the work.
//!
//! `CmXmlParser::new` parses via `roxmltree` with an internal `.unwrap()`, so
//! malformed XML *panics*; it and every RPU build therefore run inside the
//! `catch_unwind` guard, turning any parser failure into a clean error.

use anyhow::{bail, Result};
use std::collections::BTreeSet;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::generate::GenerateProfile;
use dolby_vision::xml::{CmXmlParser, XmlParserOpts};

use crate::dv::levels::DvAggregate;
use crate::dv::rpu::guard;
use crate::model::MasteringDisplay;

use super::{finalize_dv, Payload, ASSUMED_CANVAS};

/// The Level 0 (global) frame rate and mastering display that libdovi's parser
/// drops — it never reads `<EditRate>`, and folds the mastering display into a
/// lossy PQ code. Both are read straight from the raw XML (exact, and cheap: the
/// elements sit in the file head), independent of the libdovi aggregation.
pub struct GlobalMeta {
    pub fps: Option<f64>,
    pub mastering: Option<MasteringDisplay>,
}

/// Text of the first `<tag>…</tag>` at or after `from`, trimmed.
fn tag_text<'a>(xml: &'a str, tag: &str, from: usize) -> Option<&'a str> {
    let s = xml[from..].find(&format!("<{tag}>"))? + from + tag.len() + 2;
    let e = xml[s..].find(&format!("</{tag}>"))? + s;
    Some(xml[s..e].trim())
}

/// Frame rate from `<EditRate>`: "num den" (or "num,den") is a rational, a lone
/// value is the rate itself. The 2.0.5 (CM v2.9) schema instead nests a
/// `<Rate><n>num</n><d>den</d></Rate>` element. `None` if absent or unparseable.
fn parse_frame_rate(xml: &str) -> Option<f64> {
    if let Some(inner) = tag_text(xml, "EditRate", 0) {
        let nums: Vec<f64> = inner
            .split([' ', ',', '\t', '\n'])
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        match nums.as_slice() {
            [num, den] if *den != 0.0 => return Some(num / den),
            [rate] if *rate > 0.0 => return Some(*rate),
            _ => {}
        }
    }
    let r = xml.find("<Rate>")?;
    let n = tag_text(xml, "n", r)?.parse::<f64>().ok()?;
    let d = tag_text(xml, "d", r)?.parse::<f64>().ok()?;
    (n > 0.0 && d > 0.0).then_some(n / d)
}

/// Mastering-display luminance (nits) and gamut from the global
/// `<MasteringDisplay>` block. Reads are scoped to that element's own text so
/// the values can't come from a `ColorEncoding` (before it) or `TargetDisplay`
/// (after it). The gamut comes from the explicit `<Red>x y</Red>`-style
/// chromaticities, named through the shared gamut matcher — this is the only
/// mastering-primaries carrier a CM v2.9 title has (no L9), and it's exact for
/// v4.0 too. Unmatched coordinates yield no name, never a guess.
fn parse_mastering(xml: &str) -> Option<MasteringDisplay> {
    let md = mastering_element(xml)?;
    let max = tag_text(md, "PeakBrightness", 0)?.parse::<f64>().ok()?;
    let min = tag_text(md, "MinimumBrightness", 0)?.parse::<f64>().ok()?;
    let primaries = parse_primaries(md).map(str::to_string);
    // The DV XML's mastering display is Level-0 global data, so a recognized
    // gamut is tagged L0 (vs an RPU-carried L9).
    let primaries_level = primaries.is_some().then_some(0);
    Some(MasteringDisplay { max_luminance: max, min_luminance: min, primaries, primaries_level })
}

/// Body of the first `<MasteringDisplay>` element. The opening tag is bare in
/// the 4.0.2+ schemas but carries a `level="0"` attribute in the 2.0.5 (CM
/// v2.9) schema, so match the tag name and skip to the end of the opening tag.
fn mastering_element(xml: &str) -> Option<&str> {
    let mut from = 0;
    loop {
        let i = xml[from..].find("<MasteringDisplay")? + from;
        let rest = &xml[i + "<MasteringDisplay".len()..];
        // Reject a longer tag name that merely shares the prefix.
        if !matches!(rest.as_bytes().first(), Some(b'>' | b' ' | b'\t' | b'\r' | b'\n')) {
            from = i + 1;
            continue;
        }
        let body = &rest[rest.find('>')? + 1..];
        let end = body.find("</MasteringDisplay>").unwrap_or(body.len());
        return Some(&body[..end]);
    }
}

/// The R/G/B/white-point chromaticity pairs of a `<MasteringDisplay>` element's
/// `<Primaries>`, matched to a gamut name. Coordinates are separated by a space
/// (4.0.2 schema) or a comma (2.0.5). `None` when any coordinate is absent or
/// the set matches no known mastering gamut.
fn parse_primaries(md: &str) -> Option<&'static str> {
    let xy = |tag: &str| -> Option<(f64, f64)> {
        let mut it = tag_text(md, tag, 0)?
            .split([' ', ',', '\t', '\n'])
            .filter(|s| !s.is_empty())
            .filter_map(|v| v.parse::<f64>().ok());
        Some((it.next()?, it.next()?))
    };
    crate::hdr::primaries_label(xy("Red")?, xy("Green")?, xy("Blue")?, xy("WhitePoint")?)
}

pub fn parse(data: &[u8]) -> Result<(Payload, GlobalMeta)> {
    let xml = String::from_utf8_lossy(data).into_owned();
    let meta = GlobalMeta { fps: parse_frame_rate(&xml), mastering: parse_mastering(&xml) };

    let agg: Option<DvAggregate> = guard(|| {
        let opts = XmlParserOpts {
            canvas_width: Some(ASSUMED_CANVAS.0 as u16),
            canvas_height: Some(ASSUMED_CANVAS.1 as u16),
        };
        let parser = CmXmlParser::new(xml, opts).ok()?;
        let cfg = &parser.config;

        // The base RPU built exactly as `generate_rpu_list` would, once.
        let base = match cfg.profile {
            GenerateProfile::Profile5 => DoviRpu::profile5_config(cfg),
            GenerateProfile::Profile81 => DoviRpu::profile81_config(cfg),
            GenerateProfile::Profile84 => DoviRpu::profile84_config(cfg),
        }
        .ok()?;

        let mut agg = DvAggregate::default();
        // The XML declares its profile, which fixes the BL compatibility id — so
        // the label's minor digit is real, not the P8/P4 convention default.
        agg.set_compat_id(match cfg.profile {
            GenerateProfile::Profile5 => 0,
            GenerateProfile::Profile81 => 1,
            GenerateProfile::Profile84 => 4,
        });

        for shot in &cfg.shots {
            if shot.duration == 0 {
                continue;
            }

            // Representative RPU for the shot: base + the shot's metadata blocks.
            let mut shot_rpu = base.clone();
            let dm = shot_rpu.vdr_dm_data.as_mut()?;
            for block in &shot.metadata_blocks {
                dm.replace_metadata_block(block.clone()).ok()?;
            }

            // Per-frame edits override single frames. Match `generate_rpu_list`'s
            // first-edit-wins-per-offset semantics, but only for the distinct
            // in-range offsets rather than walking every frame.
            let offsets: BTreeSet<usize> = shot
                .frame_edits
                .iter()
                .map(|e| e.edit_offset)
                .filter(|&o| o < shot.duration)
                .collect();

            for &off in &offsets {
                let edit = shot
                    .frame_edits
                    .iter()
                    .find(|e| e.edit_offset == off)
                    .expect("offset came from frame_edits");
                let mut edit_rpu = shot_rpu.clone();
                let edm = edit_rpu.vdr_dm_data.as_mut()?;
                for block in &edit.metadata_blocks {
                    edm.replace_metadata_block(block.clone()).ok()?;
                }
                // A scene cut only ever lands on frame 0 (or every frame in
                // long-play mode).
                let scene_cuts = usize::from(off == 0 || cfg.long_play_mode);
                agg.add_repeated(&edit_rpu, 1, scene_cuts);
            }

            // The plain frames are those not carrying an edit.
            let plain = shot.duration - offsets.len();
            let plain_scene_cuts = if cfg.long_play_mode {
                plain
            } else {
                usize::from(!offsets.contains(&0))
            };
            agg.add_repeated(&shot_rpu, plain, plain_scene_cuts);
        }

        Some(agg)
    });

    match agg {
        Some(agg) => {
            let mut payload = finalize_dv(agg, Some(ASSUMED_CANVAS))?;
            // Prefer the exact XML luminance over the aggregate's PQ-derived one
            // (CmXmlParser folds this display into lossy 12-bit codes), but keep
            // the derived value when the XML block is absent. For the gamut,
            // prefer the aggregate's L9 name (what an RPU generated from this
            // XML would carry; it derives from these same chromaticities), and
            // fall back to the XML's own Level-0 primaries — the only carrier a
            // CM v2.9 XML has.
            if let Payload::DolbyVision(dv) = &mut payload {
                if let Some(xml_md) = &meta.mastering {
                    let mut md = xml_md.clone();
                    if let Some(agg_md) = dv.mastering_display.take() {
                        if agg_md.primaries.is_some() {
                            md.primaries = agg_md.primaries;
                            md.primaries_level = agg_md.primaries_level;
                        }
                    }
                    dv.mastering_display = Some(md);
                }
            }
            Ok((payload, meta))
        }
        None => bail!("failed to parse Dolby Vision XML"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mastering_display_reads_luminance_and_named_primaries() {
        // The mastering display's own values must win over the ColorEncoding
        // block before it (a 10000-nit PQ signal with the same P3 primaries)
        // and the TargetDisplay after it (100-nit BT.709). Layout mirrors a
        // real Resolve 4.0.2 export.
        let xml = r#"<Track><ColorEncoding>
            <Primaries><Red>0.68 0.32</Red><Green>0.265 0.69</Green><Blue>0.15 0.06</Blue></Primaries>
            <WhitePoint>0.3127 0.329</WhitePoint>
            <PeakBrightness>10000</PeakBrightness><MinimumBrightness>0</MinimumBrightness>
          </ColorEncoding>
          <DVGlobalData level="0"><MasteringDisplay>
            <ID>20</ID>
            <Primaries><Red>0.68 0.32</Red><Green>0.265 0.69</Green><Blue>0.15 0.06</Blue></Primaries>
            <WhitePoint>0.3127 0.329</WhitePoint>
            <PeakBrightness>1000</PeakBrightness><MinimumBrightness>0.0001</MinimumBrightness>
          </MasteringDisplay>
          <TargetDisplay>
            <Primaries><Red>0.64 0.33</Red><Green>0.3 0.6</Green><Blue>0.15 0.06</Blue></Primaries>
            <WhitePoint>0.3127 0.329</WhitePoint>
            <PeakBrightness>100</PeakBrightness><MinimumBrightness>0.005</MinimumBrightness>
          </TargetDisplay></DVGlobalData></Track>"#;
        let md = parse_mastering(xml).expect("mastering display parses");
        assert_eq!(md.max_luminance, 1000.0);
        assert_eq!(md.min_luminance, 0.0001);
        // The Level-0 gamut carrier — the only one a CM v2.9 XML has.
        assert_eq!(md.primaries.as_deref(), Some("DCI-P3 D65"));
        assert_eq!(md.primaries_level, Some(0));
    }

    #[test]
    fn mastering_display_parses_the_205_schema_form() {
        // The 2.0.5 (CM v2.9) schema writes an attributed opening tag and
        // comma-separated coordinates (dovi_meta output verbatim).
        let xml = r#"<MasteringDisplay level="0">
            <ID>20</ID>
            <Name>1000-nits, P3, D65, ST.2084, Full</Name>
            <Primaries><Red>0.68,0.32</Red><Green>0.265,0.69</Green><Blue>0.15,0.06</Blue></Primaries>
            <WhitePoint>0.3127,0.329</WhitePoint>
            <PeakBrightness>1000</PeakBrightness>
            <MinimumBrightness>0.0001</MinimumBrightness>
          </MasteringDisplay>"#;
        let md = parse_mastering(xml).expect("2.0.5 mastering display parses");
        assert_eq!(md.max_luminance, 1000.0);
        assert_eq!(md.primaries.as_deref(), Some("DCI-P3 D65"));
        assert_eq!(md.primaries_level, Some(0));
    }

    #[test]
    fn mastering_display_without_primaries_stays_luminance_only() {
        // No <Primaries> inside the element: the TargetDisplay's BT.709 set
        // further down must not leak in — luminance parses, gamut stays absent.
        let xml = r#"<MasteringDisplay>
            <PeakBrightness>4000</PeakBrightness><MinimumBrightness>0.005</MinimumBrightness>
          </MasteringDisplay>
          <TargetDisplay>
            <Primaries><Red>0.64 0.33</Red><Green>0.3 0.6</Green><Blue>0.15 0.06</Blue></Primaries>
            <WhitePoint>0.3127 0.329</WhitePoint>
            <PeakBrightness>100</PeakBrightness><MinimumBrightness>0.005</MinimumBrightness>
          </TargetDisplay>"#;
        let md = parse_mastering(xml).expect("mastering display parses");
        assert_eq!(md.max_luminance, 4000.0);
        assert_eq!(md.primaries, None);
        assert_eq!(md.primaries_level, None);
    }

    #[test]
    fn frame_rate_reads_both_schema_forms() {
        // 4.0.2+: a rational (or lone value) inside <EditRate>.
        assert_eq!(parse_frame_rate("<EditRate>24000 1001</EditRate>"), Some(24000.0 / 1001.0));
        assert_eq!(parse_frame_rate("<EditRate>25</EditRate>"), Some(25.0));
        // 2.0.5: <Rate> with nested numerator/denominator (dovi_meta output).
        assert_eq!(
            parse_frame_rate("<Rate>\n<n>24000</n>\n<d>1001</d>\n</Rate>"),
            Some(24000.0 / 1001.0)
        );
        assert_eq!(parse_frame_rate("<Rate><n>24</n><d>0</d></Rate>"), None);
    }

    #[test]
    fn off_gamut_mastering_primaries_are_never_guessed() {
        let xml = r#"<MasteringDisplay>
            <Primaries><Red>0.7 0.3</Red><Green>0.2 0.7</Green><Blue>0.14 0.05</Blue></Primaries>
            <WhitePoint>0.30 0.32</WhitePoint>
            <PeakBrightness>1000</PeakBrightness><MinimumBrightness>0.0001</MinimumBrightness>
          </MasteringDisplay>"#;
        let md = parse_mastering(xml).expect("mastering display parses");
        assert_eq!(md.primaries, None);
    }
}
