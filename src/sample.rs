//! Sampling strategy + parallel RPU extraction.
//!
//! Picks a head-weighted spread of access units (or all, with `--full`),
//! NAL/OBU-splits each in parallel, and feeds type-62 RPUs into the DV
//! aggregator.

use std::collections::BTreeSet;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use rayon::prelude::*;

use crate::avc::nal as avc_nal;
use crate::container::{ts, Chunk, Codec, Demux, NalFormat};
use crate::dv::levels::DvAggregate;
use crate::dv::rpu::{parse_avc_rpu, parse_hevc_rpu};
use crate::hdr::sei::{self, SeiFindings};
use crate::hevc::nal::{self, NalRef};
use crate::prefetch::Frontier;
use crate::progress::{Phase, Progress};

pub struct Options {
    pub samples: usize,
    pub full: bool,
    pub no_rpu: bool,
}

pub struct Scan {
    pub dv: DvAggregate,
    pub sei: SeiFindings,
    /// Exact completed-AU byte total of a `--full` TS/M2TS streaming walk —
    /// equal to what the old whole-stream reassembly buffer held (the trailing
    /// partial AU is excluded by both), so `main.rs` can compute the same
    /// video-stream bitrate. `Some(0)` is meaningful (no video bytes ⇒ no
    /// rate, as before); `None` on every other path.
    pub es_bytes: Option<u64>,
}

/// RPUs plus SEI findings from a single access unit.
struct ChunkScan {
    rpus: Vec<DoviRpu>,
    sei: SeiFindings,
}

/// Access units extracted per rayon batch before sequential aggregation, so a
/// `--full` scan never holds every frame's parsed RPU alive at once.
/// Deliberately larger than any realistic `--samples`, so the bounded default
/// path stays a single batch — the same shape as the old one-shot collect.
///
/// Aggregation order is load-bearing: `DvAggregate` has first-wins fields and
/// its L5 insertion order is the rendered order, and `SeiFindings::merge` is
/// first-wins — so batches run in index order and aggregate sequentially
/// within each batch (rayon's indexed collect preserves order). Never replace
/// this with a parallel reduce of partial aggregates.
const AGG_BATCH: usize = 1024;

pub fn scan(
    demux: &Demux,
    data: &[u8],
    opts: &Options,
    progress: &Progress,
    frontier: &Frontier,
) -> Scan {
    // TS/M2TS `--full`: the elementary stream was never materialized by demux;
    // stream it here in bounded windows. Checked before the empty-chunks early
    // return — the head metadata window may hold no completed AU even though
    // the stream has plenty — and before `no_rpu`, which still needs the walk's
    // byte count for the exact video bitrate.
    if let Some(plan) = demux.ts_stream.as_ref() {
        return scan_ts_full(data, plan, demux, opts, progress, frontier);
    }

    if opts.no_rpu || demux.chunks.is_empty() {
        return Scan { dv: DvAggregate::default(), sei: SeiFindings::default(), es_bytes: None };
    }

    let indices = select_indices(demux.chunks.len(), opts.samples, opts.full, demux.sps_chunk);

    // Chunks index into the reassembled elementary stream when the container
    // provides one (TS/M2TS), else directly into the mmap. The frontier only
    // means anything against the file, so a heap-buffer source gets the no-op.
    let source: &[u8] = demux.reassembled.as_deref().unwrap_or(data);
    let off = Frontier::off();
    let frontier = if demux.reassembled.is_none() { frontier } else { &off };

    let selected: Vec<Chunk> = indices.iter().map(|&i| demux.chunks[i]).collect();
    progress.begin(Phase::Scan, selected.iter().map(|c| c.size).sum());
    let mut dv = DvAggregate::default();
    let mut sei = SeiFindings::default();
    scan_chunks(source, &selected, demux.nal_format, &demux.codec, &mut dv, &mut sei, progress, frontier);

    Scan { dv, sei, es_bytes: None }
}

/// Extract RPUs + SEI from `chunks` in parallel batches, aggregating each
/// batch sequentially in index order (see `AGG_BATCH` for why order matters),
/// so peak liveness is one batch of parsed RPUs rather than all of them.
#[allow(clippy::too_many_arguments)] // two aggregators in, two passive sinks along for the ride
fn scan_chunks(
    source: &[u8],
    chunks: &[Chunk],
    fmt: NalFormat,
    codec: &Codec,
    dv: &mut DvAggregate,
    sei: &mut SeiFindings,
    progress: &Progress,
    frontier: &Frontier,
) {
    // Progress and frontier tick at the batch boundary — on the aggregating
    // thread, between rayon collects, never inside the par_iter closure (both
    // sinks are single-threaded by design).
    let mut done: u64 = 0;
    for batch in chunks.chunks(AGG_BATCH) {
        // Warm the batch's file span before its parallel extraction faults it
        // (chunks are file-ordered on every mmap-indexed container, so the
        // remote reads stay linear, one batch span at a time).
        if let Some(last) = batch.last() {
            frontier.ensure_to(last.offset + last.size);
        }
        let outs: Vec<ChunkScan> =
            batch.par_iter().map(|&c| extract_chunk(source, c, fmt, codec)).collect();
        for out in &outs {
            for rpu in &out.rpus {
                dv.add(rpu);
            }
            sei.merge(&out.sei);
        }
        done += batch.iter().map(|c| c.size).sum::<u64>();
        progress.update(done);
    }
}

/// `--full` TS/M2TS: drive the resumable reassembler over the whole stream in
/// bounded windows, scanning each window's access units with the ordinary
/// batch machinery and reusing one scratch buffer (capacity is retained across
/// `clear`, so steady state is a single ~`STREAM_WINDOW_BYTES` allocation).
/// Under `--no-rpu` the walk still runs, extraction skipped, purely to count
/// the completed-AU bytes the exact video bitrate needs.
fn scan_ts_full(
    data: &[u8],
    plan: &ts::TsFullStream,
    demux: &Demux,
    opts: &Options,
    progress: &Progress,
    frontier: &Frontier,
) -> Scan {
    let mut st = plan.streamer();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut dv = DvAggregate::default();
    let mut sei = SeiFindings::default();
    let mut es_bytes: u64 = 0;
    // Progress by the streamer's file cursor against the whole mmap — the walk
    // reads every packet, so file position is the honest denominator. The
    // per-window `scan_chunks` gets no-op sinks; the window loop owns the
    // phase, and its chunk offsets index the scratch buffer, not the file.
    progress.begin(Phase::Scan, data.len() as u64);
    // One window consumes more *file* bytes than its ES target (packet
    // overhead, other PIDs), so the frontier warms the upcoming window's file
    // span, adapted from the last window's observed density.
    let mut warm_span = ts::STREAM_WINDOW_BYTES as u64 * 2;
    loop {
        buf.clear();
        chunks.clear();
        let pos0 = st.position() as u64;
        frontier.ensure_to(pos0.saturating_add(warm_span));
        let more = st.next_window(data, &mut buf, &mut chunks, ts::STREAM_WINDOW_BYTES);
        let used = st.position() as u64 - pos0;
        if used > 0 {
            warm_span = used + used / 4;
        }
        es_bytes += buf.len() as u64;
        if !opts.no_rpu {
            scan_chunks(
                &buf,
                &chunks,
                demux.nal_format,
                &demux.codec,
                &mut dv,
                &mut sei,
                &Progress::off(),
                &Frontier::off(),
            );
        }
        if !more {
            // The cursor stops short of EOF by a partial packet; pin 100%.
            progress.update(data.len() as u64);
            return Scan { dv, sei, es_bytes: Some(es_bytes) };
        }
        progress.update(st.position() as u64);
    }
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

/// Choose access-unit indices to sample: a head run plus an even spread, plus
/// `must_include` (the demux's `sps_chunk`) when the container located one.
/// That index is the first RAP access unit — the AU the per-GOP prefix SEIs
/// (HLG alt-transfer, mastering, CLL) ride — and in a stream that starts
/// mid-GOP (common for TS captures) neither the head run nor the sparse
/// spread reliably lands on a RAP, so it is pinned explicitly.
///
/// `pub(crate)` because `prefetch::warm_sample_chunks` calls it with the same
/// inputs to warm exactly the chunks `scan` will fault on a network volume —
/// selection must stay deterministic so the two never diverge.
pub(crate) fn select_indices(
    n: usize,
    samples: usize,
    full: bool,
    must_include: Option<usize>,
) -> Vec<usize> {
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
    if let Some(m) = must_include {
        if m < n {
            set.insert(m);
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColorInfo;

    #[test]
    fn ts_full_scan_streams_and_counts_es_bytes() {
        // Dual-PID stream mirroring ts.rs's streamer tests: three completed AUs
        // totalling 9 ES bytes; trailing partials on both PIDs excluded.
        let mut data = Vec::new();
        data.extend(ts_packet(0x100, true, &pes_start(&[1, 2, 3])));
        data.extend(ts_packet(0x200, true, &pes_start(&[9, 9])));
        data.extend(ts_packet(0x100, false, &[4, 5]));
        data.extend(ts_packet(0x200, false, &[8]));
        data.extend(ts_packet(0x100, true, &pes_start(&[6])));
        data.extend(ts_packet(0x200, true, &pes_start(&[7, 7])));
        data.extend(ts_packet(0x100, true, &pes_start(&[0xAB])));
        data.extend(ts_packet(0x200, false, &[0xCD]));

        let layout = ts::detect_layout(&data).expect("layout");
        let demux = Demux {
            container: "MPEG-2 TS",
            codec: Codec::Hevc,
            nal_format: NalFormat::AnnexB,
            width: 0,
            height: 0,
            fps: None,
            duration_secs: None,
            bit_depth: None,
            chroma: None,
            codec_profile: None,
            stereo: None,
            color: ColorInfo::default(),
            dv_config: None,
            dv_dual_track: true,
            mastering: None,
            content_light: None,
            bitrate: None,
            chunks: Vec::new(), // head window empty: the plan must still walk
            sps_chunk: None,
            reassembled: Some(Vec::new()),
            ts_stream: Some(ts::TsFullStream::new(layout, vec![0x100, 0x200])),
        };

        let opts = Options { samples: 16, full: true, no_rpu: false };
        assert_eq!(scan(&demux, &data, &opts, &Progress::off(), &Frontier::off()).es_bytes, Some(9));
        // --no-rpu still walks the stream: the exact byte count is what the
        // full-path bitrate is computed from.
        let opts = Options { samples: 16, full: true, no_rpu: true };
        assert_eq!(scan(&demux, &data, &opts, &Progress::off(), &Frontier::off()).es_bytes, Some(9));
    }

    /// One 188-byte TS packet carrying exactly `payload`, adaptation-stuffed.
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0x00 };
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x30;
        let af_len = 188 - 4 - payload.len() - 1;
        pkt[4] = af_len as u8;
        if af_len > 0 {
            pkt[5] = 0x00;
        }
        let start = 5 + af_len;
        pkt[start..start + payload.len()].copy_from_slice(payload);
        pkt
    }

    /// A PES start whose ES payload is `es` (header_data_length = 0).
    fn pes_start(es: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        v.extend_from_slice(es);
        v
    }

    /// An Annex-B HEVC prefix-SEI NAL carrying a content-light message.
    fn sei_cll_nal(max_cll: u16, max_fall: u16) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x01, 0x4E, 0x01, 0x90, 0x04];
        v.extend_from_slice(&max_cll.to_be_bytes());
        v.extend_from_slice(&max_fall.to_be_bytes());
        v.push(0x80); // rbsp trailing
        v
    }

    #[test]
    fn batched_aggregation_preserves_first_wins_order() {
        // More chunks than one batch, so the loop spans batches. Chunk 0
        // carries CLL (100, 50); a chunk in the second batch carries
        // (999, 999). First-wins must see chunk 0's values regardless of
        // batching, and the second batch must still be visited at all.
        let n = AGG_BATCH + 7;
        let mut source = Vec::new();
        let mut chunks = Vec::new();
        for i in 0..n {
            let nal = if i == 0 {
                sei_cll_nal(100, 50)
            } else if i == AGG_BATCH + 3 {
                sei_cll_nal(999, 999)
            } else {
                vec![0x00, 0x00, 0x01, 0x02, 0x01, 0x00] // non-SEI filler NAL
            };
            chunks.push(Chunk { offset: source.len() as u64, size: nal.len() as u64 });
            source.extend_from_slice(&nal);
        }
        let mut dv = DvAggregate::default();
        let mut sei = SeiFindings::default();
        scan_chunks(&source, &chunks, NalFormat::AnnexB, &Codec::Hevc, &mut dv, &mut sei, &Progress::off(), &Frontier::off());
        let cl = sei.content_light.expect("cll aggregated");
        assert_eq!((cl.max_cll, cl.max_fall), (100, 50));

        // Only the second-batch chunk carries CLL: it must be reachable too.
        let mut sei = SeiFindings::default();
        scan_chunks(&source[..], &chunks[1..], NalFormat::AnnexB, &Codec::Hevc, &mut dv, &mut sei, &Progress::off(), &Frontier::off());
        let cl = sei.content_light.expect("second batch scanned");
        assert_eq!((cl.max_cll, cl.max_fall), (999, 999));
    }

    #[test]
    fn select_indices_pins_the_sps_chunk() {
        // A TS-like shape: many AUs, few samples, and the RAP at an index the
        // head run and even spread both miss (the LG HLG demo's layout: first
        // IDR ~25 AUs in, spread stride ~92).
        let picked = select_indices(1101, 16, false, Some(25));
        assert!(picked.contains(&25), "the SPS/RAP chunk must always be sampled");
        // Without the pin the same inputs must miss it (guards against the
        // spread accidentally covering it and the test asserting nothing).
        assert!(!select_indices(1101, 16, false, None).contains(&25));
        // Out-of-range pin (defensive; a demux bug) is ignored, not a panic.
        assert!(select_indices(10, 4, false, Some(99)).iter().all(|&i| i < 10));
        // Full / small-file paths already take every chunk.
        assert!(select_indices(8, 16, false, Some(3)).contains(&3));
    }
}
