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

use super::{finalize_dv, Payload, ASSUMED_CANVAS};

pub fn parse(data: &[u8]) -> Result<Payload> {
    let xml = String::from_utf8_lossy(data).into_owned();

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
        Some(agg) => finalize_dv(agg, Some(ASSUMED_CANVAS)),
        None => bail!("failed to parse Dolby Vision XML"),
    }
}
