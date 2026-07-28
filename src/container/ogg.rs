//! Ogg (`.ogv`, `.ogg`, `.oga`, `.ogm`, `.ogx`), read against **RFC 3533**, the
//! normative encapsulation format, with the per-codec mappings named where they
//! are used: the Theora specification for Theora ([`crate::theora`]) and Xiph's
//! OggVP8 mapping for VP8.
//!
//! Structurally the simplest container in the tree after ASF. A file is a flat
//! chain of pages, each declaring its own length in a segment table, so the walk
//! is arithmetic on declared sizes with no index and no search — the AVI and FLV
//! shape. Everything the report states about the picture sits in one packet
//! alone on its logical stream's first page, so the default parse is a bounded
//! head read plus, for the duration alone, a bounded tail read. `chunks` stays
//! **empty by design**: neither Theora nor VP8 has a bitstream side channel this
//! project reads (no SEI, no RPU, no T.35), so there is nothing to sample and no
//! sampler arm — the same metadata-only contract [`crate::container::mpegv`] and
//! [`super::asf`] keep.
//!
//! Seven facts are invariants, each pinned by a test.
//!
//! **Endianness flips at the page/payload seam.** RFC 3533 §6 fixes page header
//! fields as "LSB first", while both codec mappings here are big-endian, and
//! Vorbis — the audio stream in the same file — is LSb-first bit-packed again.
//! Reading the granule position big-endian yields an astronomical duration.
//!
//! **A granule position of -1 means no packet finishes on that page**, which is
//! ordinary mid-stream for a packet spanning pages. Read as a count it makes the
//! last page look like an enormous frame total, so it is skipped rather than
//! used, and the duration comes from the last page that actually finished one.
//!
//! **The physically last page need not belong to the video.** In a Theora+Vorbis
//! mux the trailing pages are routinely audio, which is why the tail window is
//! scanned for the last page *of the video serial* rather than simply parsed at
//! the end. ffmpeg reads exactly one maximum-size page (65307 bytes) from the
//! tail, which guarantees a page boundary but not a video page; this reads a
//! larger bounded window for the same reason the transport backend reads one.
//!
//! **A chained file's tail describes only its final link.** Ogg permits
//! sequential concatenation with fresh serial numbers, and it is Theora's own
//! documented mechanism for a frame-rate change mid-file, so the tail's granule
//! is not the whole file's frame count. Any page carrying a serial the head's
//! BOS run did not declare, or a BOS flag past that run, means a second link and
//! yields `None` rather than a number describing part of the file.
//!
//! **The two windows must stay disjoint, and a small file uses neither.** When
//! the tail window reaches back into the head's own BOS pages, those pages read
//! as a second link's and the file loses its duration — the exact defect the
//! program-stream backend records. Being *smaller* than the window is only half
//! of that: a file a little larger than it starts its tail scan inside the BOS
//! run, so the start is clamped past `bos_end` as well. A file smaller than the
//! window is walked from byte 0, where the first page boundary is known rather
//! than resynchronised to.
//!
//! **A tail anchor is believed only when its page run tiles the rest of the
//! file.** `OggS` occurs inside compressed payload, and two 27-byte pages are
//! enough to satisfy any fixed-depth chain check — so 54 bytes planted at the
//! scan's start position redirect it, and whatever granule they declare becomes
//! the file's duration. The chain that accounts for every remaining byte is the
//! file's own, and nothing else needs to be.
//!
//! **The header packets are not video payload.** A comment header may carry
//! cover art and run to megabytes, so `--full`'s exact byte sum skips each
//! stream's first packets by counting lacing values rather than by assuming
//! where the pages divide. Counting them would inflate a short clip's video
//! bitrate by whatever the metadata weighs.

use std::collections::BTreeSet;

use anyhow::{bail, Result};

use crate::container::{Codec, Demux, NalFormat, RawFullStream, TrackDemux};
use crate::model::Bitrate;
use crate::theora;

pub(crate) const CONTAINER_LABEL: &str = "Ogg";

/// Bytes read to find every BOS page. All of a link's BOS pages precede any
/// other page (RFC 3533 §3), and each carries one small identification packet,
/// so a real file's whole run is a few hundred bytes — this is slack of two
/// orders of magnitude, and it still holds one maximal page (65307 bytes) whole.
/// `prefetch::warm_metadata` warms exactly this span for an Ogg file rather
/// than the generic 8 MiB head, taking this constant directly, so the coupling
/// cannot drift.
pub const HEAD_SCAN_BYTES: usize = 64 << 10; // 64 KiB

/// Bytes read from the tail to find the video stream's last granule position.
/// Ogg has no duration field, so this is the transport backend's tail-PCR
/// shape. The size is the top of the range the format research recommended,
/// and the bottom of it was measured too small: a 387 KiB file holding 3
/// seconds of video multiplexed with 90 seconds of audio puts its last video
/// page 31 KiB *before* a 256 KiB window opens, so the whole file reported no
/// duration and no bitrate. What has to fit is the video's share of the tail,
/// not the tail, and a lopsided mux makes that share small.
/// `prefetch::warm_metadata` warms exactly this many bytes — keep the two in
/// step.
pub const TAIL_SCAN_BYTES: usize = 1 << 20; // 1 MiB

/// `OggS`, the page capture pattern (RFC 3533 §6).
const CAPTURE: [u8; 4] = *b"OggS";

/// Fixed part of a page header, before the segment table.
const HEADER_MIN: usize = 27;

/// Largest a page can be: the fixed header, a full 255-entry segment table, and
/// 255 bytes per entry.
const MAX_PAGE: usize = HEADER_MIN + 255 + 255 * 255;

/// `header_type` bits (RFC 3533 §6).
const FLAG_BOS: u8 = 0x02;

/// Logical streams whose identification packet is collected from the head.
/// Real files carry two or three; this only bounds what a malformed head can
/// make the walk allocate. Reaching it stops collection rather than erroring,
/// which can only make the chained-file verdict more conservative.
const MAX_STREAMS: usize = 64;

/// Longest duration accepted from the granule arithmetic. The granule is an
/// unvalidated `i64`, so a malformed one computes 182 million years and the
/// bitrate dividing by it then reports **`0 b/s`** — a rendering the project
/// forbids elsewhere by name. Every sibling backend caps the same way
/// (`ps::MAX_SPAN_SECS`, `ts::pcr_duration`, `asf::MAX_DURATION_SECS`); the
/// transport ones use 26 hours because that is their clock's own range, and
/// Ogg's frame counter has no wrap to borrow, so this takes ASF's two days.
const MAX_DURATION_SECS: f64 = 48.0 * 3600.0;

/// Consecutive pages a candidate tail anchor must chain through before its
/// whole run is checked. One page's declared length landing exactly on another
/// capture pattern is already strong, so this is a cheap filter ahead of the
/// authoritative test in [`tiles_to_end`] rather than the decision itself.
const CHAIN_CONFIRM: usize = 2;

/// Page parses the tail anchor search may spend in total, across every
/// candidate it validates. A real file's first or second candidate is the true
/// chain, so this is never approached; it exists because a crafted window can
/// hold many candidates that each chain a long way before failing, and without
/// a shared budget that product — candidates times pages — is what grows.
const MAX_ANCHOR_PARSES: usize = 4 * (TAIL_SCAN_BYTES / HEADER_MIN);

/// One page, as parsed from its header.
#[derive(Debug, Clone, Copy)]
struct Page {
    /// Offset of the capture pattern.
    start: usize,
    /// Bytes from `start` to the payload.
    header_len: usize,
    /// Payload bytes.
    body_len: usize,
    granule: i64,
    serial: u32,
    header_type: u8,
}

impl Page {
    fn end(&self) -> usize {
        self.start + self.header_len + self.body_len
    }
    fn body_start(&self) -> usize {
        self.start + self.header_len
    }
    fn is_bos(&self) -> bool {
        self.header_type & FLAG_BOS != 0
    }
    /// The segment table, whose lacing values both size the payload and mark
    /// where packets end (any value below 255 terminates one).
    fn segments<'a>(&self, d: &'a [u8]) -> &'a [u8] {
        &d[self.start + HEADER_MIN..self.start + self.header_len]
    }
}

/// Parse the page whose capture pattern starts at `at`, bounded by `end`.
/// `None` unless the whole page — header, segment table and payload — lies
/// inside the bound, so every caller can slice the result without a second
/// check.
fn parse_page(d: &[u8], at: usize, end: usize) -> Option<Page> {
    if at.checked_add(HEADER_MIN)? > end || end > d.len() {
        return None;
    }
    if d[at..at + 4] != CAPTURE || d[at + 4] != 0 {
        return None;
    }
    let nsegs = d[at + 26] as usize;
    let header_len = HEADER_MIN + nsegs;
    if at + header_len > end {
        return None;
    }
    let body_len: usize = d[at + HEADER_MIN..at + header_len].iter().map(|&s| s as usize).sum();
    if at + header_len + body_len > end {
        return None;
    }
    // A page is self-bounding by construction: at most 255 lacing values of at
    // most 255 bytes each. Nothing downstream needs a length cap of its own,
    // and this is where that reasoning is checked rather than assumed.
    debug_assert!(header_len + body_len <= MAX_PAGE);
    Some(Page {
        start: at,
        header_len,
        body_len,
        granule: i64::from_le_bytes(d[at + 6..at + 14].try_into().ok()?),
        serial: u32::from_le_bytes(d[at + 14..at + 18].try_into().ok()?),
        header_type: d[at + 5],
    })
}

/// True when these bytes open an Ogg file: the capture pattern, the only
/// defined stream structure version, and the beginning-of-stream flag the first
/// page of a physical stream always carries. The BOS requirement is what keeps
/// the sniffer from claiming a fragment cut mid-file, which this backend could
/// not report anyway.
pub fn is_ogg(data: &[u8]) -> bool {
    data.len() >= HEADER_MIN
        && data[..4] == CAPTURE
        && data[4] == 0
        && data[5] & FLAG_BOS != 0
}

/// A video logical bitstream identified at BOS.
#[derive(Debug)]
enum Video {
    Theora(theora::IdHeader),
    Vp8(Vp8Id),
}

/// The head walk's result: the video streams found, every serial the head
/// declared, and where the BOS run ended.
struct Head {
    videos: Vec<(u32, Video)>,
    serials: BTreeSet<u32>,
    bos_end: usize,
    /// Why no video track was produced, when a video magic *was* recognised.
    /// Kept so the refusal names what the file holds rather than claiming it
    /// holds no video at all.
    refused: Option<Refusal>,
}

/// A recognised video magic that still produced no track. The two cases must
/// stay apart in the message: telling a user that Theora is "not supported"
/// when their file's header is simply damaged is wrong about the tool *and*
/// wrong about their file.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    /// A codec this project has no parser for (Dirac, the OGM wrapper).
    NoParser(&'static str),
    /// A codec this project does parse, whose identification header failed
    /// validation.
    Malformed(&'static str),
}

impl Refusal {
    fn message(self) -> String {
        match self {
            Refusal::NoParser(name) => format!("Ogg video codec not supported: {name}"),
            Refusal::Malformed(name) => {
                format!("malformed {name} identification header in the Ogg stream")
            }
        }
    }
}

/// Walk the leading BOS pages, which RFC 3533 §3 places before any other page,
/// and identify each logical stream from its first packet's magic.
fn walk_head(d: &[u8]) -> Head {
    let end = HEAD_SCAN_BYTES.min(d.len());
    let mut head = Head { videos: Vec::new(), serials: BTreeSet::new(), bos_end: 0, refused: None };
    let mut pos = 0;
    while let Some(page) = parse_page(d, pos, end) {
        if !page.is_bos() {
            break;
        }
        head.bos_end = page.end();
        // Both halves of this test are load-bearing. The cap is read off
        // `serials` but bounds `videos`, so without the `insert` — which
        // returns false for a serial already seen — a head of BOS pages all
        // carrying the *same* serial leaves `serials.len()` at 1 forever while
        // `videos` grows once per page: 14,000 reported video tracks and a
        // 3.7 MB JSON out of a 980 KiB file, measured. RFC 3533 §3 allows one
        // BOS page per logical stream in any case, so a repeat is malformed and
        // ignoring it is also the conforming reading.
        if head.serials.len() < MAX_STREAMS && head.serials.insert(page.serial) {
            let payload = &d[page.body_start()..page.body_start() + page.body_len];
            match identify(payload) {
                Some(Ok(v)) => head.videos.push((page.serial, v)),
                Some(Err(refusal)) => {
                    head.refused.get_or_insert(refusal);
                }
                None => {}
            }
        }
        pos = page.end();
        // Belt and braces. `page.end()` is `start + 27` at minimum, since the
        // fixed header alone is that long, so the walk cannot fail to advance —
        // this and its four siblings guard against a future change to
        // `parse_page` rather than against any input.
        if pos <= page.start {
            break;
        }
    }
    head
}

/// Identify a logical stream from its identification packet.
///
/// `Some(Ok(..))` for a video codec this project parses, `Some(Err(name))` for
/// one it recognises but cannot describe, `None` for everything else — audio,
/// Skeleton, and any magic not listed. The audio magics are deliberately not
/// enumerated: nothing here needs to name them, and a magic that is merely
/// unlisted produces exactly the same outcome as one identified as audio.
fn identify(payload: &[u8]) -> Option<Result<Video, Refusal>> {
    if payload.starts_with(theora::ID_MAGIC) {
        // A header that fails validation is a damaged file of a codec this
        // project parses, not a different codec — and returning `None` here
        // would let it fall through to "no video stream", which is a third
        // wrong answer.
        return Some(
            theora::parse_id_header(payload)
                .map(Video::Theora)
                .ok_or(Refusal::Malformed("Theora")),
        );
    }
    if payload.starts_with(VP8_MAGIC) {
        return Some(parse_vp8_id(payload).map(Video::Vp8).ok_or(Refusal::Malformed("VP8")));
    }
    // Recognised, but with no parser: Dirac's sequence header and the OGM
    // wrapper's own stream header are both structures this project has no
    // primary source for, so naming them beats reporting a zero-sized track.
    if payload.starts_with(b"BBCD\0") {
        return Some(Err(Refusal::NoParser("Dirac")));
    }
    if payload.starts_with(b"\x01video") {
        return Some(Err(Refusal::NoParser("OGM video")));
    }
    None
}

// ---------------------------------------------------------------- VP8 mapping

/// Xiph's OggVP8 identification header magic (`'O'` plus the codec's FourCC).
const VP8_MAGIC: &[u8] = b"OVP80";

/// Length of that header. Its field widths are Xiph's mapping, and they are
/// confirmed byte for byte by `testfiles/sdr/vp8.ogv`, whose BOS page declares
/// exactly 26: the alternative reading with 16-bit aspect-ratio terms totals 24
/// and the one with four trailing reserved bytes totals 28, so only this layout
/// fits, and only under it do the frame rate terms decode to the 25/1 ffprobe
/// reports.
const VP8_ID_LEN: usize = 26;

/// What the OggVP8 identification header states. VP8 signals no colour anywhere
/// a container-level parse can reach — the one colour bit it has lives in each
/// frame's payload — so there is nothing here beyond geometry and rate.
#[derive(Debug)]
struct Vp8Id {
    width: u32,
    height: u32,
    fps: f64,
    /// The rate's own terms, kept rather than recovered from `fps`. The
    /// duration is `frames * fpsd / fpsn` as an exact rational: rounding the
    /// quotient back to an integer denominator reports 24 fps content as 24
    /// and 30000/1001 as 30, which is 0.1% short — 7 seconds on a two-hour
    /// title, and inconsistent with the `fps` field in the same report.
    fpsn: u32,
    fpsd: u32,
}

/// Parse the OggVP8 identification header. Big-endian throughout, like Theora's
/// and unlike the page around it.
fn parse_vp8_id(p: &[u8]) -> Option<Vp8Id> {
    if p.len() < VP8_ID_LEN || !p.starts_with(VP8_MAGIC) {
        return None;
    }
    // Header type 1 is the identification packet; major version 1 is the only
    // one the mapping defines, and a future incompatible major would not have
    // these fields at these offsets.
    if p[5] != 0x01 || p[6] != 1 {
        return None;
    }
    let width = u16::from_be_bytes([p[8], p[9]]) as u32;
    let height = u16::from_be_bytes([p[10], p[11]]) as u32;
    let fpsn = u32::from_be_bytes([p[18], p[19], p[20], p[21]]);
    let fpsd = u32::from_be_bytes([p[22], p[23], p[24], p[25]]);
    if width == 0 || height == 0 || fpsn == 0 || fpsd == 0 {
        return None;
    }
    // Same unvalidated-quotient bound the Theora header takes.
    let fps = super::plausible_fps(fpsn as f64 / fpsd as f64)?;
    Some(Vp8Id { width, height, fps, fpsn, fpsd })
}

/// Frames finished by a VP8 granule position. The mapping puts the frame count
/// in the upper 32 bits and packs an invisible-frame count and the distance to
/// the last keyframe into the lower ones — which is what ffmpeg's
/// `oggparsevp8.c` reads too. Confirmed against `testfiles/sdr/vp8.ogv`: its
/// final granule is `0x32_C0000108`, giving the 50 frames that are the 2.000 s
/// ffprobe reports at 25 fps.
fn vp8_granule_frames(gp: i64) -> Option<u64> {
    (gp >= 0).then_some((gp as u64) >> 32)
}

// -------------------------------------------------------------------- Duration

/// Which video stream a duration is being computed for, and how its granule
/// position turns into a frame count.
struct Timing {
    serial: u32,
    kind: TimingKind,
    /// Frame duration as an exact rational, so the total is never routed
    /// through a rounded frames-per-second value.
    num: u32,
    den: u32,
}

enum TimingKind {
    Theora { kfgshift: u8, pre_321: bool },
    Vp8,
}

impl Timing {
    fn frames(&self, gp: i64) -> Option<u64> {
        match self.kind {
            TimingKind::Theora { kfgshift, pre_321 } => {
                theora::granule_frames(gp, kfgshift, pre_321)
            }
            TimingKind::Vp8 => vp8_granule_frames(gp),
        }
    }
    fn secs(&self, frames: u64) -> Option<f64> {
        let secs = frames as f64 * self.num as f64 / self.den as f64;
        (secs > 0.0 && secs <= MAX_DURATION_SECS).then_some(secs)
    }
}

/// The video stream's duration, from the last granule position that finishes a
/// packet on its own serial.
///
/// `None` — never a guess — when the file is chained (a later link's pages carry
/// serials the head never declared), when no page of this serial finishes a
/// packet inside the tail window, or when the window cannot be resynchronised
/// to a page boundary at all.
fn duration(d: &[u8], head: &Head, timing: &Timing) -> Option<f64> {
    let size = d.len();
    // The windows must not overlap, and `size <= TAIL_SCAN_BYTES` is only half
    // of that. A file a little *larger* than the window starts its tail scan
    // inside the head's own BOS run, anchors on one of those pages, and the
    // beginning-of-stream test below then reads it as a second link — a legal
    // unchained file silently losing both its duration and its bitrate. The
    // band is as wide as the last BOS page's offset, so `.max(bos_end)` closes
    // it, which is the same `tail_start.max(head_end)` the program-stream
    // backend already carries. Below the window the whole file is walked from
    // byte 0, where the first page boundary is known rather than searched for.
    let (mut pos, whole_file) = if size <= TAIL_SCAN_BYTES {
        (0, true)
    } else {
        (tail_anchor(d, (size - TAIL_SCAN_BYTES).max(head.bos_end), size)?, false)
    };

    let mut last = None;
    while let Some(page) = parse_page(d, pos, size) {
        // A serial the head did not declare, or a beginning-of-stream flag
        // past the head's own BOS run, is a second link — whose granule
        // positions restart, so the tail describes part of the file only.
        if !head.serials.contains(&page.serial) {
            return None;
        }
        if page.is_bos() && (!whole_file || page.start >= head.bos_end) {
            return None;
        }
        if page.serial == timing.serial && page.granule >= 0 {
            last = Some(page.granule);
        }
        let next = page.end();
        if next <= pos {
            break;
        }
        pos = next;
    }
    timing.frames(last?).and_then(|f| timing.secs(f))
}

/// Find the real page boundary inside `[from, end)`.
///
/// A bare capture-pattern search is not enough: `OggS` occurs inside compressed
/// payload, and a false anchor puts every field of the walk one page out of
/// phase. Two 27-byte pages are enough to satisfy a fixed-depth chain check, so
/// **54 bytes planted in a page body redirect the whole tail scan** and whatever
/// granule they declare becomes the reported duration. The
/// authoritative test is therefore whether the run *tiles the rest of the file*
/// — which only the true chain does — with the cheap fixed-depth check kept
/// ahead of it as a filter so the expensive walk runs rarely.
fn tail_anchor(d: &[u8], from: usize, end: usize) -> Option<usize> {
    let mut at = from;
    let mut budget = MAX_ANCHOR_PARSES;
    while at + HEADER_MIN <= end {
        let rel = d[at..end].windows(4).position(|w| w == CAPTURE)?;
        let candidate = at + rel;
        if chain_holds(d, candidate, end) && tiles_to_end(d, candidate, end, &mut budget) {
            return Some(candidate);
        }
        at = candidate + 1;
    }
    None
}

/// True when `at` begins `CHAIN_CONFIRM` self-consistent pages, or fewer if the
/// run reaches `end` first. The cheap filter, not the decision.
fn chain_holds(d: &[u8], at: usize, end: usize) -> bool {
    let mut pos = at;
    for _ in 0..CHAIN_CONFIRM {
        let Some(page) = parse_page(d, pos, end) else { return false };
        let next = page.end();
        if next <= pos {
            return false;
        }
        if next == end {
            return true;
        }
        pos = next;
    }
    true
}

/// True when the page run starting at `at` accounts for every remaining byte:
/// it lands exactly on `end`, or it stops with less than one maximal page left,
/// which is what a final page cut short by truncation looks like. A run that
/// dies with more than that remaining was not the file's own chain.
///
/// `budget` is shared across every candidate in one search, so a window full of
/// candidates that each chain a long way cannot multiply into a slow scan.
fn tiles_to_end(d: &[u8], at: usize, end: usize, budget: &mut usize) -> bool {
    let mut pos = at;
    while pos < end {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        // Same split as `walk_pages`: only a page whose declared extent runs
        // past the end is truncation. Bytes that are not a page header at all
        // end the run wherever they sit, so a stray capture pattern cannot buy
        // a false anchor the near-the-end tolerance.
        if end - pos >= HEADER_MIN && (d[pos..pos + 4] != CAPTURE || d[pos + 4] != 0) {
            return false;
        }
        let Some(page) = parse_page(d, pos, end) else {
            return end - pos < MAX_PAGE;
        };
        let next = page.end();
        if next <= pos {
            return false;
        }
        pos = next;
    }
    pos == end
}

// ------------------------------------------------------------ `--full` walk

/// Header packets each mapping places before its first video packet: Theora's
/// identification, comment and setup headers, and VP8's identification and
/// comment headers.
fn header_packets(v: &Video) -> u8 {
    match v {
        Video::Theora(_) => 3,
        Video::Vp8(_) => 2,
    }
}

/// Sum the video stream's payload bytes over the whole file, skipping its
/// header packets, and tick the progress bar and remote-read frontier by walk
/// position. This is what turns Ogg's file-length `overall` rate into an exact
/// per-stream one under `--full`.
///
/// Packets are counted through the lacing values rather than by assuming the
/// header packets fill whole pages, so a mux that packs the first video packet
/// behind the last header packet still measures correctly.
///
/// `None` when the page chain breaks mid-file, because the sum would then be
/// short against a duration derived from the whole span — a wrong exact rate,
/// which is worse than the honest `overall` one it would replace. A final page
/// cut short by truncation is not a break: it ends the walk, and the duration
/// derived from the last complete page describes the same bytes.
pub fn walk_pages(
    data: &[u8],
    serial: u32,
    header_packets: u8,
    mut tick: impl FnMut(usize),
) -> Option<u64> {
    let size = data.len();
    let mut pos = 0usize;
    let mut bytes = 0u64;
    let mut packets_done = 0u32;
    let skip = header_packets as u32;
    while pos < size {
        tick(pos);
        // The two ways a walk can stop are not interchangeable, and separating
        // them here rather than after the fact is what keeps the distinction
        // honest. Too few bytes left for a header, or a page whose declared
        // extent runs past the end, is a **truncated final page**: the walk
        // ends and the sum describes the same bytes the duration does. Bytes
        // that are not a page header at all are a **broken chain**: the sum
        // would be short against a duration spanning the whole file, i.e. a
        // wrong rate wearing the exact-measurement label. The version byte is
        // the trap — it is the one header field whose failure `parse_page`
        // reports the same way as truncation, and a single wrong byte there
        // was measured reporting 805 kb/s against a true 2.31 Mb/s.
        if size - pos < HEADER_MIN {
            break;
        }
        if data[pos..pos + 4] != CAPTURE || data[pos + 4] != 0 {
            return None;
        }
        let Some(page) = parse_page(data, pos, size) else {
            break;
        };
        if page.serial == serial {
            let mut off = 0usize;
            for &lace in page.segments(data) {
                if packets_done >= skip {
                    bytes += lace as u64;
                }
                off += lace as usize;
                // Any lacing value below 255 terminates a packet; 255 means the
                // packet continues into the next segment.
                if lace < 255 {
                    packets_done += 1;
                }
            }
            debug_assert_eq!(off, page.body_len);
        }
        let next = page.end();
        if next <= pos {
            return None;
        }
        pos = next;
    }
    tick(size);
    Some(bytes)
}

// ------------------------------------------------------------------- Backend

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    if !is_ogg(data) {
        bail!("not an Ogg stream: no OggS capture pattern at a beginning-of-stream page");
    }
    let head = walk_head(data);
    if head.videos.is_empty() {
        match head.refused {
            Some(r) => bail!("{}", r.message()),
            None => bail!("no video logical bitstream in the Ogg file"),
        }
    }

    // One reported track per video logical bitstream, in BOS order — the
    // project's track model, which for Ogg is what BOS order already gives.
    let timing = first_timing(&head);
    let duration_secs = timing.as_ref().and_then(|t| duration(data, &head, t));

    // The exact per-stream sum exists only after the `--full` walk, so demux
    // leaves the rate unset on that path and `main.rs` applies what the scan
    // measured — the FLV shape. Everything else gets the file-length `overall`
    // rate, which counts audio and page overhead and is labelled as such —
    // but only on a sole video track: stamped on each of several it would
    // claim every track averages the whole file's rate, against a duration
    // that is the *first* video stream's (the schema contract since 2.0;
    // the TS/PS/MKV backends carry the same gate).
    let stream_plan = full.then(|| single_video_plan(&head)).flatten();
    let overall = (stream_plan.is_none() && head.videos.len() == 1)
        .then(|| Bitrate::overall(data.len() as u64, duration_secs))
        .flatten();

    let tracks: Vec<TrackDemux> = head
        .videos
        .iter()
        .map(|(serial, v)| track_of(*serial, v, overall))
        .collect();

    Ok(Demux {
        container: CONTAINER_LABEL,
        duration_secs,
        tracks,
        ts_stream: None,
        mkv_stream: None,
        raw_stream: stream_plan,
        bounded_index: false,
        declared_short: false,
    })
}

/// The timing plan for the first video stream, which is the one whose duration
/// the file reports. Theora's mapping requires its BOS page to come first
/// "to facilitate identification of Ogg Theora files", so in a conforming file
/// this is the primary video stream.
fn first_timing(head: &Head) -> Option<Timing> {
    let (serial, v) = head.videos.first()?;
    Some(match v {
        Video::Theora(h) => Timing {
            serial: *serial,
            kind: TimingKind::Theora { kfgshift: h.kfgshift, pre_321: h.pre_321 },
            num: h.frd,
            den: h.frn,
        },
        Video::Vp8(id) => {
            Timing { serial: *serial, kind: TimingKind::Vp8, num: id.fpsd, den: id.fpsn }
        }
    })
}

/// The `--full` walk plan, but **only for a single-video file**.
///
/// `sample::scan` returns one `TrackScan` per raw-stream walk and `main.rs`
/// zips that against the demuxed tracks, so a plan on a file with two video
/// streams would drop the second from the report entirely. Multi-video Ogg does
/// not occur in practice; if one appears, `--full` reports what the default
/// path does rather than losing a track.
fn single_video_plan(head: &Head) -> Option<RawFullStream> {
    let [(serial, v)] = &head.videos[..] else { return None };
    Some(RawFullStream::Ogg { serial: *serial, header_packets: header_packets(v) })
}

fn track_of(serial: u32, v: &Video, bitrate: Option<Bitrate>) -> TrackDemux {
    let mut td = match v {
        Video::Theora(h) => {
            let (color, color_source) = h.color.clone().unwrap_or_default();
            TrackDemux {
                width: h.width,
                height: h.height,
                fps: Some(h.fps),
                fps_rational: Some((u64::from(h.frn), u64::from(h.frd))),
                bit_depth: Some(theora::BIT_DEPTH),
                chroma: Some(h.chroma.to_string()),
                pixel_aspect: h.pixel_aspect,
                // Interlace is "not supported by the format at all" (format
                // reference §8) — structural, like the bit depth.
                scan_type: Some("progressive"),
                color,
                color_source,
                // Theora defines no profiles; `QUAL` is an encoder quality hint,
                // not a conformance point, so there is nothing to report.
                ..TrackDemux::new(Codec::Theora, NalFormat::AnnexB)
            }
        }
        Video::Vp8(id) => TrackDemux {
            width: id.width,
            height: id.height,
            fps: Some(id.fps),
            fps_rational: Some((u64::from(id.fpsn), u64::from(id.fpsd))),
            // RFC 6386 §2: "VP8 works exclusively with an 8-bit YUV 4:2:0 image
            // format." Both are format constants rather than fields that were
            // read, the same standing as MPEG-2's 8 bits and ProRes's
            // per-family depth.
            bit_depth: Some(8),
            chroma: Some("4:2:0".to_string()),
            // No VP8 parser here, so the codec keeps its container facts and
            // renders its name verbatim — D10's `Codec::Other` path, and the
            // same label the FLV backend gives a VP8 track.
            ..TrackDemux::new(Codec::Other("VP8".to_string()), NalFormat::AnnexB)
        },
    };
    // The logical bitstream's serial number, which is what Ogg has instead of a
    // small ordinal — a 32-bit value the muxer picks at random, so it reads
    // nothing like an MKV TrackNumber or an MP4 track_ID.
    td.track_number = Some(serial as u64);
    // The BOS packet's mapping name, Ogg's codec identifier.
    td.codec_id = Some(
        match v {
            Video::Theora(_) => "theora",
            Video::Vp8(_) => "vp8",
        }
        .to_string(),
    );
    td.bitrate = bitrate;
    td
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a page carrying whole packets: `OggS`, version 0, the given flags,
    /// granule, serial and sequence, then a segment table in which each packet
    /// ends on a lacing value below 255, as the format requires.
    fn page_packets(flags: u8, granule: i64, serial: u32, seq: u32, packets: &[&[u8]]) -> Vec<u8> {
        let mut laces = Vec::new();
        let mut body = Vec::new();
        for packet in packets {
            let mut left = packet.len();
            loop {
                if left >= 255 {
                    laces.push(255u8);
                    left -= 255;
                } else {
                    laces.push(left as u8);
                    break;
                }
            }
            body.extend_from_slice(packet);
        }
        assert!(laces.len() <= 255, "fixture does not fit one page");
        let mut p = Vec::new();
        p.extend_from_slice(&CAPTURE);
        p.push(0);
        p.push(flags);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&serial.to_le_bytes());
        p.extend_from_slice(&seq.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes()); // CRC, never checked here
        p.push(laces.len() as u8);
        p.extend_from_slice(&laces);
        p.extend_from_slice(&body);
        p
    }

    /// A page carrying exactly one whole packet, the ordinary case here.
    fn page(flags: u8, granule: i64, serial: u32, seq: u32, body: &[u8]) -> Vec<u8> {
        page_packets(flags, granule, serial, seq, &[body])
    }

    /// `testfiles/sdr/theora.ogv`'s identification header verbatim.
    const THEORA_ID: [u8; 42] = [
        0x80, 0x74, 0x68, 0x65, 0x6f, 0x72, 0x61, 0x03, 0x02, 0x01, 0x00, 0x14, 0x00, 0x0f, 0x00,
        0x01, 0x40, 0x00, 0x00, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x03, 0x0d, 0x40, 0x00, 0xc0,
    ];

    /// `testfiles/sdr/vp8.ogv`'s identification header verbatim.
    const VP8_ID: [u8; 26] = [
        0x4f, 0x56, 0x50, 0x38, 0x30, 0x01, 0x01, 0x00, 0x01, 0x40, 0x00, 0xf0, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x01,
    ];

    const SERIAL: u32 = 0x1b60_5f90;
    const AUDIO_SERIAL: u32 = 0x89b6_11e5;

    /// A minimal but complete single-stream Theora file: BOS page, a header
    /// page holding the comment and setup packets, then data pages.
    fn theora_file(last_granule: i64) -> Vec<u8> {
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        // The comment and setup packets share one page, as libtheora writes
        // them — two packets, so the walk's lacing-value count sees three
        // header packets in total.
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0, 64, SERIAL, 2, &[1u8; 300]));
        d.extend(page(0x04, last_granule, SERIAL, 3, &[2u8; 100]));
        d
    }

    #[test]
    fn a_real_theora_head_reports_its_picture_and_duration() {
        let d = theora_file(3137);
        let dm = demux(&d, false).expect("demuxes");
        assert_eq!(dm.container, "Ogg");
        assert_eq!(dm.tracks.len(), 1);
        let t = &dm.tracks[0];
        assert_eq!(t.codec, Codec::Theora);
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.fps, Some(25.0));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        // 3137 splits into keyframe 49 plus offset 1: 50 frames at 25 fps.
        assert_eq!(dm.duration_secs, Some(2.0));
        // Metadata-only: nothing to sample, so no chunk index at all.
        assert!(t.chunks.is_empty());
        // `CS` 0 in this header, so no colour is signalled and none is filled.
        assert_eq!(t.color.primaries, None);
    }

    #[test]
    fn a_real_vp8_head_reports_its_picture_and_duration() {
        let mut d = page(FLAG_BOS, 0, 7, 0, &VP8_ID);
        d.extend(page(0, 0, 7, 1, &[9u8; 30])); // comment header
        d.extend(page(0x04, 0x32_C000_0108, 7, 2, &[3u8; 200]));
        let dm = demux(&d, false).expect("demuxes");
        let t = &dm.tracks[0];
        assert_eq!(t.codec, Codec::Other("VP8".to_string()));
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.fps, Some(25.0));
        // The mapping's frame count lives in the upper 32 bits alone.
        assert_eq!(dm.duration_secs, Some(2.0));
        assert_eq!(t.bit_depth, Some(8), "RFC 6386 §2 fixes VP8 at 8-bit 4:2:0");
    }

    #[test]
    fn the_page_header_is_little_endian_while_the_payload_is_big_endian() {
        // The granule is written LE by `page`; read BE it would be an
        // astronomical frame count rather than 3137, and the duration would be
        // billions of seconds instead of 2.
        let d = theora_file(3137);
        assert_eq!(demux(&d, false).unwrap().duration_secs, Some(2.0));
        // Meanwhile the Theora header inside the same file is big-endian: read
        // LE its macroblock counts would give 5120x3840.
        assert_eq!((demux(&d, false).unwrap().tracks[0].width), 320);
    }

    #[test]
    fn a_minus_one_granule_is_skipped_rather_than_counted() {
        // The final page finishes no packet, so the duration comes from the
        // page before it. Read as a count, -1 is 2^64-1 frames.
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0, 3137, SERIAL, 2, &[1u8; 300]));
        d.extend(page(0x04, -1, SERIAL, 3, &[2u8; 100]));
        assert_eq!(demux(&d, false).unwrap().duration_secs, Some(2.0));
    }

    #[test]
    fn the_last_page_of_another_serial_does_not_end_the_video() {
        // A Theora+Vorbis mux routinely ends on audio pages. The duration must
        // come from the last *video* page, not the last page.
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page(FLAG_BOS, 0, 999, 0, b"\x01vorbis-------------------"));
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0, 3137, SERIAL, 2, &[1u8; 300]));
        d.extend(page(0x04, 44100 * 2, 999, 1, &[5u8; 200]));
        let dm = demux(&d, false).unwrap();
        assert_eq!(dm.tracks.len(), 1, "the Vorbis stream is not a video track");
        assert_eq!(dm.duration_secs, Some(2.0));
    }

    #[test]
    fn a_chained_file_reports_no_duration_rather_than_its_last_link() {
        // A second link restarts the granule positions, so its tail describes
        // part of the file. Ogg permits this and Theora documents it as the way
        // to change frame rate mid-file.
        let mut d = theora_file(3137);
        let mut link2 = page(FLAG_BOS, 0, 4242, 0, &THEORA_ID);
        link2.extend(page(0, 0, 4242, 1, &[7u8; 40]));
        link2.extend(page(0x04, 1280, 4242, 2, &[1u8; 300]));
        d.extend(link2);
        let dm = demux(&d, false).unwrap();
        assert_eq!(dm.duration_secs, None, "a chained file's tail is not the whole file");
        // And with no duration there is no bitrate to divide, either.
        assert!(dm.tracks[0].bitrate.is_none());
    }

    #[test]
    fn a_short_file_is_walked_from_byte_zero_rather_than_resynchronised() {
        // Below the tail window the head's own BOS pages would fall inside a
        // tail scan and read as a second link, dropping every short file's
        // duration — the overlap defect the program-stream backend records.
        let d = theora_file(3137);
        assert!(d.len() < TAIL_SCAN_BYTES, "fixture must exercise the small-file path");
        assert_eq!(demux(&d, false).unwrap().duration_secs, Some(2.0));
    }

    #[test]
    fn duplicate_bos_serials_cannot_multiply_the_track_list() {
        // The cap is read off the serial set but bounds the track vector, so
        // repeating one serial kept the set at 1 while the vector grew per
        // page: 14,000 tracks and a 3.7 MB JSON out of a 980 KiB file.
        let mut d = Vec::new();
        for seq in 0..500 {
            d.extend(page(FLAG_BOS, 0, SERIAL, seq, &THEORA_ID));
        }
        d.extend(page_packets(0, 0, SERIAL, 500, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0x04, 3137, SERIAL, 501, &[2u8; 100]));
        let dm = demux(&d, false).expect("demuxes");
        assert_eq!(dm.tracks.len(), 1, "one logical stream, however many BOS pages claim it");
        assert_eq!(dm.duration_secs, Some(2.0));

        // Distinct serials are still capped, and the cap is the stream count.
        let mut many = Vec::new();
        for s in 0..(MAX_STREAMS as u32 + 40) {
            many.extend(page(FLAG_BOS, 0, s + 1, 0, &THEORA_ID));
        }
        assert_eq!(demux(&many, false).unwrap().tracks.len(), MAX_STREAMS);
    }

    #[test]
    fn an_implausible_granule_yields_no_duration() {
        // An i64::MAX granule computed 1601279867509 hours, and the bitrate
        // dividing by it rendered `0 b/s` — the shape the project forbids by
        // name. Every sibling backend caps its derived span the same way.
        let d = theora_file(i64::MAX);
        let dm = demux(&d, false).expect("demuxes");
        assert_eq!(dm.duration_secs, None);
        assert!(dm.tracks[0].bitrate.is_none(), "no duration, so nothing to divide");

        // Exactly on the cap still reports: 48 h at 25 fps is 4,320,000 frames,
        // which is that keyframe index shifted by KFGSHIFT with no offset.
        let ok = theora_file(4_320_000 << 6);
        assert_eq!(demux(&ok, false).unwrap().duration_secs, Some(48.0 * 3600.0));
        // One frame past it is refused, so the boundary is the stated one.
        let over = theora_file((4_320_001 << 6) | 1);
        assert_eq!(demux(&over, false).unwrap().duration_secs, None);
    }

    #[test]
    fn the_tail_window_resynchronises_past_a_false_capture_pattern() {
        // Push the file past the tail window with payload containing `OggS`,
        // which is what a bare backward search for the pattern would latch on
        // to. The chain check is what rejects it.
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        let mut seq = 2;
        let mut filler = vec![0u8; 250];
        filler[..4].copy_from_slice(&CAPTURE);
        filler[100..104].copy_from_slice(&CAPTURE);
        while d.len() < TAIL_SCAN_BYTES * 2 {
            d.extend(page(0, 64 * seq as i64, SERIAL, seq, &filler));
            seq += 1;
        }
        d.extend(page(0x04, 3137, SERIAL, seq, &[2u8; 100]));
        assert_eq!(demux(&d, false).unwrap().duration_secs, Some(2.0));
    }

    /// A **Theora+Vorbis** file carrying `filler` bytes of video payload, split
    /// across as many pages as that needs. Growing `filler` by one grows the
    /// file by one, so a test can place the total exactly where it needs the
    /// tail window to land.
    ///
    /// The second logical stream is not decoration. With one BOS page, at
    /// offset 0, a tail scan can never begin at or before a BOS page's start —
    /// so a fixture built that way exercises the window-overlap clamp only
    /// vacuously, and passes with the clamp deleted. A mux is also what the
    /// defect was found on.
    fn padded(filler: usize) -> Vec<u8> {
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page(FLAG_BOS, 0, AUDIO_SERIAL, 0, b"\x01vorbis-------------------"));
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        let mut left = filler;
        let mut seq = 2;
        while left > 0 {
            let take = left.min(60_000);
            d.extend(page(0, 64, SERIAL, seq, &vec![1u8; take]));
            left -= take;
            seq += 1;
        }
        d.extend(page(0x04, 3137, SERIAL, seq, &[2u8; 100]));
        d
    }

    /// Offset of the second BOS page, which is where the tail window's start
    /// has to land for the overlap clamp to be doing anything.
    fn second_bos_at() -> usize {
        page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID).len()
    }

    /// End of the whole BOS run.
    fn bos_run_end() -> usize {
        second_bos_at() + page(FLAG_BOS, 0, AUDIO_SERIAL, 0, b"\x01vorbis-------------------").len()
    }

    #[test]
    fn a_file_just_past_the_tail_window_does_not_read_its_own_head_as_a_second_link() {
        // The tail scan starts at `size - TAIL_SCAN_BYTES`. When that lands at
        // or before a BOS page's start, the scan anchors on the file's own
        // beginning-of-stream page and the chained check then drops both the
        // duration and the bitrate on a perfectly ordinary file. The sensitive
        // band therefore runs from the *second* BOS page's offset down — a
        // start past the whole run is safe, and a start before the second
        // page's offset cannot reach it — so sweep sizes and assert on every
        // one that lands in it.
        let (second_bos, run_end) = (second_bos_at(), bos_run_end());
        // Page and lacing overhead depends on how many filler pages there are,
        // so measure it once and correct rather than computing it.
        let overhead = padded(TAIL_SCAN_BYTES).len() - TAIL_SCAN_BYTES;
        let base = TAIL_SCAN_BYTES.saturating_sub(overhead);
        let mut in_band = 0;
        for pad in 0..(run_end + 40) {
            let d = padded(base + pad);
            let start = d.len().saturating_sub(TAIL_SCAN_BYTES);
            // Only sizes whose unclamped start falls on or before the second
            // BOS page can distinguish the clamp from its absence.
            if d.len() <= TAIL_SCAN_BYTES || start > second_bos {
                continue;
            }
            in_band += 1;
            let dm = demux(&d, false).expect("demuxes");
            assert_eq!(dm.duration_secs, Some(2.0), "size {} must still report", d.len());
            assert!(dm.tracks[0].bitrate.is_some(), "size {}", d.len());
        }
        assert!(in_band >= 10, "the sweep must actually reach the band, hit {in_band}");
    }

    #[test]
    fn a_planted_page_pair_cannot_redirect_the_tail_anchor() {
        // Two 28-byte pages satisfy any fixed-depth chain check, so a plant at
        // the resync start used to become the anchor and its granule became the
        // duration. Only a run that tiles the rest of the file is the file's
        // own chain.
        let mut d = padded(TAIL_SCAN_BYTES + 4096);
        let plant_at = d.len() - TAIL_SCAN_BYTES;
        let mut plant = page(0, 999_999_999, SERIAL, 77, &[]);
        plant.extend(page(0, 999_999_999, SERIAL, 78, &[]));
        assert_eq!(plant.len(), 2 * (HEADER_MIN + 1), "two minimal pages");
        assert!(plant_at > bos_run_end(), "the plant must sit past the BOS run");
        d[plant_at..plant_at + plant.len()].copy_from_slice(&plant);

        let dm = demux(&d, false).expect("demuxes");
        assert_eq!(dm.duration_secs, Some(2.0), "the real chain still wins");
    }

    #[test]
    fn a_file_with_no_video_stream_errors_rather_than_reporting() {
        let mut d = page(FLAG_BOS, 0, 1, 0, b"\x01vorbis-------------------");
        d.extend(page(0, 0, 1, 1, &[0u8; 40]));
        let e = demux(&d, false).unwrap_err().to_string();
        assert!(e.contains("no video logical bitstream"), "{e}");

        // A recognised codec with no parser here names itself, so the refusal
        // says what the file holds instead of claiming it has no video.
        let dirac = page(FLAG_BOS, 0, 1, 0, b"BBCD\0---------------------");
        let e = demux(&dirac, false).unwrap_err().to_string();
        assert!(e.contains("Dirac"), "{e}");
    }

    #[test]
    fn non_ogg_bytes_are_refused_before_any_walk() {
        assert!(demux(&[], false).is_err());
        assert!(demux(&[0u8; 4096], false).is_err());
        assert!(!is_ogg(b"OggS"), "too short to be a page header");
        // A page that is not beginning-of-stream cannot be a file's first page.
        let mid = page(0, 0, 1, 5, &[0u8; 8]);
        assert!(!is_ogg(&mid));
        // Stream structure version 1 does not exist.
        let mut bad_version = page(FLAG_BOS, 0, 1, 0, &THEORA_ID);
        bad_version[4] = 1;
        assert!(!is_ogg(&bad_version));
    }

    #[test]
    fn the_full_walk_sums_video_payload_and_skips_the_header_packets() {
        let d = theora_file(3137);
        // Three header packets: the 42-byte identification header, then the
        // comment and setup packets sharing the second page. The data pages
        // hold 300 and 100 bytes.
        let sum = walk_pages(&d, SERIAL, 3, |_| {}).expect("intact chain");
        assert_eq!(sum, 400, "header packets are metadata, not video payload");

        // Counting them would inflate a short clip by whatever the metadata
        // weighs — a comment header may carry cover art.
        assert_ne!(sum, 482);

        // The skip is by packet, not by page: told there are two header
        // packets, the walk starts counting inside the second page and picks
        // up its setup packet as payload.
        assert_eq!(walk_pages(&d, SERIAL, 2, |_| {}), Some(420));
    }

    #[test]
    fn the_full_walk_ignores_other_serials_and_refuses_a_broken_chain() {
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page(FLAG_BOS, 0, 999, 0, b"\x01vorbis-------------------"));
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0, 3137, SERIAL, 2, &[1u8; 300]));
        d.extend(page(0, 88, 999, 1, &[5u8; 500]));
        assert_eq!(walk_pages(&d, SERIAL, 3, |_| {}), Some(300));

        // Corrupt a mid-file capture pattern: the sum would be short against a
        // duration spanning the whole file, so no exact rate is offered.
        let mut broken = d.clone();
        let at = broken.len() - 500 - 28;
        broken[at] = b'X';
        assert_eq!(walk_pages(&broken, SERIAL, 3, |_| {}), None);

        // A final page cut short is truncation, not corruption: the walk ends
        // and reports what it measured.
        let mut cut = d.clone();
        cut.truncate(cut.len() - 200);
        assert_eq!(walk_pages(&cut, SERIAL, 3, |_| {}), Some(300));
    }

    #[test]
    fn a_broken_page_chain_is_not_read_as_truncation() {
        // The stream structure version is the one page-header field whose
        // failure `parse_page` reports exactly as truncation does. Read as
        // truncation it ends the walk early and returns a short sum, which
        // main.rs then divides by a duration spanning the whole file — a wrong
        // rate wearing the exact-measurement label. Measured on a 1.7 MiB
        // Theora file: 805 kb/s against a true 2.31 Mb/s.
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.extend(page_packets(0, 0, SERIAL, 1, &[&[7u8; 20], &[8u8; 20]]));
        d.extend(page(0, 64, SERIAL, 2, &[1u8; 300]));
        let broken_at = d.len();
        d.extend(page(0, 128, SERIAL, 3, &[1u8; 400]));
        d.extend(page(0x04, 3137, SERIAL, 4, &[2u8; 100]));
        assert_eq!(walk_pages(&d, SERIAL, 3, |_| {}), Some(800), "intact");

        // Every other page-header byte already refused correctly; this one did
        // not, because it is the only one that is neither the capture pattern
        // nor a length.
        let mut bad = d.clone();
        bad[broken_at + 4] = 1;
        assert_eq!(walk_pages(&bad, SERIAL, 3, |_| {}), None, "a bad version is a broken chain");
        assert_ne!(walk_pages(&bad, SERIAL, 3, |_| {}), Some(300), "not a short measurement");

        // The capture pattern, for contrast, was always refused.
        let mut nocap = d.clone();
        nocap[broken_at] = b'X';
        assert_eq!(walk_pages(&nocap, SERIAL, 3, |_| {}), None);
    }

    #[test]
    fn full_leaves_the_rate_to_the_scan_only_when_one_video_stream_exists() {
        let d = theora_file(3137);
        let dm = demux(&d, true).expect("demuxes");
        assert!(matches!(dm.raw_stream, Some(RawFullStream::Ogg { header_packets: 3, .. })));
        assert!(dm.tracks[0].bitrate.is_none(), "the scan's exact sum fills this");

        // Two video streams: the scan produces one TrackScan and main.rs zips
        // it against the tracks, so a plan here would drop the second track.
        let mut two = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        two.extend(page(FLAG_BOS, 0, 555, 0, &VP8_ID));
        two.extend(page(0, 0, SERIAL, 1, &[7u8; 40]));
        two.extend(page(0x04, 3137, SERIAL, 2, &[1u8; 300]));
        let dm = demux(&two, true).expect("demuxes");
        assert_eq!(dm.tracks.len(), 2);
        assert!(dm.raw_stream.is_none(), "no plan, so neither track is dropped");
        // The whole-file fallback is withheld too: it belongs to a sole video
        // track (the schema contract since 2.0), and its denominator here
        // would be the *first* stream's duration stamped on both tracks.
        assert!(dm.duration_secs.is_some(), "the fallback was constructible; the gate withheld it");
        assert!(dm.tracks.iter().all(|t| t.bitrate.is_none()));
        let dm = demux(&two, false).expect("demuxes");
        assert!(dm.tracks.iter().all(|t| t.bitrate.is_none()), "the default path likewise");
    }

    #[test]
    fn a_page_declaring_more_than_it_holds_is_refused() {
        // Every consumer slices the payload the segment table declares, so a
        // page whose lacing values run past the buffer must not parse at all.
        let mut d = page(FLAG_BOS, 0, SERIAL, 0, &THEORA_ID);
        d.truncate(d.len() - 10);
        assert!(parse_page(&d, 0, d.len()).is_none());
        assert!(demux(&d, false).is_err());

        // A page header cut inside its own segment table, likewise.
        let mut short = page(FLAG_BOS, 0, SERIAL, 0, &[0u8; 600]);
        short.truncate(28);
        assert!(parse_page(&short, 0, short.len()).is_none());
    }

    #[test]
    fn max_page_bounds_what_one_page_can_declare() {
        // 27-byte header, 255 lacing values, 255 bytes each. Nothing here reads
        // a length that is not either bounded by this or by the buffer.
        assert_eq!(MAX_PAGE, 65307);
        // 255 lacing values of 255 bytes: the largest page the format permits,
        // and by construction a packet that continues into the next page —
        // there is no lacing value left to terminate it.
        let mut big = Vec::new();
        big.extend_from_slice(&CAPTURE);
        big.extend_from_slice(&[0u8, 0]); // version, flags
        big.extend_from_slice(&0i64.to_le_bytes());
        big.extend_from_slice(&SERIAL.to_le_bytes());
        big.extend_from_slice(&0u32.to_le_bytes());
        big.extend_from_slice(&0u32.to_le_bytes());
        big.push(255);
        big.extend_from_slice(&[255u8; 255]);
        big.extend_from_slice(&vec![0u8; 255 * 255]);
        let p = parse_page(&big, 0, big.len()).expect("a maximal page is legal");
        assert_eq!(p.end(), MAX_PAGE);
        assert_eq!(big.len(), MAX_PAGE);
    }

    #[test]
    fn the_vp8_identification_header_is_twenty_six_bytes() {
        // The layout is confirmed by arithmetic: only a 26-byte reading puts
        // the frame rate terms where they decode to 25/1, which is what the
        // file's own BOS segment table declares and what ffprobe reports.
        assert_eq!(VP8_ID.len(), VP8_ID_LEN);
        let id = parse_vp8_id(&VP8_ID).expect("valid");
        assert_eq!((id.width, id.height), (320, 240));
        assert_eq!(id.fps, 25.0);

        assert!(parse_vp8_id(&VP8_ID[..25]).is_none(), "one byte short");
        let mut bad = VP8_ID;
        bad[5] = 2;
        assert!(parse_vp8_id(&bad).is_none(), "header type 2 is not identification");
        bad = VP8_ID;
        bad[6] = 2;
        assert!(parse_vp8_id(&bad).is_none(), "major version 2 is not this layout");
        bad = VP8_ID;
        bad[21] = 0;
        assert!(parse_vp8_id(&bad).is_none(), "a zero frame rate numerator");
    }

    #[test]
    fn vp8_duration_uses_the_headers_exact_rational_not_a_rounded_rate() {
        // 30000/1001 is 29.97, and rounding it to an integer denominator makes
        // every duration 0.1% short — 7 seconds on a two-hour title, and
        // inconsistent with the `fps` the same report prints. The terms are in
        // the header; there is nothing to recover from the quotient.
        let mut id = VP8_ID;
        id[18..22].copy_from_slice(&30_000u32.to_be_bytes());
        id[22..26].copy_from_slice(&1001u32.to_be_bytes());
        let mut d = page(FLAG_BOS, 0, 7, 0, &id);
        d.extend(page(0, 0, 7, 1, &[9u8; 30]));
        d.extend(page(0x04, 300i64 << 32, 7, 2, &[3u8; 200]));

        let dm = demux(&d, false).expect("demuxes");
        let exact = 300.0 * 1001.0 / 30_000.0; // 10.01 s
        assert_eq!(dm.duration_secs, Some(exact));
        assert_ne!(dm.duration_secs, Some(10.0), "the rounded rate's answer");
        // And the reported rate is the same rational, so the two agree.
        assert_eq!(dm.tracks[0].fps, Some(30_000.0 / 1001.0));
    }

    #[test]
    fn an_implausible_frame_rate_refuses_the_vp8_header() {
        // Both terms are free 32-bit integers, so the quotient needs the same
        // bound every declared rate in the tree takes: `FRN` of 0xFFFFFFFF
        // reported four billion fps and a 36.8 Tb/s bitrate from a 53 KiB file,
        // and a huge denominator printed `0.000 fps`.
        let mut fast = VP8_ID;
        fast[18..22].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_vp8_id(&fast).is_none(), "4 billion fps is a misread field");
        let mut slow = VP8_ID;
        slow[22..26].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_vp8_id(&slow).is_none(), "0.000 fps reads as a stated zero");

        // A refused header still names the codec, so the file is refused
        // honestly rather than reported as having no video at all.
        let d = page(FLAG_BOS, 0, 7, 0, &fast);
        assert!(demux(&d, false).unwrap_err().to_string().contains("VP8"));
    }

    #[test]
    fn vp8_granule_frames_reads_the_upper_half_only() {
        // `testfiles/sdr/vp8.ogv`'s final granule. The lower 32 bits hold an
        // invisible-frame count and the distance to the last keyframe; folding
        // them in would give billions of frames.
        assert_eq!(vp8_granule_frames(0x32_C000_0108), Some(50));
        assert_eq!(vp8_granule_frames(-1), None);
        assert_eq!(vp8_granule_frames(0), Some(0));
    }

    #[test]
    fn a_malformed_theora_header_refuses_the_file_rather_than_reporting_no_video() {
        // The magic identifies the codec; a header failing validation means a
        // broken Theora file, which must not read as "this file has no video".
        let mut bad = THEORA_ID;
        bad[11] = 0; // FMBW 0
        let d = page(FLAG_BOS, 0, SERIAL, 0, &bad);
        let e = demux(&d, false).unwrap_err().to_string();
        assert!(e.contains("Theora"), "{e}");
    }
}
