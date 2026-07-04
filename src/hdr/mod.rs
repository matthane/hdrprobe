//! Static HDR assembly: classify the overall format (SDR / HDR10 / HLG /
//! HDR10+ / Dolby Vision and combinations) and gather mastering-display +
//! content-light info.

pub mod sei;

use crate::container::Demux;
use crate::hdr::sei::SeiFindings;
use crate::model::{DolbyVision, Hdr, MasteringDisplay};

pub fn assemble(demux: &Demux, dv: Option<&DolbyVision>, sei: &SeiFindings) -> Hdr {
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

    // A base signalled in Dolby's IPT-PQ-c2 colour space (matrix 15, Profile 20 /
    // MV-HEVC) is not a standard, independently viewable HDR10/HLG signal even
    // though its colr carries PQ/HLG — like Profile 5, its cross-compatibility is
    // governed solely by the DV compatibility id (0=none, 4=HLG). So don't let the
    // raw transfer imply a fallback here; fall through to the compat-id branch.
    let ipt_base = demux.color.matrix.as_deref() == Some("IPT-PQ-c2");

    let base = if is_pq && !ipt_base {
        // HDR10 fallback is implied when DV rides on a PQ base layer.
        if dv.is_some() {
            Some("HDR10 (fallback)")
        } else {
            Some("HDR10")
        }
    } else if is_hlg && !ipt_base {
        if dv.is_some() {
            Some("HLG (fallback)")
        } else {
            Some("HLG")
        }
    } else if let Some(dv) = dv {
        // No independently viewable base — infer it from the DV BL compatibility id
        // (1=HDR10, 2=SDR, 4=HLG). Profiles 5 and 20 (compat 0) have no directly
        // viewable base, so we show no base tag.
        //
        // Profile 4 is defined with an SDR (BT.709/BT.1886) base layer, so its base
        // is SDR even when the container omits the compatibility id (older P4 TS
        // descriptors carry no compat nibble) — infer it from the profile.
        if dv.profile.starts_with('4') {
            Some("SDR (fallback)")
        } else {
            match dv.bl_compatibility_id {
                Some(1) => Some("HDR10 (fallback)"),
                Some(2) => Some("SDR (fallback)"),
                Some(4) => Some("HLG (fallback)"),
                _ => None,
            }
        }
    } else {
        Some("SDR")
    };
    if let Some(b) = base {
        formats.push(b.to_string());
    }

    let format = formats.join(" + ");

    // L6 is the DV carriage of HDR10 static metadata, and Dolby's
    // profiles/levels spec defines it as meaningful only for the compat-id-1
    // (HDR10) base signal — so both L6 fallbacks below apply only on an HDR10
    // base. Every other base has no consumer for it: IPT-PQ-c2 (P5/P20/AV1
    // 10.0) has no viewable base at all, HLG (8.4/10.4) is scene-referred and
    // consumes no static metadata (corpus 8.4/10.4 titles carry a zeroed L6
    // placeholder, exactly like P5), and an SDR base likewise signals none.
    let hdr10_base = base.is_some_and(|b| b.starts_with("HDR10"));

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

/// Match raw CIE 1931 mastering-display chromaticities (R, G, B primaries +
/// white point) against the gamuts mastering displays actually use. Labels
/// follow the DV L9 naming (`dv::levels::primary_name`) so the HDR mastering
/// line and the L9 line agree. The 0.0005 per-coordinate tolerance comfortably
/// absorbs both encodings' quantization (ST.2086's 0.00002 units, AV1's
/// 1/65536) while keeping apart the tightest real split, the DCI theatrical
/// vs D65 white x (0.0013). Unrecognized coordinates yield `None` — the
/// luminance still shows, the gamut tag is just omitted, never guessed.
pub(crate) fn primaries_label(
    r: (f64, f64),
    g: (f64, f64),
    b: (f64, f64),
    wp: (f64, f64),
) -> Option<&'static str> {
    const D65: (f64, f64) = (0.3127, 0.3290);
    const DCI: (f64, f64) = (0.3140, 0.3510);
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
