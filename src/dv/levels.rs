//! Aggregation of per-RPU Dolby Vision metadata into title-stable levels.
//!
//! Respects the project rule: union of present static levels, distinct L5
//! active areas, set of L2/L8 trim *targets*. We deliberately drop L1 and the
//! per-shot trim *values*.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use bitvec_helpers::bitstream_io_writer::BitstreamIoWriter;
use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock;
use dolby_vision::rpu::rpu_data_nlq::DoviELType;
use dolby_vision::rpu::vdr_dm_data::VdrDmData;

use crate::container::DvConfig;
use crate::model::{
    ActiveArea, DolbyVision, DvCensus, FelBrightnessExpansion, L6, LevelPresence,
    MasteringDisplay, MasteringPrimariesMismatch, MetadataCadence, TrimTarget,
};

/// Metadata levels we census, in report order.
const CENSUS_LEVELS: &[u8] = &[1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 254];

/// Levels folded into [`dm_fingerprint`]: every block-bearing level libdovi
/// models except L4. The title-constant levels (6/9/10/11/254) are included on
/// purpose — they serialize identically on every frame, so they cost a few
/// bytes and spare a judgment call about which levels count as "dynamic". L4
/// is excluded because its temporal-filtering anchors are a per-frame running
/// average by mechanism, present even in shot-based authoring (corpus-verified
/// via dovi_tool export on the P7 FEL CM v2.9 clip: adjacent same-shot frames
/// differ *only* in L4, while the clip's own CM XML is per-shot) — including
/// it would misread every L4-carrying title as per-frame.
const FINGERPRINT_LEVELS: &[u8] = &[1, 2, 3, 5, 6, 8, 9, 10, 11, 254, 255];

#[derive(Default)]
pub struct DvAggregate {
    profile: Option<u8>,
    el_type: Option<DoviELType>,
    /// The RPU header's `vdr_bit_depth` (the composer's reconstructed-signal
    /// depth), from the first RPU whose header carried the sequence-info block.
    /// Every parsed RPU passed libdovi's header validation (BL/EL exactly
    /// 10-bit, vdr <= 14), so the value is bounded; whether it *renders* is
    /// decided at finalize (FEL only).
    vdr_bit_depth: Option<u8>,
    rpu_count: usize,
    cm_v40: bool,
    /// L254 dm_version_index (CM v4.0 sub-version), from the first RPU carrying it.
    dm_version_index: Option<u8>,
    /// Distinct L5 offset rectangles (left, right, top, bottom).
    l5_offsets: Vec<(u16, u16, u16, u16)>,
    /// DV mastering-display luminance range from the vdr_dm_data header
    /// (`source_min_pq`, `source_max_pq`, 12-bit PQ codes), first RPU carrying
    /// it. This is the grade's own mastering display — on a Profile 7 title it
    /// can exceed the base layer's ST.2086 SEI (e.g. a 4000-nit DV grade over a
    /// 1000-nit HDR10 base).
    source_pq: Option<(u16, u16)>,
    l6: Option<L6>,
    l9_primary: Option<u8>,
    /// L9 custom chromaticities (R,G,B,WP x/y in 0.15 fixed point, ÷32767),
    /// carried only by the 17-byte form (index 255).
    l9_custom: Option<[u16; 8]>,
    l11_content: Option<u8>,
    l11_white_point: Option<u8>,
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
    /// BL compatibility id known without a container dvcC — a DV XML declares its
    /// profile (so the minor digit is real, not a convention default). `None` for
    /// a raw RPU bin, which carries no compatibility id at all.
    compat_override: Option<u8>,
    /// Metadata-only sidecar (RPU bin / DV XML): no base layer to back a
    /// convention-default compat minor, so such a default is flagged as assumed.
    metadata_only: bool,
    /// Whether folds arrive as *every* frame in stream order (the `--full`
    /// video scan and the raw RPU-bin sidecar), enabling the consecutive-frame
    /// DM comparison behind the metadata-cadence verdict. The sampled default
    /// folds scattered frames, so it never sets this: a pair spanning a
    /// sampling gap would read as a change.
    track_consecutive: bool,
    /// Previous folded frame's DM fingerprint, when tracking consecutively.
    prev_dm_fp: Option<u64>,
    /// Consecutive-frame DM comparisons made / how many differed. Also fed
    /// directly by the DV XML path (`add_cadence_pairs`), whose fold order is
    /// per-shot rather than stream order.
    cadence_pairs: usize,
    cadence_changes: usize,
}

impl DvAggregate {
    /// Record a compatibility id known from outside a container dvcC (a DV XML's
    /// declared profile), so the label's minor digit is real rather than assumed.
    pub fn set_compat_id(&mut self, id: u8) {
        self.compat_override = Some(id);
    }

    /// Mark this as a metadata-only sidecar (no base layer), enabling the
    /// `[compat assumed]` flag when the compat minor is a convention default.
    pub fn mark_metadata_only(&mut self) {
        self.metadata_only = true;
    }

    /// Declare that every frame's RPU will be folded, in stream order, enabling
    /// the consecutive-frame DM comparison behind the metadata-cadence verdict.
    /// Callers: the `--full` video scan (all containers fold decode-order AUs)
    /// and the raw RPU-bin sidecar (runs of identical frames, in stream order).
    pub fn track_consecutive(&mut self) {
        self.track_consecutive = true;
    }

    /// Fold externally computed cadence evidence: `pairs` consecutive-frame DM
    /// comparisons of which `changes` differed. Used by the DV XML path, which
    /// walks its declared shot/edit structure instead of folding frames in
    /// stream order.
    pub fn add_cadence_pairs(&mut self, pairs: usize, changes: usize) {
        self.cadence_pairs += pairs;
        self.cadence_changes += changes;
    }

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
        // Header bit depths are only parsed when the sequence-info block is
        // present (always, for an RPU libdovi accepted — validation requires
        // the depths it carries); the gate keeps a defaulted 0 from reading
        // as 8-bit if that ever changes.
        if self.vdr_bit_depth.is_none() && rpu.header.vdr_seq_info_present_flag {
            self.vdr_bit_depth = Some(8 + rpu.header.vdr_bit_depth_minus8 as u8);
        }

        let Some(dm) = &rpu.vdr_dm_data else { return };

        self.scene_cuts += scene_cuts;
        // Consecutive-frame DM comparison for the metadata-cadence verdict.
        // A run of `weight` identical frames contributes its boundary pair
        // against the previous fold plus `weight - 1` internal pairs, none of
        // which change (identical frames by construction on both callers).
        if self.track_consecutive {
            let fp = dm_fingerprint(dm);
            if let Some(prev) = self.prev_dm_fp.replace(fp) {
                self.cadence_pairs += 1;
                self.cadence_changes += usize::from(prev != fp);
            }
            self.cadence_pairs += weight.saturating_sub(1);
        }
        // Mastering-display range of the DV grade, from the DM data header.
        // Title-stable in practice; a zero max is an absent/defaulted value.
        if self.source_pq.is_none() && dm.source_max_pq > 0 {
            self.source_pq = Some((dm.source_min_pq, dm.source_max_pq));
        }
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
                self.l6 = Some(L6 {
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
                if b.length > 1 {
                    self.l9_custom = Some([
                        b.source_primary_red_x,
                        b.source_primary_red_y,
                        b.source_primary_green_x,
                        b.source_primary_green_y,
                        b.source_primary_blue_x,
                        b.source_primary_blue_y,
                        b.source_primary_white_x,
                        b.source_primary_white_y,
                    ]);
                }
            }
        }

        if self.l11_content.is_none() {
            if let Some(ExtMetadataBlock::Level11(b)) = dm.get_block(11) {
                self.l11_content = Some(b.content_type);
                self.l11_white_point = Some(b.whitepoint);
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

        // Reconstructed bit depth: the header's signaled vdr_bit_depth, shown
        // only when a FEL residual exists to actually reconstruct beyond the
        // 10-bit base (P7 signals 12, P4 signals 14 — reported verbatim, never
        // assumed from the profile). MEL and single-layer RPUs signal 12 too,
        // but that is composer precision with no residual data behind it, so
        // it stays off the report rather than misreading as content depth.
        let reconstructed_bit_depth = match self.el_type {
            Some(DoviELType::FEL) => self.vdr_bit_depth,
            _ => None,
        };

        // Compat id from the container dvcC/dvvC, else a DV XML's declared profile.
        // When neither carries it, the label's minor digit is a convention default
        // (P8 -> .1, P7 -> .6, P4 -> .2). That's only flagged as assumed for a
        // metadata-only sidecar: a video input's base-layer VUI backs the
        // inference officially.
        let compat_id = cfg.and_then(|c| c.bl_compatibility_id).or(self.compat_override);
        let profile_compat_assumed =
            self.metadata_only && compat_id.is_none() && matches!(profile, 4 | 7 | 8);
        let profile_str = dv_profile_label(profile, compat_id, self.el_type.as_ref());

        // Presence: prefer explicit container flags (translated to the logical
        // track by `track_presence`), else derive from profile — with the same
        // dual-track override, since a folded group holds both layers whatever
        // the surviving declaration says.
        let (bl, el, rpu) = match cfg {
            Some(c) => track_presence(c, dual_track),
            None => (true, profile == 7 || dual_track, true),
        };

        // A dual-layer-authored RPU in an EL-less carriage: the NLQ composer
        // payload (whose MEL/FEL fingerprint is `el_type`) exists only when
        // the RPU was authored for a dual-layer (P4/P7) encode, so its
        // presence where the carriage demonstrably has no EL marks an
        // unconverted RPU — the classic custom-transcode case of a UHD-BD P7
        // RPU injected without dovi_tool `--mode 2`. Hard gates: the no-EL
        // side must be *declared* (an explicit config with el_present == 0,
        // and no folded EL stream — a dual-track group's carriage demonstrably
        // has one, hence the derived `el` rather than the raw flag) or
        // definitional (AV1, whose DV carriage is single-layer by
        // construction — the same reasoning `el` derives from above). A
        // config-less raw HEVC stream never fires (its EL may legitimately
        // ride in-band), and a metadata sidecar has no carriage to compare
        // (cfg None, not AV1). Provenance observation, not an error claim:
        // the stray payload is inert for playback.
        let no_el_carriage = match cfg {
            Some(_) => !el,
            None => is_av1,
        };
        let unconverted_dual_layer_rpu = self.el_type.is_some() && no_el_carriage;

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

        let compatibility = compat_id.and_then(compat_str);

        let trim_targets =
            merge_trim_targets(&self.trim_targets, &self.l8_target_indices, &self.l10_targets);

        let metadata_cadence = cadence_verdict(self.cadence_pairs, self.cadence_changes);

        let census = full.then(|| DvCensus {
            scene_cuts: self.scene_cuts,
            dm_version_index: self.dm_version_index,
            level_presence: self
                .level_counts
                .iter()
                .map(|(&level, &rpus_with)| LevelPresence { level, rpus_with })
                .collect(),
        });

        // The mastering display's gamut: L9 is the RPU's only carrier of it, so
        // the recognized L9 name rides the Mastering line; "custom"/"unknown"
        // stay on the standalone L9 line instead, never guessed. A CM v2.9 RPU
        // (no L9) genuinely carries no display gamut, so its Mastering line is
        // luminance-only. The DM data header's ycc_to_rgb/rgb_to_lms matrices
        // are NOT a substitute fingerprint: they describe the *signal* space,
        // not the display — verified empirically (July 2026) across the corpus,
        // where every BT.2020-container title carries the identical BT.2020
        // rgb_to_lms set [7222, 8771, 390, ...] regardless of mastering display
        // (including P3-D65-mastered titles, both CM v2.9 ones per their BL
        // MDCV and CM v4.0 ones per their own L9), and P5/P20 carry the
        // IPTPQc2 crosstalk matrix. Don't reintroduce matrix matching.
        let l9_label = l9_label(self.l9_primary, self.l9_custom);
        let l9_recognized =
            l9_label.clone().filter(|l| l != "custom" && l != "unknown");
        let mastering_display = self.source_pq.map(source_pq_to_mastering).map(|mut m| {
            m.primaries_level = l9_recognized.is_some().then_some(9);
            m.primaries = l9_recognized;
            m
        });

        Some(DolbyVision {
            profile: profile_str,
            profile_compat_assumed,
            structure,
            level: cfg.and_then(|c| c.level),
            // Filled by `fill_derived_level` (main.rs only) when no config
            // declared a level: the derivation needs the track's real coded
            // dimensions and frame rate, which a metadata sidecar (assumed
            // canvas, declared-not-coded rate) doesn't have.
            level_derived: false,
            bl_present: bl,
            el_present: el,
            rpu_present: rpu,
            el_type,
            unconverted_dual_layer_rpu,
            reconstructed_bit_depth,
            bl_compatibility_id: compat_id,
            compatibility,
            cm_version,
            l5_active_areas,
            l5_assumed_canvas: None,
            mastering_display,
            // Both filled after the base layer's mastering display is known
            // (`flag_fel_brightness_expansion`, `flag_mastering_primaries_mismatch`);
            // the RPU alone can't decide either.
            fel_brightness_expansion: None,
            mastering_primaries_mismatch: None,
            l6: self.l6,
            l9_mastering: l9_label,
            l11_content: self.l11_content.map(content_type_name),
            l11_white_point: self.l11_white_point.map(white_point_name),
            l11_reference_mode: self.l11_ref_mode,
            trim_targets,
            rpu_count: self.rpu_count,
            sampled: !full,
            metadata_cadence,
            census,
        })
    }
}

/// Fingerprint of a DM payload's *content*: every extension-metadata block
/// serialized to its exact bitstream form, plus the header's mastering range
/// (`source_min/max_pq`), hashed. `scene_refresh_flag` is deliberately outside
/// the fingerprint, so a shot's first frame — identical to the rest of the
/// shot but for that flag — compares equal. The composer/NLQ payload is also
/// outside on purpose: it can vary independently of the CM metadata (a FEL
/// residual's mapping), and cadence is a CM-authoring fact. Equality is only
/// ever checked between frames folded within one process run, so std's
/// run-seeded hasher is fine.
pub(crate) fn dm_fingerprint(dm: &VdrDmData) -> u64 {
    let mut w = BitstreamIoWriter::with_capacity(128);
    for &lvl in FINGERPRINT_LEVELS {
        for block in dm.level_blocks_iter(lvl) {
            // A block that refuses to serialize (reserved payloads) is skipped
            // on every frame alike, so it skews nothing.
            let _ = block.write(&mut w);
        }
    }
    let _ = w.byte_align();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (dm.source_min_pq, dm.source_max_pq).hash(&mut h);
    if let Some(bytes) = w.as_slice() {
        bytes.hash(&mut h);
    }
    h.finish()
}

/// Classify the title's metadata cadence from consecutive-frame DM
/// comparisons: per-shot (values change only at scene cuts — the standard CM
/// authoring workflow) vs per-frame (each frame carries its own analysis, e.g.
/// converted or per-frame-graded metadata). The line is a quarter of the
/// pairs, not exact zero or a simple majority, because both classes carry
/// bounded slack: shot-based changes are one per shot transition, so the
/// fraction is 1/(average shot length) — corpus-observed at 0–2.6% even with
/// decode-order stragglers at open-GOP cuts — while per-frame analysis
/// produces equal neighbours over static stretches (corpus-observed at
/// 55–64%: the HLG-converted and P20 live-analysis titles) and would halve
/// again on a high-rate stream that duplicates each analysed frame, still
/// staying above a quarter. No pairs at all (a single RPU, or nothing folded
/// consecutively) yields `None`, never a guess.
fn cadence_verdict(pairs: usize, changes: usize) -> Option<MetadataCadence> {
    (pairs > 0).then(|| MetadataCadence {
        cadence: if changes * 4 >= pairs { "per-frame" } else { "per-shot" }.to_string(),
        frame_pairs: pairs,
        changed_pairs: changes,
    })
}

/// The reported layer-presence flags for the logical track a config describes.
///
/// A `DvConfig` is the verbatim carriage declaration of one *sub-stream*, and
/// on a real dual-track mux it rides the EL track/PID, whose `bl_present == 0`
/// truthfully says "no base layer *in this stream*" (it names its BL via
/// `dependency_pid`). Once that EL is folded into its base layer's group, the
/// report describes the merged logical track — which holds both layers by
/// construction: `dv_dual_track` is set only when an EL stream folded into a
/// group containing a BL (every backend), and an EL-only mux with no BL to
/// fold into keeps it false, so a genuinely BL-less stream still reports its
/// declared `bl_present == 0` here. `rpu_present` passes through: the fold
/// implies layers, not an RPU declaration.
fn track_presence(cfg: &DvConfig, dual_track: bool) -> (bool, bool, bool) {
    (cfg.bl_present || dual_track, cfg.el_present || dual_track, cfg.rpu_present)
}

/// Build a DV section from the container config alone (no RPU parse), as used
/// by `--no-rpu`. Static levels are absent since they live in the RPU.
pub fn container_only(cfg: &DvConfig, dual_track: bool) -> DolbyVision {
    // No RPU parse here, so the enhancement-layer kind (FEL/MEL) is unknown.
    let profile_str = dv_profile_label(cfg.profile, cfg.bl_compatibility_id, None);
    let (bl, el, rpu) = track_presence(cfg, dual_track);
    DolbyVision {
        profile: profile_str,
        // Container path: the convention-default compat minor is backed by the
        // base layer, not a metadata-only guess, so it is never flagged assumed.
        profile_compat_assumed: false,
        structure: structure_str(el, dual_track),
        level: cfg.level,
        level_derived: false,
        bl_present: bl,
        el_present: el,
        rpu_present: rpu,
        el_type: None,
        // Requires the RPU's own composer payload as evidence, so the
        // config-only path can never establish it.
        unconverted_dual_layer_rpu: false,
        reconstructed_bit_depth: None,
        bl_compatibility_id: cfg.bl_compatibility_id,
        compatibility: cfg.bl_compatibility_id.and_then(compat_str),
        cm_version: None,
        l5_active_areas: Vec::new(),
        l5_assumed_canvas: None,
        mastering_display: None,
        fel_brightness_expansion: None,
        mastering_primaries_mismatch: None,
        l6: None,
        l9_mastering: None,
        l11_content: None,
        l11_white_point: None,
        l11_reference_mode: None,
        trim_targets: Vec::new(),
        rpu_count: 0,
        sampled: false,
        metadata_cadence: None,
        census: None,
    }
}

/// Flag likely FEL brightness expansion: the grade's own mastering display
/// (`source_max_pq`, already on `dv.mastering_display`) is meaningfully
/// brighter than the base layer's declared mastering max in nits
/// (`bl_max_nits`, from the container MDCV or the ST.2086 SEI, never the
/// RPU's own L6 fallback, which would be self-referential). Gated to FEL: a
/// MEL's residual is empty, so it can never carry brightness the BL lacks,
/// however the mastering displays differ. This is a metadata verdict only;
/// confirming the general case would mean decoding and comparing composed vs
/// BL pixels, which hdrprobe never does, so a missing flag is not proof of no
/// expansion.
pub fn flag_fel_brightness_expansion(dv: &mut DolbyVision, bl_max_nits: Option<f64>) {
    if dv.el_type.as_deref() != Some("FEL") {
        return;
    }
    let Some(rpu_max) = dv.mastering_display.as_ref().map(|m| m.max_luminance) else { return };
    let Some(bl_max) = bl_max_nits.filter(|&b| b > 0.0) else { return };
    // A 10% margin separates real mastering-target steps (1000 -> 2000/4000,
    // always 2x or more) from quantisation noise between the two encodings
    // (bounded by the 4% snap tolerance), without hardcoding the canonical
    // 1000/4000 pair.
    if rpu_max > bl_max * 1.1 {
        dv.fel_brightness_expansion =
            Some(FelBrightnessExpansion { bl_max_nits: bl_max, rpu_max_nits: rpu_max });
    }
}

/// Flag a mastering-gamut disagreement: the DV grade's L9 primaries name (on
/// `dv.mastering_display`, present only when a recognized L9 filled it) differs
/// from the base layer's own declared mastering primaries (`bl_primaries`, the
/// label from a *signalled* container MDCV box or ST.2086 SEI — never the L6
/// fallback, whose primaries are the L9 itself, a self-comparison). Both names
/// come from the one shared matcher (`hdr::primaries_label` / the L9 name
/// table), so string inequality is the whole verdict — no tolerance here, the
/// matcher already absorbed quantization. Gated to L9 (`primaries_level` 9):
/// a DV XML's Level-0 fills the same field tagged 0, but that's a sidecar,
/// which has no base layer to disagree with. Either side unrecognized or
/// absent: no comparison, never a guess.
pub fn flag_mastering_primaries_mismatch(dv: &mut DolbyVision, bl_primaries: Option<&str>) {
    let Some(l9) = dv
        .mastering_display
        .as_ref()
        .filter(|m| m.primaries_level == Some(9))
        .and_then(|m| m.primaries.as_deref())
    else {
        return;
    };
    let Some(bl) = bl_primaries else { return };
    if l9 != bl {
        dv.mastering_primaries_mismatch = Some(MasteringPrimariesMismatch {
            bl_primaries: bl.to_string(),
            rpu_primaries: l9.to_string(),
        });
    }
}

/// The Dolby Vision level table ("Dolby Vision Profiles and Levels", the
/// dsigPL table Dolby's own dlb_mp4base muxer derives levels from): each
/// level's max pixel rate is exactly its anchor format's `width x height x
/// fps`, plus a max-width axis that splits the equal-rate UHD@120 / 8K@30
/// pair (levels 10/11). Rows are `(level, max pixels/second, max width)`,
/// ascending, so the first admitting row is the smallest sufficient level.
const DV_LEVEL_LIMITS: [(u8, u64, u32); 13] = [
    (1, 22_118_400, 1280),     // 1280x720x24
    (2, 27_648_000, 1280),     // 1280x720x30
    (3, 49_766_400, 1920),     // 1920x1080x24
    (4, 62_208_000, 1920),     // 1920x1080x30
    (5, 124_416_000, 1920),    // 1920x1080x60
    (6, 199_065_600, 3840),    // 3840x2160x24
    (7, 248_832_000, 3840),    // 3840x2160x30
    (8, 398_131_200, 3840),    // 3840x2160x48
    (9, 497_664_000, 3840),    // 3840x2160x60
    (10, 995_328_000, 3840),   // 3840x2160x120
    (11, 995_328_000, 7680),   // 7680x4320x30
    (12, 1_990_656_000, 7680), // 7680x4320x60
    (13, 3_981_312_000, 7680), // 7680x4320x120
];

/// Fill a missing DV level from the coded stream's shape: the smallest level
/// of `DV_LEVEL_LIMITS` admitting `width x height x fps`, flagged via
/// `level_derived`. A declared container level always wins (the fill is gated
/// on `level == None`), and nothing is derived without real dimensions and a
/// known frame rate — never a guess.
///
/// This exists because the level is otherwise unobtainable for authentic disc
/// content: a UHD-BD M2TS signals DV via the playlist STN table, not a PMT
/// `0xB0` descriptor (only remuxes add one), and the RPU doesn't carry the
/// level either. The derivation is a pixel-rate floor only — the table's
/// bitrate/tier axis is deliberately not compared (the default probe reads a
/// bounded head window, and consumers probe truncated chunks), which matches
/// how the level is defined: the anchor-format pixel rate is the level's
/// identity, the tier a bound within it. `main.rs` is the only caller, on the
/// video path: a metadata sidecar's canvas is an *assumed* UHD and a DV XML's
/// rate is an authoring declaration, so deriving there would manufacture a
/// stream fact no stream backs.
pub fn fill_derived_level(dv: &mut DolbyVision, width: u32, height: u32, fps: Option<f64>) {
    if dv.level.is_some() || width == 0 || height == 0 {
        return;
    }
    let Some(fps) = fps.filter(|f| *f > 0.0) else { return };
    let px_rate = width as f64 * height as f64 * fps;
    let fit = DV_LEVEL_LIMITS
        .iter()
        .find(|&&(_, max_rate, max_width)| px_rate <= max_rate as f64 && width <= max_width);
    if let Some(&(level, _, _)) = fit {
        dv.level = Some(level);
        dv.level_derived = true;
    }
}

/// Format the Dolby Vision profile as `profile.compatibility` (e.g. `5.0`,
/// `7.6`, `8.1`, `9.2`, `10.4`, `20.0`), tagging the enhancement-layer kind for
/// the dual-layer profiles (4 and 7). The minor digit is the container's
/// `dv_bl_signal_compatibility_id`, printed verbatim so an atypical id (e.g. the
/// Blu-ray Profile 7 value 6) is reported rather than clamped.
///
/// Some inputs carry no container compat id; the minor digit is then taken from
/// the profile's definition rather than guessed. Profile 8 mandates
/// cross-compatibility signalling, so a raw P8 RPU (a `.bin`/`.xml` sidecar, or
/// an AV1 P10 RPU libdovi reports as 8) is labelled `8.1` by convention, matching
/// dovi_tool. Profile 7 is defined with CCID 6 only (the UHD Blu-ray HDR10 base) —
/// and its most common carrier, an untouched BDMV M2TS, has *no* DV descriptor to
/// read (Blu-ray signals DV via the playlist STN table, not the PMT `0xB0`
/// descriptor a remux would add) — so a descriptor-less P7 is labelled `7.6`.
/// Profile 4 is SDR-compatible by definition (CCID 2), so a legacy P4
/// mux whose compact descriptor omits the nibble is labelled `4.2` — consistent
/// with `hdr::assemble` inferring P4's SDR base from the profile. Any other
/// profile without a compat id prints its bare number.
fn dv_profile_label(profile: u8, compat: Option<u8>, el_type: Option<&DoviELType>) -> String {
    let base = match compat {
        Some(id) => format!("{profile}.{id}"),
        None if profile == 8 => "8.1".to_string(),
        None if profile == 7 => "7.6".to_string(),
        None if profile == 4 => "4.2".to_string(),
        None => profile.to_string(),
    };
    match (profile, el_type) {
        (4 | 7, Some(DoviELType::FEL)) => format!("{base} (FEL)"),
        (4 | 7, Some(DoviELType::MEL)) => format!("{base} (MEL)"),
        _ => base,
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

/// Resolve an L9 block to a gamut name: a predefined index through the Dolby
/// index table, a custom index (255) by matching its raw chromaticities (0.15
/// fixed point, ÷32767) against the shared gamut matcher — falling back to
/// "custom" when the coordinates match no known mastering gamut.
fn l9_label(primary: Option<u8>, custom: Option<[u16; 8]>) -> Option<String> {
    match (primary, custom) {
        (Some(255), Some(c)) => {
            let f = |x: u16, y: u16| (x as f64 / 32767.0, y as f64 / 32767.0);
            crate::hdr::primaries_label(f(c[0], c[1]), f(c[2], c[3]), f(c[4], c[5]), f(c[6], c[7]))
                .map(str::to_string)
                .or(Some("custom".to_string()))
        }
        (Some(idx), _) => Some(primary_name(idx)),
        (None, _) => None,
    }
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

/// L11 content-type names per Dolby's "Dolby Vision IQ - Content Type Metadata
/// (L11)" knowledge-base article. 0 is a defined value ("Default": the legacy
/// consumer experience, auto-added when tools upconvert a 4.0.2 XML to 5.1.0),
/// not reserved.
fn content_type_name(t: u8) -> String {
    match t {
        0 => "Default",
        1 => "Movies",
        2 => "Game",
        3 => "Sport",
        4 => "User Generated Content",
        _ => "Unknown",
    }
    .to_string()
}

/// L11 intended white point. Dolby publishes names only for 0 (D65, the
/// default) and 8 (D93); the rest of the 0..=15 range is accepted by metafier
/// but unnamed, so it renders as the raw code rather than a guess.
fn white_point_name(wp: u8) -> String {
    match wp {
        0 => "D65".to_string(),
        8 => "D93".to_string(),
        n => format!("code {n}"),
    }
}

/// The RPU DM header's (`source_min_pq`, `source_max_pq`) pair -> luminance in
/// nits. The max snaps to the standard mastering targets (the codes are 12-bit
/// quantisations of exactly those values: 3079 ≈ 1000, 3696 ≈ 4000); the min is
/// sub-nit (e.g. code 7 ≈ 0.0001), so it's rounded to 4 decimals instead.
fn source_pq_to_mastering((min, max): (u16, u16)) -> MasteringDisplay {
    MasteringDisplay {
        max_luminance: snap_nits(pq12_to_nits(max)) as f64,
        min_luminance: (pq12_to_nits(min) * 10000.0).round() / 10000.0,
        primaries: None,
        primaries_level: None,
    }
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

/// Dolby L8 `target_display_index` -> nits, for the predefined targets.
///
/// Values verified against metafier 5.5.0's canonical target-display table
/// (redefining a referenced target in a CM XML makes `--validate` warn with
/// the canonical `pq(0,N)`/`gamma_dci(0,N)` definition; probed per index).
/// 16/18/21 are 48-nit theatrical projector targets and 42 is the 108-nit
/// Dolby Cinema target — the index never encodes nits directly. Indices
/// metafier doesn't define (e.g. 20/22/23, which belong to the *mastering*
/// display ID namespace) are absent on purpose: dropped, never guessed.
fn l8_index_to_nits(idx: u8) -> Option<u32> {
    Some(match idx {
        1 => 100,
        16 | 18 | 21 => 48,
        24 | 25 => 300,
        27 | 28 => 600,
        37 | 38 => 2000,
        42 => 108,
        48 | 49 => 1000,
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

/// Merge the read L2/L8 trim targets and the title's L10-defined target
/// displays into the rendered set. L8 indices resolve to nits via the L10 map
/// (a custom index like 255 is defined by an L10 block in this title) or the
/// predefined table; an index with neither can't be stated in nits, so it's
/// dropped rather than guessed. Each nit value keeps the set of levels that
/// produced it, so a value present in both L2 and L8 reads `[L2/L8]`.
///
/// An L10-defined display counts as an L8 target whether or not a read L8
/// referenced it: L2 is self-contained (it carries its target's nits
/// directly), so a display index is a CM v4.0 (L8) mechanism by construction —
/// an L10 definition can serve nothing else. And unlike the per-shot trims,
/// the definition rides every RPU's global extension payload (it is the
/// compiled form of the CM XML's Level-0 target-display list — the displays
/// trims were authored for), so it is title-level evidence independent of
/// which shots a sample read. L10 itself never appears as a provenance tag;
/// it is bitstream plumbing, not a trim level.
fn merge_trim_targets(
    l2_nits: &BTreeSet<u32>,
    l8_indices: &BTreeSet<u8>,
    l10_targets: &BTreeMap<u8, u32>,
) -> Vec<TrimTarget> {
    let mut target_levels: BTreeMap<u32, BTreeSet<u8>> = BTreeMap::new();
    for &nits in l2_nits {
        target_levels.entry(nits).or_default().insert(2);
    }
    for &idx in l8_indices {
        if let Some(nits) = resolve_l8_nits(idx, l10_targets) {
            target_levels.entry(nits).or_default().insert(8);
        }
    }
    for &nits in l10_targets.values() {
        target_levels.entry(nits).or_default().insert(8);
    }
    target_levels
        .into_iter()
        .map(|(nits, levels)| TrimTarget { nits, levels: levels.into_iter().collect() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pq_decodes_the_common_mastering_ranges() {
        // The 12-bit PQ codes real titles carry: 3079/3696 are the quantised
        // 1000/4000-nit peaks (the P7 "1000-nit BL, 4000-nit grade" case), and
        // min code 7 is the ubiquitous 0.0001 cd/m² floor.
        let m = source_pq_to_mastering((7, 3079));
        assert_eq!(m.max_luminance, 1000.0);
        assert_eq!(m.min_luminance, 0.0001);
        let m = source_pq_to_mastering((7, 3696));
        assert_eq!(m.max_luminance, 4000.0);
        // Profile 8.4 (HLG) convention: 62/3079 -> 0.005 / 1000 nits.
        let m = source_pq_to_mastering((62, 3079));
        assert_eq!(m.min_luminance, 0.005);
    }

    #[test]
    fn l9_custom_chromaticities_match_known_gamuts() {
        // Index 255 carries raw 0.15 fixed-point chromaticities: P3 D65 encodes
        // as round(coord × 32767). Matched through the shared gamut matcher so
        // a custom-coded standard gamut still gets its name.
        let q = |v: f64| (v * 32767.0).round() as u16;
        let p3d65 = [
            q(0.680), q(0.320), q(0.265), q(0.690), q(0.150), q(0.060), q(0.3127), q(0.3290),
        ];
        assert_eq!(l9_label(Some(255), Some(p3d65)).as_deref(), Some("DCI-P3 D65"));
        // Off-gamut coordinates fall back to "custom", never a guessed name.
        let odd = [q(0.7), q(0.3), q(0.2), q(0.7), q(0.14), q(0.05), q(0.30), q(0.32)];
        assert_eq!(l9_label(Some(255), Some(odd)).as_deref(), Some("custom"));
        // Predefined indices keep the table name; no L9 at all stays None.
        assert_eq!(l9_label(Some(0), None).as_deref(), Some("DCI-P3 D65"));
        assert_eq!(l9_label(None, None), None);
    }

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
    fn l8_predefined_table_matches_dolby_canon() {
        // Metafier-verified canonical targets. Index 48 is the 1000-nit P3 D65
        // home target (a real disc: Speed Racer UHD carries L8 TIDs {1, 48});
        // the actual 48-nit displays are the theatrical 16/18/21. Mastering-
        // display-namespace IDs (20/22/23) must stay unmapped, not guessed.
        let none = BTreeMap::new();
        assert_eq!(resolve_l8_nits(48, &none), Some(1000));
        assert_eq!(resolve_l8_nits(49, &none), Some(1000));
        for cinema in [16, 18, 21] {
            assert_eq!(resolve_l8_nits(cinema, &none), Some(48));
        }
        assert_eq!(resolve_l8_nits(42, &none), Some(108), "Dolby Cinema");
        assert_eq!(resolve_l8_nits(28, &none), Some(600), "600-nit BT.2020");
        for unmapped in [20, 22, 23] {
            assert_eq!(resolve_l8_nits(unmapped, &none), None);
        }
    }

    #[test]
    fn reconstructed_bit_depth_is_signaled_and_fel_gated() {
        // DoviRpu has private fields, so it can't be built by struct literal
        // here; default-then-mutate the public ones the aggregate reads.
        let rpu = |el_type: Option<DoviELType>, seq_info: bool, vdr_minus8: u64| {
            let mut r = DoviRpu::default();
            r.dovi_profile = 7;
            r.el_type = el_type;
            r.header.vdr_seq_info_present_flag = seq_info;
            r.header.vdr_bit_depth_minus8 = vdr_minus8;
            r
        };
        let finalize = |r: DoviRpu| {
            let mut agg = DvAggregate::default();
            agg.add(&r);
            agg.finalize(3840, 2160, None, false, false, false).unwrap()
        };

        // A P7 FEL reports the header's signaled 12-bit VDR depth.
        let dv = finalize(rpu(Some(DoviELType::FEL), true, 4));
        assert_eq!(dv.reconstructed_bit_depth, Some(12));
        // A P4 FEL signals 14-bit (corpus-verified); reported verbatim, never
        // assumed to be 12 from "FEL".
        let dv = finalize(rpu(Some(DoviELType::FEL), true, 6));
        assert_eq!(dv.reconstructed_bit_depth, Some(14));
        // MEL signals a vdr depth too, but its residual is empty — composer
        // precision, not content depth — so the field stays absent.
        let dv = finalize(rpu(Some(DoviELType::MEL), true, 4));
        assert_eq!(dv.reconstructed_bit_depth, None);
        // Single-layer (no EL type): absent.
        let dv = finalize(rpu(None, true, 4));
        assert_eq!(dv.reconstructed_bit_depth, None);
        // No sequence-info block: the defaulted 0 must not read as "8-bit".
        let dv = finalize(rpu(Some(DoviELType::FEL), false, 0));
        assert_eq!(dv.reconstructed_bit_depth, None);
    }

    #[test]
    fn unconverted_dual_layer_rpu_needs_nlq_and_an_el_less_carriage() {
        let rpu = |el_type: Option<DoviELType>| {
            let mut r = DoviRpu::default();
            r.dovi_profile = 7;
            r.el_type = el_type;
            r
        };
        let cfg = |profile: u8, el_present: bool| DvConfig {
            profile,
            level: None,
            bl_present: true,
            el_present,
            rpu_present: true,
            bl_compatibility_id: Some(6),
        };
        let finalize = |r: DoviRpu, cfg: Option<&DvConfig>, is_av1: bool| {
            let mut agg = DvAggregate::default();
            agg.add(&r);
            agg.finalize(3840, 2160, cfg, false, is_av1, false).unwrap()
        };

        // The motivating case: a UHD-BD P7 MEL RPU injected into an AV1 mux
        // whose dvvC declares no EL (mkvmerge then writes profile 10 with the
        // RPU-guessed compat 6, the out-of-spec "10.6").
        let dv = finalize(rpu(Some(DoviELType::MEL)), Some(&cfg(10, false)), true);
        assert!(dv.unconverted_dual_layer_rpu);
        // The HEVC sibling: an unconverted FEL RPU under a single-layer dvcC.
        let dv = finalize(rpu(Some(DoviELType::FEL)), Some(&cfg(8, false)), false);
        assert!(dv.unconverted_dual_layer_rpu);
        // A real dual-layer mux declares its EL: never flagged.
        let dv = finalize(rpu(Some(DoviELType::FEL)), Some(&cfg(7, true)), false);
        assert!(!dv.unconverted_dual_layer_rpu);
        // A properly converted RPU carries no NLQ payload: nothing to flag.
        let dv = finalize(rpu(None), Some(&cfg(8, false)), false);
        assert!(!dv.unconverted_dual_layer_rpu);
        // Config-less raw HEVC may carry its EL in-band (a P7 BL+EL Annex-B
        // stream) — and a metadata-only RPU sidecar shares this state with no
        // carriage to compare at all: never inferred for either.
        let dv = finalize(rpu(Some(DoviELType::MEL)), None, false);
        assert!(!dv.unconverted_dual_layer_rpu);
        // Config-less raw AV1 is single-layer by construction: flagged.
        let dv = finalize(rpu(Some(DoviELType::MEL)), None, true);
        assert!(dv.unconverted_dual_layer_rpu);
    }

    #[test]
    fn derived_level_is_the_smallest_admitting_pixel_rate_and_width() {
        let derive = |w: u32, h: u32, fps: Option<f64>| {
            let cfg = DvConfig {
                profile: 7,
                level: None,
                bl_present: true,
                el_present: true,
                rpu_present: true,
                bl_compatibility_id: Some(6),
            };
            let mut dv = container_only(&cfg, false);
            fill_derived_level(&mut dv, w, h, fps);
            (dv.level, dv.level_derived)
        };

        // The motivating case: a genuine UHD-BD clip (no PMT descriptor),
        // 3840x2160 @ 23.976 — just under level 6's anchor rate.
        assert_eq!(derive(3840, 2160, Some(24000.0 / 1001.0)), (Some(6), true));
        // Each anchor format sits exactly at its own level's bound.
        assert_eq!(derive(1280, 720, Some(24.0)), (Some(1), true));
        assert_eq!(derive(1920, 1080, Some(24.0)), (Some(3), true));
        assert_eq!(derive(1920, 1080, Some(30000.0 / 1001.0)), (Some(4), true));
        assert_eq!(derive(3840, 2160, Some(60000.0 / 1001.0)), (Some(9), true));
        assert_eq!(derive(3840, 2160, Some(120.0)), (Some(10), true));
        // The equal-rate UHD@120 / 8K@30 pair splits on the width axis.
        assert_eq!(derive(7680, 4320, Some(30.0)), (Some(11), true));
        assert_eq!(derive(7680, 4320, Some(120.0)), (Some(13), true));
        // Beyond the table: absent, never a guess.
        assert_eq!(derive(7680, 4320, Some(144.0)), (None, false));
        // No frame rate / degenerate dimensions: absent, never a guess.
        assert_eq!(derive(3840, 2160, None), (None, false));
        assert_eq!(derive(3840, 2160, Some(0.0)), (None, false));
        assert_eq!(derive(0, 0, Some(24.0)), (None, false));

        // A declared container level always wins, unflagged.
        let cfg = DvConfig {
            profile: 7,
            level: Some(9),
            bl_present: true,
            el_present: true,
            rpu_present: true,
            bl_compatibility_id: Some(6),
        };
        let mut dv = container_only(&cfg, false);
        fill_derived_level(&mut dv, 3840, 2160, Some(24000.0 / 1001.0));
        assert_eq!((dv.level, dv.level_derived), (Some(9), false));
    }

    #[test]
    fn dual_track_fold_reports_the_logical_tracks_layers() {
        // On a real dual-track mux the surviving config is the EL sub-stream's
        // declaration (dvcC / TS 0xB0 with bl_present == 0, truthful for that
        // stream's own carriage) — but the merged report describes the logical
        // track, which holds both layers by construction of the fold.
        let el_cfg = DvConfig {
            profile: 7,
            level: Some(6),
            bl_present: false,
            el_present: true,
            rpu_present: true,
            bl_compatibility_id: Some(6),
        };
        let finalize = |cfg: &DvConfig, dual_track: bool| {
            let mut agg = DvAggregate::default();
            let mut r = DoviRpu::default();
            r.dovi_profile = 7;
            agg.add(&r);
            agg.finalize(3840, 2160, Some(cfg), false, false, dual_track).unwrap()
        };

        let dv = finalize(&el_cfg, true);
        assert!(dv.bl_present && dv.el_present && dv.rpu_present);
        assert_eq!(dv.structure.as_deref(), Some("Dual track, dual layer"));
        // A genuinely BL-less input (EL-only cut, nothing folded) keeps its
        // declared absence.
        let dv = finalize(&el_cfg, false);
        assert!(!dv.bl_present);

        // The --no-rpu config-only path follows the same rule.
        let dv = container_only(&el_cfg, true);
        assert!(dv.bl_present && dv.el_present);
        assert_eq!(dv.structure.as_deref(), Some("Dual track, dual layer"));
        assert!(!container_only(&el_cfg, false).bl_present);

        // A dual-track group's carriage demonstrably has an EL, so a config
        // declaring none must not fire the unconverted-RPU verdict there.
        let bl_cfg = DvConfig { bl_present: true, el_present: false, ..el_cfg.clone() };
        let finalize_mel = |cfg: &DvConfig, dual_track: bool| {
            let mut agg = DvAggregate::default();
            let mut r = DoviRpu::default();
            r.dovi_profile = 7;
            r.el_type = Some(DoviELType::MEL);
            agg.add(&r);
            agg.finalize(3840, 2160, Some(cfg), false, false, dual_track).unwrap()
        };
        assert!(!finalize_mel(&bl_cfg, true).unconverted_dual_layer_rpu);
        assert!(finalize_mel(&bl_cfg, false).unconverted_dual_layer_rpu);
    }

    #[test]
    fn fel_brightness_expansion_needs_fel_and_a_brighter_grade() {
        let dv = |el: Option<&str>, rpu_max: f64| {
            let cfg = DvConfig {
                profile: 7,
                level: None,
                bl_present: true,
                el_present: true,
                rpu_present: true,
                bl_compatibility_id: Some(6),
            };
            let mut d = container_only(&cfg, false);
            d.el_type = el.map(str::to_string);
            d.mastering_display = Some(MasteringDisplay {
                max_luminance: rpu_max,
                min_luminance: 0.0001,
                primaries: None,
                primaries_level: None,
            });
            d
        };
        // The canonical case: a 4000-nit FEL grade over a 1000-nit HDR10 base.
        let mut d = dv(Some("FEL"), 4000.0);
        flag_fel_brightness_expansion(&mut d, Some(1000.0));
        let x = d.fel_brightness_expansion.expect("expansion flagged");
        assert_eq!((x.bl_max_nits, x.rpu_max_nits), (1000.0, 4000.0));
        // A MEL's residual is empty; it can never out-bright the BL.
        let mut d = dv(Some("MEL"), 4000.0);
        flag_fel_brightness_expansion(&mut d, Some(1000.0));
        assert!(d.fel_brightness_expansion.is_none());
        // Matching targets (and sub-margin noise) stay unflagged.
        let mut d = dv(Some("FEL"), 4000.0);
        flag_fel_brightness_expansion(&mut d, Some(4000.0));
        assert!(d.fel_brightness_expansion.is_none());
        let mut d = dv(Some("FEL"), 1000.0);
        flag_fel_brightness_expansion(&mut d, Some(994.0));
        assert!(d.fel_brightness_expansion.is_none());
        // No (or zeroed) BL mastering: no comparison, never a guess.
        let mut d = dv(Some("FEL"), 4000.0);
        flag_fel_brightness_expansion(&mut d, None);
        assert!(d.fel_brightness_expansion.is_none());
        let mut d = dv(Some("FEL"), 4000.0);
        flag_fel_brightness_expansion(&mut d, Some(0.0));
        assert!(d.fel_brightness_expansion.is_none());
    }

    #[test]
    fn mastering_primaries_mismatch_needs_l9_and_a_differing_signalled_label() {
        let dv = |primaries: Option<&str>, level: Option<u8>| {
            let cfg = DvConfig {
                profile: 8,
                level: None,
                bl_present: true,
                el_present: false,
                rpu_present: true,
                bl_compatibility_id: Some(1),
            };
            let mut d = container_only(&cfg, false);
            d.mastering_display = Some(MasteringDisplay {
                max_luminance: 4000.0,
                min_luminance: 0.0001,
                primaries: primaries.map(str::to_string),
                primaries_level: level,
            });
            d
        };
        // The classic drift: a BT.2020-claiming MDCV over a P3-D65 L9.
        let mut d = dv(Some("DCI-P3 D65"), Some(9));
        flag_mastering_primaries_mismatch(&mut d, Some("BT.2020"));
        let m = d.mastering_primaries_mismatch.expect("mismatch flagged");
        assert_eq!((m.bl_primaries.as_str(), m.rpu_primaries.as_str()), ("BT.2020", "DCI-P3 D65"));
        // Agreement stays unflagged.
        let mut d = dv(Some("DCI-P3 D65"), Some(9));
        flag_mastering_primaries_mismatch(&mut d, Some("DCI-P3 D65"));
        assert!(d.mastering_primaries_mismatch.is_none());
        // No recognized L9 (a CM v2.9 luminance-only display): no comparison.
        let mut d = dv(None, None);
        flag_mastering_primaries_mismatch(&mut d, Some("BT.2020"));
        assert!(d.mastering_primaries_mismatch.is_none());
        // A non-L9 provenance (a DV XML's Level-0) never fires the flag.
        let mut d = dv(Some("DCI-P3 D65"), Some(0));
        flag_mastering_primaries_mismatch(&mut d, Some("BT.2020"));
        assert!(d.mastering_primaries_mismatch.is_none());
        // No signalled BL primaries (unrecognized or absent MDCV): silent.
        let mut d = dv(Some("DCI-P3 D65"), Some(9));
        flag_mastering_primaries_mismatch(&mut d, None);
        assert!(d.mastering_primaries_mismatch.is_none());
    }

    #[test]
    fn l10_definition_overrides_predefined_table() {
        // If a title redefines a predefined index via L10, the title's own
        // definition wins (it describes the actual target on this master).
        let l10 = BTreeMap::from([(27u8, 1000u32)]);
        assert_eq!(resolve_l8_nits(27, &l10), Some(1000));
    }

    #[test]
    fn l10_defined_target_missing_from_read_trims_is_surfaced_as_l8() {
        // A custom 300-nit display defined by L10 whose L8 trims were never
        // read (buried in unsampled shots): the definition is global — it rides
        // every RPU — and a display index is an L8-only mechanism, so the
        // target renders as an L8 target.
        let l10 = BTreeMap::from([(255u8, 300u32)]);
        let targets = merge_trim_targets(&BTreeSet::new(), &BTreeSet::new(), &l10);
        assert_eq!(targets.len(), 1);
        assert_eq!((targets[0].nits, targets[0].levels.as_slice()), (300, &[8u8][..]));
        // A read L8 for the same display dedupes into the same entry.
        let targets = merge_trim_targets(&BTreeSet::new(), &BTreeSet::from([255u8]), &l10);
        assert_eq!(targets.len(), 1);
        assert_eq!((targets[0].nits, targets[0].levels.as_slice()), (300, &[8u8][..]));
    }

    /// The shared real RPU (a dovi_tool `.bin` payload): the cadence tests
    /// need genuine DM data with extension blocks behind the fingerprint.
    fn real_rpu() -> DoviRpu {
        crate::dv::rpu::parse_avc_rpu(crate::dv::rpu::TEST_AVC_RPU_NAL).expect("test RPU parses")
    }

    /// The same RPU with its L1 analysis nudged — a frame carrying its own
    /// per-frame values.
    fn edited_rpu() -> DoviRpu {
        let mut r = real_rpu();
        let dm = r.vdr_dm_data.as_mut().expect("test RPU has DM data");
        match dm.level_blocks_iter_mut(1).next() {
            Some(ExtMetadataBlock::Level1(b)) => b.avg_pq ^= 1,
            _ => panic!("test RPU carries an L1 block"),
        }
        r
    }

    #[test]
    fn dm_fingerprint_tracks_content_not_the_scene_flag() {
        let base = real_rpu();
        let base_fp = dm_fingerprint(base.vdr_dm_data.as_ref().unwrap());
        // The scene_refresh_flag is a cut marker, not content: a shot's first
        // frame must fingerprint equal to the rest of its shot.
        let mut flagged = real_rpu();
        flagged.vdr_dm_data.as_mut().unwrap().scene_refresh_flag ^= 1;
        assert_eq!(dm_fingerprint(flagged.vdr_dm_data.as_ref().unwrap()), base_fp);
        // A changed analysis value must change the fingerprint.
        let edited = edited_rpu();
        assert_ne!(dm_fingerprint(edited.vdr_dm_data.as_ref().unwrap()), base_fp);
    }

    #[test]
    fn cadence_counts_pairs_across_folds_and_weighted_runs() {
        let base = real_rpu();
        let edited = edited_rpu();
        let mut agg = DvAggregate::default();
        agg.track_consecutive();
        agg.add_repeated(&base, 10, 1); // a shot: run of 10 identical frames
        agg.add(&edited); // one frame carrying its own values
        agg.add_repeated(&base, 3, 0); // back to the shot's values
        let dv = agg.finalize(3840, 2160, None, true, false, false).unwrap();
        let cad = dv.metadata_cadence.expect("consecutive folds yield a verdict");
        // 9 internal pairs, boundary (change), boundary (change), 2 internal.
        assert_eq!((cad.frame_pairs, cad.changed_pairs), (13, 2));
        assert_eq!(cad.cadence, "per-shot");
    }

    #[test]
    fn cadence_frequent_changes_read_per_frame() {
        let base = real_rpu();
        let edited = edited_rpu();
        let mut agg = DvAggregate::default();
        agg.track_consecutive();
        for _ in 0..3 {
            agg.add(&base);
            agg.add(&edited);
        }
        let dv = agg.finalize(3840, 2160, None, true, false, false).unwrap();
        let cad = dv.metadata_cadence.unwrap();
        assert_eq!((cad.frame_pairs, cad.changed_pairs), (5, 5));
        assert_eq!(cad.cadence, "per-frame");
        // Per-frame analysis riding duplicated frames (a high-rate stream
        // repeating each analysed frame) changes at only ~half of all pairs;
        // still comfortably past the quarter line.
        let mut agg = DvAggregate::default();
        agg.track_consecutive();
        for _ in 0..3 {
            agg.add_repeated(&base, 2, 0);
            agg.add_repeated(&edited, 2, 0);
        }
        let dv = agg.finalize(3840, 2160, None, true, false, false).unwrap();
        let cad = dv.metadata_cadence.unwrap();
        assert_eq!((cad.frame_pairs, cad.changed_pairs), (11, 5));
        assert_eq!(cad.cadence, "per-frame");
    }

    #[test]
    fn cadence_absent_without_consecutive_tracking_or_pairs() {
        // Untracked folds (the sampled default's scattered frames): no verdict,
        // never a guess — even though these two frames happen to differ.
        let mut agg = DvAggregate::default();
        agg.add(&real_rpu());
        agg.add(&edited_rpu());
        let dv = agg.finalize(3840, 2160, None, false, false, false).unwrap();
        assert!(dv.metadata_cadence.is_none());
        // A single tracked RPU has no pair to compare.
        let mut agg = DvAggregate::default();
        agg.track_consecutive();
        agg.add(&real_rpu());
        let dv = agg.finalize(3840, 2160, None, true, false, false).unwrap();
        assert!(dv.metadata_cadence.is_none());
    }

    #[test]
    fn merged_targets_keep_the_combined_provenance_shape() {
        // A 100-nit target from both L2 and L8, a 600-nit L2-only value, and an
        // unread 300-nit L10 definition: [L2/L8], [L2], [L8], sorted by nits.
        let l2 = BTreeSet::from([100u32, 600]);
        let l8 = BTreeSet::from([1u8]); // predefined 100-nit target
        let l10 = BTreeMap::from([(255u8, 300u32)]);
        let targets = merge_trim_targets(&l2, &l8, &l10);
        let shape: Vec<(u32, &[u8])> =
            targets.iter().map(|t| (t.nits, t.levels.as_slice())).collect();
        assert_eq!(
            shape,
            vec![(100, &[2u8, 8][..]), (300, &[8u8][..]), (600, &[2u8][..])]
        );
    }
}
