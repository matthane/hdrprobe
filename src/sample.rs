//! Sampling strategy + parallel RPU extraction.
//!
//! Picks a head-weighted spread of access units (or all, with `--full`),
//! NAL/OBU-splits each in parallel, and feeds type-62 RPUs into the DV
//! aggregator.

use std::collections::BTreeSet;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use rayon::prelude::*;

use crate::avc::nal as avc_nal;
use crate::container::{Chunk, Codec, Demux, NalFormat};
use crate::dv::levels::DvAggregate;
use crate::dv::rpu::{parse_avc_rpu, parse_hevc_rpu};
use crate::hdr::sei::{self, SeiFindings};
use crate::hevc::nal::{self, NalRef};

pub struct Options {
    pub samples: usize,
    pub full: bool,
    pub no_rpu: bool,
}

pub struct Scan {
    pub dv: DvAggregate,
    pub sei: SeiFindings,
}

/// RPUs plus SEI findings from a single access unit.
struct ChunkScan {
    rpus: Vec<DoviRpu>,
    sei: SeiFindings,
}

pub fn scan(demux: &Demux, data: &[u8], opts: &Options) -> Scan {
    if opts.no_rpu || demux.chunks.is_empty() {
        return Scan { dv: DvAggregate::default(), sei: SeiFindings::default() };
    }

    let indices = select_indices(demux.chunks.len(), opts.samples, opts.full);

    // Chunks index into the reassembled elementary stream when the container
    // provides one (TS/M2TS), else directly into the mmap.
    let source: &[u8] = demux.reassembled.as_deref().unwrap_or(data);

    // Extract RPUs + SEI in parallel across sampled chunks.
    let outs: Vec<ChunkScan> = indices
        .par_iter()
        .map(|&i| extract_chunk(source, demux.chunks[i], demux.nal_format, &demux.codec))
        .collect();

    let mut dv = DvAggregate::default();
    let mut merged_sei = SeiFindings::default();
    for out in &outs {
        for rpu in &out.rpus {
            dv.add(rpu);
        }
        merged_sei.merge(&out.sei);
    }

    Scan { dv, sei: merged_sei }
}

/// RPUs + SEI findings from a single access unit's NAL stream.
fn extract_chunk(data: &[u8], chunk: Chunk, fmt: NalFormat, codec: &Codec) -> ChunkScan {
    let start = chunk.offset as usize;
    let end = (chunk.offset + chunk.size) as usize;
    if end > data.len() || start >= end {
        return ChunkScan { rpus: Vec::new(), sei: SeiFindings::default() };
    }
    let slice = &data[start..end];

    let mut rpus = Vec::new();
    let mut sei_findings = SeiFindings::default();
    match codec {
        Codec::Hevc => {
            let mut nals: Vec<NalRef> = Vec::new();
            match fmt {
                NalFormat::AnnexB => nal::split_annexb(slice, &mut nals),
                NalFormat::LengthPrefixed(n) => nal::split_length_prefixed(slice, n, &mut nals),
            }
            for n in nals {
                match n.nal_type {
                    nal::NAL_UNSPEC62_RPU => {
                        if let Some(rpu) = parse_hevc_rpu(&slice[n.start..n.end]) {
                            rpus.push(rpu);
                        }
                    }
                    nal::NAL_PREFIX_SEI | nal::NAL_SUFFIX_SEI => {
                        sei_findings.merge(&sei::parse_sei_nal(&slice[n.start..n.end]));
                    }
                    _ => {}
                }
            }
        }
        Codec::Avc => {
            let mut nals: Vec<avc_nal::NalRef> = Vec::new();
            match fmt {
                NalFormat::AnnexB => avc_nal::split_annexb(slice, &mut nals),
                NalFormat::LengthPrefixed(n) => avc_nal::split_length_prefixed(slice, n, &mut nals),
            }
            for n in nals {
                let payload = &slice[n.start..n.end];
                if n.nal_type == avc_nal::NAL_SEI {
                    sei_findings.merge(&sei::parse_sei_nal_avc(payload));
                } else if avc_nal::is_unspecified(n.nal_type) {
                    // Content-verify before parsing: an unspecified NAL is only a
                    // DV RPU if its payload starts with the `0x19` rpu_nal_prefix
                    // (byte after the 1-byte NAL header). Guards against treating
                    // some other unspecified NAL as an RPU.
                    if payload.get(1) == Some(&0x19) {
                        if let Some(rpu) = parse_avc_rpu(payload) {
                            rpus.push(rpu);
                        }
                    }
                }
            }
        }
        Codec::Av1 => {
            let scan = crate::av1::obu::scan_obus(slice);
            rpus = scan.rpus;
            sei_findings = scan.sei;
        }
        Codec::Other(_) => {}
    }
    ChunkScan { rpus, sei: sei_findings }
}

/// Choose access-unit indices to sample: a head run plus an even spread.
///
/// `pub(crate)` because `prefetch::warm_sample_chunks` calls it with the same
/// inputs to warm exactly the chunks `scan` will fault on a network volume —
/// selection must stay deterministic so the two never diverge.
pub(crate) fn select_indices(n: usize, samples: usize, full: bool) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    if full || n <= samples.max(1) {
        return (0..n).collect();
    }
    let samples = samples.max(1);
    let mut set: BTreeSet<usize> = BTreeSet::new();

    // Head: first third of the budget, where most static levels appear.
    let head = (samples / 3).max(1);
    for i in 0..head.min(n) {
        set.insert(i);
    }
    // Spread the remainder across the rest of the file.
    let remaining = samples.saturating_sub(set.len());
    for k in 0..remaining {
        let pos = (k + 1) * (n - 1) / (remaining + 1);
        set.insert(pos);
    }
    set.into_iter().collect()
}
