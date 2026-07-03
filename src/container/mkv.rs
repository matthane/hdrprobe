//! Minimal Matroska / WebM (EBML) demuxer. Walks the element tree to recover the
//! video track's codec config (CodecPrivate = hvcC), DV config (dvcC/dvvC via the
//! track's BlockAdditionMapping), colour/mastering info, and a per-block byte-range
//! index from the clusters. Never reads block payloads here — only their ranges.
//!
//! Bounded by default like the other backends: the fast path indexes only a head
//! byte-window of blocks (`HEAD_SPAN_BYTES`) so large movies don't page in the
//! whole file to walk every block header; `--full` indexes every cluster.
//!
//! Single-track only: Profile 7 dual-track (EL in BlockAdditions) is deferred to M7.

use anyhow::{Context, Result};

use crate::container::{Chunk, Codec, Demux, DvConfig, NalFormat};
use crate::model::{Bitrate, ColorInfo, ContentLight, MasteringDisplay};

// --- EBML element IDs (stored with their length-descriptor marker retained). ---
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEKHEAD: u32 = 0x114D_9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEK_ID: u32 = 0x53AB;
const ID_SEEK_POSITION: u32 = 0x53AC;
const ID_INFO: u32 = 0x1549_A966;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_ATTACHMENTS: u32 = 0x1941_A469;
const ID_CHAPTERS: u32 = 0x1043_A770;
const ID_TAGS: u32 = 0x1254_C367;

const ID_TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
const ID_DURATION: u32 = 0x4489;

const ID_TRACKENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_UID: u32 = 0x73C5;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_DEFAULT_DURATION: u32 = 0x0023_E383;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_COLOUR: u32 = 0x55B0;
const ID_MATRIX: u32 = 0x55B1;
const ID_RANGE: u32 = 0x55B9;
const ID_TRANSFER: u32 = 0x55BA;
const ID_PRIMARIES: u32 = 0x55BB;
const ID_MAX_CLL: u32 = 0x55BC;
const ID_MAX_FALL: u32 = 0x55BD;
const ID_MASTERING: u32 = 0x55D0;
// PrimaryRChromaticityX (0x55D1) .. WhitePointChromaticityY (0x55D8): the
// R/G/B primary + white point x,y floats, contiguous and in that order.
const ID_CHROMA_FIRST: u32 = 0x55D1;
const ID_CHROMA_LAST: u32 = 0x55D8;
const ID_LUMINANCE_MAX: u32 = 0x55D9;
const ID_LUMINANCE_MIN: u32 = 0x55DA;

// `Tags` subtree (mkvmerge per-track statistics: BPS, NUMBER_OF_BYTES, ...).
const ID_TAG: u32 = 0x7373;
const ID_TARGETS: u32 = 0x63C0;
const ID_TAG_TRACK_UID: u32 = 0x63C5;
const ID_SIMPLE_TAG: u32 = 0x67C8;
const ID_TAG_NAME: u32 = 0x45A3;
const ID_TAG_STRING: u32 = 0x4487;

const ID_BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
const ID_BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
const ID_BLOCK_ADD_ID_EXTRA: u32 = 0x41ED;

const ID_SIMPLEBLOCK: u32 = 0xA3;
const ID_BLOCKGROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;

// BlockAddIDType FourCCs identifying the DV config record carried in the track.
const DVCC: u32 = 0x6476_6343; // 'dvcC'
const DVVC: u32 = 0x6476_7643; // 'dvvC'

const TIMESTAMP_SCALE_DEFAULT: u64 = 1_000_000; // ns per tick

/// Default (non-`--full`) bound on how far into the stream we index: we stop once
/// the indexed blocks span this many bytes from the first one. The title-stable
/// DV levels and static HDR SEI all appear in the opening frames, so a few MB is
/// plenty. Bounding by *bytes* rather than block count keeps the walked region
/// small and bitrate-independent (512 4K frames span tens of MB), so the demux
/// doesn't fault hundreds of scattered block headers deep into the file and
/// `prefetch`'s head warm covers the whole working set in one pipelined read.
/// Keep `prefetch::HEAD_WARM` >= first-block offset + this. `--full` removes the
/// bound and indexes every block.
const HEAD_SPAN_BYTES: u64 = 4 << 20; // 4 MiB
/// Safety cap on indexed blocks so a run of degenerate tiny blocks can't grow the
/// index unbounded before the byte span is reached.
const HEAD_BLOCK_CAP: usize = 8192;

/// Read an EBML element ID (marker retained). IDs are 1..=4 bytes.
fn read_id(d: &[u8], p: usize) -> Option<(u32, usize)> {
    let b0 = *d.get(p)?;
    if b0 == 0 {
        return None;
    }
    let len = b0.leading_zeros() as usize + 1;
    if len > 4 || p + len > d.len() {
        return None;
    }
    let mut id = 0u32;
    for i in 0..len {
        id = (id << 8) | d[p + i] as u32;
    }
    Some((id, p + len))
}

/// Read an EBML VINT size (marker stripped). Returns `None` size for the
/// all-ones "unknown size" encoding.
fn read_size(d: &[u8], p: usize) -> Option<(Option<u64>, usize)> {
    let b0 = *d.get(p)?;
    if b0 == 0 {
        return None;
    }
    let len = b0.leading_zeros() as usize + 1;
    if p + len > d.len() {
        return None;
    }
    let mask = (1u64 << (8 - len)) - 1;
    let mut val = (b0 as u64) & mask;
    let mut all_ones = val == mask;
    for i in 1..len {
        let byte = d[p + i];
        val = (val << 8) | byte as u64;
        if byte != 0xFF {
            all_ones = false;
        }
    }
    Some((if all_ones { None } else { Some(val) }, p + len))
}

/// Read a VINT interpreted as a plain number (e.g. a block's track number).
fn read_vint_num(d: &[u8], p: usize) -> Option<(u64, usize)> {
    let (v, np) = read_size(d, p)?;
    Some((v.unwrap_or(0), np))
}

fn read_uint(d: &[u8], start: usize, size: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..size.min(8) {
        v = (v << 8) | d[start + i] as u64;
    }
    v
}

fn read_float(d: &[u8], start: usize, size: usize) -> Option<f64> {
    match size {
        4 => Some(f32::from_be_bytes([d[start], d[start + 1], d[start + 2], d[start + 3]]) as f64),
        8 => Some(f64::from_be_bytes([
            d[start],
            d[start + 1],
            d[start + 2],
            d[start + 3],
            d[start + 4],
            d[start + 5],
            d[start + 6],
            d[start + 7],
        ])),
        _ => None,
    }
}

fn is_level1_id(id: u32) -> bool {
    matches!(
        id,
        ID_SEEKHEAD
            | ID_INFO
            | ID_TRACKS
            | ID_CLUSTER
            | ID_CUES
            | ID_ATTACHMENTS
            | ID_CHAPTERS
            | ID_TAGS
    )
}

/// Byte extent `[start, end)` of the Segment's data (the EBML header precedes
/// it), from the top-level element headers; `None` if there is no Segment.
fn segment_extent(data: &[u8]) -> Option<(usize, usize)> {
    let mut p = 0usize;
    while p + 2 <= data.len() {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let end = match size {
            Some(s) => (p2 + s as usize).min(data.len()),
            None => data.len(),
        };
        if id == ID_SEGMENT {
            return Some((p2, end));
        }
        if end <= p {
            break;
        }
        p = end;
    }
    None
}

/// Byte extent `[start, end)` of the `Tags` element, resolved via the front
/// SeekHead, for the prefetch warmer to stream a tail-located `Tags` (mkvmerge's
/// layout) in one pipelined read on network filesystems. `None` if there is no
/// Segment, no SeekHead entry for `Tags`, or the pointer doesn't land on one.
pub fn tags_extent(data: &[u8]) -> Option<(usize, usize)> {
    let (seg_start, seg_end) = segment_extent(data)?;
    let mut p = seg_start;
    let mut tags_off = None;
    while p < seg_end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let end = match size {
            Some(s) => (p2 + s as usize).min(seg_end),
            None => break,
        };
        if id == ID_SEEKHEAD {
            if let Some(off) = seekhead_tags_offset(data, p2, end, seg_start) {
                tags_off = Some(off);
                break;
            }
        }
        // The Tags pointer lives in the front SeekHead; don't walk into clusters.
        if id == ID_CLUSTER || end <= p {
            break;
        }
        p = end;
    }
    let off = tags_off?;
    let (id, p1) = read_id(data, off)?;
    if id != ID_TAGS {
        return None;
    }
    let (size, p2) = read_size(data, p1)?;
    let end = match size {
        Some(s) => (p2 + s as usize).min(data.len()),
        None => data.len(),
    };
    Some((off, end))
}

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    // Locate the Segment among the top-level elements (EBML header precedes it).
    let (seg_start, seg_end) =
        segment_extent(data).context("no Segment element (not a Matroska file)")?;

    let mut timestamp_scale = TIMESTAMP_SCALE_DEFAULT;
    let mut duration_ticks: Option<f64> = None;
    let mut track: Option<TrackInfo> = None;
    let mut chunks: Vec<Chunk> = Vec::new();
    // Per-track statistics from the `Tags` element (mkvmerge writes it at the
    // front, inside the warmed head window). Collected regardless of element
    // order and resolved against the video track's UID after the walk.
    let mut stat_tags: Vec<TrackStats> = Vec::new();
    // Absolute offset of a tail-placed `Tags` element, learned from the front
    // SeekHead, so we can read it with one bounded seek if the head walk stops
    // before reaching it (mkvmerge writes `Tags` after the clusters).
    let mut tags_offset: Option<usize> = None;
    // Set when the default fast path stops indexing before the last cluster, so
    // `chunks.len()` is a head window rather than the whole-file frame count.
    let mut stopped_early = false;

    let mut p = seg_start;
    while p < seg_end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        match id {
            ID_INFO => {
                let end = seg_child_end(p2, size, seg_end);
                parse_info(data, p2, end, &mut timestamp_scale, &mut duration_ticks);
                p = end;
            }
            ID_TRACKS => {
                let end = seg_child_end(p2, size, seg_end);
                if track.is_none() {
                    track = parse_tracks(data, p2, end);
                }
                p = end;
            }
            ID_SEEKHEAD => {
                let end = seg_child_end(p2, size, seg_end);
                if tags_offset.is_none() {
                    tags_offset = seekhead_tags_offset(data, p2, end, seg_start);
                }
                p = end;
            }
            ID_TAGS => {
                let end = seg_child_end(p2, size, seg_end);
                parse_tags(data, p2, end, &mut stat_tags);
                p = end;
            }
            ID_CLUSTER => {
                let video_track = track.as_ref().map(|t| t.track_number);
                // Fast path bounds indexing to a head byte-window; `--full` scans all.
                let head_limit = if full { None } else { Some(HEAD_SPAN_BYTES) };
                match size {
                    Some(s) => {
                        let end = (p2 + s as usize).min(seg_end);
                        parse_cluster(data, p2, end, false, video_track, &mut chunks, head_limit);
                        p = end;
                    }
                    None => {
                        // Unknown-size cluster: parse until the next level-1 element.
                        p = parse_cluster(data, p2, seg_end, true, video_track, &mut chunks, head_limit);
                    }
                }
                // Static DV metadata (dvcC + first RPU) and HDR SEI sit in the
                // opening frames, so once the indexed blocks span the head window
                // there's no need to walk block headers across the whole file.
                if head_reached(&chunks, head_limit) {
                    stopped_early = true;
                    break;
                }
            }
            _ => match size {
                Some(s) => p = (p2 + s as usize).min(seg_end),
                None => break,
            },
        }
    }

    let track = track.context("no video track found in Matroska file")?;

    // If the head walk didn't pass the `Tags` element (mkvmerge writes it after
    // the clusters), read it now via the SeekHead offset — one bounded tail seek
    // for the small statistics element, not a cluster walk.
    if stat_tags.is_empty() {
        if let Some(pos) = tags_offset {
            parse_tags_at(data, pos, &mut stat_tags);
        }
    }

    let duration_secs = duration_ticks
        .map(|d| d * timestamp_scale as f64 / 1_000_000_000.0)
        .filter(|d| *d > 0.0);
    let fps = match (track.default_duration_ns, duration_secs, chunks.len()) {
        (Some(dd), _, _) if dd > 0 => Some(1_000_000_000.0 / dd as f64),
        // Frame-count / duration fallback is only valid when we indexed every
        // block; a bounded head window would divide a partial count by the full
        // runtime and report a nonsensically low fps.
        (_, Some(d), n) if d > 0.0 && n > 0 && !stopped_early => Some(n as f64 / d),
        _ => None,
    };

    // Per-stream video bitrate, preferring the mkvmerge statistics tag for the
    // video track — the source MediaInfo reports, and cheap to read. `BPS` is the
    // exact per-stream rate (already over the video track's own duration, which a
    // whole-file duration only approximates); `NUMBER_OF_BYTES` gives the exact
    // size when only that is present. A track may carry several `Tag` entries
    // (e.g. a SOURCE_ID tag before the statistics tag), so take the first with a
    // usable value. Failing a tag, sum the block index when it's complete (whole
    // file walked; a bounded head sample would undercount), else fall back to the
    // container's overall rate from the file length.
    let vstat = track.track_uid.and_then(|uid| {
        stat_tags
            .iter()
            .filter(|s| s.track_uid == Some(uid))
            .find(|s| s.bps.is_some() || s.number_of_bytes.is_some())
    });
    let bitrate = if let Some(bps) = vstat.and_then(|s| s.bps) {
        Some(Bitrate::video_stream_bps(bps))
    } else if let Some(bytes) = vstat.and_then(|s| s.number_of_bytes) {
        Bitrate::video_stream(bytes, duration_secs)
    } else if !stopped_early {
        Bitrate::video_stream(chunks.iter().map(|c| c.size).sum::<u64>(), duration_secs)
    } else {
        Bitrate::overall(data.len() as u64, duration_secs)
    };

    Ok(Demux {
        container: "Matroska",
        codec: track.codec,
        nal_format: track.nal_format,
        width: track.width,
        height: track.height,
        fps,
        duration_secs,
        bit_depth: track.bit_depth,
        chroma: track.chroma,
        codec_profile: track.codec_profile,
        stereo: None,
        color: track.color,
        dv_config: track.dv_config,
        // Matroska interleaves the Profile-7 BL and EL in one track, so it is
        // always single-track (dual layer when an EL is present).
        dv_dual_track: false,
        mastering: track.mastering,
        content_light: track.content_light,
        bitrate,
        chunks,
        reassembled: None,
    })
}

/// End offset of a Segment child; unknown-size non-cluster children are rare and
/// treated as empty rather than swallowing the rest of the file.
fn seg_child_end(payload: usize, size: Option<u64>, seg_end: usize) -> usize {
    match size {
        Some(s) => (payload + s as usize).min(seg_end),
        None => payload,
    }
}

fn parse_info(
    data: &[u8],
    start: usize,
    end: usize,
    timestamp_scale: &mut u64,
    duration_ticks: &mut Option<f64>,
) {
    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_TIMESTAMP_SCALE => {
                let v = read_uint(data, p2, s);
                if v > 0 {
                    *timestamp_scale = v;
                }
            }
            ID_DURATION => *duration_ticks = read_float(data, p2, s),
            _ => {}
        }
        p = cend;
    }
}

/// One `Tag` element's target track plus the statistics values we consume.
struct TrackStats {
    track_uid: Option<u64>,
    number_of_bytes: Option<u64>,
    bps: Option<f64>,
}

/// Scan a `SeekHead`, returning the absolute offset of the `Tags` element if it
/// is listed. `SeekPosition` is relative to the start of Segment data
/// (`seg_start`). Lets us read a tail-placed `Tags` (mkvmerge's default layout)
/// with one bounded read instead of walking the whole file to reach it.
fn seekhead_tags_offset(data: &[u8], start: usize, end: usize, seg_start: usize) -> Option<usize> {
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let cend = (p2 + size.unwrap_or(0) as usize).min(end);
        if id == ID_SEEK {
            if let Some(pos) = seek_entry_tags_pos(data, p2, cend) {
                return Some(seg_start + pos);
            }
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
    None
}

/// A `Seek` entry's `SeekPosition`, but only when its `SeekID` targets `Tags`.
fn seek_entry_tags_pos(data: &[u8], start: usize, end: usize) -> Option<usize> {
    let mut is_tags = false;
    let mut pos = None;
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_SEEK_ID => is_tags = data.get(p2..cend) == Some(&ID_TAGS.to_be_bytes()[..]),
            ID_SEEK_POSITION => pos = Some(read_uint(data, p2, s) as usize),
            _ => {}
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
    if is_tags {
        pos
    } else {
        None
    }
}

/// Parse a `Tags` element located at `pos` (its ID byte), bounding to its declared
/// size. Used to read a tail-placed `Tags` the head walk didn't reach.
fn parse_tags_at(data: &[u8], pos: usize, out: &mut Vec<TrackStats>) {
    let Some((id, p1)) = read_id(data, pos) else { return };
    if id != ID_TAGS {
        return;
    }
    let Some((size, p2)) = read_size(data, p1) else { return };
    let end = match size {
        Some(s) => (p2 + s as usize).min(data.len()),
        None => data.len(),
    };
    parse_tags(data, p2, end, out);
}

/// Parse the `Tags` element, collecting each `Tag`'s target TrackUID and the
/// mkvmerge statistics values used for a per-stream bitrate. Non-statistics tags
/// are ignored.
fn parse_tags(data: &[u8], start: usize, end: usize, out: &mut Vec<TrackStats>) {
    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        let cend = (p2 + size.unwrap_or(0) as usize).min(end);
        if id == ID_TAG {
            if let Some(st) = parse_tag(data, p2, cend) {
                out.push(st);
            }
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
}

fn parse_tag(data: &[u8], start: usize, end: usize) -> Option<TrackStats> {
    let mut st = TrackStats { track_uid: None, number_of_bytes: None, bps: None };
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let cend = (p2 + size.unwrap_or(0) as usize).min(end);
        match id {
            ID_TARGETS => st.track_uid = parse_target_track_uid(data, p2, cend),
            ID_SIMPLE_TAG => {
                if let Some((name, value)) = parse_simple_tag(data, p2, cend) {
                    match name {
                        "NUMBER_OF_BYTES" => st.number_of_bytes = value.trim().parse().ok(),
                        "BPS" => st.bps = value.trim().parse().ok(),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
    Some(st)
}

/// The `TagTrackUID` inside a `Targets`, if any; absence means a whole-file tag.
fn parse_target_track_uid(data: &[u8], start: usize, end: usize) -> Option<u64> {
    let mut p = start;
    let mut uid = None;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        if id == ID_TAG_TRACK_UID {
            uid = Some(read_uint(data, p2, s));
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
    uid
}

/// A `SimpleTag`'s (TagName, TagString) as UTF-8 str slices, if both are present.
fn parse_simple_tag(data: &[u8], start: usize, end: usize) -> Option<(&str, &str)> {
    let mut name = None;
    let mut value = None;
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let cend = (p2 + size.unwrap_or(0) as usize).min(end);
        match id {
            ID_TAG_NAME => name = std::str::from_utf8(data.get(p2..cend)?).ok(),
            ID_TAG_STRING => value = std::str::from_utf8(data.get(p2..cend)?).ok(),
            _ => {}
        }
        if cend <= p {
            break;
        }
        p = cend;
    }
    Some((name?, value?))
}

struct TrackInfo {
    track_number: u64,
    /// TrackUID, used to match this track's `Tags` statistics entries.
    track_uid: Option<u64>,
    codec: Codec,
    nal_format: NalFormat,
    bit_depth: Option<u8>,
    chroma: Option<String>,
    codec_profile: Option<String>,
    width: u32,
    height: u32,
    color: ColorInfo,
    mastering: Option<MasteringDisplay>,
    content_light: Option<ContentLight>,
    dv_config: Option<DvConfig>,
    default_duration_ns: Option<u64>,
}

fn parse_tracks(data: &[u8], start: usize, end: usize) -> Option<TrackInfo> {
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let s = size.unwrap_or(0) as usize;
        let tend = (p2 + s).min(end);
        if id == ID_TRACKENTRY {
            if let Some(t) = parse_track_entry(data, p2, tend) {
                return Some(t);
            }
        }
        p = tend;
    }
    None
}

/// Parse one TrackEntry; returns it only if it is a video track we can handle.
fn parse_track_entry(data: &[u8], start: usize, end: usize) -> Option<TrackInfo> {
    let mut track_number: u64 = 0;
    let mut track_uid: Option<u64> = None;
    let mut track_type: u64 = 0;
    let mut codec_id: &[u8] = &[];
    let mut codec_private: &[u8] = &[];
    let mut default_duration_ns: Option<u64> = None;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut color = ColorInfo::default();
    let mut mastering = None;
    let mut content_light = None;
    let mut dv_config = None;

    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_TRACK_NUMBER => track_number = read_uint(data, p2, s),
            ID_TRACK_UID => track_uid = Some(read_uint(data, p2, s)),
            ID_TRACK_TYPE => track_type = read_uint(data, p2, s),
            ID_CODEC_ID => codec_id = &data[p2..cend],
            ID_CODEC_PRIVATE => codec_private = &data[p2..cend],
            ID_DEFAULT_DURATION => {
                let v = read_uint(data, p2, s);
                if v > 0 {
                    default_duration_ns = Some(v);
                }
            }
            ID_VIDEO => parse_video(
                data,
                p2,
                cend,
                &mut width,
                &mut height,
                &mut color,
                &mut mastering,
                &mut content_light,
            ),
            ID_BLOCK_ADDITION_MAPPING => {
                if let Some(dv) = parse_block_addition_mapping(data, p2, cend) {
                    dv_config = Some(dv);
                }
            }
            _ => {}
        }
        p = cend;
    }

    // Video tracks only (TrackType 1).
    if track_type != 1 {
        return None;
    }

    let cc = classify_codec(codec_id, codec_private);

    // No container Colour element? Recover colour from the SPS in CodecPrivate.
    if color.transfer.is_none() && matches!(cc.codec, Codec::Hevc) {
        if let Some(c) = super::color_from_hvcc(codec_private) {
            color = c;
        }
    }

    Some(TrackInfo {
        track_number,
        track_uid,
        codec: cc.codec,
        nal_format: cc.nal_format,
        bit_depth: cc.bit_depth,
        chroma: cc.chroma,
        codec_profile: cc.codec_profile,
        width,
        height,
        color,
        mastering,
        content_light,
        dv_config,
        default_duration_ns,
    })
}

struct CodecConfig {
    codec: Codec,
    nal_format: NalFormat,
    bit_depth: Option<u8>,
    chroma: Option<String>,
    codec_profile: Option<String>,
}

/// Map a Matroska CodecID (+ CodecPrivate) to codec, NAL framing and codec config.
fn classify_codec(codec_id: &[u8], codec_private: &[u8]) -> CodecConfig {
    if codec_id.starts_with(b"V_MPEGH/ISO/HEVC") {
        // CodecPrivate is an HEVCDecoderConfigurationRecord; blocks are
        // length-prefixed NAL units per its lengthSizeMinusOne.
        let mut cfg = CodecConfig {
            codec: Codec::Hevc,
            nal_format: NalFormat::LengthPrefixed(4),
            bit_depth: None,
            chroma: None,
            codec_profile: None,
        };
        if let Some(h) = super::parse_hvcc_record(codec_private) {
            cfg.nal_format = NalFormat::LengthPrefixed(h.nal_len);
            cfg.bit_depth = Some(h.bit_depth);
            cfg.chroma = Some(h.chroma.to_string());
            cfg.codec_profile = Some(h.profile_str);
        }
        cfg
    } else if codec_id.starts_with(b"V_AV1") {
        // CodecPrivate is an AV1CodecConfigurationRecord (same layout as `av1C`),
        // which carries profile/tier/level and bit depth.
        let (bit_depth, chroma, codec_profile) = match super::parse_av1c_record(codec_private) {
            Some((bd, ch, prof)) => (Some(bd), Some(ch.to_string()), Some(prof)),
            None => (None, None, None),
        };
        CodecConfig {
            codec: Codec::Av1,
            nal_format: NalFormat::LengthPrefixed(4),
            bit_depth,
            chroma,
            codec_profile,
        }
    } else {
        CodecConfig {
            codec: Codec::Other(String::from_utf8_lossy(codec_id).to_string()),
            nal_format: NalFormat::LengthPrefixed(4),
            bit_depth: None,
            chroma: None,
            codec_profile: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_video(
    data: &[u8],
    start: usize,
    end: usize,
    width: &mut u32,
    height: &mut u32,
    color: &mut ColorInfo,
    mastering: &mut Option<MasteringDisplay>,
    content_light: &mut Option<ContentLight>,
) {
    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_PIXEL_WIDTH => *width = read_uint(data, p2, s) as u32,
            ID_PIXEL_HEIGHT => *height = read_uint(data, p2, s) as u32,
            ID_COLOUR => parse_colour(data, p2, cend, color, mastering, content_light),
            _ => {}
        }
        p = cend;
    }
}

fn parse_colour(
    data: &[u8],
    start: usize,
    end: usize,
    color: &mut ColorInfo,
    mastering: &mut Option<MasteringDisplay>,
    content_light: &mut Option<ContentLight>,
) {
    let mut max_cll: Option<u64> = None;
    let mut max_fall: Option<u64> = None;

    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_MATRIX => color.matrix = super::cicp_matrix(read_uint(data, p2, s) as u16).map(str::to_string),
            ID_TRANSFER => {
                color.transfer = super::cicp_transfer(read_uint(data, p2, s) as u16).map(str::to_string)
            }
            ID_PRIMARIES => {
                color.primaries = super::cicp_primaries(read_uint(data, p2, s) as u16).map(str::to_string)
            }
            ID_RANGE => {
                color.range = match read_uint(data, p2, s) {
                    1 => Some("limited".to_string()),
                    2 => Some("full".to_string()),
                    _ => None,
                }
            }
            ID_MAX_CLL => max_cll = Some(read_uint(data, p2, s)),
            ID_MAX_FALL => max_fall = Some(read_uint(data, p2, s)),
            ID_MASTERING => {
                if let Some(m) = parse_mastering(data, p2, cend) {
                    *mastering = Some(m);
                }
            }
            _ => {}
        }
        p = cend;
    }

    if max_cll.is_some() || max_fall.is_some() {
        *content_light = Some(ContentLight::new(max_cll.unwrap_or(0) as u16, max_fall.unwrap_or(0) as u16));
    }
}

fn parse_mastering(data: &[u8], start: usize, end: usize) -> Option<MasteringDisplay> {
    let mut max_lum = None;
    let mut min_lum = None;
    // Rx, Ry, Gx, Gy, Bx, By, Wx, Wy — CIE 1931 floats, no scaling needed.
    let mut chroma = [None::<f64>; 8];
    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { break };
        let Some((size, p2)) = read_size(data, p1) else { break };
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_LUMINANCE_MAX => max_lum = read_float(data, p2, s),
            ID_LUMINANCE_MIN => min_lum = read_float(data, p2, s),
            ID_CHROMA_FIRST..=ID_CHROMA_LAST => {
                chroma[(id - ID_CHROMA_FIRST) as usize] = read_float(data, p2, s)
            }
            _ => {}
        }
        p = cend;
    }
    if max_lum.is_none() && min_lum.is_none() {
        return None;
    }
    let primaries = if chroma.iter().all(Option::is_some) {
        let c = chroma.map(|v| v.unwrap_or(0.0));
        crate::hdr::primaries_label((c[0], c[1]), (c[2], c[3]), (c[4], c[5]), (c[6], c[7]))
    } else {
        None
    };
    Some(MasteringDisplay {
        max_luminance: max_lum.unwrap_or(0.0),
        min_luminance: min_lum.unwrap_or(0.0),
        primaries: primaries.map(str::to_string),
        primaries_level: None,
    })
}

/// Recover a DV config record from a track's BlockAdditionMapping (the DV EL/RPU
/// carriage marker). Type `dvcC`/`dvvC` with the config in BlockAddIDExtraData.
fn parse_block_addition_mapping(data: &[u8], start: usize, end: usize) -> Option<DvConfig> {
    let mut id_type: Option<u32> = None;
    let mut extra: &[u8] = &[];
    let mut p = start;
    while p < end {
        let (id, p1) = read_id(data, p)?;
        let (size, p2) = read_size(data, p1)?;
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_BLOCK_ADD_ID_TYPE => id_type = Some(read_uint(data, p2, s) as u32),
            ID_BLOCK_ADD_ID_EXTRA => extra = &data[p2..cend],
            _ => {}
        }
        p = cend;
    }
    match id_type {
        Some(DVCC) | Some(DVVC) => super::parse_dovi_config(extra),
        _ => None,
    }
}

/// Walk a cluster's children, recording video-block byte ranges. For an
/// unknown-size cluster, stops at (and returns) the next level-1 element; for a
/// known-size cluster, returns `end`.
fn parse_cluster(
    data: &[u8],
    start: usize,
    end: usize,
    unknown: bool,
    video_track: Option<u64>,
    chunks: &mut Vec<Chunk>,
    head_limit: Option<u64>,
) -> usize {
    let mut p = start;
    while p < end {
        let Some((id, p1)) = read_id(data, p) else { return end };
        if unknown && is_level1_id(id) {
            return p;
        }
        let Some((size, p2)) = read_size(data, p1) else { return end };
        let s = size.unwrap_or(0) as usize;
        let cend = (p2 + s).min(end);
        match id {
            ID_SIMPLEBLOCK => record_block(data, p2, cend, video_track, chunks),
            ID_BLOCKGROUP => {
                // The primary frame rides in a Block child (BlockAdditions =
                // dual-track EL, handled in M7).
                let mut q = p2;
                while q < cend {
                    let Some((cid, q1)) = read_id(data, q) else { break };
                    let Some((csz, q2)) = read_size(data, q1) else { break };
                    let cs = csz.unwrap_or(0) as usize;
                    let bend = (q2 + cs).min(cend);
                    if cid == ID_BLOCK {
                        record_block(data, q2, bend, video_track, chunks);
                    }
                    q = bend;
                }
            }
            _ => {}
        }
        p = cend;
        // Stop walking block headers once the head byte-window is covered, so a
        // large cluster on a network filesystem isn't faulted in past what we
        // sample. The demux loop then breaks out entirely.
        if head_reached(chunks, head_limit) {
            return p;
        }
    }
    end
}

/// Whether the default fast path has indexed enough: the recorded blocks span the
/// head byte-window, or a degenerate run of tiny blocks hit the safety cap.
/// Always `false` when unbounded (`--full`, `head_limit == None`).
fn head_reached(chunks: &[Chunk], head_limit: Option<u64>) -> bool {
    let Some(limit) = head_limit else { return false };
    if chunks.len() >= HEAD_BLOCK_CAP {
        return true;
    }
    match (chunks.first(), chunks.last()) {
        (Some(f), Some(l)) => (l.offset + l.size).saturating_sub(f.offset) >= limit,
        _ => false,
    }
}

/// Record the frame-data byte range of a (Simple)Block for the video track.
/// Handles the common unlaced case; laced blocks (rare for video) are skipped.
fn record_block(
    data: &[u8],
    start: usize,
    end: usize,
    video_track: Option<u64>,
    chunks: &mut Vec<Chunk>,
) {
    let Some(vt) = video_track else { return };
    let Some((tnum, p1)) = read_vint_num(data, start) else { return };
    if tnum != vt {
        return;
    }
    // int16 relative timecode + 1 flags byte.
    if p1 + 3 > end {
        return;
    }
    let flags = data[p1 + 2];
    let lacing = (flags >> 1) & 0x03;
    if lacing != 0 {
        return;
    }
    let frame_start = p1 + 3;
    if frame_start >= end {
        return;
    }
    chunks.push(Chunk {
        offset: frame_start as u64,
        size: (end - frame_start) as u64,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_size_known_and_unknown() {
        // 1-byte size 0x81 -> value 1.
        assert_eq!(read_size(&[0x81], 0), Some((Some(1), 1)));
        // 2-byte size 0x40 0x02 -> value 2.
        assert_eq!(read_size(&[0x40, 0x02], 0), Some((Some(2), 2)));
        // 1-byte unknown size 0xFF.
        assert_eq!(read_size(&[0xFF], 0), Some((None, 1)));
        // 2-byte unknown size 0x7F 0xFF.
        assert_eq!(read_size(&[0x7F, 0xFF], 0), Some((None, 2)));
    }

    #[test]
    fn id_length_from_leading_bits() {
        // Segment ID is 4 bytes.
        assert_eq!(read_id(&[0x18, 0x53, 0x80, 0x67], 0), Some((ID_SEGMENT, 4)));
        // TrackEntry ID is 1 byte.
        assert_eq!(read_id(&[0xAE], 0), Some((ID_TRACKENTRY, 1)));
    }

    #[test]
    fn simpleblock_frame_range_unlaced() {
        // track number 0x81 (=1), timecode 0x0000, flags 0x00, then 3 frame bytes.
        let block = [0x81, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC];
        let mut chunks = Vec::new();
        record_block(&block, 0, block.len(), Some(1), &mut chunks);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset, 4);
        assert_eq!(chunks[0].size, 3);
    }

    #[test]
    fn simpleblock_wrong_track_skipped() {
        let block = [0x82, 0x00, 0x00, 0x00, 0xAA];
        let mut chunks = Vec::new();
        record_block(&block, 0, block.len(), Some(1), &mut chunks);
        assert!(chunks.is_empty());
    }

    #[test]
    fn laced_block_skipped() {
        // flags 0x06 -> lacing bits set.
        let block = [0x81, 0x00, 0x00, 0x06, 0xAA, 0xBB];
        let mut chunks = Vec::new();
        record_block(&block, 0, block.len(), Some(1), &mut chunks);
        assert!(chunks.is_empty());
    }

    /// Encode an EBML element with a 1-byte size (payloads here stay < 127).
    fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 0x7F);
        let mut v = id.to_vec();
        v.push(0x80 | payload.len() as u8);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn stat_tags_pick_statistics_over_a_source_id_tag() {
        // A real mkvmerge `Tags` element lists a SOURCE_ID-only Tag for the video
        // track *before* the statistics Tag for the same TrackUID. The parser must
        // surface both, and the selection must skip the empty one and use BPS.
        let uid: u64 = 0x0102_0304_0506_0708;
        let simple = |name: &str, val: &str| {
            let mut body = el(&[0x45, 0xA3], name.as_bytes()); // TagName
            body.extend(el(&[0x44, 0x87], val.as_bytes())); // TagString
            el(&[0x67, 0xC8], &body) // SimpleTag
        };
        let targets = el(&[0x63, 0xC0], &el(&[0x63, 0xC5], &uid.to_be_bytes())); // Targets>TagTrackUID

        let mut source_only = targets.clone();
        source_only.extend(simple("SOURCE_ID", "001011"));
        let mut stats = targets.clone();
        stats.extend(simple("BPS", "81679541"));
        stats.extend(simple("NUMBER_OF_BYTES", "304052094"));

        let mut payload = el(&[0x73, 0x73], &source_only); // Tag (SOURCE_ID only)
        payload.extend(el(&[0x73, 0x73], &stats)); // Tag (statistics)

        let mut out = Vec::new();
        parse_tags(&payload, 0, payload.len(), &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0].bps.is_none() && out[0].number_of_bytes.is_none(), "SOURCE_ID tag has no stats");

        // Mirror the demux selection: first entry for the UID that carries a value.
        let picked = out
            .iter()
            .filter(|s| s.track_uid == Some(uid))
            .find(|s| s.bps.is_some() || s.number_of_bytes.is_some())
            .expect("a statistics entry for the video UID");
        assert_eq!(picked.bps, Some(81_679_541.0));
        assert_eq!(picked.number_of_bytes, Some(304_052_094));
    }

    #[test]
    fn seekhead_locates_tags_offset() {
        // A Seek entry whose SeekID targets Tags yields seg_start + SeekPosition.
        let seek = {
            let mut b = el(&[0x53, 0xAB], &ID_TAGS.to_be_bytes()); // SeekID = Tags
            b.extend(el(&[0x53, 0xAC], &[0x10])); // SeekPosition = 16
            el(&[0x4D, 0xBB], &b) // Seek
        };
        assert_eq!(seekhead_tags_offset(&seek, 0, seek.len(), 1000), Some(1016));
    }

    #[test]
    fn tags_extent_resolves_a_tail_tags_via_seekhead() {
        // Segment { SeekHead(→Tags), Tags }: the prefetch warmer must recover the
        // exact byte extent of the tail Tags element from the front SeekHead.
        let tags = el(&ID_TAGS.to_be_bytes(), &[0u8; 10]); // opaque payload; only the header matters
        let pos = 19u8; // byte length of the SeekHead element built below
        let seek = {
            let mut b = el(&[0x53, 0xAB], &ID_TAGS.to_be_bytes());
            b.extend(el(&[0x53, 0xAC], &[pos])); // SeekPosition, relative to Segment data
            el(&[0x4D, 0xBB], &b)
        };
        let seekhead = el(&ID_SEEKHEAD.to_be_bytes(), &seek);
        assert_eq!(seekhead.len(), pos as usize, "SeekPosition must match SeekHead length");

        let mut seg_payload = seekhead.clone();
        seg_payload.extend(tags.clone());
        let data = el(&ID_SEGMENT.to_be_bytes(), &seg_payload);

        let seg_start = data.len() - seg_payload.len();
        let start = seg_start + seekhead.len();
        assert_eq!(tags_extent(&data), Some((start, start + tags.len())));
    }
}
