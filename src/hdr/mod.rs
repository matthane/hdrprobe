//! Static HDR assembly: classify the overall format (SDR / HDR10 / HLG /
//! HDR10+ / Dolby Vision and combinations) and gather mastering-display +
//! content-light info.

pub mod sei;

use crate::container::TrackDemux;
use crate::hdr::sei::SeiFindings;
use crate::model::{DolbyVision, Hdr, MasteringDisplay};

pub fn assemble(demux: &TrackDemux, dv: Option<&DolbyVision>, sei: &SeiFindings) -> Hdr {
    let hdr10plus = sei.hdr10plus.is_some();
    // The HLG alt-transfer SEI (147) overrides the VUI transfer for the purpose
    // of format classification (VUI often signals BT.2020, SEI says HLG/PQ).
    let transfer = demux.color.transfer.as_deref().unwrap_or("");
    let is_pq = transfer.contains("PQ") || sei.preferred_transfer == Some(16);
    let is_hlg = transfer.contains("HLG") || sei.preferred_transfer == Some(18);

    let mut formats: Vec<String> = Vec::new();
    if dv.is_some() {
        formats.push("Dolby Vision".to_string());
    }
    if hdr10plus {
        formats.push("HDR10+".to_string());
    }
    // SL-HDR (ETSI TS 103 433) is dynamic reconstruction metadata over an
    // ordinary base, so like HDR10+ it prepends the base tag: mode 2 rides a
    // directly viewable PQ base ("SL-HDR2 / HDR10"), mode 3 an HLG base, and
    // mode 1 an SDR base the ordinary SDR fallthrough already names.
    if let Some(sl) = &sei.sl_hdr {
        formats.push(format!("SL-HDR{}", sl.mode));
    }
    // HDR Vivid (CUVA, T/UWA 005) likewise: dynamic tone-mapping metadata
    // over an ordinary PQ or HLG base, which keeps its own tag. Either signal
    // suffices — the per-frame T.35 SEI/OBU, or the MP4 `cuvv` container
    // declaration (which still fires when no frame is read, e.g. --no-rpu).
    if sei.hdr_vivid.is_some() || demux.cuvv_version_map.is_some() {
        formats.push("HDR Vivid".to_string());
    }

    // A Dolby Vision title's cross-compatible base is decided by its
    // compatibility id, not by the base layer's raw transfer: the id *is* the
    // declaration of what a non-DV decoder gets. Reading the transfer instead
    // mis-classifies the two cases where the two disagree — a Profile 5 or 20
    // base is PQ-encoded in Dolby's own IPT-PQ-c2 space (id 0: nothing viewable
    // without a DV decoder), and a Profile 4 base is SDR however its container
    // is tagged. Ids: 0 none, 1 HDR10, 2 SDR, 4 HLG, 6 HDR10 per UHD Blu-ray.
    let ccid = dv.and_then(|d| d.bl_compatibility_id);
    let base = match dv {
        Some(_) => match ccid {
            Some(0) => None,
            Some(1) | Some(6) => Some("HDR10 (fallback)"),
            Some(2) => Some("SDR (fallback)"),
            Some(4) => Some("HLG (fallback)"),
            // Unresolved (a Profile 8 whose carriage declares nothing and whose
            // VUI separates nothing) or an id outside the defined set: fall back
            // to whatever the base layer itself signals, which is all there is.
            _ => {
                if is_pq {
                    Some("HDR10 (fallback)")
                } else if is_hlg {
                    Some("HLG (fallback)")
                } else {
                    None
                }
            }
        },
        None if is_pq => Some("HDR10"),
        None if is_hlg => Some("HLG"),
        None => Some("SDR"),
    };
    if let Some(b) = base {
        formats.push(b.to_string());
    }

    let format = formats.join(" / ");

    // L6 is the DV carriage of HDR10 static metadata, and Dolby's
    // profiles/levels spec defines it as meaningful only for the HDR10 base
    // signal — so both L6 fallbacks below apply only there. Every other base has
    // no consumer for it: CCID 0 (P5/P20/AV1 10.0) has no viewable base at all,
    // HLG (8.4/10.4) is scene-referred and consumes no static metadata (corpus
    // 8.4/10.4 titles carry a zeroed L6 placeholder, exactly like P5), and an
    // SDR base likewise signals none. Same verdict the text report's own L6 line
    // uses, from the one shared gate.
    let hdr10_base = dv.is_some_and(|d| {
        crate::dv::ccid::hdr10_base(ccid, crate::dv::levels::profile_major(&d.profile))
    });

    // Prefer container mastering, then the SEI ST.2086 message, then DV L6.
    // This line means the *base layer's own* declared display, so the L6
    // fallback is gated on an HDR10 base, where L6 by spec mirrors the base's
    // MDCV SEI. On any other base the stream declares nothing itself, and
    // L6's mastering half is just the grade's display re-encoded, already
    // shown authoritatively on the DV Mastering line (DM header
    // source_min/max_pq) — falling back would duplicate it as a base-layer
    // fact. A signalled MDCV always shows regardless of base.
    let mastering = demux
        .mastering
        .clone()
        .or_else(|| sei.mastering.clone())
        .or_else(|| {
            hdr10_base.then_some(())?;
            dv.and_then(|d| d.l6.as_ref()).map(|l6| MasteringDisplay {
                max_luminance: l6.max_mastering as f64,
                min_luminance: l6.min_mastering as f64 / 10000.0,
                // The display's own primaries per the DV metadata (L9), not the
                // coded video gamut — a P3-mastered title still carries BT.2020
                // CICP, so tagging with CICP would misstate the display. No L9
                // (CM v2.9) → no tag, never a guess.
                primaries: dv.and_then(|d| d.l9_mastering.clone()),
                primaries_level: dv.and_then(|d| d.l9_mastering.as_ref()).map(|_| 9),
            })
        });

    // MaxCLL/MaxFALL is HDR10 static-metadata convention (CTA-861.3; Dolby's
    // profiles/levels spec names it only in the compat-id-1 base-signal
    // definition), so the L6 fallback applies only on an HDR10 base. No other
    // base consumes CLL, and the L6 of an IPT or HLG title is a zeroed
    // placeholder (corpus P5, 8.4 and 10.4 files and Dolby's own demo alike),
    // so falling back would render noise. A CLL the container/SEI actually
    // signals still shows — observed bytes are always reported.
    let content_light = demux.content_light.or(sei.content_light).or_else(|| {
        hdr10_base.then_some(())?;
        dv.and_then(|d| d.l6.as_ref()).map(|l6| crate::model::ContentLight::new(l6.max_cll, l6.max_fall))
    });

    Hdr { format, mastering, content_light }
}

/// HDR Vivid target codes (12-bit PQ) -> distinct nits, sorted ascending.
/// Routed through the DV standard-target snap so an authored anchor prints
/// its round value (2770/4095 computes to 499.98 and snaps to 500 — the same
/// quantisation-absorbing treatment the DV mastering luminance gets).
pub(crate) fn pq_targets_to_nits(codes: &[u16]) -> Vec<u32> {
    let mut nits: Vec<u32> =
        codes.iter().map(|&c| crate::dv::levels::snap_nits(crate::dv::levels::pq12_to_nits(c))).collect();
    nits.sort_unstable();
    nits.dedup();
    nits
}

/// Match raw CIE 1931 mastering-display chromaticities (R, G, B primaries +
/// white point) against the gamuts mastering displays actually use. Labels
/// follow the DV L9 naming (`dv::levels::primary_name`) so the HDR mastering
/// line and the L9 line agree. The 0.0005 per-coordinate tolerance comfortably
/// absorbs both encodings' quantization (ST.2086's 0.00002 units, AV1's
/// 1/65536) while keeping apart the tightest real split, the DCI theatrical
/// vs D65 white x (0.0013). Unrecognized coordinates yield `None` — the
/// luminance still shows, the gamut tag is just omitted, never guessed.
///
/// Matching is order-insensitive: the three primaries are first canonicalized
/// by their chromaticity role (blue has the smallest y, red the largest x of
/// the rest — unambiguous for every recognized gamut), because real muxers
/// store them rotated across the slots (google/video-file writes a WebM
/// MasteringMetadata whose R element holds blue's coordinates; the *set* is
/// still exactly Display P3, and MediaInfo labels it as such). Same single
/// tolerance, same label value space — only the slot assignment moves.
pub(crate) fn primaries_label(
    r: (f64, f64),
    g: (f64, f64),
    b: (f64, f64),
    wp: (f64, f64),
) -> Option<&'static str> {
    const D65: (f64, f64) = (0.3127, 0.3290);
    const DCI: (f64, f64) = (0.3140, 0.3510);
    let mut tri3 = [r, g, b];
    tri3.sort_by(|a, c| a.1.total_cmp(&c.1)); // blue first (min y)
    let b = tri3[0];
    let (r, g) = if tri3[1].0 >= tri3[2].0 { (tri3[1], tri3[2]) } else { (tri3[2], tri3[1]) };
    let near =
        |a: (f64, f64), t: (f64, f64)| (a.0 - t.0).abs() < 0.0005 && (a.1 - t.1).abs() < 0.0005;
    let tri = |tr, tg, tb| near(r, tr) && near(g, tg) && near(b, tb);
    if tri((0.708, 0.292), (0.170, 0.797), (0.131, 0.046)) && near(wp, D65) {
        return Some("BT.2020");
    }
    if tri((0.680, 0.320), (0.265, 0.690), (0.150, 0.060)) {
        // P3 primaries: the white point decides display (D65) vs theatrical.
        if near(wp, D65) {
            return Some("DCI-P3 D65");
        }
        if near(wp, DCI) {
            return Some("DCI-P3");
        }
        return None;
    }
    if tri((0.640, 0.330), (0.300, 0.600), (0.150, 0.060)) && near(wp, D65) {
        return Some("BT.709");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::primaries_label;

    #[test]
    fn classifies_the_common_mastering_gamuts() {
        let d65 = (0.3127, 0.329);
        assert_eq!(
            primaries_label((0.708, 0.292), (0.170, 0.797), (0.131, 0.046), d65),
            Some("BT.2020")
        );
        assert_eq!(
            primaries_label((0.680, 0.320), (0.265, 0.690), (0.150, 0.060), d65),
            Some("DCI-P3 D65")
        );
        assert_eq!(
            primaries_label((0.680, 0.320), (0.265, 0.690), (0.150, 0.060), (0.314, 0.351)),
            Some("DCI-P3")
        );
        assert_eq!(
            primaries_label((0.640, 0.330), (0.300, 0.600), (0.150, 0.060), d65),
            Some("BT.709")
        );
    }

    #[test]
    fn rotated_primary_slots_still_classify() {
        // The corpus vp9_hdr10plus.webm's MasteringMetadata verbatim: Display
        // P3 primaries stored rotated across the R/G/B elements (the R slot
        // holds blue's coordinates, etc. — a google/video-file muxer quirk).
        // The set is exactly P3 with a D65 white, so the role-canonicalized
        // match must label it; MediaInfo reports "Display P3" for this file.
        assert_eq!(
            primaries_label(
                (0.15, 0.06),
                (0.68, 0.32),
                (0.26496, 0.69),
                (0.31268, 0.329)
            ),
            Some("DCI-P3 D65")
        );
    }

    #[test]
    fn unknown_coordinates_yield_none() {
        // P3 primaries with an off-spec white point: no guess.
        assert_eq!(
            primaries_label((0.680, 0.320), (0.265, 0.690), (0.150, 0.060), (0.320, 0.340)),
            None
        );
        // Zero-filled (absent) chromaticities.
        assert_eq!(primaries_label((0.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)), None);
    }
}
