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

    let base = if is_pq {
        // HDR10 fallback is implied when DV rides on a PQ base layer.
        if dv.is_some() {
            Some("HDR10 (fallback)")
        } else {
            Some("HDR10")
        }
    } else if is_hlg {
        if dv.is_some() {
            Some("HLG (fallback)")
        } else {
            Some("HLG")
        }
    } else if let Some(dv) = dv {
        // No container colour info — infer the base layer from the DV BL
        // compatibility id (1=HDR10, 2=SDR, 4=HLG). Profile 5 (compat 0) has no
        // directly viewable base, so we show no base tag.
        match dv.bl_compatibility_id {
            Some(1) => Some("HDR10 (fallback)"),
            Some(2) => Some("SDR (fallback)"),
            Some(4) => Some("HLG (fallback)"),
            _ => None,
        }
    } else {
        Some("SDR")
    };
    if let Some(b) = base {
        formats.push(b.to_string());
    }

    let format = formats.join(" + ");

    // Prefer container mastering, then the SEI ST.2086 message, then DV L6.
    let mastering = demux
        .mastering
        .clone()
        .or_else(|| sei.mastering.clone())
        .or_else(|| {
            dv.and_then(|d| d.l6_fallback.as_ref()).map(|l6| MasteringDisplay {
                max_luminance: l6.max_mastering as f64,
                min_luminance: l6.min_mastering as f64 / 10000.0,
                primaries: demux.color.primaries.clone(),
            })
        });

    let content_light = demux.content_light.or(sei.content_light).or_else(|| {
        dv.and_then(|d| d.l6_fallback.as_ref()).map(|l6| crate::model::ContentLight {
            max_cll: l6.max_cll,
            max_fall: l6.max_fall,
        })
    });

    Hdr { format, mastering, content_light }
}
