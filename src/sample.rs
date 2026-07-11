//! Sampling strategy + parallel RPU extraction.
//!
//! Picks a head-weighted spread of access units (or all, with `--full`),
//! NAL/OBU-splits each in parallel, and feeds type-62 RPUs into the DV
//! aggregator.

use std::collections::BTreeSet;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use rayon::prelude::*;

use crate::avc::nal as avc_nal;
use crate::container::{annexb, av1, ts, Chunk, Codec, Demux, NalFormat, RawFullStream};
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

/// Per-file scan result: one `TrackScan` per `Demux::tracks` entry, parallel
/// and in the same order.
pub struct Scan {
    pub tracks: Vec<TrackScan>,
}

impl Scan {
    /// One default `TrackScan` per demux track (the `--no-rpu` / empty shape).
    fn empty(demux: &Demux) -> Scan {
        Scan { tracks: demux.tracks.iter().map(|_| TrackScan::default()).collect() }
    }
}

/// What the scan learned about one video track.
#[derive(Default)]
pub struct TrackScan {
    pub dv: DvAggregate,
    pub sei: SeiFindings,
    /// Exact completed-AU byte total of a `--full` TS/M2TS streaming walk —
    /// equal to what the old whole-stream reassembly buffer held (the trailing
    /// partial AU is excluded by both), so `main.rs` can compute the same
    /// video-stream bitrate. `Some(0)` is meaningful (no video bytes ⇒ no
    /// rate, as before); `None` on every other path. The MKV `--full`
    /// streaming walk fills it too (exact summed block bytes — what the old
    /// exhaustive index summed in `mkv::demux`).
    pub es_bytes: Option<u64>,
    /// Exact video-block count of a `--full` MKV streaming walk (feeds the fps
    /// fallback, count ÷ duration, the demux computed from its own complete
    /// index before the walk moved here) and of the `--full` raw AV1 fused
    /// walks (temporal-delimiter / IVF frame count). `None` elsewhere.
    pub frame_count: Option<u64>,
    /// Whole-stream average fps measured by the `--full` raw IVF fused walk —
    /// the value the demux-time exhaustive walk used to compute before that
    /// walk moved here. `None` on every other path (raw OBU's rate comes from
    /// the sequence header, already on the track's `fps`).
    pub fps: Option<f64>,
    /// Duration (frame count ÷ fps) recovered by the `--full` raw AV1 fused
    /// walks — raw AV1 has no duration box, so it exists only once the whole
    /// stream has been walked. `None` on every other path.
    pub duration_secs: Option<f64>,
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

    // MKV `--full`: demux kept only its bounded head index; walk every cluster
    // here, extracting each window's blocks as they are discovered (index and
    // scan fused — one pass over the file). Also before the `no_rpu` early
    // return: the walk's byte and frame totals feed the exact bitrate and the
    // fps fallback either way.
    if let Some(plan) = demux.mkv_stream.as_ref() {
        return scan_mkv_full(data, plan, demux, opts, progress, frontier);
    }

    // Raw elementary streams `--full`: demux kept only its bounded head walk;
    // split the whole stream here, extracting completed access units batch by
    // batch as they are discovered (index and scan fused — one pass over the
    // file, like MKV/TS above). Also before the `no_rpu` early return: the raw
    // AV1 walk's frame count feeds the exact duration either way.
    if let Some(plan) = demux.raw_stream.as_ref() {
        return scan_raw_full(data, plan, demux, opts, progress, frontier);
    }

    if opts.no_rpu || demux.tracks.iter().all(|t| t.chunks.is_empty()) {
        return Scan::empty(demux);
    }

    // Single track — the overwhelming majority — keeps the exact historical
    // call sequence: one selection, one scan_chunks pass.
    if demux.tracks.len() == 1 {
        let track = &demux.tracks[0];
        let indices = select_indices(track.chunks.len(), opts.samples, opts.full, track.sps_chunk);

        // Chunks index into the reassembled elementary stream when the container
        // provides one (TS/M2TS), else directly into the mmap. The frontier only
        // means anything against the file, so a heap-buffer source gets the no-op.
        let source: &[u8] = track.reassembled.as_deref().unwrap_or(data);
        let off = Frontier::off();
        let frontier = if track.reassembled.is_none() { frontier } else { &off };

        let selected: Vec<Chunk> = indices.iter().map(|&i| track.chunks[i]).collect();
        progress.begin(Phase::Scan, selected.iter().map(|c| c.size).sum());
        let mut ts = TrackScan::default();
        // Under `--full` the selection is every AU in decode order, so consecutive
        // folds are adjacent frames and the cadence verdict has real pairs to
        // compare. The sampled default folds scattered AUs — a pair spanning a
        // sampling gap would read as a change — so it stays untracked (no verdict).
        if opts.full {
            ts.dv.track_consecutive();
        }
        scan_chunks(source, &selected, track.nal_format, &track.codec, &mut ts.dv, &mut ts.sei, progress, frontier);
        // Container-carried T.35 (MKV BlockAdditions): file ranges inside the
        // demux's head block window, already warmed — never buffer offsets.
        merge_t35_chunks(data, &track.t35_chunks, &mut ts.sei);
        return Scan { tracks: vec![ts] };
    }

    // Multiple tracks. Mmap-backed containers (MKV, MP4) scan one merged
    // file-ordered pass so the remote frontier stays linear and the file
    // crosses the wire once; TS tracks each index their own reassembled
    // buffer, so they scan per track against it (no file I/O is at stake —
    // the buffers are already in memory — and the default path's progress
    // sink is Off by construction).
    let mut tracks: Vec<TrackScan> = demux.tracks.iter().map(|_| TrackScan::default()).collect();
    if opts.full {
        for ts in tracks.iter_mut() {
            ts.dv.track_consecutive();
        }
    }
    if demux.tracks.iter().any(|t| t.reassembled.is_some()) {
        progress.begin(Phase::Scan, 0);
        for (track, ts) in demux.tracks.iter().zip(tracks.iter_mut()) {
            let indices =
                select_indices(track.chunks.len(), opts.samples, opts.full, track.sps_chunk);
            let source: &[u8] = track.reassembled.as_deref().unwrap_or(data);
            let selected: Vec<Chunk> = indices.iter().map(|&i| track.chunks[i]).collect();
            scan_chunks(
                source,
                &selected,
                track.nal_format,
                &track.codec,
                &mut ts.dv,
                &mut ts.sei,
                &Progress::off(),
                &Frontier::off(),
            );
        }
    } else {
        let items = select_track_chunks(demux, opts.samples, opts.full);
        progress.begin(Phase::Scan, items.iter().map(|(_, c)| c.size).sum());
        scan_chunks_routed(data, &items, demux, &mut tracks, progress, frontier);
    }
    for (track, ts) in demux.tracks.iter().zip(tracks.iter_mut()) {
        merge_t35_chunks(data, &track.t35_chunks, &mut ts.sei);
    }
    Scan { tracks }
}

/// The default path's per-track sampled selection, merged into file order —
/// each track's own `select_indices` (with its own SPS pin), then a stable
/// merge by chunk offset so the pass over the file stays linear. `pub(crate)`
/// because `prefetch::warm_sample_chunks` replays it with identical inputs:
/// sharing the function is what keeps the warm and the sampler from drifting.
/// Tracks whose chunks index a reassembled buffer (TS) are excluded — their
/// offsets are not file positions.
pub(crate) fn select_track_chunks(demux: &Demux, samples: usize, full: bool) -> Vec<(usize, Chunk)> {
    let mut items: Vec<(usize, Chunk)> = Vec::new();
    for (ti, track) in demux.tracks.iter().enumerate() {
        if track.reassembled.is_some() {
            continue;
        }
        for i in select_indices(track.chunks.len(), samples, full, track.sps_chunk) {
            items.push((ti, track.chunks[i]));
        }
    }
    // Each track's selection is already file-ordered; the stable sort is a
    // merge by offset that preserves per-track index order.
    items.sort_by_key(|(_, c)| c.offset);
    items
}

/// The multi-track sibling of `scan_chunks`: one merged, file-ordered pass in
/// the same `AGG_BATCH` rayon batches, routing each access unit's results into
/// its track's aggregates during the sequential per-batch fold. Within a track
/// the items keep that track's index order (stable merge), so the first-wins /
/// L5-order / cadence semantics are per-track exactly as the single-track pass
/// — never a parallel reduce of partial aggregates.
fn scan_chunks_routed(
    data: &[u8],
    items: &[(usize, Chunk)],
    demux: &Demux,
    tracks: &mut [TrackScan],
    progress: &Progress,
    frontier: &Frontier,
) {
    let mut done: u64 = 0;
    for batch in items.chunks(AGG_BATCH) {
        if let Some((_, last)) = batch.last() {
            frontier.ensure_to(last.offset + last.size);
        }
        let outs: Vec<(usize, ChunkScan)> = batch
            .par_iter()
            .map(|&(ti, c)| {
                let t = &demux.tracks[ti];
                (ti, extract_chunk(data, c, t.nal_format, &t.codec))
            })
            .collect();
        for (ti, out) in &outs {
            let ts = &mut tracks[*ti];
            for rpu in &out.rpus {
                ts.dv.add(rpu);
            }
            ts.sei.merge(&out.sei);
        }
        done += batch.iter().map(|(_, c)| c.size).sum::<u64>();
        progress.update(done);
    }
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
    // Windows arrive routed per track group, each scanned into its own
    // aggregates; the scratch buffers are reused across windows.
    let mut outs: Vec<ts::EsOut> = (0..plan.track_count()).map(|_| ts::EsOut::default()).collect();
    let mut tracks: Vec<TrackScan> = demux.tracks.iter().map(|_| TrackScan::default()).collect();
    for ts in tracks.iter_mut() {
        // Windows arrive sequentially and each group's completed AUs are in
        // stream order, so folds are adjacent frames — cadence pairs are real.
        ts.dv.track_consecutive();
        ts.es_bytes = Some(0);
    }
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
        for o in outs.iter_mut() {
            o.clear();
        }
        let pos0 = st.position() as u64;
        frontier.ensure_to(pos0.saturating_add(warm_span));
        let more = st.next_window(data, &mut outs, ts::STREAM_WINDOW_BYTES);
        let used = st.position() as u64 - pos0;
        if used > 0 {
            warm_span = used + used / 4;
        }
        for ((out, ts), track) in outs.iter().zip(tracks.iter_mut()).zip(&demux.tracks) {
            *ts.es_bytes.get_or_insert(0) += out.buf.len() as u64;
            if !opts.no_rpu {
                scan_chunks(
                    &out.buf,
                    &out.chunks,
                    track.nal_format,
                    &track.codec,
                    &mut ts.dv,
                    &mut ts.sei,
                    &Progress::off(),
                    &Frontier::off(),
                );
            }
        }
        if !more {
            // The cursor stops short of EOF by a partial packet; pin 100%.
            progress.update(data.len() as u64);
            return Scan { tracks };
        }
        progress.update(st.position() as u64);
    }
}

/// `--full` MKV: drive the resumable cluster walker over the whole Segment in
/// bounded windows, scanning each window's blocks with the ordinary batch
/// machinery as they are discovered — the index pass and the scan pass fused,
/// so the file is read once, in order, at any size (the old shape indexed
/// every cluster in demux and then re-read every block here; on a remote file
/// larger than RAM that meant two transfers). Chunks are absolute file ranges
/// (unlike TS's buffer-relative ones), already warmed by the walker's frontier
/// ticks. Under `--no-rpu` the walk still runs, extraction skipped, to count
/// the exact block bytes (bitrate) and blocks (fps fallback).
fn scan_mkv_full(
    data: &[u8],
    plan: &crate::container::mkv::MkvFullStream,
    demux: &Demux,
    opts: &Options,
    progress: &Progress,
    frontier: &Frontier,
) -> Scan {
    let mut st = plan.streamer();
    // Blocks arrive routed per track, each window's batches extracted into
    // their track's aggregates — still one walk over the clusters. T.35
    // BlockAdditions (HDR10+ on VP9) ride a parallel routed list.
    let mut outs: Vec<Vec<Chunk>> = vec![Vec::new(); plan.track_count()];
    let mut t35_outs: Vec<Vec<Chunk>> = vec![Vec::new(); plan.track_count()];
    let mut tracks: Vec<TrackScan> = demux.tracks.iter().map(|_| TrackScan::default()).collect();
    for ts in tracks.iter_mut() {
        // Cluster windows arrive sequentially with blocks in stream order, so
        // folds are adjacent frames — cadence pairs are real.
        ts.dv.track_consecutive();
        ts.es_bytes = Some(0);
        ts.frame_count = Some(0);
    }
    // One phase, one bar: position over the whole mmap, like the TS walk.
    progress.begin(Phase::Scan, data.len() as u64);
    loop {
        for o in outs.iter_mut().chain(t35_outs.iter_mut()) {
            o.clear();
        }
        let more = st.next_window(
            data,
            &mut outs,
            &mut t35_outs,
            crate::container::mkv::STREAM_SPAN_BYTES,
            frontier,
        );
        for ((chunks, ts), track) in outs.iter().zip(tracks.iter_mut()).zip(&demux.tracks) {
            *ts.es_bytes.get_or_insert(0) += chunks.iter().map(|c| c.size).sum::<u64>();
            *ts.frame_count.get_or_insert(0) += chunks.len() as u64;
            if !opts.no_rpu {
                scan_chunks(
                    data,
                    chunks,
                    track.nal_format,
                    &track.codec,
                    &mut ts.dv,
                    &mut ts.sei,
                    &Progress::off(),
                    frontier,
                );
            }
        }
        if !opts.no_rpu {
            for (chunks, ts) in t35_outs.iter().zip(tracks.iter_mut()) {
                merge_t35_chunks(data, chunks, &mut ts.sei);
            }
        }
        if !more {
            progress.update(data.len() as u64);
            return Scan { tracks };
        }
        progress.update(st.position() as u64);
    }
}

/// `--full` raw elementary streams (Annex-B HEVC, AV1 OBU/IVF): drive the
/// format's whole-stream walk here, batching completed access units into the
/// ordinary `scan_chunks` machinery as they are discovered — the old
/// demux-time index pass and the scan pass fused, so the file is read once,
/// in order, at any size (the two-pass shape crossed the wire twice on a
/// remote file larger than RAM). Chunks are absolute mmap ranges, the walk's
/// byte position drives the single `Scan` phase and the remote-read frontier.
/// Under `--no-rpu` the walk still runs count-only — the AV1 frame count is
/// what the exact duration is computed from — with extraction skipped.
fn scan_raw_full(
    data: &[u8],
    plan: &RawFullStream,
    demux: &Demux,
    opts: &Options,
    progress: &Progress,
    frontier: &Frontier,
) -> Scan {
    progress.begin(Phase::Scan, data.len() as u64);
    // Raw elementary streams are single-track by definition.
    let mut dv = DvAggregate::default();
    // The fused walk emits completed AUs in stream order, batch after batch,
    // so folds are adjacent frames — cadence pairs are real.
    dv.track_consecutive();
    let mut sei = SeiFindings::default();
    let mut pending: Vec<Chunk> = Vec::with_capacity(AGG_BATCH);

    // One tick site for all three walks: the walk position is monotonic and
    // covers every byte, so it is the honest bar denominator.
    let tick = |pos: usize| {
        frontier.ensure(pos as u64);
        progress.update(pos as u64);
    };
    // Extraction at every `AGG_BATCH` completed AUs, right behind the walk
    // front while those pages are still resident — peak liveness stays one
    // batch of parsed RPUs, exactly like `scan_chunks` over an indexed file.
    macro_rules! push_au {
        () => {
            |c: Chunk| {
                pending.push(c);
                if pending.len() >= AGG_BATCH {
                    flush_raw_batch(data, &mut pending, demux, opts.no_rpu, &mut dv, &mut sei, frontier);
                }
            }
        };
    }

    let (frame_count, fps) = match plan {
        RawFullStream::HevcAnnexB => {
            annexb::walk_aus(data, tick, push_au!());
            // A raw HEVC stream has no duration source (VUI timing gives a
            // rate, not a length), so nothing beyond the extraction comes
            // back from the walk.
            (None, None)
        }
        RawFullStream::Av1Obu => {
            let count = av1::walk_obu_tus(data, tick, push_au!());
            (count, None)
        }
        RawFullStream::Ivf { data_start, ticks_per_sec } => {
            let walk = av1::walk_ivf_frames(data, *data_start, data.len(), tick, push_au!());
            let fps = av1::ivf_fps(walk.frames, walk.span, *ticks_per_sec);
            ((walk.frames > 0).then_some(walk.frames as u64), fps)
        }
    };
    flush_raw_batch(data, &mut pending, demux, opts.no_rpu, &mut dv, &mut sei, frontier);
    progress.update(data.len() as u64);

    // Raw AV1's duration is frames ÷ fps — the same product the old demux-time
    // exhaustive walk fed `build_demux`, now known only after this walk. The
    // rate is the walk's own measurement (IVF) or the sequence header's
    // constant rate already on the demux (OBU); raw HEVC stays duration-less.
    let duration_secs = match (frame_count, fps.or(demux.tracks[0].fps)) {
        (Some(n), Some(f)) if f > 0.0 => Some(n as f64 / f),
        _ => None,
    };
    Scan { tracks: vec![TrackScan { dv, sei, es_bytes: None, frame_count, fps, duration_secs }] }
}

/// Extract one accumulated batch of the raw fused walk's access units (unless
/// `--no-rpu` skips extraction), then clear it for the next batch.
fn flush_raw_batch(
    data: &[u8],
    pending: &mut Vec<Chunk>,
    demux: &Demux,
    no_rpu: bool,
    dv: &mut DvAggregate,
    sei: &mut SeiFindings,
    frontier: &Frontier,
) {
    if !no_rpu && !pending.is_empty() {
        let track = &demux.tracks[0];
        scan_chunks(data, pending, track.nal_format, &track.codec, dv, sei, &Progress::off(), frontier);
    }
    pending.clear();
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
        // VP9 has no in-band SEI/RPU side channel: its static HDR rides the
        // container's colour signalling and its HDR10+ rides MKV
        // BlockAdditions (`TrackDemux::t35_chunks`, merged separately) — the
        // frame bytes themselves carry nothing to extract.
        Codec::Vp9 => {}
        // ProRes has no bitstream side channel at all: static HDR rides the
        // container's colour/mastering signalling, dynamic HDR does not exist
        // for ProRes carriage, and DV masters pair with CM XML sidecars (the
        // sidecar path). Nothing to extract from the frame bytes.
        Codec::ProRes => {}
        Codec::Other(_) => {}
    }
    ChunkScan { rpus, sei: sei_findings }
}

/// Merge container-carried ITU-T T.35 payloads (MKV `BlockAdditional` ranges,
/// the HDR10+ carriage for VP9 in WebM) into a track's findings, gated the
/// same way as the AV1 T.35 OBU route: only the HDR10+ signature is parsed,
/// anything else ignored. First-wins like `SeiFindings::merge`.
fn merge_t35_chunks(data: &[u8], chunks: &[Chunk], sei: &mut SeiFindings) {
    for c in chunks {
        if sei.hdr10plus.is_some() {
            return;
        }
        let start = c.offset as usize;
        let end = (c.offset + c.size) as usize;
        let Some(p) = data.get(start..end) else { continue };
        if let Some(h) = sei::parse_hdr10plus(p) {
            sei.hdr10plus = Some(h);
        }
    }
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
    use crate::container::TrackDemux;

    /// Scan a single-track demux and unwrap its one `TrackScan`.
    fn scan1(demux: &Demux, data: &[u8], opts: &Options) -> TrackScan {
        let mut s = scan(demux, data, opts, &Progress::off(), &Frontier::off());
        assert_eq!(s.tracks.len(), demux.tracks.len());
        s.tracks.swap_remove(0)
    }

    #[test]
    fn multi_track_scan_keeps_per_track_findings_isolated() {
        // Two mmap-backed tracks whose AUs carry different CLL SEIs,
        // interleaved in file order: each track's aggregates must see only its
        // own values — no cross-track leakage through the merged routed pass.
        let cll_a = sei_cll_nal(1000, 400);
        let cll_b = sei_cll_nal(300, 60);
        let mut data = cll_a.clone();
        data.extend_from_slice(&cll_b);
        let t = |offset: u64, size: u64| TrackDemux {
            chunks: vec![Chunk { offset, size }],
            ..TrackDemux::new(Codec::Hevc, NalFormat::AnnexB)
        };
        let demux = Demux {
            container: "Matroska",
            duration_secs: None,
            tracks: vec![t(0, cll_a.len() as u64), t(cll_a.len() as u64, cll_b.len() as u64)],
            ts_stream: None,
            mkv_stream: None,
            raw_stream: None,
        };
        let opts = Options { samples: 16, full: false, no_rpu: false };
        let s = scan(&demux, &data, &opts, &Progress::off(), &Frontier::off());
        assert_eq!(s.tracks.len(), 2);
        let a = s.tracks[0].sei.content_light.expect("track 1 CLL");
        let b = s.tracks[1].sei.content_light.expect("track 2 CLL");
        assert_eq!((a.max_cll, a.max_fall), (1000, 400));
        assert_eq!((b.max_cll, b.max_fall), (300, 60));
    }

    #[test]
    fn select_track_chunks_merges_in_file_order_with_pins() {
        // Interleaved chunk offsets across two tracks merge by offset, keeping
        // each track's own index order and its SPS pin; a reassembled-buffer
        // track contributes nothing (its offsets are not file positions).
        let mk = |offsets: &[u64], sps: Option<usize>| TrackDemux {
            chunks: offsets.iter().map(|&o| Chunk { offset: o, size: 1 }).collect(),
            sps_chunk: sps,
            ..TrackDemux::new(Codec::Hevc, NalFormat::AnnexB)
        };
        let mut ts_track = mk(&[5], None);
        ts_track.reassembled = Some(Vec::new());
        let demux = Demux {
            container: "Matroska",
            duration_secs: None,
            tracks: vec![mk(&[0, 20, 40, 60], Some(3)), mk(&[10, 30, 50], None), ts_track],
            ts_stream: None,
            mkv_stream: None,
            raw_stream: None,
        };
        // A tiny budget still includes each track's head run and pin.
        let items = select_track_chunks(&demux, 2, false);
        assert!(items.windows(2).all(|w| w[0].1.offset <= w[1].1.offset), "file-ordered");
        assert!(items.iter().all(|&(ti, _)| ti < 2), "reassembled track excluded");
        assert!(
            items.iter().any(|&(ti, c)| ti == 0 && c.offset == 60),
            "track 0's SPS pin (index 3) survives the merge"
        );
        // Full selection covers every chunk of both mmap tracks.
        let items = select_track_chunks(&demux, 2, true);
        assert_eq!(items.len(), 7);
    }

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
        let track = TrackDemux {
            dv_dual_track: true,
            // head window empty: the plan must still walk
            reassembled: Some(Vec::new()),
            ..TrackDemux::new(Codec::Hevc, NalFormat::AnnexB)
        };
        let mut demux = Demux::single("MPEG-2 TS", None, track);
        demux.ts_stream = Some(ts::TsFullStream::new(layout, vec![0x100, 0x200]));

        let opts = Options { samples: 16, full: true, no_rpu: false };
        assert_eq!(scan1(&demux, &data, &opts).es_bytes, Some(9));
        // --no-rpu still walks the stream: the exact byte count is what the
        // full-path bitrate is computed from.
        let opts = Options { samples: 16, full: true, no_rpu: true };
        assert_eq!(scan1(&demux, &data, &opts).es_bytes, Some(9));
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

    /// Minimal EBML element with a 1-byte size (payloads stay < 127).
    fn ebml(id: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 0x7F);
        let mut v = id.to_vec();
        v.push(0x80 | payload.len() as u8);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn mkv_full_scan_streams_blocks_and_counts() {
        // Segment { Cluster{SimpleBlock(CLL SEI)}, Cluster{SimpleBlock(filler)} }
        // walked via the streaming plan: exact byte/frame totals either way,
        // extraction only without --no-rpu.
        let cll = sei_cll_nal(300, 60);
        let filler = vec![0x00, 0x00, 0x01, 0x02, 0x01, 0x00];
        let block = |payload: &[u8]| {
            let mut b = vec![0x81, 0x00, 0x00, 0x00]; // track 1, timecode, flags
            b.extend_from_slice(payload);
            ebml(&[0xA3], &b)
        };
        let cluster_id = [0x1F, 0x43, 0xB6, 0x75];
        let mut seg = ebml(&cluster_id, &block(&cll));
        seg.extend(ebml(&cluster_id, &block(&filler)));
        let data = ebml(&[0x18, 0x53, 0x80, 0x67], &seg);
        let seg_start = data.len() - seg.len();

        // head window ignored: the plan walks
        let track = TrackDemux {
            width: 3840,
            height: 2160,
            ..TrackDemux::new(Codec::Hevc, NalFormat::AnnexB)
        };
        let mut demux = Demux::single("Matroska", None, track);
        demux.mkv_stream =
            Some(crate::container::mkv::MkvFullStream::new(seg_start, data.len(), 1));
        let total = (cll.len() + filler.len()) as u64;

        let opts = Options { samples: 16, full: true, no_rpu: false };
        let s = scan1(&demux, &data, &opts);
        assert_eq!(s.es_bytes, Some(total));
        assert_eq!(s.frame_count, Some(2));
        let cl = s.sei.content_light.expect("CLL extracted from the streamed block");
        assert_eq!((cl.max_cll, cl.max_fall), (300, 60));

        // --no-rpu: the walk still yields the exact totals, extraction skipped.
        let opts = Options { samples: 16, full: true, no_rpu: true };
        let s = scan1(&demux, &data, &opts);
        assert_eq!(s.es_bytes, Some(total));
        assert_eq!(s.frame_count, Some(2));
        assert!(s.sei.content_light.is_none());
    }

    /// A minimal `Demux` for the raw fused-walk tests: chunks empty (the plan
    /// walks), everything else inert.
    fn raw_demux(codec: Codec, fps: Option<f64>, plan: RawFullStream) -> Demux {
        let nal_format = match codec {
            Codec::Hevc => NalFormat::AnnexB,
            _ => NalFormat::LengthPrefixed(0),
        };
        let track = TrackDemux { fps, ..TrackDemux::new(codec, nal_format) };
        let mut demux = Demux::single("raw", None, track);
        demux.raw_stream = Some(plan);
        demux
    }

    #[test]
    fn raw_hevc_full_scan_fuses_walk_and_extraction() {
        // Three AUs: [prefix SEI (CLL) + VCL], [VCL], [VCL] — the fused walk
        // must extract from the AUs it discovers, with no demux index at all.
        let cll = sei_cll_nal(777, 88);
        let vcl = vec![0x00, 0x00, 0x01, 0x02, 0x01, 0x00];
        let mut data = cll.clone();
        for _ in 0..3 {
            data.extend_from_slice(&vcl);
        }
        let demux = raw_demux(Codec::Hevc, None, RawFullStream::HevcAnnexB);

        let opts = Options { samples: 16, full: true, no_rpu: false };
        let s = scan1(&demux, &data, &opts);
        let cl = s.sei.content_light.expect("CLL from the fused walk");
        assert_eq!((cl.max_cll, cl.max_fall), (777, 88));
        // Raw HEVC has no duration source; nothing else comes back.
        assert_eq!(s.es_bytes, None);
        assert_eq!(s.frame_count, None);
        assert_eq!(s.duration_secs, None);

        // --no-rpu: the walk is count-only, extraction skipped.
        let opts = Options { samples: 16, full: true, no_rpu: true };
        let s = scan1(&demux, &data, &opts);
        assert!(s.sei.content_light.is_none());
    }

    #[test]
    fn raw_obu_full_scan_counts_frames_and_derives_duration() {
        // TD, [CLL metadata OBU], TD, TD → three temporal units. Frame count
        // and duration (count ÷ the sequence header's rate, here 24 fps on the
        // demux) must come back with or without extraction.
        let td = [0x12u8, 0x00];
        let mut data = td.to_vec();
        data.extend_from_slice(&[0x2A, 0x05, 0x01, 0x03, 0xE8, 0x01, 0x90]); // CLL 1000/400
        data.extend_from_slice(&td);
        data.extend_from_slice(&td);
        let demux = raw_demux(Codec::Av1, Some(24.0), RawFullStream::Av1Obu);

        let opts = Options { samples: 16, full: true, no_rpu: false };
        let s = scan1(&demux, &data, &opts);
        assert_eq!(s.frame_count, Some(3));
        assert_eq!(s.duration_secs, Some(3.0 / 24.0));
        assert_eq!(s.fps, None, "OBU rate is the sequence header's, not the walk's");
        let cl = s.sei.content_light.expect("CLL from the fused walk");
        assert_eq!((cl.max_cll, cl.max_fall), (1000, 400));

        let opts = Options { samples: 16, full: true, no_rpu: true };
        let s = scan1(&demux, &data, &opts);
        assert_eq!(s.frame_count, Some(3), "--no-rpu still walks for the totals");
        assert_eq!(s.duration_secs, Some(3.0 / 24.0));
        assert!(s.sei.content_light.is_none());
    }

    #[test]
    fn raw_ivf_full_scan_measures_fps_and_duration() {
        // Minimal IVF, 24 ticks/sec, three frames at ts 0,1,2: demuxed under
        // `--full` the head walk defers fps/duration to the fused scan, which
        // must measure 24 fps and 0.125 s — what the old demux-time exhaustive
        // walk produced.
        let mut data = vec![0u8; 32];
        data[0..4].copy_from_slice(b"DKIF");
        data[6..8].copy_from_slice(&32u16.to_le_bytes());
        data[16..20].copy_from_slice(&24u32.to_le_bytes());
        data[20..24].copy_from_slice(&1u32.to_le_bytes());
        data[24..28].copy_from_slice(&3u32.to_le_bytes());
        for ts in 0u64..3 {
            data.extend_from_slice(&2u32.to_le_bytes());
            data.extend_from_slice(&ts.to_le_bytes());
            data.extend_from_slice(&[0x12, 0x00]); // one TD OBU per frame
        }

        let demux = crate::container::av1::demux(&data, true, &Progress::off(), &Frontier::off())
            .expect("valid IVF");
        assert_eq!(demux.tracks[0].fps, None, "under --full the fused scan owns the rate");
        assert_eq!(demux.duration_secs, None);
        assert!(demux.raw_stream.is_some());

        let opts = Options { samples: 16, full: true, no_rpu: false };
        let s = scan1(&demux, &data, &opts);
        assert_eq!(s.frame_count, Some(3));
        assert_eq!(s.fps, Some(24.0));
        assert_eq!(s.duration_secs, Some(0.125));
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
