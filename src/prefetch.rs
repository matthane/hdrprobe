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
    fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
}
#[cfg(windows)]
const DRIVE_REMOTE: u32 = 4;

/// Warm the container metadata region so a network filesystem streams it in a
/// pipelined read instead of many synchronous page faults. Best-effort and a
/// no-op on local volumes; never changes what is parsed.
pub fn warm_metadata(file: &File, path: &Path, data: &[u8]) {
    if !should_warm(path) {
        return;
    }
    let size = data.len();

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
    warm(file, 0, head.min(size));

    // TS/M2TS has no duration box, so it also reads a small tail window for the
    // last PCR (see `ts::pcr_duration`). Warm that trailing window in one read;
    // skip it when the tail overlaps the already-warmed head (small files).
    if is_ts {
        let tail = crate::container::ts::TAIL_SCAN_BYTES as usize;
        let start = size.saturating_sub(tail);
        if start >= head {
            warm(file, start as u64, size - start);
        }
    }

    // MP4 with `moov` at the tail is the one common metadata region the head
    // window misses; warm the part of its extent not already covered.
    if looks_like_mp4(path, data) {
        if let Some((start, end)) = crate::container::mp4::moov_extent(data) {
            let warm_start = start.max(HEAD_WARM);
            if warm_start < end {
                warm(file, warm_start as u64, end - warm_start);
            }
        }
    }

    // MKV writes the statistics `Tags` after the clusters (past the head window);
    // the demux reads it for the per-stream bitrate via the front SeekHead. Warm
    // that small tail element in one read so the read isn't a lone RTT; the front
    // SeekHead it's resolved from is already in the warmed head. Skip when it
    // falls inside the head (front-placed or small file).
    if looks_like_mkv(path, data) {
        if let Some((start, end)) = crate::container::mkv::tags_extent(data) {
            let warm_start = start.max(HEAD_WARM);
            if warm_start < end {
                warm(file, warm_start as u64, end - warm_start);
            }
        }
    }
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

/// Whether warming is worth it. On Windows we restrict it to network volumes so
/// the fast local path is untouched; elsewhere a page-cache warm is cheap and
/// helps CIFS/NFS, so it is always on.
#[cfg(windows)]
fn should_warm(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(Component::Prefix(pref)) = abs.components().next() else {
        return false;
    };
    let disk = match pref.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return true,
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
        _ => return false,
    };
    let root: [u16; 4] = [disk.to_ascii_uppercase() as u16, b':' as u16, b'\\' as u16, 0];
    // SAFETY: `root` is a valid NUL-terminated wide string that outlives the call.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
}

#[cfg(not(windows))]
fn should_warm(_path: &Path) -> bool {
    true
}
