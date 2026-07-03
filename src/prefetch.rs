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
//! This module warms the byte ranges we're about to parse with one sequential,
//! pipelined positioned read per range. On Windows the memory-mapped section and
//! cached `ReadFile` share the Cache Manager's pages for the same file, so the
//! warm read populates exactly the pages the subsequent mmap faults will hit; the
//! same holds for the shared page cache on Linux CIFS/NFS. Parsing still runs
//! against the mmap — nothing is copied into the report path, so the zero-copy
//! `Chunk` model is untouched. Warming only affects *timing*, never what we parse.
//!
//! It is gated to remote volumes on Windows so the tight local path is unchanged.

use std::fs::File;
use std::path::Path;

use crate::container::Demux;

/// Head window warmed for every remote file. Covers the front-loaded metadata of
/// most backends: MKV (EBML header + Info + Tracks + the bounded head window of
/// blocks it walks — see `mkv::HEAD_SPAN_BYTES`), raw-HEVC/AV1 front windows, and
/// MP4 faststart `ftyp` + `moov`. Sized to hold the front metadata plus that 4 MiB
/// block span so the whole fast-path working set arrives in one pipelined read
/// instead of hundreds of scattered faults. TS/M2TS is warmed separately with a
/// larger window (`ts::HEAD_SCAN_BYTES`) since its in-band SPS sits a GOP in.
///
/// Raw AV1's bounded head walk (`av1::HEAD_SCAN_BYTES`) is deliberately kept `<=`
/// this so the warm covers it whole; shrink this below that and the AV1 window's
/// tail faults in one page at a time on the NAS again.
const HEAD_WARM: usize = 8 << 20; // 8 MiB

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
/// once per file); never changes what is parsed.
pub fn warm_metadata(remote: bool, file: &File, path: &Path, data: &[u8]) {
    if !remote {
        return;
    }
    let size = data.len();

    // Gather every range first, then stream them concurrently in one pass. The
    // extent discoveries below (`moov_extent`, `tags_extent`, `detect_layout`)
    // each fault only a handful of pages, so running them before any warm costs
    // a few cold round-trips but lets all ranges — head and tail — overlap
    // instead of the head warm serially delaying the extent the demux is
    // actually blocked on.
    let mut ranges: Vec<(u64, usize)> = Vec::new();

    // Front-loaded metadata (and, for the bounded fast path, the sampled blocks).
    // TS/M2TS is the exception: it has no container box, so resolution/colour come
    // from the in-band SPS at the first IDR (~a GOP in), and the default demux
    // reads a larger head window to reach it — warm that whole region instead.
    let is_ts = looks_like_ts(path, data);
    let head = if is_ts {
        crate::container::ts::HEAD_SCAN_BYTES as usize
    } else {
        HEAD_WARM
    };
    ranges.push((0, head.min(size)));

    // TS/M2TS has no duration box, so it also reads a small tail window for the
    // last PCR (see `ts::pcr_duration`). Overlap with the head on small files is
    // coalesced away by `warm_ranges`.
    if is_ts {
        let tail = crate::container::ts::TAIL_SCAN_BYTES as usize;
        let start = size.saturating_sub(tail);
        ranges.push((start as u64, size - start));
    }

    // MP4 with `moov` at the tail is the one common metadata region the head
    // window misses; a front-placed `moov` merges into the head range.
    if looks_like_mp4(path, data) {
        if let Some((start, end)) = crate::container::mp4::moov_extent(data) {
            ranges.push((start as u64, end - start));
        }
    }

    // MKV writes the statistics `Tags` after the clusters (past the head window);
    // the demux reads it for the per-stream bitrate via the front SeekHead. Warm
    // that small tail element so the read isn't a lone RTT; a front-placed `Tags`
    // (inside the head) merges away.
    if looks_like_mkv(path, data) {
        if let Some((start, end)) = crate::container::mkv::tags_extent(data) {
            ranges.push((start as u64, end - start));
        }
    }

    warm_ranges(file, ranges);
}

/// Merge overlapping/adjacent ranges and warm them concurrently — scattered
/// ranges become ~pool-width parallel positioned reads instead of a serial
/// chain, so one range's network latency hides another's. Positioned reads on a
/// shared `&File` are safe: each carries its own offset, and nothing in the
/// program relies on the file cursor.
fn warm_ranges(file: &File, ranges: Vec<(u64, usize)>) {
    use rayon::prelude::*;

    merge_ranges(ranges)
        .par_iter()
        .for_each(|&(start, end)| warm(file, start, (end - start) as usize));
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
/// reads. Callers gate this to the default path: under `--full` every chunk is
/// read and pre-reading a whole movie would be a regression, and under
/// `--no-rpu` no chunk is read at all.
pub fn warm_sample_chunks(remote: bool, file: &File, demux: &Demux, samples: usize) {
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
        // Already streamed by the metadata head warm (MKV/AV1 head chunks, the
        // head run of a front-mdat MP4).
        if c.offset.saturating_add(len as u64) <= HEAD_WARM as u64 {
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
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB scratch
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
