//! Network-filesystem prefetch warmer.
//!
//! Parsing reads the file through the `mmap`, so every byte the demuxer touches
//! is served by a page fault. Locally that's microseconds; over SMB/NFS each
//! fault region that isn't cached becomes a *synchronous* network round-trip
//! with almost none of the pipelined read-ahead a sequential `read()` would get.
//! A metadata scan that touches a few hundred scattered 4 KiB regions then costs
//! hundreds of RTTs — the multi-hundred-ms "same file is 25 ms local / 800 ms on
//! the NAS" gap.
//!
//! This module warms the byte ranges we're about to parse with pipelined
//! positioned reads, in two stages: `warm_metadata` before demux (container
//! metadata regions, per backend) and `warm_sample_chunks` after demux (the
//! exact access units the sampler will fault). Both feed `warm_ranges`, which
//! coalesces overlapping ranges and streams them concurrently so one range's
//! network latency hides another's. On Windows the memory-mapped section and
//! cached `ReadFile` share the Cache Manager's pages for the same file, so the
//! warm read populates exactly the pages the subsequent mmap faults will hit; the
//! same holds for the shared page cache on Linux CIFS/NFS. Parsing still runs
//! against the mmap — nothing is copied into the report path, so the zero-copy
//! `Chunk` model is untouched. Warming only affects *timing*, never what we parse.
//!
//! It is gated to remote volumes on Windows (`is_remote`, decided from the open
//! handle at zero network cost) so the tight local path is unchanged.

use std::fs::File;
use std::path::Path;

use crate::container::Demux;

/// Generic head window for remote files whose front working set can't be
/// resolved by exact extent: raw AV1 and raw HEVC (their bounded head walks),
/// an MKV whose SeekHead has no Cluster entry (fallback below), and
/// unrecognized formats. TS/M2TS is warmed with a larger window
/// (`ts::HEAD_SCAN_BYTES`) since its in-band SPS sits a GOP in; MP4 and
/// extent-resolved MKV with smaller ones (`MP4_HEAD_WARM` / `MKV_HEAD_WARM`)
/// since their real regions are warmed by exact extent.
///
/// The raw bounded head walks (`av1::HEAD_SCAN_BYTES`, `annexb::HEAD_SCAN_BYTES`)
/// are deliberately kept `<=` this so the warm covers them whole; shrink this
/// below them and those windows' tails fault in one page at a time on the NAS
/// again. The MKV fallback relies on this covering the first block offset +
/// `mkv::HEAD_SPAN_BYTES`.
const HEAD_WARM: usize = 8 << 20; // 8 MiB

/// Head window for ISOBMFF (a `moov` was found): everything the MP4 path reads
/// is warmed by exact extent — the `moov` itself, a front `sidx`'s fragment
/// heads, and the sampled AUs (`warm_sample_chunks`, whose head-run AUs sit at
/// the start of `mdat`) — so a generic multi-MiB head would mostly stream
/// `mdat` bytes nothing parses. That waste is pure transfer time: ~8 MiB is
/// ~80 ms of a NAS probe at 1 GbE, which used to dominate the whole scan. This
/// covers `ftyp` and incidental front boxes only.
const MP4_HEAD_WARM: usize = 1 << 20; // 1 MiB

/// Head window for an MKV whose first-Cluster offset resolved
/// (`mkv::head_blocks_extent`): the block walk's span is then warmed by exact
/// extent, so the generic head would only re-stream bytes that extent already
/// covers (or skipped attachment payloads the walk never reads). This holds
/// the front metadata the demux walks element-by-element — EBML header,
/// SeekHead, Info, Tracks — which sits well inside 1 MiB in practice; a
/// front element pushed past it by attachments faults in a handful of
/// id/size headers, not payloads. Without a resolved Cluster offset the
/// generic `HEAD_WARM` fallback applies (see its coupling note).
const MKV_HEAD_WARM: usize = 1 << 20; // 1 MiB

#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetFileInformationByHandleEx(
        h_file: *mut std::ffi::c_void,
        file_information_class: u32,
        file_information: *mut std::ffi::c_void,
        buffer_size: u32,
    ) -> i32;
}
/// `FILE_INFO_BY_HANDLE_CLASS::FileRemoteProtocolInfo`.
#[cfg(windows)]
const FILE_REMOTE_PROTOCOL_INFO_CLASS: u32 = 13;

/// Whether the open file lives on a network filesystem — the gate for every
/// warm. Decided from the already-open handle, so it costs no extra network
/// round-trip (unlike a path canonicalization, which re-opens the file over
/// SMB) and is correct through mapped drives, UNC paths, symlinks, and subst.
/// On Windows, `FileRemoteProtocolInfo` succeeds only for remote files; the
/// verdict is just that success. Elsewhere a page-cache warm is cheap and
/// helps CIFS/NFS, so it is always on.
#[cfg(windows)]
pub fn is_remote(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    // FILE_REMOTE_PROTOCOL_INFO is 116 bytes, 4-byte aligned; only the call's
    // success matters, never the contents.
    let mut info = [0u32; 29];
    // SAFETY: the handle is valid for the lifetime of `file`, and the buffer is
    // a live, writable allocation of the documented size.
    unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_REMOTE_PROTOCOL_INFO_CLASS,
            info.as_mut_ptr().cast(),
            std::mem::size_of_val(&info) as u32,
        ) != 0
    }
}

#[cfg(not(windows))]
pub fn is_remote(_file: &File) -> bool {
    true
}

/// Warm the container metadata region so a network filesystem streams it in a
/// pipelined read instead of many synchronous page faults. Best-effort and a
/// no-op on local volumes (`remote` is the caller's `is_remote` verdict, decided
/// once per file); never changes what is parsed. Returns the length of the
/// contiguous warmed prefix from byte 0 (after coalescing — an MKV head that
/// merges into its block span counts whole), so `warm_sample_chunks` can skip
/// ranges already covered.
pub fn warm_metadata(remote: bool, file: &File, path: &Path, data: &[u8]) -> usize {
    if !remote {
        return 0;
    }
    let size = data.len();

    // Gather every range first, then stream them concurrently in one pass. The
    // extent discoveries below (`moov_extent`, `tags_extent`, `detect_layout`,
    // `head_blocks_extent`) each fault only a handful of pages, so running them
    // before any warm costs a few cold round-trips but lets all ranges — head
    // and tail — overlap instead of the head warm serially delaying the extent
    // the demux is actually blocked on.
    let mut ranges: Vec<(u64, usize)> = Vec::new();

    let is_ts = looks_like_ts(path, data);
    let is_mp4 = looks_like_mp4(path, data);
    let is_mkv = looks_like_mkv(path, data);
    let moov = if is_mp4 { crate::container::mp4::moov_extent(data) } else { None };
    let mkv_blocks = if is_mkv { crate::container::mkv::head_blocks_extent(data) } else { None };

    // Front-loaded metadata (and, for the bounded fast path, the sampled blocks).
    // The head is sized to what the front parse actually consumes: TS/M2TS has
    // no container box, so resolution/colour come from the in-band SPS at the
    // first IDR (~a GOP in) and the default demux reads a large head window to
    // reach it. A confirmed ISOBMFF (moov found) or extent-resolved MKV needs
    // almost none of the generic head — their regions are warmed by exact
    // extent below, and a generic head would stream bytes nothing parses. Raw
    // HEVC/AV1 head walks are covered by the generic head (the `<=` couplings
    // on `HEAD_WARM`).
    let head = if is_ts {
        crate::container::ts::HEAD_SCAN_BYTES as usize
    } else if moov.is_some() {
        MP4_HEAD_WARM
    } else if mkv_blocks.is_some() {
        MKV_HEAD_WARM
    } else {
        HEAD_WARM
    }
    .min(size);
    ranges.push((0, head));

    // TS/M2TS has no duration box, so it also reads a small tail window for the
    // last PCR (see `ts::pcr_duration`). Overlap with the head on small files is
    // coalesced away by `warm_ranges`.
    if is_ts {
        let tail = crate::container::ts::TAIL_SCAN_BYTES as usize;
        let start = size.saturating_sub(tail);
        ranges.push((start as u64, size - start));
    }

    // The `moov` is warmed by its exact extent, wherever it sits (front-placed
    // merges into the head range; tail-placed is the one common metadata
    // region a head window could never cover).
    if let Some((start, end)) = moov {
        ranges.push((start as u64, end - start));
    }
    if is_mp4 {
        // Fragmented MP4: a front `sidx` (or, failing that, the tail `mfra`
        // random-access index — one extra tail round-trip to probe) lists every
        // fragment's position, so the moof header regions can be streamed
        // concurrently instead of the fragment index's serial moof → moof
        // pointer chase faulting one round-trip per fragment. Hint-only: the
        // index is still built from the moof boxes themselves.
        if let Some(heads) = crate::container::mp4::sidx_fragment_heads(data, HEAD_WARM) {
            ranges.extend(heads);
        } else if let Some(heads) = crate::container::mp4::mfra_fragment_heads(data) {
            ranges.extend(heads);
        }
    }

    // MKV writes the statistics `Tags` after the clusters (past the head window);
    // the demux reads it for the per-stream bitrate via the front SeekHead. Warm
    // that small tail element so the read isn't a lone RTT; a front-placed `Tags`
    // (inside the head) merges away.
    if is_mkv {
        if let Some((start, end)) = crate::container::mkv::tags_extent(data) {
            ranges.push((start as u64, end - start));
        }
        // The bounded head block window, from wherever the clusters actually
        // start — the block-header walk plus the sampled AUs consume most of
        // it, and attachments (cover art, fonts) can push it past any generic
        // head. Front-cluster layouts merge into the head range.
        if let Some((start, end)) = mkv_blocks {
            ranges.push((start as u64, end - start));
        }
    }

    // Warm once, then report the contiguous prefix from byte 0 so the chunk
    // warm can skip AUs the coalesced head region already streamed.
    let merged = merge_ranges(ranges);
    warm_merged(file, &merged);
    match merged.first() {
        Some(&(0, end)) => end as usize,
        _ => 0,
    }
}

/// Merge overlapping/adjacent ranges and warm them concurrently — scattered
/// ranges become ~pool-width parallel positioned reads instead of a serial
/// chain, so one range's network latency hides another's. Positioned reads on a
/// shared `&File` are safe: each carries its own offset, and nothing in the
/// program relies on the file cursor.
fn warm_ranges(file: &File, ranges: Vec<(u64, usize)>) {
    warm_merged(file, &merge_ranges(ranges));
}

/// Warm already-merged `(start, end)` extents concurrently.
fn warm_merged(file: &File, merged: &[(u64, u64)]) {
    use rayon::prelude::*;

    merged.par_iter().for_each(|&(start, end)| warm(file, start, (end - start) as usize));
}

/// Sort `(offset, len)` ranges and coalesce overlapping/adjacent ones into
/// disjoint `(start, end)` extents, dropping empties.
fn merge_ranges(mut ranges: Vec<(u64, usize)>) -> Vec<(u64, u64)> {
    ranges.retain(|r| r.1 > 0);
    ranges.sort_unstable_by_key(|r| r.0);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (off, len) in ranges {
        let end = off.saturating_add(len as u64);
        match merged.last_mut() {
            Some(last) if off <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((off, end)),
        }
    }
    merged
}

/// Per-range / total caps on the sampled-chunk warm, so a corrupt sample index
/// (box-declared sizes are attacker-controlled) can't drive the warmer into
/// streaming gigabytes. Generous against real content: a 4K IDR access unit is
/// single-digit MiB, and the default 16 samples total well under the budget.
const CHUNK_WARM_RANGE_CAP: usize = 32 << 20; // 32 MiB
const CHUNK_WARM_TOTAL_CAP: usize = 128 << 20; // 128 MiB

/// Warm the access units the sampler is about to fault. `select_indices` with
/// the same inputs yields exactly the chunks `sample::scan` will read, and each
/// chunk's byte range is known from the container index, so the scattered
/// mmap faults (one ~32 KiB round-trip each) collapse into a few pipelined
/// reads. `warmed_head` is `warm_metadata`'s return: ranges it already covered
/// are skipped. Callers gate this to the default path: under `--full` every
/// chunk is read and pre-reading a whole movie would be a regression, and
/// under `--no-rpu` no chunk is read at all.
pub fn warm_sample_chunks(
    remote: bool,
    file: &File,
    demux: &Demux,
    samples: usize,
    warmed_head: usize,
) {
    // TS/M2TS chunks index into the reassembled buffer, not the file — its
    // file-side working set (head + tail windows) is warmed by `warm_metadata`.
    if !remote || demux.reassembled.is_some() || demux.chunks.is_empty() {
        return;
    }
    let mut ranges: Vec<(u64, usize)> = Vec::new();
    let mut total = 0usize;
    for i in crate::sample::select_indices(demux.chunks.len(), samples, false) {
        let c = demux.chunks[i];
        let len = (c.size as usize).min(CHUNK_WARM_RANGE_CAP);
        // Already streamed by the metadata head warm (MKV/AV1 head chunks).
        if c.offset.saturating_add(len as u64) <= warmed_head as u64 {
            continue;
        }
        total += len;
        if total > CHUNK_WARM_TOTAL_CAP {
            break;
        }
        ranges.push((c.offset, len));
    }
    warm_ranges(file, ranges);
}

/// Sequentially read `len` bytes from `offset` into a scratch buffer and discard
/// them, pulling the range into the OS/SMB cache. Positioned reads leave the
/// file cursor and the mmap untouched; errors are ignored (parsing still works,
/// just without the warm cache).
fn warm(file: &File, offset: u64, len: usize) {
    if len == 0 {
        return;
    }
    let mut buf = vec![0u8; len.min(1 << 20)]; // scratch, 1 MiB cap
    let mut pos = offset;
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        match read_at(file, &mut buf[..want], pos) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pos += n as u64;
                remaining -= n;
            }
        }
    }
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    file.seek_read(buf, off)
}
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    file.read_at(buf, off)
}

fn looks_like_mp4(path: &Path, data: &[u8]) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "m4a")
        || (data.len() >= 8 && &data[4..8] == b"ftyp")
}

fn looks_like_ts(path: &Path, data: &[u8]) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "ts" | "m2ts" | "mts")
        || crate::container::ts::detect_layout(data).is_some()
}

fn looks_like_mkv(path: &Path, data: &[u8]) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "mkv" | "webm" | "mka")
        || (data.len() >= 4 && data[0] == 0x1A && data[1] == 0x45 && data[2] == 0xDF && data[3] == 0xA3)
}

#[cfg(test)]
mod tests {
    #[test]
    fn merge_ranges_coalesces_overlaps_and_drops_empties() {
        // Out-of-order input; the 2nd range overlaps the 1st, the adjacent 3rd
        // extends it, the empty one vanishes, and the far one stays separate.
        let merged = super::merge_ranges(vec![
            (100, 50),  // 100..150
            (0, 0),     // empty
            (900, 10),  // 900..910
            (120, 80),  // overlaps -> ..200
            (200, 25),  // adjacent -> ..225
        ]);
        assert_eq!(merged, vec![(100, 225), (900, 910)]);
    }

    #[cfg(windows)]
    #[test]
    fn is_remote_is_false_for_a_local_temp_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("hdrprobe_is_remote_smoke");
        let file = std::fs::File::create(&path).unwrap();
        assert!(!super::is_remote(&file), "local temp file must not warm");
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
