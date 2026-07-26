//! AVI (RIFF / OpenDML), `.avi`.
//!
//! Structurally the simplest container in the tree to *walk* — everything is a
//! `ckID`/`ckSize` pair and every position is arithmetic on declared sizes, so
//! there is no scanning anywhere on the default path — and the least generous
//! about what it records.
//!
//! **AVI carries no colour description of any kind, in any chunk, in any
//! published extension.** Confirmed four ways in `dev/sdr-format-reference.md`
//! §5: the Microsoft chunk inventory has no colour chunk; `BITMAPINFOHEADER`'s
//! only colour-adjacent members are palette counts; a full-text search of the
//! OpenDML 2.0 spec finds only a "future work" note, SMPTE *timecode*
//! ColorFrame fields and an M-JPEG-annex `JPEGColorSpaceID`; and ffmpeg's
//! `avidec.c` contains no `color`/`primaries`/`AVCOL_` match at all. So every
//! colour field a report shows for an AVI came out of the *bitstream*, which is
//! why this backend routes the first video chunk (or the `strf` extradata) into
//! the codec parsers rather than treating that as an optional extra. A
//! `biSize > 40` means extradata follows, never a `BITMAPV4HEADER`'s
//! chromaticity block: no muxer writes V4/V5 into a video `strf`, and every
//! demuxer reads past offset 40 as codec-private bytes.
//!
//! Eight facts about the format are invariants a later change would otherwise
//! undo quietly. Each is pinned by a test.
//!
//! **The frame rate signal is a stream *unit* rate, not a picture rate.**
//! `strh.dwRate / dwScale` counts the units `dwLength` counts, and those are
//! pictures only when the muxer wrote one chunk per picture. `ffmpeg -i x.mp4
//! -c:v copy out.avi` — an extremely common operation — writes 600/1 with 1200
//! units for a 2-second 25 fps clip, of which 1150 are zero-length padding
//! chunks; MediaInfo duly reports 600.000 fps. So the coded stream's own rate
//! wins when it has one (an H.264/HEVC SPS VUI, an MPEG-2 `frame_rate_code`)
//! and this is the fallback, which is the same ordering `mpeg4part2`'s
//! `fixed_vop_rate` rule reaches from the other side. The *duration* is
//! unaffected either way: `dwLength * dwScale / dwRate` is 2.000 s on that file
//! and correct, because both halves count the same units.
//!
//! **`avih.dwTotalFrames` is wrong on every OpenDML file** — it covers the
//! first RIFF chunk only, and ffmpeg's counter increments before the rollover
//! check so it is not even an exact count of that. It is never read here.
//! `strh.dwLength` is the whole-file count on both ffmpeg and VirtualDub, which
//! accumulate it across every RIFF chunk and snapshot only `avih`.
//!
//! **The `odml` LIST is written as a `JUNK` chunk on a single-RIFF file, and
//! its `dwTotalFrames` is zero.** ffmpeg reserves the block up front
//! (`ff_start_tag(pb, "JUNK")` in `avienc.c`) and rewrites the id to `LIST` in
//! the trailer only if the file actually rolled over to OpenDML. Every AVI in
//! the corpus — including the one named `h264_odml.avi` — carries a
//! `JUNK`-wrapped `dmlh` reading 0. So `dmlh` must be reached by descending a
//! real `LIST odml` and never by searching for its FourCC, which is what makes
//! this a trap rather than a curiosity: a byte search finds a plausible chunk
//! and reads a zero frame count out of it.
//!
//! **The same reserve-then-rename trick is applied to the `indx` super-index**,
//! and that instance is the more dangerous one. Every `strl` in the corpus holds
//! a 4120-byte `JUNK` chunk whose body is a zeroed super-index — right
//! `wLongsPerEntry`, right `bIndexType`, right `dwChunkId`, and
//! `nEntriesInUse == 0` — renamed to `indx` only on rollover. Finding it by
//! FourCC would yield a structurally valid index declaring nothing, i.e. a
//! silently empty result rather than an obviously wrong one. Both are reached
//! here by chunk id inside the walk, which is what makes them invisible.
//!
//! **The two index forms use two different offset conventions.** `idx1`
//! entries are offsets to the chunk *header*, relative to the position of the
//! `movi` FOURCC (so the first entry reads 4, not 0) — though Microsoft's own
//! reference admits "in some AVI files it is given as an offset from the start
//! of the file", which VirtualDub emitted before build 4936. OpenDML `ix##`
//! entries are offsets to the chunk *data*, relative to the index's own
//! `qwBaseOffset`. Both bases are therefore *derived and validated* here — read
//! the candidate position, check the chunk id and size match what the index
//! entry claims — rather than assumed.
//!
//! **`idx1` covers only the first RIFF chunk of an OpenDML file** (measured:
//! 4661 of 6000 entries, a 22% byte undercount), so summing it for a bitrate is
//! gated on the file being single-RIFF. Multi-RIFF files sum the `ix##` chunks
//! the `indx` super-index points at, which cover the whole file by
//! construction.
//!
//! **The two OpenDML index levels multiply, and clamping each one is not
//! enough.** The super-index calls the standard-index parser once per entry
//! into a single shared vector, so N super entries all naming the same M-entry
//! `ix##` describe N x M chunks in a file holding one — and nothing forbids the
//! repetition, since only entry 0's base is validated. Both counts are already
//! clamped against the bytes their own chunk holds, which is the house rule and
//! is individually correct; the product is what escapes. Measured before
//! `chunk_ceiling` existed, on the default path with exit 0: a 156 KiB file
//! allocated 771 MB, quadratic in file size. This is the *same shape* as the
//! parameter-set scan being bounded to 32 chunks while each chunk's declared
//! span stayed unbounded, and both were found by the same review: when two
//! declared quantities meet, bound the product, not the factors.
//!
//! **`idx1` entry 0 is not necessarily a video chunk** — in the corpus's
//! multi-stream file it is `01wb`, the MP3 track — so entries are filtered by
//! the two-digit stream-index prefix and the offset base is derived from entry
//! 0's *own* chunk id. Summing unfiltered gives a rate 12.5% high there.
//!
//! Two things this backend deliberately does not do. It does not parse `vprp`:
//! that chunk carries display aspect ratio, field order, refresh rate and
//! active geometry, and the first two are out of scope plan-wide (no `model.rs`
//! field exists for either, and adding one populated only for AVI would be
//! inconsistent with every codec that signals the same thing in its own
//! headers), while the last two duplicate `strh` and `strf`. And it never reads
//! `dwMaxBytesPerSec`, which is a whole-file *maximum* including audio, nor
//! `biBitCount`, which is display bits per pixel and not a bit depth
//! ([`super::bmih`] refuses to expose it at all).

use anyhow::{bail, Result};

use crate::model::Bitrate;

use super::{bmih, Chunk, Codec, Demux, NalFormat, TrackDemux};

/// How far into `movi` the fallback walk may go when a file carries no usable
/// index. Only reached by files ffmpeg and VirtualDub do not write; a normal
/// AVI resolves every chunk from `idx1` or `ix##` by arithmetic.
///
/// Sized to `prefetch::HEAD_WARM`, but **not** the same coupling `annexb`,
/// `av1`, `mpegv` and `ps` keep: those walk from byte 0, so their window sits
/// wholly inside the warm, while this one runs from the `movi` body. The last
/// `movi_body` bytes of the walk therefore fall outside the warmed head. That
/// is timing-only, on a path no mainstream writer produces, and raising the
/// constant to compensate would enlarge the warm for every file to cover a case
/// almost none reach — so the honest statement is that the overlap is partial
/// here, not that it is complete.
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

/// Bound on chunks read from any one RIFF list. The walks are all
/// declared-size arithmetic rather than scans, so this only stops a malformed
/// file whose sizes chain in a tiny loop from spinning; a real `hdrl` holds a
/// handful of chunks and a real `movi` walk is additionally byte-bounded.
const MAX_LIST_CHUNKS: usize = 1 << 20;

/// Bound on RIFF segments (`AVI ` plus its `AVIX` continuations). The advisory
/// cap is 1 GiB per segment, so this admits a 4 TiB file — far past anything
/// the format is used for, while keeping a size-zero chain finite.
const MAX_RIFF_SEGMENTS: usize = 4096;

/// Longest duration accepted from the header arithmetic. `dwLength * dwScale /
/// dwRate` is three unvalidated `u32`s, so a malformed file can compute 10^19
/// seconds; a bitrate then divides by it and reports 0 b/s over a "duration"
/// of half a trillion years. Two days is generous for a container whose
/// heritage is a 4 GiB-per-segment cap.
const MAX_DURATION_SECS: f64 = 48.0 * 3600.0;

/// Access units the in-band parameter-set search may read, matching the
/// `.take(32)` the MPEG-2 and Part 2 gap-fillers use. See the call site: the
/// chunk index here spans the whole file, so this is what keeps the default
/// path bounded.
const SPS_SCAN_CHUNKS: usize = 32;

pub(crate) const CONTAINER_LABEL: &str = "AVI (RIFF)";

pub fn demux(data: &[u8]) -> Result<Demux> {
    if !is_avi(data) {
        bail!("not an AVI file (no RIFF....AVI header)");
    }
    let (segments, complete) = riff_segments(data);
    let Some(first) = segments.first() else {
        bail!("AVI RIFF header declares no body");
    };
    // OpenDML iff a continuation segment follows. This is what gates `idx1`
    // summing below, and it is a structural test rather than a flag: the
    // `AVIF_HASINDEX` bits and the `dmlh` block both lie about it (see the
    // module doc).
    let single_riff = segments.len() == 1;

    let Some(hdrl) = find_list(data, first.body, first.end, b"hdrl") else {
        bail!("AVI has no hdrl header list");
    };
    let streams = parse_hdrl(data, hdrl.0, hdrl.1);
    if streams.is_empty() {
        bail!("AVI hdrl declares no streams");
    }
    // The only `avih` field read at all: a last-resort frame period for a video
    // stream whose own `strh` timing is 0/0. Reference §5 gotchas 2 and 5
    // sanction exactly that narrow use. `dwTotalFrames` beside it is never read
    // — it is wrong on every OpenDML file and off by one even on the first
    // segment.
    let usec_per_frame = find_chunk(data, hdrl.0, hdrl.1, b"avih")
        .map_or(0, |(b, _)| u32le(data, b));

    // The whole file's duration is the longest stream's, audio included — which
    // is what MediaInfo's General duration reports, and on the corpus's
    // video+MP3 file the audio really is the longer of the two. Each video
    // track keeps its *own* duration as the bitrate denominator, the same split
    // MP4 makes between `mvhd` and the track's `mdhd`.
    let duration_secs = streams
        .iter()
        .filter_map(|s| s.duration(usec_per_frame))
        .fold(None::<f64>, |acc, d| Some(acc.map_or(d, |a| a.max(d))));

    let movi: Vec<(usize, usize)> = segments
        .iter()
        .filter_map(|s| find_list(data, s.body, s.end, b"movi"))
        .collect();
    let Some(&(movi_body, _)) = movi.first() else {
        bail!("AVI has no movi data list");
    };
    // `idx1` follows `movi` in the first segment. Present on essentially every
    // file, and free to reach: no scan, just the next chunk after a list whose
    // size the header already declared.
    let idx1 = find_chunk(data, first.body, first.end, b"idx1");

    let mut tracks = Vec::new();
    let mut bounded = false;
    for (index, s) in streams.iter().enumerate() {
        if &s.kind != b"vids" {
            continue;
        }
        let Some((strf_start, strf_end)) = s.strf else { continue };
        let Some(bh) = bmih::parse(&data[strf_start..strf_end]) else { continue };
        let extradata = &data[(strf_start + bmih::HEADER_LEN).min(strf_end)..strf_end];

        let idx = build_index(data, s, index, &movi, movi_body, idx1, single_riff);
        let chunks = idx.chunks;
        // An index read out of a truncated file is not an exact byte count even
        // when every entry it still holds validates: the index chunk itself was
        // cut, and a cut index reads as a shorter well-formed one. Measured on a
        // real file cut inside its own `idx1`: 331 kb/s reported as an exact
        // video-stream rate against a true 752. The surviving chunks are still
        // usable — they were checked against the bytes present — so they are
        // kept, and dropping the count is also what flags the index as bounded.
        let exact_bytes = idx.bytes.filter(|_| complete);
        // A bounded fallback walk saw only the head of `movi`, so a `--full`
        // scan reads every chunk that exists without having seen the whole
        // stream; the report keeps its sampled marks on for that.
        bounded |= exact_bytes.is_none() && !chunks.is_empty();

        let codec = bmih::codec_from_fourcc(&bh.compression)
            .unwrap_or_else(|| Codec::Other(fourcc_label(&bh.compression)));
        let mut td = TrackDemux {
            track_number: Some(index as u64),
            width: bh.width,
            height: bh.height,
            chunks,
            ..TrackDemux::new(codec, NalFormat::AnnexB)
        };
        fill_from_bitstream(&mut td, data, extradata);
        // The container's rate is the fallback, never the override: it counts
        // stream units, and a padding-chunk remux makes those 24x the pictures
        // (module doc). Everything above has already filled `fps` from the
        // coded stream when the coded stream said anything.
        if td.fps.is_none() {
            td.fps = s.fps(usec_per_frame);
        }
        td.bitrate = match exact_bytes {
            Some(bytes) => Bitrate::video_stream(bytes, s.duration(usec_per_frame)),
            // No usable index: the byte count is unknown, so the honest answer
            // is the whole-container rate, which counts audio and every chunk
            // header and is labelled distinctly for exactly that reason.
            //
            // Except over a truncated file, where it is not honest at all: the
            // numerator is the bytes present and the denominator is the whole
            // declared runtime, so the answer is low by exactly the fraction
            // missing. A 4 MiB prefix of a 5 MiB file reports 581 kb/s against
            // a true 758 (MediaInfo makes the same mistake; ffmpeg instead
            // rescales the *duration*, which is a guess about where the cut
            // landed). The declared duration is a header fact and stands.
            // The whole-container fallback is withheld on the same evidence,
            // and on one more: an index that accounted for fewer units than the
            // header declares says the file is short even where every declared
            // size still fits — which is exactly how a two-segment file cut at
            // its `AVIX` header presents.
            None if complete && !idx.short => {
                Bitrate::overall(data.len() as u64, duration_secs)
            }
            None => None,
        };
        tracks.push(td);
    }

    if tracks.is_empty() {
        bail!("no video stream in the AVI header list");
    }

    Ok(Demux {
        container: CONTAINER_LABEL,
        duration_secs,
        tracks,
        ts_stream: None,
        mkv_stream: None,
        raw_stream: None,
        bounded_index: bounded,
    })
}

/// `RIFF` at 0 and `AVI ` at 8 — the trailing space is part of the form type.
pub(crate) fn is_avi(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"AVI "
}

// --- RIFF walking ------------------------------------------------------------

fn u16le(d: &[u8], at: usize) -> u16 {
    match d.get(at..at + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}

fn u32le(d: &[u8], at: usize) -> u32 {
    match d.get(at..at + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

fn u64le(d: &[u8], at: usize) -> u64 {
    match d.get(at..at + 8) {
        Some(b) => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => 0,
    }
}

fn fourcc(d: &[u8], at: usize) -> [u8; 4] {
    match d.get(at..at + 4) {
        Some(b) => [b[0], b[1], b[2], b[3]],
        None => [0; 4],
    }
}

/// One RIFF segment's payload: the bytes after the 12-byte `RIFF`/size/form
/// header, clamped to the buffer.
struct Segment {
    body: usize,
    end: usize,
}

/// The `AVI ` segment and any `AVIX` continuations, in file order, plus whether
/// the file holds every byte they declare.
///
/// Stops at the first thing that is not a RIFF segment of one of those two
/// forms, so trailing junk (or a second unrelated RIFF) never extends the file.
///
/// The completeness flag is the truncation signal ffmpeg reads too
/// (`riff_end > file_size`), and it is exact rather than heuristic: every AVI
/// observed declares a size that lands precisely on the next segment or on EOF,
/// because the writer seeks back and patches it at close. A file cut short — an
/// aborted download, a capture that never closed — declares more than it holds,
/// and the byte counts a bitrate divides then describe a file that is not there.
fn riff_segments(data: &[u8]) -> (Vec<Segment>, bool) {
    let mut out = Vec::new();
    let mut complete = true;
    let mut pos = 0usize;
    while out.len() < MAX_RIFF_SEGMENTS {
        if pos.saturating_add(12) > data.len() || fourcc(data, pos) != *b"RIFF" {
            break;
        }
        let form = fourcc(data, pos + 8);
        if out.is_empty() {
            if form != *b"AVI " {
                break;
            }
        } else if form != *b"AVIX" {
            break;
        }
        let size = u32le(data, pos + 4) as usize;
        let declared = pos.saturating_add(8).saturating_add(size);
        complete &= declared <= data.len();
        out.push(Segment { body: pos + 12, end: declared.min(data.len()) });
        pos = declared.saturating_add(size & 1);
    }
    (out, complete)
}

/// Iterate the chunks of one list body. Yields `(id, body_start, body_end)`
/// with `body_end` clamped to the parent, so every consumer's slice is in
/// bounds no matter what `ckSize` declares.
///
/// `ckSize` excludes the 8-byte header *and* the pad byte, so the step is
/// `8 + ckSize + (ckSize & 1)` — a chunk always advances by at least 8, which
/// is what makes the walk terminate on a file full of zero-size chunks.
fn chunks_in(data: &[u8], start: usize, end: usize) -> impl Iterator<Item = ([u8; 4], usize, usize)> + '_ {
    let mut pos = start;
    let mut left = MAX_LIST_CHUNKS;
    std::iter::from_fn(move || {
        if left == 0 || pos.saturating_add(8) > end {
            return None;
        }
        left -= 1;
        let id = fourcc(data, pos);
        let size = u32le(data, pos + 4) as usize;
        let body = pos + 8;
        let body_end = body.saturating_add(size).min(end);
        pos = body.saturating_add(size).saturating_add(size & 1);
        Some((id, body, body_end))
    })
}

/// Find a `LIST` of the given type within `start..end`, returning its body
/// (past the 4-byte list type) and end.
fn find_list(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    chunks_in(data, start, end).find_map(|(id, body, body_end)| {
        (id == *b"LIST" && fourcc(data, body) == *want && body + 4 <= body_end)
            .then_some((body + 4, body_end))
    })
}

/// Find a plain chunk within `start..end`, returning its body range.
fn find_chunk(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    chunks_in(data, start, end).find(|&(id, _, _)| id == *want).map(|(_, b, e)| (b, e))
}

// --- header list -------------------------------------------------------------

/// One `strl` entry, in `hdrl` order — which is the stream number the `##dc`
/// chunk ids and the index entries are keyed by.
struct Stream {
    /// `strh.fccType`: `vids`, `auds`, `txts`, ...
    kind: [u8; 4],
    scale: u32,
    rate: u32,
    length: u32,
    /// `strf` body range (a `BITMAPINFOHEADER` plus extradata, for video).
    strf: Option<(usize, usize)>,
    /// `indx` super-index body range, present on OpenDML files.
    indx: Option<(usize, usize)>,
}

impl Stream {
    /// `dwRate / dwScale`, the stream's unit rate — the *fallback* frame rate,
    /// for the reason the module doc gives.
    ///
    /// Falls back in turn to `avih.dwMicroSecPerFrame` for a video stream whose
    /// own timing is 0/0, which is what files from broken software carry.
    /// ffmpeg's source calls preferring that field "a bad idea" and it is a
    /// whole-file average that says nothing about a second stream, so it is
    /// reached only when there is nothing else and only for video.
    fn fps(&self, usec_per_frame: u32) -> Option<f64> {
        if self.rate > 0 && self.scale > 0 {
            return Some(self.rate as f64 / self.scale as f64);
        }
        (&self.kind == b"vids" && usec_per_frame > 0)
            .then(|| 1_000_000.0 / usec_per_frame as f64)
    }

    /// `dwLength * dwScale / dwRate`, the stream's own playback duration.
    ///
    /// Correct even where the unit rate is not the picture rate, because both
    /// halves count the same units — and correct through the `avih` fallback
    /// too, since `dwLength` counts the same units either way.
    fn duration(&self, usec_per_frame: u32) -> Option<f64> {
        if self.length == 0 {
            return None;
        }
        // Multiply by the scale rather than divide by the derived rate: the
        // ratio form is exact for the common integer timebases (100 units at
        // 3/125 is 2.4, not 2.4000000000000004).
        let d = if self.rate > 0 && self.scale > 0 {
            self.length as f64 * self.scale as f64 / self.rate as f64
        } else {
            self.length as f64 / self.fps(usec_per_frame)?
        };
        (d.is_finite() && d > 0.0 && d <= MAX_DURATION_SECS).then_some(d)
    }
}

fn parse_hdrl(data: &[u8], start: usize, end: usize) -> Vec<Stream> {
    let mut out = Vec::new();
    for (id, body, body_end) in chunks_in(data, start, end) {
        if id != *b"LIST" || fourcc(data, body) != *b"strl" {
            continue;
        }
        let mut s = Stream {
            kind: [0; 4],
            scale: 0,
            rate: 0,
            length: 0,
            strf: None,
            indx: None,
        };
        let mut saw_strh = false;
        for (cid, cbody, cend) in chunks_in(data, body + 4, body_end) {
            match &cid {
                b"strh" => {
                    // 56 bytes: fccType(0) fccHandler(4) dwFlags(8) wPriority(12)
                    // wLanguage(14) dwInitialFrames(16) dwScale(20) dwRate(24)
                    // dwStart(28) dwLength(32) ... These are per stream, unlike
                    // `avih`'s whole-file fields.
                    if cend.saturating_sub(cbody) >= 36 {
                        s.kind = fourcc(data, cbody);
                        s.scale = u32le(data, cbody + 20);
                        s.rate = u32le(data, cbody + 24);
                        s.length = u32le(data, cbody + 32);
                        saw_strh = true;
                    }
                }
                b"strf" => s.strf = Some((cbody, cend)),
                b"indx" => s.indx = Some((cbody, cend)),
                _ => {}
            }
        }
        // A `strl` without a readable `strh` names no stream number reliably —
        // but it still occupies one, so it is pushed as a placeholder rather
        // than dropped. Dropping it would shift every later stream's index and
        // point the chunk filter at the wrong `##dc` prefix.
        if !saw_strh {
            s.kind = [0; 4];
        }
        out.push(s);
    }
    // `dmlh.dwTotalFrames` is not read here on purpose. It is redundant with
    // `strh.dwLength` wherever both are real (both writers accumulate the same
    // count), and on a single-RIFF ffmpeg file it is a `JUNK`-wrapped zero —
    // see the module doc. Nothing is gained by preferring it and a byte search
    // for it actively invents a zero frame count.
    out
}

// --- chunk index and exact byte count ---------------------------------------

/// What an index yielded for one stream.
struct Index {
    chunks: Vec<Chunk>,
    /// Exact encoded byte total, when the source covers the whole stream.
    bytes: Option<u64>,
    /// True when an index was read and accounted for **fewer units than
    /// `strh.dwLength` declares**, which means the file does not hold the whole
    /// stream and no byte count taken from it describes one.
    short: bool,
}

/// Build the video chunk index for one stream, and the exact encoded byte count
/// when the index used covers the whole file.
///
/// Three sources in preference order: the OpenDML `ix##` chunks (whole file by
/// construction), `idx1` on a single-segment file (exact there, a 22% undercount
/// on an OpenDML one), and a bounded walk of `movi` (chunk positions, plus a
/// byte total only when the walk can show it reached the end).
///
/// **`strh.dwLength` is the cross-check that makes the preference order safe.**
/// The single-segment gate is structural, and a two-segment file truncated at or
/// before its `AVIX` header presents as single-segment with every declared size
/// still fitting — so `idx1` looks complete and is not. Comparing the units the
/// index accounts for against the whole-file count the header declares catches
/// it exactly: measured on a 1.22 GiB two-segment file cut to segment 0, the
/// index holds 346 units against a declared 420 and used to report a 17.6%
/// undercount *labelled as an exact video-stream rate*. The two agree on all ten
/// corpus AVIs and disagree on both rollover files, so this is a clean
/// discriminator rather than a tolerance.
fn build_index(
    data: &[u8],
    s: &Stream,
    index: usize,
    movi: &[(usize, usize)],
    movi_body: usize,
    idx1: Option<(usize, usize)>,
    single_riff: bool,
) -> Index {
    let Some(prefix) = stream_prefix(index) else {
        return Index { chunks: Vec::new(), bytes: None, short: false };
    };
    // A declared count of zero states nothing to check against (`dwLength` is 0
    // in files from broken software), so the comparison is skipped rather than
    // failed.
    let declared = (s.length > 0).then_some(s.length as usize);
    if let Some((b, e)) = s.indx {
        if let Some((chunks, bytes, units)) = index_from_super(data, b, e, prefix) {
            let short = declared.is_some_and(|d| units < d);
            return Index { chunks, bytes: bytes.filter(|_| !short), short };
        }
    }
    if single_riff {
        if let Some((b, e)) = idx1 {
            if let Some((chunks, bytes, units)) = index_from_idx1(data, b, e, movi_body, prefix) {
                let short = declared.is_some_and(|d| units < d);
                return Index { chunks, bytes: bytes.filter(|_| !short), short };
            }
        }
    }
    let (chunks, bytes) = walk_movi(data, movi, prefix);
    Index { chunks, bytes, short: false }
}

/// The two ASCII digits every `movi` chunk id and index entry begins with.
/// `None` past 99, which no real file reaches (`avih.dwStreams` is a handful)
/// and which has no two-digit spelling.
fn stream_prefix(index: usize) -> Option<[u8; 2]> {
    (index < 100).then(|| [b'0' + (index / 10) as u8, b'0' + (index % 10) as u8])
}

/// The most chunks a file of this size can physically hold: every chunk costs
/// at least its own 8-byte header.
///
/// **This is what stops the two OpenDML index levels multiplying.** Each parser
/// clamps its own `nEntriesInUse` against the bytes its own chunk holds, which
/// is the house rule and is individually correct — but the super-index calls
/// the standard-index parser once per entry into *one shared vector*, so N
/// super entries each naming the same M-entry `ix##` yield N×M `Chunk` structs
/// from a file containing one chunk. Nothing forbids the repetition: only entry
/// 0's base is validated, and every entry may legally repeat it. Measured
/// before this cap, on the default path with an exit code of 0: a 156 KiB file
/// declaring 2000 × 16000 allocated 771 MB, and the growth is quadratic in file
/// size — the same input at 78 KiB took 194 MB. A megabyte-scale file would
/// have reached this machine's whole RAM.
///
/// An index claiming more chunks than the file can hold is not describing this
/// file, so the whole index is declined and the caller falls back to walking.
fn chunk_ceiling(data: &[u8]) -> usize {
    data.len() / 8
}

/// Read the chunk header at `at` and check it is the chunk an index entry
/// claims: same id, same size. This is what turns both offset-base derivations
/// below from an assumption into a check — the `idx1` base is documented as
/// ambiguous by Microsoft itself, and the `ix##` base has no fixture here at
/// all.
fn chunk_matches(data: &[u8], at: usize, id: [u8; 4], size: u32) -> bool {
    at.saturating_add(8) <= data.len() && fourcc(data, at) == id && u32le(data, at + 4) == size
}

/// `idx1`: 16-byte entries of `dwChunkId, dwFlags, dwOffset, dwSize`.
///
/// Offsets are to the chunk *header*. The base is normally the position of the
/// `movi` FOURCC (so the first entry reads 4), but Microsoft's `AVIOLDINDEX`
/// reference states outright that "in some AVI files it is given as an offset
/// from the start of the file" — VirtualDub wrote absolute offsets before build
/// 4936. Both candidates are tried and the one whose target actually holds the
/// declared chunk wins; neither matching yields `None` and the caller falls
/// back to walking, rather than indexing garbage.
fn index_from_idx1(
    data: &[u8],
    start: usize,
    end: usize,
    movi_body: usize,
    prefix: [u8; 2],
) -> Option<(Vec<Chunk>, Option<u64>, usize)> {
    let n = end.saturating_sub(start) / 16;
    if n == 0 {
        return None;
    }
    // The base is derived from entry 0's *own* chunk id, which need not be a
    // video chunk: on the corpus's video+MP3 file entry 0 is `01wb`.
    let e0 = start;
    let (id0, off0, size0) = (fourcc(data, e0), u32le(data, e0 + 8), u32le(data, e0 + 12));
    // `movi_body` is the byte after the `movi` FOURCC; the FOURCC itself is the
    // documented base, four bytes earlier.
    let movi_fourcc = movi_body.checked_sub(4)?;
    let base = [movi_fourcc, 0]
        .into_iter()
        .find(|&b| chunk_matches(data, b.saturating_add(off0 as usize), id0, size0))?;

    let mut chunks = Vec::new();
    let mut bytes = 0u64;
    // Every entry for this stream, zero-length padding included: this is the
    // count `strh.dwLength` is compared against, and `dwLength` counts units,
    // not payload-bearing ones.
    let mut units = 0usize;
    for k in 0..n {
        let e = start + k * 16;
        if data.get(e..e + 2) != Some(&prefix[..]) {
            continue;
        }
        units += 1;
        let size = u32le(data, e + 12) as u64;
        bytes += size;
        // A zero-length entry is a padding chunk (a held frame), carrying no
        // bytes to parse. It contributes nothing to either the sum or a
        // sampler's budget, so it is not indexed.
        if size == 0 {
            continue;
        }
        let offset = base.saturating_add(u32le(data, e + 8) as usize).saturating_add(8);
        // An entry pointing outside the file means this index does not describe
        // the bytes in hand, so the whole index is declined rather than partly
        // used. **The sum is what makes this load-bearing**: a byte total over
        // the entries that happen to be reachable is reported as an exact
        // video-stream rate, and on a file cut inside its own `idx1` that read
        // 331 kb/s against a true 752.
        if offset.saturating_add(size as usize) > data.len() {
            return None;
        }
        if chunks.len() >= chunk_ceiling(data) {
            return None;
        }
        chunks.push(Chunk { offset: offset as u64, size });
    }
    (!chunks.is_empty()).then_some((chunks, Some(bytes), units))
}

/// OpenDML two-level index: the `indx` super-index in the `strl` points at one
/// `ix##` standard index per RIFF segment.
///
/// Layouts, both opening with the same 12-byte preamble
/// (`wLongsPerEntry, bIndexSubType, bIndexType, nEntriesInUse, dwChunkId`):
///
/// - super (`bIndexType == 0`, 4 longs/entry): 12 reserved bytes, then
///   `{qwOffset u64, dwSize u32, dwDuration u32}` — `qwOffset` is an absolute
///   file position of an `ix##` chunk *header*.
/// - standard (`bIndexType == 1`, 2 longs/entry): `qwBaseOffset u64`, 4
///   reserved bytes, then `{dwOffset u32, dwSize u32}` — and here `dwOffset`
///   is relative to `qwBaseOffset` and points at the chunk **data**, not its
///   header, the opposite of `idx1`. `dwSize`'s bit 31 marks a non-keyframe
///   and is not part of the length.
///
/// The field-index subtype (3 longs/entry) has a different entry layout and is
/// declined rather than misread. Any structural surprise returns `None`, which
/// costs a fallback walk and never a wrong index.
fn index_from_super(
    data: &[u8],
    start: usize,
    end: usize,
    prefix: [u8; 2],
) -> Option<(Vec<Chunk>, Option<u64>, usize)> {
    // `wLongsPerEntry == 4` and `bIndexType == 0` together identify the
    // super-index layout; the field-index subtype has three longs per entry and
    // is declined here rather than misread as this one.
    if u16le(data, start) != 4 || data.get(start + 3) != Some(&0) {
        return None;
    }
    if fourcc(data, start + 8).get(..2) != Some(&prefix[..]) {
        return None;
    }
    let avail = end.saturating_sub(start.saturating_add(24)) / 16;
    let n = (u32le(data, start + 4) as usize).min(avail);
    if n == 0 {
        return None;
    }
    let mut chunks = Vec::new();
    let mut bytes = 0u64;
    let mut units = 0usize;
    for k in 0..n {
        let e = start + 24 + k * 16;
        let at = u64le(data, e);
        let size = u32le(data, e + 8) as usize;
        let at = usize::try_from(at).ok()?;
        // `qwOffset` addresses the `ix##` chunk header; its own declared size
        // bounds the sub-index, clamped to what the file actually holds.
        let sub_start = at.checked_add(8)?;
        let sub_end = sub_start.saturating_add(size.saturating_sub(8)).min(data.len());
        index_from_standard(data, sub_start, sub_end, prefix, &mut chunks, &mut bytes, &mut units)?;
    }
    (!chunks.is_empty()).then_some((chunks, Some(bytes), units))
}

fn index_from_standard(
    data: &[u8],
    start: usize,
    end: usize,
    prefix: [u8; 2],
    chunks: &mut Vec<Chunk>,
    bytes: &mut u64,
    units: &mut usize,
) -> Option<()> {
    if u16le(data, start) != 2 || data.get(start + 3) != Some(&1) {
        return None;
    }
    let id = fourcc(data, start + 8);
    if id.get(..2) != Some(&prefix[..]) {
        return None;
    }
    let base = usize::try_from(u64le(data, start + 12)).ok()?;
    let avail = end.saturating_sub(start.saturating_add(24)) / 8;
    let n = (u32le(data, start + 4) as usize).min(avail);
    if n == 0 {
        return None;
    }
    // Validate the base on entry 0 before trusting any of them: the entry
    // addresses chunk data, so the header sits eight bytes earlier.
    let first = base.saturating_add(u32le(data, start + 24) as usize);
    let first_size = u32le(data, start + 28) & 0x7FFF_FFFF;
    if !chunk_matches(data, first.checked_sub(8)?, id, first_size) {
        return None;
    }
    for k in 0..n {
        let e = start + 24 + k * 8;
        *units += 1;
        let size = (u32le(data, e + 4) & 0x7FFF_FFFF) as u64;
        *bytes += size;
        if size == 0 {
            continue;
        }
        let offset = base.saturating_add(u32le(data, e) as usize);
        // Declines the whole index for the same reason `index_from_idx1` does:
        // a total summed over the reachable half is not an exact byte count.
        if offset.saturating_add(size as usize) > data.len() {
            return None;
        }
        // The running total across *every* sub-index this super-index names,
        // not just this one — see `chunk_ceiling`. Checked inside the loop so a
        // single hostile sub-index cannot outrun it either.
        if chunks.len() >= chunk_ceiling(data) {
            return None;
        }
        chunks.push(Chunk { offset: offset as u64, size });
    }
    Some(())
}

/// Bounded fallback: walk the `movi` lists themselves.
///
/// Only reached by a file carrying neither index form, which neither ffmpeg nor
/// VirtualDub produces.
///
/// The walk stops at [`HEAD_SCAN_BYTES`], so on a large file it yields chunk
/// positions and no byte total: a sum would describe the head window rather than
/// the stream, which is the wrong-number-shaped answer the bitrate contract
/// forbids. But when every `movi` list ended *inside* that bound the walk
/// provably saw the whole stream, and then the sum is as exact as an index's —
/// so the total is reported. Nothing is guessed either way; the difference is
/// whether the walk can show it finished.
fn walk_movi(data: &[u8], movi: &[(usize, usize)], prefix: [u8; 2]) -> (Vec<Chunk>, Option<u64>) {
    let mut chunks = Vec::new();
    let Some(&(first_body, _)) = movi.first() else { return (chunks, None) };
    let limit = first_body.saturating_add(HEAD_SCAN_BYTES);
    let mut whole = true;
    for &(body, end) in movi {
        if body >= limit || end > limit {
            whole = false;
        }
        if body >= limit {
            break;
        }
        collect_movi(data, body, end.min(limit), prefix, &mut chunks, 0);
    }
    let bytes = whole.then(|| chunks.iter().map(|c| c.size).sum());
    (chunks, bytes)
}

/// Chunks may be grouped in `LIST rec ` blocks, which is why this recurses —
/// one level deep in every writer observed, and bounded regardless.
fn collect_movi(
    data: &[u8],
    start: usize,
    end: usize,
    prefix: [u8; 2],
    out: &mut Vec<Chunk>,
    depth: u8,
) {
    for (id, body, body_end) in chunks_in(data, start, end) {
        if id == *b"LIST" {
            if depth < 2 && body + 4 <= body_end {
                collect_movi(data, body + 4, body_end, prefix, out, depth + 1);
            }
            continue;
        }
        if id.get(..2) != Some(&prefix[..]) {
            continue;
        }
        // A chunk straddling the walk's bound arrives with `body_end` already
        // clamped, so pushing it would hand the sampler a cut access unit that
        // parses as a complete one. The byte total is withheld in that case
        // anyway; the chunk has to go too.
        //
        // The test is the chunk's own *declared* size, read back from the
        // header four bytes behind the body — the clamped length cannot tell a
        // truncated chunk from one that legitimately ends where the list does,
        // which is every walk's last chunk.
        let size = body_end.saturating_sub(body);
        let declared = u32le(data, body.wrapping_sub(4)) as usize;
        if size > 0 && body.saturating_add(declared) <= end {
            out.push(Chunk { offset: body as u64, size: size as u64 });
        }
    }
}

// --- codec identity and bitstream fields -------------------------------------

/// Render a `biCompression` value for the report.
///
/// Printable ASCII is the FourCC itself, which is the identifier a user
/// recognises and what MediaInfo shows as CodecID. Anything else becomes the
/// hex form — `BI_RGB`, uncompressed video, is the integer 0, and printing four
/// NUL bytes (or four attacker-chosen bytes that spell an ANSI escape) into a
/// terminal is not an option. MediaInfo prints `0x00000000` for that same file,
/// so the two agree.
fn fourcc_label(f: &[u8; 4]) -> String {
    if f.iter().all(|b| (0x20..=0x7E).contains(b)) && f.iter().any(|b| *b != b' ') {
        return String::from_utf8_lossy(f).trim_end().to_string();
    }
    format!("0x{:08X}", u32::from_le_bytes(*f))
}

/// Fill everything the container cannot state — depth, chroma, profile, colour
/// and (preferentially) the frame rate — from the codec's own headers.
///
/// AVI puts those headers in one of two places and the split is per codec, not
/// per file: an AVC or HEVC remux carries a configuration record in the `strf`
/// extradata, while a fresh encode of anything carries its headers in the first
/// `movi` chunk. Verified across the corpus: `h264_odml.avi` and
/// `mpeg4p2_real.avi` both declare `biSize == 40` with zero extradata and open
/// their first chunk with an Annex-B SPS and a VOS respectively.
fn fill_from_bitstream(td: &mut TrackDemux, data: &[u8], extradata: &[u8]) {
    match td.codec {
        Codec::Avc | Codec::Hevc if bmih::is_config_record(extradata) => {
            fill_from_config_record(td, extradata)
        }
        Codec::Avc | Codec::Hevc => {
            // **Bounded to the head of the index, like every sibling arm.**
            // `best_sps` walks whatever chunk list it is handed, splitting each
            // into NAL units, and only stops early at 3840 pixels wide — which
            // an AVI essentially never reaches. Every other caller passes a
            // bounded reassembled buffer; this one has the *whole file's* index
            // in hand, so an unbounded call reads every access unit of it on the
            // **default** path: measured at 643 ms for a 354 MB 1080p AVI, and
            // far worse on a network volume, where only the sampler's sparse
            // spread is warmed. Nothing is lost by bounding it: AVI has no
            // parameter-set slot for Annex-B, so the SPS opens chunk 0 in every
            // file observed, and the MPEG and Part 2 fills beside this take the
            // same first 32.
            let head = &td.chunks[..td.chunks.len().min(SPS_SCAN_CHUNKS)];
            let best = super::best_sps(data, head, &td.codec);
            td.sps_chunk = best.as_ref().map(|b| b.chunk);
            let (w, h, depth, chroma, profile, color, fps) = super::sps_fields(best);
            if w > 0 && h > 0 {
                (td.width, td.height) = (w, h);
            }
            td.bit_depth = depth;
            td.chroma = chroma;
            td.codec_profile = profile;
            (td.color, td.color_source) = color;
            td.fps = fps;
        }
        Codec::Mpeg1 | Codec::Mpeg2 => super::fill_mpeg2_stream_fields(td, data),
        // Part 2 keeps the container-extradata path as well as the chunk scan:
        // some muxes copy the VOS/VOL header set into `strf`, and reading it
        // there costs no sample access at all.
        Codec::Mpeg4Part2 => super::fill_mpeg4part2_stream_fields(td, extradata, data),
        // VC-1 alone gets no chunk fallback, which is the shared helper's rule
        // rather than an omission here: Simple and Main have no in-band
        // sequence header to find, and Advanced Profile's is required to be in
        // the configuration by every carriage spec that defines one, so a chunk
        // scan could only find what `extradata` already held.
        Codec::Vc1 => super::fill_vc1_stream_fields(td, extradata),
        _ => {}
    }
}

/// The `avcC`/`hvcC` path: the record states the NAL length prefix, the profile
/// and the depth/chroma, and its embedded SPS states the colour — the same
/// treatment MP4 gives the same bytes.
fn fill_from_config_record(td: &mut TrackDemux, rec: &[u8]) {
    match td.codec {
        Codec::Hevc => {
            let Some(info) = super::parse_hvcc_record(rec) else { return };
            td.nal_format = NalFormat::LengthPrefixed(info.nal_len);
            td.bit_depth = Some(info.bit_depth);
            td.chroma = Some(info.chroma.to_string());
            td.codec_profile = Some(info.profile_str);
            if let Some(c) = super::color_from_hvcc(rec) {
                (td.color, td.color_source) = c;
            }
            if let Some(sps) =
                crate::hevc::sps::find_sps_in_hvcc(rec).and_then(crate::hevc::sps::parse_sps)
            {
                td.fps = sps.frame_rate;
                // The coded picture size, which outranks `strf`'s the same way
                // the Annex-B arm's does — `biWidth`/`biHeight` are the muxer's
                // word and the SPS is the bitstream's.
                if sps.width > 0 && sps.height > 0 {
                    (td.width, td.height) = (sps.width, sps.height);
                }
            }
        }
        _ => {
            let Some(info) = super::parse_avcc_record(rec) else { return };
            td.nal_format = NalFormat::LengthPrefixed(info.nal_len);
            td.bit_depth = Some(info.bit_depth);
            td.chroma = Some(info.chroma.to_string());
            td.codec_profile = Some(info.profile_str);
            if let Some(c) = super::color_from_avcc(rec) {
                (td.color, td.color_source) = c;
            }
            if let Some(sps) =
                crate::avc::nal::find_sps_in_avcc(rec).and_then(crate::avc::sps::parse_sps)
            {
                td.fps = sps.frame_rate;
                if sps.width > 0 && sps.height > 0 {
                    (td.width, td.height) = (sps.width, sps.height);
                }
            }
        }
    }
}

// --- prefetch support --------------------------------------------------------

/// Byte ranges a remote probe should warm beyond the head window: the `idx1`
/// tail chunk and any OpenDML `ix##` sub-indexes.
///
/// The exact-extent analogue of the MKV `Tags` and TS tail-PCR warms. Both
/// index forms sit outside any head window — `idx1` after all the data, `ix##`
/// once per RIFF segment — and both are reached by arithmetic, so their
/// positions are known before a byte of them is faulted. Hint-only: a wrong or
/// missing extent costs a cold read, never a changed report.
pub fn index_extents(data: &[u8]) -> Vec<(u64, usize)> {
    let mut out = Vec::new();
    if !is_avi(data) {
        return out;
    }
    let (segments, _) = riff_segments(data);
    let Some(first) = segments.first() else { return out };
    // Only on a single-segment file, matching `build_index`: a multi-segment
    // file never reads `idx1`, so warming it would stream bytes nothing parses.
    if segments.len() == 1 {
        if let Some((b, e)) = find_chunk(data, first.body, first.end, b"idx1") {
            out.push((b as u64, e.saturating_sub(b)));
        }
    }
    let Some(hdrl) = find_list(data, first.body, first.end, b"hdrl") else { return out };
    for (index, s) in parse_hdrl(data, hdrl.0, hdrl.1).into_iter().enumerate() {
        // Video streams only, and only a super-index `index_from_super` would
        // itself accept. Without both tests this warms the audio track's
        // sub-indexes and any placeholder that happens to sit here — bytes the
        // demux never reads, which on a network volume is pure transfer time.
        let Some((b, e)) = s.indx.filter(|_| &s.kind == b"vids") else { continue };
        if u16le(data, b) != 4 || data.get(b + 3) != Some(&0) {
            continue;
        }
        if stream_prefix(index).is_none_or(|p| fourcc(data, b + 8).get(..2) != Some(&p[..])) {
            continue;
        }
        let avail = e.saturating_sub(b.saturating_add(24)) / 16;
        let n = (u32le(data, b + 4) as usize).min(avail);
        for k in 0..n {
            let entry = b + 24 + k * 16;
            let Ok(at) = usize::try_from(u64le(data, entry)) else { continue };
            let size = u32le(data, entry + 8) as usize;
            if at < data.len() && size > 0 {
                out.push((at as u64, size.min(data.len() - at)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/h264_avcc.avi`'s `strf` extradata verbatim: a real 46-byte
    /// `avcC` from an `ffmpeg -i x.mp4 -c:v copy out.avi` remux. Its embedded
    /// SPS is High profile, level 1.3, 8-bit 4:2:0, with VUI timing for 25 fps —
    /// which is the *coded* rate the container on that file contradicts.
    const AVCC: [u8; 46] = [
        0x01, 0x64, 0x00, 0x0D, 0xFF, 0xE1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x0D, 0xAC, 0xD9, 0x41,
        0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xF1,
        0x42, 0x99, 0x60, 0x01, 0x00, 0x06, 0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0, 0xFD, 0xF8, 0xF8,
        0x00,
    ];

    // --- fixture assembly ---------------------------------------------------

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        // `ckSize` excludes the pad byte, so odd bodies are followed by one.
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = kind.to_vec();
        inner.extend_from_slice(body);
        chunk(b"LIST", &inner)
    }

    fn strh(kind: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, length: u32) -> Vec<u8> {
        let mut b = vec![0u8; 56];
        b[0..4].copy_from_slice(kind);
        b[4..8].copy_from_slice(handler);
        b[20..24].copy_from_slice(&scale.to_le_bytes());
        b[24..28].copy_from_slice(&rate.to_le_bytes());
        b[32..36].copy_from_slice(&length.to_le_bytes());
        chunk(b"strh", &b)
    }

    fn strf(w: i32, h: i32, cc: &[u8; 4], extradata: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[0..4].copy_from_slice(&(40u32).to_le_bytes());
        b[4..8].copy_from_slice(&w.to_le_bytes());
        b[8..12].copy_from_slice(&h.to_le_bytes());
        // `biBitCount` 24 over 8-bit 4:2:0 content — the value that must never
        // be reported as a depth.
        b[14..16].copy_from_slice(&24u16.to_le_bytes());
        b[16..20].copy_from_slice(cc);
        b.extend_from_slice(extradata);
        chunk(b"strf", &b)
    }

    /// The `odml` block exactly as ffmpeg writes it on a single-RIFF file: a
    /// `JUNK` chunk whose body opens with the FourCC `odml`, holding a `dmlh`
    /// whose frame count was never filled in.
    fn junk_odml() -> Vec<u8> {
        let mut inner = b"odml".to_vec();
        inner.extend_from_slice(&chunk(b"dmlh", &[0u8; 248]));
        chunk(b"JUNK", &inner)
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Idx1 {
        None,
        Relative,
        Absolute,
        /// Entry 0 points somewhere that holds no matching chunk.
        Broken,
    }

    /// Assemble a single-RIFF AVI: `hdrl_body` verbatim, then a `movi` holding
    /// `frames` in order, then an optional `idx1`.
    fn build(hdrl_body: &[u8], frames: &[([u8; 4], Vec<u8>)], mode: Idx1) -> Vec<u8> {
        let mut movi_body = Vec::new();
        // Offsets are recorded relative to the `movi` FOURCC, which sits four
        // bytes before the list body — hence the leading 4.
        let mut entries: Vec<([u8; 4], u32, u32)> = Vec::new();
        for (id, payload) in frames {
            entries.push((*id, 4 + movi_body.len() as u32, payload.len() as u32));
            movi_body.extend_from_slice(&chunk(id, payload));
        }
        let hdrl = list(b"hdrl", hdrl_body);
        let movi = list(b"movi", &movi_body);
        // 12 bytes of `RIFF`/size/`AVI `, then hdrl, then the movi list header
        // and its type word.
        let movi_fourcc = 12 + hdrl.len() + 8;

        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(&hdrl);
        body.extend_from_slice(&movi);
        if mode != Idx1::None {
            let mut idx = Vec::new();
            for (i, (id, rel, size)) in entries.iter().enumerate() {
                idx.extend_from_slice(id);
                idx.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
                let off = match mode {
                    Idx1::Relative | Idx1::None => *rel,
                    Idx1::Absolute => movi_fourcc as u32 + *rel,
                    // Only entry 0 decides the base, so breaking it is enough.
                    // Zero rather than something wildly out of range: an
                    // unreachable offset is caught by the bounds check further
                    // down, which would mask the base derivation entirely. This
                    // one is perfectly in range and simply names the wrong
                    // place — the `movi` FOURCC rather than a chunk header — so
                    // only the validation can refuse it.
                    Idx1::Broken if i == 0 => 0,
                    Idx1::Broken => *rel,
                };
                idx.extend_from_slice(&off.to_le_bytes());
                idx.extend_from_slice(&size.to_le_bytes());
            }
            body.extend_from_slice(&chunk(b"idx1", &idx));
        }
        chunk(b"RIFF", &body)
    }

    /// One video stream, 320x240, `length` units at `rate/scale`.
    fn video_hdrl(cc: &[u8; 4], scale: u32, rate: u32, length: u32, extradata: &[u8]) -> Vec<u8> {
        let mut strl = strh(b"vids", cc, scale, rate, length);
        strl.extend_from_slice(&strf(320, 240, cc, extradata));
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        hdrl.extend_from_slice(&list(b"strl", &strl));
        hdrl.extend_from_slice(&junk_odml());
        hdrl
    }

    fn frames(id: &[u8; 4], sizes: &[usize]) -> Vec<([u8; 4], Vec<u8>)> {
        sizes.iter().map(|&n| (*id, vec![0xAA; n])).collect()
    }

    // --- tests --------------------------------------------------------------

    #[test]
    fn a_single_riff_avi_reports_its_header_facts_and_an_exact_bitrate() {
        let f = frames(b"00dc", &[1000, 500, 500]);
        let d = demux(&build(&video_hdrl(b"XVID", 1, 25, 3, &[]), &f, Idx1::Relative))
            .expect("demuxes");
        assert_eq!(d.container, CONTAINER_LABEL);
        // 3 units at 25/1.
        assert_eq!(d.duration_secs, Some(0.12));
        let t = &d.tracks[0];
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.codec, Codec::Mpeg4Part2);
        assert_eq!(t.fps, Some(25.0));
        assert_eq!(t.chunks.len(), 3);
        // 2000 bytes over 0.12 s, from the index's own sizes.
        let b = t.bitrate.expect("exact rate");
        assert_eq!(b.scope, crate::model::BitrateScope::VideoStream);
        assert!((b.bits_per_sec - 2000.0 * 8.0 / 0.12).abs() < 1e-6);
        // An index that covers the file is an exhaustive index, so `--full`
        // really does see every access unit.
        assert!(!d.bounded_index);
    }

    #[test]
    fn the_frame_rate_prefers_the_coded_stream_over_the_container_unit_rate() {
        // `h264_avcc.avi`'s shape: an `-c:v copy` remux declares 1200 units at
        // 600/1 for a 2-second 25 fps clip, padding the difference with
        // zero-length chunks. MediaInfo reports 600.000 fps for it.
        let mut f = frames(b"00dc", &[4000]);
        f.extend(frames(b"00dc", &[0; 23]));
        let d = demux(&build(&video_hdrl(b"avc1", 1, 600, 24, &AVCC), &f, Idx1::Relative))
            .expect("demuxes");
        let t = &d.tracks[0];
        assert_eq!(t.codec, Codec::Avc);
        // The coded stream's own VUI timing wins. Delete the `fps.is_none()`
        // guard in `demux` and this reads 600.0.
        assert_eq!(t.fps, Some(25.0));
        // The duration is the container's either way, and is right: both halves
        // of `dwLength / (dwRate/dwScale)` count the same units.
        assert_eq!(d.duration_secs, Some(0.04));
        // The record states the framing, the profile and the depth.
        assert!(matches!(t.nal_format, NalFormat::LengthPrefixed(4)));
        assert_eq!(t.codec_profile.as_deref(), Some("High @ L1.3"));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        // The 23 padding chunks carry nothing and are not indexed.
        assert_eq!(t.chunks.len(), 1);
    }

    #[test]
    fn a_container_rate_still_applies_when_the_stream_signals_none() {
        // MJPEG has no parser here, so nothing fills `fps` from the bitstream
        // and the container's rate is the answer — which is the normal case for
        // every codec whose headers state no rate.
        let f = frames(b"00dc", &[100, 100]);
        let d =
            demux(&build(&video_hdrl(b"MJPG", 1, 25, 2, &[]), &f, Idx1::Relative)).expect("demuxes");
        assert_eq!(d.tracks[0].fps, Some(25.0));
        assert_eq!(d.tracks[0].codec, Codec::Other("MJPG".to_string()));
    }

    #[test]
    fn a_junk_wrapped_dmlh_is_never_a_frame_count() {
        // ffmpeg reserves the `odml` block as a `JUNK` chunk and rewrites the id
        // to `LIST` only if the file rolls over to OpenDML, so on a single-RIFF
        // file `dmlh.dwTotalFrames` is a zero placeholder. Every corpus AVI is
        // like this. A byte search for `dmlh` finds it and reads 0, which would
        // make the duration 0 and the bitrate infinite.
        let built = build(&video_hdrl(b"XVID", 1, 25, 50, &[]), &frames(b"00dc", &[400]), Idx1::Relative);
        // The trap really is present in the fixture.
        let at = built.windows(4).position(|w| w == b"dmlh").expect("dmlh present");
        assert_eq!(u32le(&built, at + 8), 0, "the placeholder frame count is zero");
        assert_eq!(&built[at - 12..at - 8], b"JUNK", "and it is inside a JUNK chunk");
        // 50 units at 25/1 from `strh.dwLength`, not 0 from `dmlh`.
        assert_eq!(demux(&built).unwrap().duration_secs, Some(2.0));
    }

    /// Locate a built fixture's `movi` body and `idx1` body, so a test can call
    /// an index parser directly instead of through `demux`.
    ///
    /// **Going through `demux` is what made two earlier versions of these tests
    /// untestable**: `build_index` falls back to `walk_movi`, and on a fixture
    /// small enough to fit inside the walk's 8 MiB bound the walk returns a
    /// byte-identical chunk list and byte total. So an index parser could be
    /// deleted outright and the assertions would still pass, rescued by the
    /// fallback. These are the two parsers with no real-file fixture anywhere,
    /// which makes their unit tests the only thing holding them up.
    fn index_regions(built: &[u8]) -> (usize, (usize, usize)) {
        let movi = built.windows(4).position(|w| w == b"movi").expect("movi") + 4;
        let at = built.windows(4).position(|w| w == b"idx1").expect("idx1");
        let size = u32le(built, at + 4) as usize;
        (movi, (at + 8, at + 8 + size))
    }

    #[test]
    fn the_idx1_offset_base_is_derived_from_entry_zero_not_assumed() {
        let f = frames(b"00dc", &[1000, 500]);
        let hdrl = video_hdrl(b"XVID", 1, 25, 2, &[]);
        // Both conventions occur — Microsoft's own reference says so, and
        // VirtualDub wrote absolute offsets before build 4936 — and the parser
        // must accept both. Called directly, because the fallback walk would
        // otherwise supply the same answer whatever this parser did.
        let mut got = Vec::new();
        for mode in [Idx1::Relative, Idx1::Absolute] {
            let built = build(&hdrl, &f, mode);
            let (movi, (b, e)) = index_regions(&built);
            let (chunks, bytes, units) = index_from_idx1(&built, b, e, movi, *b"00")
                .unwrap_or_else(|| panic!("{:?} base must resolve", mode as u8));
            assert_eq!((chunks.len(), bytes, units), (2, Some(1500), 2));
            // The offsets really name payload: the fixture fills frames 0xAA,
            // so a base off by even the 8-byte chunk header lands on a header.
            for c in &chunks {
                let s = c.offset as usize;
                assert!(built[s..s + c.size as usize].iter().all(|&b| b == 0xAA));
            }
            got.push(chunks.iter().map(|c| c.offset as usize - movi).collect::<Vec<_>>());
        }
        // Both conventions describe the same chunks, relative to `movi`.
        assert_eq!(got[0], got[1]);
    }

    /// Every chunk in `t` covers payload the fixtures fill with `0xAA` — which
    /// an offset shifted by so much as the 8-byte chunk header would break,
    /// since the byte before a payload is the last of `00dc`'s size field.
    fn chunks_point_at_payload(file: &[u8], chunks: &[Chunk]) {
        assert!(!chunks.is_empty());
        for c in chunks {
            let s = c.offset as usize;
            let e = s + c.size as usize;
            assert!(
                file[s..e].iter().all(|&b| b == 0xAA),
                "chunk at {s} is not payload: {:02X?}",
                &file[s..e.min(s + 8)]
            );
        }
    }

    #[test]
    fn an_unvalidatable_idx1_falls_back_to_walking_rather_than_indexing_garbage() {
        let f = frames(b"00dc", &[1000, 500]);
        let built = build(&video_hdrl(b"XVID", 1, 25, 2, &[]), &f, Idx1::Broken);
        let d = demux(&built).unwrap();
        let t = &d.tracks[0];
        // The walk finds both chunks and they point at real payload, rather than
        // at whatever entry 0's bogus 0xDEAD offset addressed. Remove the
        // `chunk_matches` check in `index_from_idx1` and the offsets shift.
        assert_eq!(t.chunks.len(), 2);
        chunks_point_at_payload(&built, &t.chunks);
        // This file is small enough that the walk provably finished, so its own
        // sum is exact — and it is the walk's 1500 bytes, never the broken
        // index's.
        let b = t.bitrate.expect("the completed walk states a rate");
        assert!((b.bits_per_sec - 1500.0 * 8.0 / 0.08).abs() < 1e-6, "{b:?}");
    }

    #[test]
    fn a_walk_that_ran_past_its_bound_states_no_byte_total() {
        // Neither index form present and a `movi` larger than the walk's window,
        // so the walk cannot show it finished. Three chunks of 3 MiB: two fit
        // inside the 8 MiB bound, the third straddles it. The two that fit are
        // usable, but a sum over them would describe the window rather than the
        // stream, so the rate falls to the whole-container one and the index is
        // marked bounded — which is what keeps `--full`'s sampled marks on.
        let span = 3 << 20;
        let f = frames(b"00dc", &[span, span, span]);
        let built = build(&video_hdrl(b"XVID", 1, 25, 3, &[]), &f, Idx1::None);
        let d = demux(&built).unwrap();
        let t = &d.tracks[0];
        // **The straddling chunk is dropped, not truncated.** `chunks_in`
        // clamps a chunk that runs past the bound, so indexing it would hand
        // the sampler a cut access unit that parses as a whole one.
        assert_eq!(t.chunks.len(), 2, "the third chunk crosses the bound and is not indexed");
        for c in &t.chunks {
            assert_eq!(c.size, span as u64, "and the two kept are whole");
        }
        chunks_point_at_payload(&built, &t.chunks);
        assert_eq!(t.bitrate.map(|b| b.scope), Some(crate::model::BitrateScope::Overall));
        assert!(d.bounded_index, "a bounded walk keeps the report's sampled marks on");
    }

    #[test]
    fn index_entries_are_filtered_by_stream_and_entry_zero_may_be_audio() {
        // The corpus's `mpeg4p2_multistream.avi` shape: `idx1` entry 0 is the
        // MP3 track, and summing unfiltered puts the video rate 12.5% high.
        let mut strl = strh(b"vids", b"XVID", 1, 25, 2);
        strl.extend_from_slice(&strf(320, 240, b"XVID", &[]));
        let mut audio = strh(b"auds", &[0; 4], 3, 125, 100);
        audio.extend_from_slice(&chunk(b"strf", &[0u8; 18]));
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        hdrl.extend_from_slice(&list(b"strl", &strl));
        hdrl.extend_from_slice(&list(b"strl", &audio));

        let f = vec![
            (*b"01wb", vec![0x11; 384]),
            (*b"00dc", vec![0xAA; 1000]),
            (*b"01wb", vec![0x11; 384]),
            (*b"00dc", vec![0xAA; 1000]),
        ];
        let d = demux(&build(&hdrl, &f, Idx1::Relative)).unwrap();
        assert_eq!(d.tracks.len(), 1, "only the vids stream is reported");
        let t = &d.tracks[0];
        assert_eq!(t.chunks.len(), 2);
        // 2000 video bytes over the *video* stream's 0.08 s — not the 2768
        // total, and not over the file duration, which the longer audio stream
        // sets to 2.4 s.
        assert_eq!(d.duration_secs, Some(2.4));
        let b = t.bitrate.unwrap();
        assert!((b.bits_per_sec - 2000.0 * 8.0 / 0.08).abs() < 1e-6, "{b:?}");
    }

    // --- OpenDML ------------------------------------------------------------

    /// Assemble a two-segment OpenDML file with an `indx` super-index and one
    /// `ix00` per segment, laid out as ffmpeg does: the sub-index follows its
    /// segment's `movi`, the super-index sits in the `strl`, and an `idx1`
    /// covering **only the first segment** closes it — which ffmpeg writes
    /// unconditionally (`if (avi->riff_id == 1) avi_write_idx1(s)`) and which is
    /// exactly the index that must not be summed here.
    ///
    /// Offsets are patched after assembly, which is what the real writer does
    /// too (it seeks back at trailer time).
    fn build_opendml(seg0: &[usize], seg1: &[usize], data_relative: bool) -> Vec<u8> {
        fn movi_and_ix(frames: &[usize], base_hint: usize) -> (Vec<u8>, Vec<(u32, u32)>) {
            let mut body = Vec::new();
            let mut entries = Vec::new();
            for &n in frames {
                // `dwOffset` addresses the chunk *data*, so it skips the header.
                entries.push(((base_hint + body.len() + 8) as u32, n as u32));
                body.extend_from_slice(&chunk(b"00dc", &vec![0xAA; n]));
            }
            (body, entries)
        }
        fn ix_chunk(base: u64, entries: &[(u32, u32)], data_relative: bool) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(&2u16.to_le_bytes()); // wLongsPerEntry
            b.push(0); // bIndexSubType
            b.push(1); // bIndexType = AVI_INDEX_OF_CHUNKS
            b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            b.extend_from_slice(b"00dc");
            b.extend_from_slice(&base.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes()); // reserved
            for &(off, size) in entries {
                let off = if data_relative { off } else { off - 8 };
                b.extend_from_slice(&off.to_le_bytes());
                // Bit 31 marks a non-keyframe and is not part of the length.
                b.extend_from_slice(&(size | 0x8000_0000).to_le_bytes());
            }
            chunk(b"ix00", &b)
        }

        let mut strl = strh(b"vids", b"XVID", 1, 25, (seg0.len() + seg1.len()) as u32);
        strl.extend_from_slice(&strf(320, 240, b"XVID", &[]));
        // Two 16-byte super-index entries, patched below.
        let mut sup = Vec::new();
        sup.extend_from_slice(&4u16.to_le_bytes());
        sup.push(0);
        sup.push(0); // AVI_INDEX_OF_INDEXES
        sup.extend_from_slice(&2u32.to_le_bytes());
        sup.extend_from_slice(b"00dc");
        sup.extend_from_slice(&[0u8; 12]);
        sup.extend_from_slice(&[0u8; 32]);
        strl.extend_from_slice(&chunk(b"indx", &sup));
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        hdrl.extend_from_slice(&list(b"strl", &strl));
        let hdrl = list(b"hdrl", &hdrl);

        // Segment 0.
        let movi0_fourcc = 12 + hdrl.len() + 8;
        let (movi0_body, e0) = movi_and_ix(seg0, 4);
        let ix0 = ix_chunk(movi0_fourcc as u64, &e0, data_relative);
        let movi0 = list(b"movi", &movi0_body);
        let ix0_pos = 12 + hdrl.len() + movi0.len();
        let mut idx1 = Vec::new();
        let mut rel = 4u32;
        for &n in seg0 {
            idx1.extend_from_slice(b"00dc");
            idx1.extend_from_slice(&0x10u32.to_le_bytes());
            idx1.extend_from_slice(&rel.to_le_bytes());
            idx1.extend_from_slice(&(n as u32).to_le_bytes());
            rel += 8 + n as u32 + (n as u32 & 1);
        }
        let mut b0 = b"AVI ".to_vec();
        b0.extend_from_slice(&hdrl);
        b0.extend_from_slice(&movi0);
        b0.extend_from_slice(&ix0);
        b0.extend_from_slice(&chunk(b"idx1", &idx1));
        let seg0_bytes = chunk(b"RIFF", &b0);

        // Segment 1.
        let movi1_fourcc = seg0_bytes.len() + 12 + 8;
        let (movi1_body, e1) = movi_and_ix(seg1, 4);
        let ix1 = ix_chunk(movi1_fourcc as u64, &e1, data_relative);
        let movi1 = list(b"movi", &movi1_body);
        let ix1_pos = seg0_bytes.len() + 12 + movi1.len();
        let mut b1 = b"AVIX".to_vec();
        b1.extend_from_slice(&movi1);
        b1.extend_from_slice(&ix1);
        let seg1_bytes = chunk(b"RIFF", &b1);

        let mut out = seg0_bytes;
        out.extend_from_slice(&seg1_bytes);

        // Patch the super-index entries: absolute position of each `ix00`
        // chunk header, and its size *including* that header.
        let sup_body = out.windows(4).position(|w| w == b"indx").expect("indx") + 8;
        for (k, (pos, len)) in [(ix0_pos, ix0.len()), (ix1_pos, ix1.len())].into_iter().enumerate() {
            let e = sup_body + 24 + k * 16;
            out[e..e + 8].copy_from_slice(&(pos as u64).to_le_bytes());
            out[e + 8..e + 12].copy_from_slice(&(len as u32).to_le_bytes());
        }
        out
    }

    #[test]
    fn an_opendml_super_index_spans_every_riff_segment() {
        let built = build_opendml(&[1000, 1000], &[500], true);
        // Called directly: through `demux` the fallback walk covers both
        // segments of a fixture this small and returns the same three chunks
        // and the same 2500 bytes, so every assertion below would pass with
        // `index_from_super` deleted outright.
        let at = built.windows(4).position(|w| w == b"indx").expect("indx") + 8;
        let end = at + u32le(&built, at - 4) as usize;
        let (chunks, bytes, units) =
            index_from_super(&built, at, end, *b"00").expect("the super-index must resolve");
        // Three chunks across two RIFF segments — the second of which `idx1`
        // could never have covered — with bit 31 of every size masked off.
        assert_eq!((chunks.len(), bytes, units), (3, Some(2500), 3));
        chunks_point_at_payload(&built, &chunks);

        // And the whole report agrees.
        let d = demux(&built).expect("demuxes");
        assert_eq!(d.duration_secs, Some(0.12));
        let b = d.tracks[0].bitrate.expect("exact rate");
        assert_eq!(b.scope, crate::model::BitrateScope::VideoStream);
        assert!((b.bits_per_sec - 2500.0 * 8.0 / 0.12).abs() < 1e-6, "{b:?}");
        assert!(!d.bounded_index);
    }

    #[test]
    fn a_standard_index_entry_addresses_chunk_data_not_its_header() {
        // The convention differs from `idx1`'s, and getting it wrong shifts
        // every chunk eight bytes early, onto the `00dc` header. `data_relative:
        // false` builds the file a header-relative reading would expect: the
        // validation must reject that index and fall back, rather than adopt it
        // and index the whole stream off by a header.
        let right = build_opendml(&[1000], &[500], true);
        let d = demux(&right).unwrap();
        chunks_point_at_payload(&right, &d.tracks[0].chunks);

        let wrong = build_opendml(&[1000], &[500], false);
        let d = demux(&wrong).unwrap();
        // Drop the `checked_sub(8)` in `index_from_standard` and this file's
        // index validates, every chunk lands on a header, and this fails.
        chunks_point_at_payload(&wrong, &d.tracks[0].chunks);
    }

    #[test]
    fn idx1_is_not_summed_when_a_second_riff_segment_exists() {
        // `idx1` covers only the first RIFF chunk — measured at a 22% byte
        // undercount on a real two-segment file — so a multi-segment file whose
        // super-index is unusable gets an honest whole-container rate instead of
        // an exact-looking one computed off two thirds of the data.
        let mut f = build_opendml(&[1000, 1000], &[500], true);
        // Blank the super-index id so only the first segment's `idx1` remains,
        // which is the situation the single-RIFF gate exists for.
        let at = f.windows(4).position(|w| w == b"indx").unwrap();
        f[at..at + 4].copy_from_slice(b"JUNK");
        // The fixture really does still carry that `idx1`, or this test could
        // not fail: without it the gate has nothing to gate.
        assert!(f.windows(4).any(|w| w == b"idx1"), "the first segment's index is present");
        let d = demux(&f).unwrap();
        // The walk covers both segments and states their true 2500 bytes.
        // Summing the `idx1` instead would give the first segment's 2000, an
        // exact-looking rate 20% low — which is the OpenDML undercount in
        // miniature (measured at 22% on a real two-segment file).
        let b = d.tracks[0].bitrate.expect("the completed walk states a rate");
        assert!((b.bits_per_sec - 2500.0 * 8.0 / 0.12).abs() < 1e-6, "{b:?}");
    }

    // --- refusals and malformed input ---------------------------------------

    #[test]
    fn a_truncated_file_reports_no_bitrate_rather_than_a_low_one() {
        let hdrl = video_hdrl(b"XVID", 1, 25, 3, &[]);
        let full = build(&hdrl, &frames(b"00dc", &[1000, 1000, 1000]), Idx1::Relative);

        // Cut inside the `idx1`. The surviving entries are readable and sum to
        // a plausible-looking total, which used to be reported as an exact
        // video-stream rate: on a real 5 MiB file cut this way it read
        // 331 kb/s against a true 752. The index must describe the whole file
        // or none of it.
        let idx1_at = full.windows(4).position(|w| w == b"idx1").unwrap();
        let cut = &full[..idx1_at + 8 + 16];
        let d = demux(cut).expect("still reports what it can");
        assert_eq!(d.duration_secs, Some(0.12), "the declared duration is a header fact");
        assert!(d.tracks[0].bitrate.is_none(), "no rate at all beats a low one");

        // Cut before the index entirely: no exact count is even reachable, and
        // the whole-container fallback would divide the *present* bytes by the
        // *declared* runtime.
        let d = demux(&full[..full.len() / 2]).expect("still reports what it can");
        assert!(d.tracks[0].bitrate.is_none());
        // A complete file is unaffected — this is the control that stops the
        // guard from simply switching the rate off everywhere.
        assert!(demux(&full).unwrap().tracks[0].bitrate.is_some());
    }

    #[test]
    fn an_index_entry_pointing_outside_the_file_declines_the_whole_index() {
        // Distinct from truncation, and not covered by either check for it: the
        // RIFF size is right, the entry count matches `dwLength`, and one entry
        // simply names bytes that are not there — a damaged or crafted index
        // rather than a cut file. Skipping just that entry would leave the
        // chunk list short while the byte total still counted its size, so the
        // rate would be reported as exact over data nobody read.
        let mut f = build(
            &video_hdrl(b"XVID", 1, 25, 2, &[]),
            &frames(b"00dc", &[1000, 500]),
            Idx1::Relative,
        );
        let idx1_at = f.windows(4).position(|w| w == b"idx1").unwrap() + 8;
        // Entry 1's offset, out past EOF. Entry 0 is untouched, so the base
        // still derives and only the bounds check can refuse this.
        f[idx1_at + 16 + 8..idx1_at + 16 + 12].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        let d = demux(&f).unwrap();
        assert_eq!(
            d.tracks[0].chunks.len(),
            2,
            "the fallback walk supplies a complete list; a partly-used index would give 1"
        );
    }

    #[test]
    fn an_index_covering_fewer_units_than_the_header_declares_states_no_rate() {
        // A two-segment file cut at or before its `AVIX` header presents as
        // single-segment with every declared size still fitting, so the
        // structural gate passes and `idx1` looks complete. It is not: it covers
        // segment 0 only. Measured on a 1.22 GiB file cut that way, this
        // reported a 17.6% undercount *labelled as an exact video-stream rate*.
        // `strh.dwLength` is the whole-file unit count and catches it exactly.
        let f = frames(b"00dc", &[1000, 1000]);
        let short = demux(&build(&video_hdrl(b"XVID", 1, 25, 5, &[]), &f, Idx1::Relative)).unwrap();
        assert_eq!(short.tracks[0].chunks.len(), 2, "the chunks it does hold stay usable");
        assert!(short.tracks[0].bitrate.is_none(), "neither an exact rate nor an overall one");
        // The control: the same file whose header declares what it holds.
        let ok = demux(&build(&video_hdrl(b"XVID", 1, 25, 2, &[]), &f, Idx1::Relative)).unwrap();
        assert!(ok.tracks[0].bitrate.is_some());
    }

    #[test]
    fn broken_timing_falls_back_to_the_whole_file_frame_period() {
        // `dwScale`/`dwRate` of 0/0 comes from broken software, and reference
        // §5 sanctions `avih.dwMicroSecPerFrame` for exactly that case. Without
        // it such a file reports no rate, no duration and no bitrate where
        // ffmpeg reports all three.
        let mut hdrl = strh(b"vids", b"XVID", 0, 0, 2);
        hdrl.extend_from_slice(&strf(320, 240, b"XVID", &[]));
        let mut avih = [0u8; 56];
        avih[0..4].copy_from_slice(&40_000u32.to_le_bytes()); // 25 fps
        let mut full = chunk(b"avih", &avih);
        full.extend_from_slice(&list(b"strl", &hdrl));
        let d = demux(&build(&full, &frames(b"00dc", &[1000, 1000]), Idx1::Relative)).unwrap();
        assert_eq!(d.tracks[0].fps, Some(25.0));
        assert_eq!(d.duration_secs, Some(0.08));
        assert!(d.tracks[0].bitrate.is_some(), "and the rate has a denominator again");

        // It is video-only and last-resort: an audio stream with no timing of
        // its own must not borrow the video frame period as its duration.
        let mut audio = strh(b"auds", &[0; 4], 0, 0, 900);
        audio.extend_from_slice(&chunk(b"strf", &[0u8; 18]));
        let mut two = full.clone();
        two.extend_from_slice(&list(b"strl", &audio));
        let d = demux(&build(&two, &frames(b"00dc", &[1000, 1000]), Idx1::Relative)).unwrap();
        assert_eq!(d.duration_secs, Some(0.08), "not 900 units of the video's period");
    }

    #[test]
    fn a_riff_that_is_not_an_avi_is_refused() {
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&64u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&[0u8; 60]);
        assert!(demux(&wav).is_err());
        assert!(!is_avi(&wav));
        assert!(demux(&[]).is_err());
        assert!(demux(b"RIFF").is_err());
        // The form type carries a trailing space and it is part of the magic.
        let mut nearly = wav.clone();
        nearly[8..12].copy_from_slice(b"AVIX");
        assert!(!is_avi(&nearly));
    }

    #[test]
    fn a_header_list_with_no_video_stream_declines() {
        let mut audio = strh(b"auds", &[0; 4], 1, 44100, 100);
        audio.extend_from_slice(&chunk(b"strf", &[0u8; 18]));
        let mut hdrl = chunk(b"avih", &[0u8; 56]);
        hdrl.extend_from_slice(&list(b"strl", &audio));
        let err = demux(&build(&hdrl, &frames(b"01wb", &[100]), Idx1::Relative)).unwrap_err();
        assert!(err.to_string().contains("no video stream"), "{err}");
    }

    #[test]
    fn degenerate_timing_fields_yield_no_rate_and_no_duration() {
        // `dwScale`/`dwRate` of 0 comes from broken software, and `dwLength`
        // times a huge scale computes a duration no AVI can hold. Neither may
        // reach the report, and neither may divide.
        for (scale, rate, length) in
            [(0u32, 25u32, 50u32), (1, 0, 50), (1, 25, 0), (u32::MAX, 1, u32::MAX)]
        {
            let d = demux(&build(
                &video_hdrl(b"XVID", scale, rate, length, &[]),
                &frames(b"00dc", &[100]),
                Idx1::Relative,
            ))
            .expect("still demuxes");
            assert_eq!(d.duration_secs, None, "{scale}/{rate}/{length}");
            // No duration means no rate at all, rather than a division by zero
            // or an infinity.
            assert!(d.tracks[0].bitrate.is_none(), "{scale}/{rate}/{length}");
        }
    }

    #[test]
    fn declared_sizes_past_the_buffer_neither_panic_nor_run_away() {
        let good = build(&video_hdrl(b"XVID", 1, 25, 2, &[]), &frames(b"00dc", &[1000]), Idx1::Relative);
        // Every 4-byte size field in the file, set to values that overflow, sit
        // at the header boundary, or undercut it.
        for at in (0..good.len().saturating_sub(4)).step_by(1) {
            for v in [u32::MAX, u32::MAX - 7, 0, 1, 7] {
                let mut d = good.clone();
                d[at..at + 4].copy_from_slice(&v.to_le_bytes());
                let _ = demux(&d);
                // `index_extents` runs inside `prefetch::warm_metadata`, on the
                // same untrusted bytes and before any of `demux`'s checks, so
                // it is swept here too.
                let _ = index_extents(&d);
            }
        }
        // And a truncation at every length.
        for n in 0..good.len() {
            let _ = demux(&good[..n]);
            let _ = index_extents(&good[..n]);
        }
    }

    #[test]
    fn a_bitmapinfoheader_that_undercuts_itself_drops_the_track() {
        let mut built =
            build(&video_hdrl(b"XVID", 1, 25, 2, &[]), &frames(b"00dc", &[100]), Idx1::Relative);
        let at = built.windows(4).position(|w| w == b"strf").unwrap() + 8;
        // 12 is a `BITMAPCOREHEADER`: no FourCC, nothing recoverable.
        built[at..at + 4].copy_from_slice(&12u32.to_le_bytes());
        assert!(demux(&built).is_err());
    }

    #[test]
    fn an_unprintable_fourcc_renders_as_hex_rather_than_reaching_the_terminal() {
        // `BI_RGB`, uncompressed video, is the integer 0. MediaInfo prints
        // `0x00000000` for the same file, so the two agree; printing the bytes
        // would emit four NULs, and four attacker-chosen bytes could spell an
        // ANSI escape.
        assert_eq!(fourcc_label(&[0, 0, 0, 0]), "0x00000000");
        assert_eq!(fourcc_label(&[0x1B, b'[', b'3', b'1']), "0x31335B1B");
        assert_eq!(fourcc_label(b"XVID"), "XVID");
        // `dvc ` and friends are padded with spaces, which are printable.
        assert_eq!(fourcc_label(b"dvc "), "dvc");
        assert_eq!(fourcc_label(b"    "), "0x20202020");
    }

    #[test]
    fn repeated_super_index_entries_cannot_multiply_into_an_unbounded_index() {
        // Each index level clamps its own `nEntriesInUse` against the bytes its
        // own chunk holds, which is the house rule and is individually correct.
        // But the super-index calls the standard-index parser once per entry
        // into one shared vector, so N super entries all naming the same
        // M-entry `ix##` describe N x M chunks in a file that holds one. Only
        // entry 0's base is validated, so the repetition is legal.
        //
        // Measured before the ceiling, on the default path with exit 0: a
        // 156 KiB file declaring 2000 x 16000 allocated 771 MB, growing
        // quadratically with file size.
        // Purpose-built so the declared product genuinely exceeds the ceiling:
        // 200 super entries all naming one 200-entry sub-index, in a file of a
        // few KB. 40,000 chunks claimed, a few hundred physically possible.
        let (n_super, n_std) = (200usize, 200usize);
        let mut ix = vec![0u8; 24 + 8 * n_std];
        ix[0..2].copy_from_slice(&2u16.to_le_bytes());
        ix[3] = 1;
        ix[4..8].copy_from_slice(&(n_std as u32).to_le_bytes());
        ix[8..12].copy_from_slice(b"00dc");
        let mut sup = vec![0u8; 24 + 16 * n_super];
        sup[0..2].copy_from_slice(&4u16.to_le_bytes());
        sup[4..8].copy_from_slice(&(n_super as u32).to_le_bytes());
        sup[8..12].copy_from_slice(b"00dc");

        let mut strl = strh(b"vids", b"XVID", 1, 25, 1);
        strl.extend_from_slice(&strf(320, 240, b"XVID", &[]));
        strl.extend_from_slice(&chunk(b"indx", &sup));
        let mut hdrl_body = chunk(b"avih", &[0u8; 56]);
        hdrl_body.extend_from_slice(&list(b"strl", &strl));
        let hdrl = list(b"hdrl", &hdrl_body);
        let movi = list(b"movi", &chunk(b"00dc", &[0xAA]));
        let movi_fourcc = 12 + hdrl.len() + 8;
        let ix_pos = 12 + hdrl.len() + movi.len();
        ix[12..20].copy_from_slice(&(movi_fourcc as u64).to_le_bytes());
        for k in 0..n_std {
            let e = 24 + k * 8;
            ix[e..e + 4].copy_from_slice(&12u32.to_le_bytes()); // the one chunk's data
            ix[e + 4..e + 8].copy_from_slice(&1u32.to_le_bytes());
        }
        let ixc = chunk(b"ix00", &ix);
        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(&hdrl);
        body.extend_from_slice(&movi);
        body.extend_from_slice(&ixc);
        let mut f = chunk(b"RIFF", &body);
        let sup_at = f.windows(4).position(|w| w == b"indx").unwrap() + 8;
        for k in 0..n_super {
            let e = sup_at + 24 + k * 16;
            f[e..e + 8].copy_from_slice(&(ix_pos as u64).to_le_bytes());
            f[e + 8..e + 12].copy_from_slice(&(ixc.len() as u32).to_le_bytes());
        }
        let end = sup_at + u32le(&f, sup_at - 4) as usize;
        let ceiling = chunk_ceiling(&f);
        assert!(
            n_super * n_std > ceiling * 4,
            "the fixture must claim far more than the file can hold: {} vs {}",
            n_super * n_std,
            ceiling
        );
        // Declined outright: an index naming more chunks than the file can
        // physically hold — one per 8 bytes, its own header — is not
        // describing this file.
        match index_from_super(&f, sup_at, end, *b"00") {
            None => {}
            Some((chunks, ..)) => {
                assert!(chunks.len() <= ceiling, "{} chunks from {} bytes", chunks.len(), f.len())
            }
        }
        // And the whole path still terminates and reports, via the walk.
        let d = demux(&f).expect("still reports");
        assert!(d.tracks[0].chunks.len() <= ceiling);
    }

    #[test]
    fn a_size_zero_segment_chain_terminates() {
        // `riff_segments` advances by the declared size, so a segment declaring
        // zero would sit still without the count bound. `MAX_RIFF_SEGMENTS`
        // stops it; this pins that it is bounded at all, since nothing else
        // reaches the constant.
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(b"AVI ");
        for _ in 0..64 {
            f.extend_from_slice(b"RIFF");
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(b"AVIX");
        }
        let (segs, complete) = riff_segments(&f);
        assert!(segs.len() <= MAX_RIFF_SEGMENTS);
        assert!(complete, "a zero size declares nothing missing");
        // And the whole path declines rather than hanging or panicking.
        assert!(demux(&f).is_err());
    }

    #[test]
    fn the_prefetch_extents_name_the_index_chunks_and_nothing_else() {
        let built =
            build(&video_hdrl(b"XVID", 1, 25, 2, &[]), &frames(b"00dc", &[1000]), Idx1::Relative);
        let ext = index_extents(&built);
        assert_eq!(ext.len(), 1);
        let (at, len) = ext[0];
        assert_eq!(&built[at as usize - 8..at as usize - 4], b"idx1");
        assert_eq!(len, 16);
        // Two segments and a super-index: one `ix00` each, and *not* the first
        // segment's `idx1`, which a multi-segment file never reads.
        assert_eq!(index_extents(&build_opendml(&[100], &[100], true)).len(), 2);
        // A hint only, so nothing here may panic on rubbish.
        assert!(index_extents(b"RIFF\0\0\0\0AVI ").is_empty());
    }
}
