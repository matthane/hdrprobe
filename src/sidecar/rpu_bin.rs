//! Raw Dolby Vision RPU stream (dovi_tool `extract-rpu` output): Annex-B
//! start-code-delimited RPU NALs, one per frame, with no picture data. dovi_tool
//! omits the `7C 01` UNSPEC62 NAL header, so each NAL begins with the
//! `rpu_nal_prefix` (0x19) rather than a type-62 header — we therefore can't
//! filter by NAL type. Instead, like libdovi's own `parse_rpu_file`, we hand
//! every NAL to the panic-guarded parser and keep the ones that validate
//! (libdovi checks framing and a CRC32, so non-RPU NALs are rejected cleanly).

use anyhow::Result;
use dolby_vision::rpu::dovi_rpu::DoviRpu;
use rayon::prelude::*;

use crate::dv::levels::DvAggregate;
use crate::dv::rpu::parse_hevc_rpu;
use crate::hevc::nal::{self, NalRef};

use super::{finalize_dv, Payload, ASSUMED_CANVAS};

pub fn parse(data: &[u8]) -> Result<Payload> {
    let mut nals: Vec<NalRef> = Vec::new();
    nal::split_annexb(data, &mut nals);

    // Dolby CM metadata is per-shot, so within a shot every frame's RPU is
    // byte-identical (only the shot's first frame differs, by its
    // scene_refresh_flag) — a feature's ~200k per-frame NALs collapse to a few
    // thousand runs of consecutive identical bytes. Identical bytes parse to the
    // identical RPU, so parsing one representative per run and folding it
    // weighted by the run length yields the exact census of parsing every frame,
    // at a fraction of the work — the same shot-collapse the DV XML path
    // performs. Content with genuinely per-frame RPUs just degenerates to one
    // run per NAL, costing only this memcmp pass.
    let mut runs: Vec<(&[u8], usize)> = Vec::new();
    for n in &nals {
        let bytes = &data[n.start..n.end];
        match runs.last_mut() {
            Some((prev, count)) if *prev == bytes => *count += 1,
            _ => runs.push((bytes, 1)),
        }
    }

    // Each run's parse is independent and CPU-bound, so fan them across the
    // rayon pool exactly as the video-sampling path does. `par_iter` + `collect`
    // preserves run order, so the fold below sees first occurrences in stream
    // order and the aggregate is identical to the sequential version.
    let rpus: Vec<(DoviRpu, usize)> = runs
        .par_iter()
        .filter_map(|&(bytes, frames)| parse_hevc_rpu(bytes).map(|rpu| (rpu, frames)))
        .collect();

    let mut agg = DvAggregate::default();
    for (rpu, frames) in &rpus {
        // All frames in a run share the RPU's own scene_refresh_flag, so the
        // scene-cut count weights the same way the per-frame fold would.
        let scene_cuts = rpu
            .vdr_dm_data
            .as_ref()
            .map_or(0, |dm| if dm.scene_refresh_flag == 1 { *frames } else { 0 });
        agg.add_repeated(rpu, *frames, scene_cuts);
    }

    // A raw RPU bin bakes L5 offsets in real pixels but records no resolution,
    // so — like the DV XML — we size the active area against an assumed UHD
    // canvas and flag it as assumed (the offsets themselves stay exact).
    finalize_dv(agg, Some(ASSUMED_CANVAS))
}
