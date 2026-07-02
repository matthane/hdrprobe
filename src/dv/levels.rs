//! Aggregation of per-RPU Dolby Vision metadata into title-stable levels.
//!
//! Respects the project rule: union of present static levels, distinct L5
//! active areas, set of L2/L8 trim *targets*. We deliberately drop L1 and the
//! per-shot trim *values*.

use std::collections::{BTreeMap, BTreeSet};

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock;
use dolby_vision::rpu::rpu_data_nlq::DoviELType;

use crate::container::DvConfig;
use crate::model::{ActiveArea, DolbyVision, DvCensus, L6Fallback, LevelPresence, TrimTarget};

/// Metadata levels we census, in report order.
const CENSUS_LEVELS: &[u8] = &[1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 254];

#[derive(Default)]
pub struct DvAggregate {
    profile: Option<u8>,
    el_type: Option<DoviELType>,
    rpu_count: usize,
    cm_v40: bool,
    /// L254 dm_version_index (CM v4.0 sub-version), from the first RPU carrying it.
    dm_version_index: Option<u8>,
    /// Distinct L5 offset rectangles (left, right, top, bottom).
    l5_offsets: Vec<(u16, u16, u16, u16)>,
    l6: Option<L6Fallback>,
    l9_primary: Option<u8>,
    l11_content: Option<u8>,
    l11_ref_mode: Option<bool>,
    /// L2 trim targets in nits (self-contained: L2 carries its own target_max_pq).
    trim_targets: BTreeSet<u32>,
    /// Distinct L8 target-display indices seen; resolved to nits at finalize via
    /// `l10_targets` (custom displays) or the predefined index table.
    l8_target_indices: BTreeSet<u8>,
    /// L10-defined custom target displays: target_display_index -> peak nits.
    l10_targets: BTreeMap<u8, u32>,
    /// Scene-cut RPUs (scene_refresh_flag set) — shot count under `--full`.
    scene_cuts: usize,
    /// Per-level RPU presence counts (level -> #RPUs carrying it).
    level_counts: BTreeMap<u8, usize>,
}

impl DvAggregate {
    /// Fold in one real RPU (one frame). Scene-cut count comes from the RPU's own
    /// `scene_refresh_flag`. Used by the raw RPU-bin path, which is genuinely
    /// per-frame.
    pub fn add(&mut self, rpu: &DoviRpu) {
        let scene_cuts = rpu
            .vdr_dm_data
            .as_ref()
            .map_or(0, |dm| usize::from(dm.scene_refresh_flag == 1));
        self.fold(rpu, 1, scene_cuts);
    }

    /// Fold in a representative RPU that stands for `frames` identical frames, of
    /// which `scene_cuts` are scene cuts. Lets the DV XML path collapse each shot
    /// to one RPU instead of materialising one clone per frame — the counts are
    /// weighted here so the census is identical to the per-frame expansion, at a
    /// fraction of the work.
    pub fn add_repeated(&mut self, rpu: &DoviRpu, frames: usize, scene_cuts: usize) {
        self.fold(rpu, frames, scene_cuts);
    }

    fn fold(&mut self, rpu: &DoviRpu, weight: usize, scene_cuts: usize) {
        self.rpu_count += weight;
        self.profile.get_or_insert(rpu.dovi_profile);
        if self.el_type.is_none() {
            self.el_type = rpu.el_type.clone();
        }

        let Some(dm) = &rpu.vdr_dm_data else { return };

        self.scene_cuts += scene_cuts;
        // Per-level presence census over the levels we care about.
        for &lvl in CENSUS_LEVELS {
            if dm.level_blocks_iter(lvl).next().is_some() {
                *self.level_counts.entry(lvl).or_default() += weight;
            }
        }

        // CM version comes from the L254 block (CM v4.0 marker), not a guess.
        if let Some(ExtMetadataBlock::Level254(b)) = dm.get_block(254) {
            self.cm_v40 = true;
            self.dm_version_index.get_or_insert(b.dm_version_index);
        }

        for block in dm.level_blocks_iter(5) {
            if let ExtMetadataBlock::Level5(b) = block {
                let rect = (
                    b.active_area_left_offset,
                    b.active_area_right_offset,
                    b.active_area_top_offset,
                    b.active_area_bottom_offset,
                );
                if !self.l5_offsets.contains(&rect) {
                    self.l5_offsets.push(rect);
                }
            }
        }

        if self.l6.is_none() {
            if let Some(ExtMetadataBlock::Level6(b)) = dm.get_block(6) {
                self.l6 = Some(L6Fallback {
                    max_cll: b.max_content_light_level,
                    max_fall: b.max_frame_average_light_level,
                    max_mastering: b.max_display_mastering_luminance,
                    min_mastering: b.min_display_mastering_luminance,
                    zeroed: b.max_content_light_level == 0 && b.max_frame_average_light_level == 0,
                });
            }
        }

        if self.l9_primary.is_none() {
            if let Some(ExtMetadataBlock::Level9(b)) = dm.get_block(9) {
                self.l9_primary = Some(b.source_primary_index);
            }
        }

        if self.l11_content.is_none() {
            if let Some(ExtMetadataBlock::Level11(b)) = dm.get_block(11) {
                self.l11_content = Some(b.content_type);
                self.l11_ref_mode = Some(b.reference_mode_flag);
            }
        }

        // L2 trim targets: target_max_pq (12-bit PQ) -> nits.
        for block in dm.level_blocks_iter(2) {
            if let ExtMetadataBlock::Level2(b) = block {
                self.trim_targets.insert(snap_nits(pq12_to_nits(b.target_max_pq)));
            }
        }
        // L8 trims reference a target display by index; record the indices and
        // resolve them to nits at finalize — index 255 (and other custom indices)
        // is defined by an L10 block in this title, not the predefined table.
        for block in dm.level_blocks_iter(8) {
            if let ExtMetadataBlock::Level8(b) = block {
                self.l8_target_indices.insert(b.target_display_index);
            }
        }
        // L10 defines custom target displays: index -> peak luminance (12-bit PQ).
        for block in dm.level_blocks_iter(10) {
            if let ExtMetadataBlock::Level10(b) = block {
                self.l10_targets
                    .entry(b.target_display_index)
                    .or_insert_with(|| snap_nits(pq12_to_nits(b.target_max_pq)));
            }
        }
    }

    /// Finalize against the picture canvas dimensions and container DV config.
    /// `full` gates the exhaustive per-level census (only meaningful when every
    /// RPU was scanned).
    pub fn finalize(
        self,
        canvas_w: u32,
        canvas_h: u32,
        cfg: Option<&DvConfig>,
        full: bool,
        is_av1: bool,
        dual_track: bool,
    ) -> Option<DolbyVision> {
        // Require at least one parsed RPU, but trust the container dvcC/dvvC for
        // the profile *number*: libdovi's RPU profile can't express AV1 P10
        // (it reports 5/8), and the container box is authoritative anyway. For
        // raw AV1 (no container config) the DV profile is 10 by construction.
        let rpu_profile = self.profile?;
        let profile = match cfg {
            Some(c) => c.profile,
            None if is_av1 => 10,
            None => rpu_profile,
        };

        let el_type = self.el_type.as_ref().map(|t| match t {
            DoviELType::FEL => "FEL".to_string(),
            DoviELType::MEL => "MEL".to_string(),
        });

        // Dual-layer profiles (4 and 7) tag the enhancement-layer kind; single-layer
        // profiles have no EL to qualify. Profile 4's MEL/FEL split is meaningful:
        // an original P4 FEL may not render HDR on all devices (see the DV spec).
        let profile_str = match profile {
            4 | 7 => match self.el_type.as_ref() {
                Some(DoviELType::FEL) => format!("{profile} (FEL)"),
                Some(DoviELType::MEL) => format!("{profile} (MEL)"),
                None => profile.to_string(),
            },
            8 => format!("8.{}", minor_from_compat(cfg)),
            p => p.to_string(),
        };

        // Presence: prefer explicit container flags, else derive from profile.
        let (bl, el, rpu) = match cfg {
            Some(c) => (c.bl_present, c.el_present, c.rpu_present),
            None => (true, profile == 7, true),
        };

        let structure = structure_str(el, dual_track);

        let l5_active_areas = self
            .l5_offsets
            .iter()
            .map(|&(left, right, top, bottom)| ActiveArea {
                width: canvas_w.saturating_sub(left as u32 + right as u32),
                height: canvas_h.saturating_sub(top as u32 + bottom as u32),
                left,
                right,
                top,
                bottom,
            })
            .collect();

        let cm_version = Some(if self.cm_v40 { "CM v4.0".to_string() } else { "CM v2.9".to_string() });

        let compatibility = cfg.and_then(|c| c.bl_compatibility_id).and_then(compat_str);

        // Resolve L8 trim targets to nits: a custom index (e.g. 255) is defined by
        // an L10 block in this title; otherwise fall back to the predefined table.
        // An index with neither a definition nor a table entry can't be stated in
        // nits, so it's dropped rather than guessed. Merged with the L2 targets,
        // but each nit value keeps the set of levels that produced it, so a value
        // present in both L2 and L8 reads `[L2/L8]` while an L8-only one reads
        // `[L8]`.
        let mut target_levels: BTreeMap<u32, BTreeSet<u8>> = BTreeMap::new();
        for nits in self.trim_targets {
            target_levels.entry(nits).or_default().insert(2);
        }
        for &idx in &self.l8_target_indices {
            if let Some(nits) = resolve_l8_nits(idx, &self.l10_targets) {
                target_levels.entry(nits).or_default().insert(8);
            }
        }
        let trim_targets = target_levels
            .into_iter()
            .map(|(nits, levels)| TrimTarget {
                nits,
                levels: levels.into_iter().collect(),
            })
            .collect();

        let census = full.then(|| DvCensus {
            scene_cuts: self.scene_cuts,
            dm_version_index: self.dm_version_index,
            level_presence: self
                .level_counts
                .iter()
                .map(|(&level, &rpus_with)| LevelPresence { level, rpus_with })
                .collect(),
        });

        Some(DolbyVision {
            profile: profile_str,
            structure,
            level: cfg.and_then(|c| c.level),
            bl_present: bl,
            el_present: el,
            rpu_present: rpu,
            el_type,
            bl_compatibility_id: cfg.and_then(|c| c.bl_compatibility_id),
            compatibility,
            cm_version,
            l5_active_areas,
            l5_assumed_canvas: None,
            l6_fallback: self.l6,
            l9_mastering: self.l9_primary.map(primary_name),
            l11_content: self.l11_content.map(content_type_name),
            l11_reference_mode: self.l11_ref_mode,
            trim_targets,
            rpu_count: self.rpu_count,
            sampled: !full,
            census,
        })
    }
}

/// Build a DV section from the container config alone (no RPU parse), as used
/// by `--no-rpu`. Static levels are absent since they live in the RPU.
pub fn container_only(cfg: &DvConfig, dual_track: bool) -> DolbyVision {
    let profile_str = match cfg.profile {
        7 => "7".to_string(),
        8 => format!("8.{}", minor_from_compat(Some(cfg))),
        p => p.to_string(),
    };
    DolbyVision {
        profile: profile_str,
        structure: structure_str(cfg.el_present, dual_track),
        level: cfg.level,
        bl_present: cfg.bl_present,
        el_present: cfg.el_present,
        rpu_present: cfg.rpu_present,
        el_type: None,
        bl_compatibility_id: cfg.bl_compatibility_id,
        compatibility: cfg.bl_compatibility_id.and_then(compat_str),
        cm_version: None,
        l5_active_areas: Vec::new(),
        l5_assumed_canvas: None,
        l6_fallback: None,
        l9_mastering: None,
        l11_content: None,
        l11_reference_mode: None,
        trim_targets: Vec::new(),
        rpu_count: 0,
        sampled: false,
        census: None,
    }
}

/// The dvcC `dv_bl_signal_compatibility_id` only constrains the minor digit
/// when we can't read it from a container box. Fall back to "1" (the common
/// 8.1 case) since the RPU alone doesn't store it.
fn minor_from_compat(cfg: Option<&DvConfig>) -> u8 {
    match cfg.and_then(|c| c.bl_compatibility_id) {
        Some(1) => 1,
        Some(2) => 2,
        Some(4) => 4,
        _ => 1,
    }
}

/// The layer/track structure line, present only for dual-layer (Profile 7)
/// content: an enhancement layer either interleaved in one track/stream
/// (single track) or carried on a separate track/PID (dual track). Single-layer
/// profiles (5, 8, 10) have no EL, so there is no "dual layer" to describe.
fn structure_str(el_present: bool, dual_track: bool) -> Option<String> {
    if !el_present {
        return None;
    }
    Some(if dual_track {
        "Dual track, dual layer".to_string()
    } else {
        "Single track, dual layer".to_string()
    })
}

fn compat_str(id: u8) -> Option<String> {
    Some(
        match id {
            0 => "no cross-compatibility",
            1 => "HDR10-compatible",
            2 => "SDR-compatible",
            4 => "HLG-compatible",
            _ => return None,
        }
        .to_string(),
    )
}

fn primary_name(idx: u8) -> String {
    match idx {
        0 => "DCI-P3 D65",
        1 => "BT.709",
        2 => "BT.2020",
        3 => "SMPTE-C",
        4 => "BT.601",
        5 => "DCI-P3",
        6 => "ACES",
        7 => "S-Gamut",
        8 => "S-Gamut-3.Cine",
        _ => "unknown",
    }
    .to_string()
}

fn content_type_name(t: u8) -> String {
    match t {
        0 => "Reserved",
        1 => "Cinema",
        2 => "Games",
        3 => "Sports",
        4 => "User-generated",
        _ => "Unknown",
    }
    .to_string()
}

/// SMPTE ST 2084 (PQ) EOTF for a 12-bit code value -> cd/m².
fn pq12_to_nits(code: u16) -> f64 {
    let e = (code as f64) / 4095.0;
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let ep = e.powf(1.0 / M2);
    let num = (ep - C1).max(0.0);
    let den = C2 - C3 * ep;
    if den <= 0.0 {
        return 0.0;
    }
    10000.0 * (num / den).powf(1.0 / M1)
}

const STANDARD_NITS: &[u32] = &[
    48, 100, 150, 200, 250, 300, 400, 500, 600, 700, 800, 1000, 1500, 2000, 2500, 3000, 4000, 10000,
];

/// Snap a computed nit value to the nearest standard mastering target.
fn snap_nits(nits: f64) -> u32 {
    let n = nits.round() as i64;
    let mut best = n as u32;
    let mut best_d = i64::MAX;
    for &s in STANDARD_NITS {
        let d = (s as i64 - n).abs();
        // Within 4% (or 10 nits at the low end) snaps to the standard value.
        let tol = ((s as f64) * 0.04).max(10.0) as i64;
        if d <= tol && d < best_d {
            best_d = d;
            best = s;
        }
    }
    best
}

/// Dolby L8 `target_display_index` -> nits, for the common predefined targets.
fn l8_index_to_nits(idx: u8) -> Option<u32> {
    Some(match idx {
        1 => 100,
        16 => 100,
        18 => 600,
        20 => 1000,
        21 => 2000,
        22 => 4000,
        23 => 10000,
        27 => 600,
        28 => 1000,
        48 => 48,
        _ => return None,
    })
}

/// Resolve an L8 trim's `target_display_index` to a peak-nits value. A custom
/// index (255, and any other display defined by an L10 block in this title) is
/// looked up in the per-title L10 map; otherwise the predefined index table is
/// used. `None` when neither knows it — the value is never guessed.
fn resolve_l8_nits(idx: u8, l10_targets: &BTreeMap<u8, u32>) -> Option<u32> {
    l10_targets.get(&idx).copied().or_else(|| l8_index_to_nits(idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l8_index_255_resolves_via_l10_definition() {
        // Profile 20's L8 trims reference target display 255, which is defined by
        // an L10 block (target_max_pq 2547 ≈ 300 nits) — not the predefined table.
        let l10 = BTreeMap::from([(255u8, 300u32)]);
        assert_eq!(resolve_l8_nits(255, &l10), Some(300), "custom index from L10");
        assert_eq!(resolve_l8_nits(255, &BTreeMap::new()), None, "no L10 → not guessed");
    }

    #[test]
    fn l8_predefined_index_still_resolves_without_l10() {
        // A predefined index keeps its table value; an unknown one with no L10 def
        // yields nothing rather than a made-up number.
        assert_eq!(resolve_l8_nits(27, &BTreeMap::new()), Some(600));
        assert_eq!(resolve_l8_nits(200, &BTreeMap::new()), None);
    }

    #[test]
    fn l10_definition_overrides_predefined_table() {
        // If a title redefines a predefined index via L10, the title's own
        // definition wins (it describes the actual target on this master).
        let l10 = BTreeMap::from([(27u8, 1000u32)]);
        assert_eq!(resolve_l8_nits(27, &l10), Some(1000));
    }
}
