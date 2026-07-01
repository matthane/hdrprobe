//! Raw Dolby Vision RPU stream (dovi_tool `extract-rpu` output): Annex-B
//! start-code-delimited RPU NALs, one per frame, with no picture data. dovi_tool
//! omits the `7C 01` UNSPEC62 NAL header, so each NAL begins with the
//! `rpu_nal_prefix` (0x19) rather than a type-62 header — we therefore can't
//! filter by NAL type. Instead, like libdovi's own `parse_rpu_file`, we hand
//! every NAL to the panic-guarded parser and keep the ones that validate
//! (libdovi checks framing and a CRC32, so non-RPU NALs are rejected cleanly).

use anyhow::Result;
use rayon::prelude::*;

use crate::dv::rpu::parse_hevc_rpu;
use crate::hevc::nal::{self, NalRef};

use super::{dv_from_rpus, Payload, ASSUMED_CANVAS};

pub fn parse(data: &[u8]) -> Result<Payload> {
    let mut nals: Vec<NalRef> = Vec::new();
    nal::split_annexb(data, &mut nals);

    // Unlike the DV XML — where per-shot metadata was redundantly expanded to one
    // clone per frame — every NAL here is a genuine per-frame RPU that must be
    // parsed. But each parse is independent and CPU-bound, so we fan them across
    // the rayon pool exactly as the video-sampling path does. `par_iter` +
    // `collect` preserves NAL order, so the subsequent fold is bit-for-bit
    // identical to the sequential version.
    let rpus: Vec<_> = nals
        .par_iter()
        .filter_map(|n| parse_hevc_rpu(&data[n.start..n.end]))
        .collect();

    // A raw RPU bin bakes L5 offsets in real pixels but records no resolution,
    // so — like the DV XML — we size the active area against an assumed UHD
    // canvas and flag it as assumed (the offsets themselves stay exact).
    dv_from_rpus(&rpus, Some(ASSUMED_CANVAS))
}
