//! FLV (Adobe Flash Video) and Enhanced FLV / E-RTMP, `.flv`.
//!
//! A 9-byte header then a flat chain of tags, each `11-byte header + DataSize
//! payload + a 4-byte back-pointer`, so the walk is declared-size arithmetic
//! with no scanning — the AVI shape, without an index. Everything is
//! **big-endian**, which the spec states outright and which is the opposite of
//! every other byte-oriented container in this tree.
//!
//! Seven facts about the format are invariants a later change would otherwise
//! undo quietly. Each is pinned by a test. (The closing paragraph, on why a
//! video access unit *is* a byte range here, is a note on the chunk-index
//! design rather than one of the seven — the same shape as [`super::asf`]'s
//! "no payload index" note. Counting bold paragraphs will therefore not give
//! seven; counting the ones between here and that note will.)
//!
//! **`TimestampExtended` is the high byte, not a fourth low byte.** A tag's
//! time is `(byte 7 << 24) | u24(bytes 1..4)`. Reading bytes 4..8 as one
//! big-endian `u32` is the obvious mistake and is wrong for every tag past
//! 16777 seconds. **`TagType` is `byte & 0x1F`** for the same class of reason:
//! the top three bits are two reserved bits and the `Filter` (encrypted) flag.
//!
//! **`onMetaData` is a hint, and its `duration` is routinely absent or zero.**
//! FLV's natural habitat is RTMP ingest, where the file is still being written;
//! ffmpeg gates `width`/`height`/`videocodecid` from it behind a
//! `trust_metadata` option. So the coded stream wins every field it states, the
//! metadata fills what is left, and a zero duration is treated as unstated
//! rather than as zero. `filesize`, when declared, is what tells a truncated
//! file from a complete one — `testfiles/sdr/flv_4k_trunc.flv` declares
//! 85,623,691 bytes over a 4 MiB prefix.
//!
//! **The ecma-array's count field is documented as approximate and must never
//! bound the parse.** The spec's own wording is "the list contains
//! *approximately* `ECMAArrayLength` number of items"; writers emit 0 over a
//! populated array. The terminator (`00 00 09`) ends a property list and the
//! tag's own `DataSize` is the outer bound. The reader here trusts neither
//! declared count it meets — the ecma-array's or a strict array's — and is
//! bounded by bytes and by a node budget instead, because a property list costs
//! three bytes per entry while each entry allocates a `String`: that product is
//! what the budget bounds (the same shape as the AVI index defect).
//!
//! **`videodatarate` is in units of 1024 bits per second, not 1000.** ffmpeg
//! writes `bit_rate / 1024` and reads it back the same way. Measured on the two
//! real corpus FLVs: 7812.5 and 29296.875 recover exactly 8 Mbit/s and
//! 30 Mbit/s at x1024, and land on 7,812,500 and 29,296,875 — 2.4% low and not
//! round — at x1000. Both ffprobe and MediaInfo report the x1024 value. It is a
//! muxer-declared average rather than a measured sum, which is why a `--full`
//! walk's exact byte count replaces it.
//!
//! **The Enhanced (E-RTMP) header must be detected before the CodecID is
//! read.** Bit 7 of a video tag's first payload byte is `isExVideoHeader`; when
//! it is set the low nibble is a `videoPacketType` and the codec is named by a
//! four-character code that follows, so reading the byte as legacy
//! `FrameType(4) | CodecID(4)` yields a nonsense codec. **A `ModEx` packet type
//! (7) prefixes a variable-length block and then re-reads the type byte**, so
//! the FourCC does not sit at a fixed offset; and **the 3-byte composition time
//! rides `CodedFrames` (1) only, and only for `avc1`/`hvc1`/`vvc1`** —
//! `CodedFramesX` (3) never carries it and `av01`/`vp08`/`vp09` never carry it,
//! so a fixed payload offset shifts an access unit by three bytes. For AV1 that
//! is not cosmetic: the RPU, the HDR10+ T.35 message and the CLL/MDCV OBUs all
//! ride metadata OBUs at the *head* of the unit, so three bytes in is past them
//! and the whole dynamic report disappears. (Reference §6 states the composition
//! time unconditionally and is wrong; it has been corrected there.) A
//! `videoFrameType` of 5 (Command) likewise carries a command byte where the
//! FourCC would be — except inside a Metadata packet, which is exactly what the
//! corpus file's own `colorInfo` tag is, so the two conditions must be tested
//! together. The corpus's `flv_enhanced_hdr.flv` carries packet types 0, 1, 3
//! and 4, so most of that is exercised by real bytes.
//!
//! **Every read in a tag header is bounded by the tag, not by the buffer.**
//! The two are not the same and the difference is reachable from well-formed
//! input: a `DataSize` of 0 puts the video header exactly on the following
//! 4-byte `PreviousTagSize`, and an empty video tag legitimately writes
//! `00 00 00 0B` there — read as a codec that is legacy id 0. Because the codec
//! gate is first-wins and `Codec::Other` has no sampler arm, one such tag makes
//! every later real video tag inert and silently costs the entire dynamic
//! report. The same rule covers the `ModEx` size bytes and the FourCC.
//!
//! **Enhanced `colorInfo` luminance is in nits, ST.2086's is in units of
//! 0.0001 cd/m².** The E-RTMP spec departs from ST.2086 deliberately and says
//! so; treating `hdrMdcv`'s `minLuminance` as ST.2086 units misreports it by a
//! factor of 10000. No file that carries a populated `hdrMdcv` has been
//! obtained — ffmpeg's muxer writes only `colorConfig.matrixCoefficients`, by
//! two independent routes — so this rule is spec-derived and is pinned by a
//! unit test built from the spec's own field definitions rather than by a
//! corpus file.
//!
//! **A video access unit is a byte range in the file**, unlike ASF's, so the
//! chunk index is real and the sampler runs: the corpus's Enhanced FLV reports
//! a mastering display and MaxCLL that live only in the HEVC bitstream's SEI
//! messages. The default path indexes a bounded head window
//! ([`HEAD_SCAN_BYTES`]) and `--full` fuses the whole-file walk with extraction
//! through [`super::RawFullStream::Flv`], which is also where the exact
//! video-stream byte count comes from.

use anyhow::{bail, Result};

use crate::model::{Bitrate, ContentLight, MasteringDisplay};

use super::{bmih, Chunk, Codec, Demux, NalFormat, RawFullStream, TrackDemux};

/// How far into the file the default tag walk indexes. Everything the General
/// section needs sits far inside it (`onMetaData` at the first tag, the
/// configuration record at the first video tag); the rest of the window buys
/// access units for the sampler.
///
/// Must stay `<=` `prefetch::HEAD_WARM` so a network volume's generic head warm
/// covers the walked span, the same coupling `annexb`, `av1`, `mpegv` and `ps`
/// keep — with one honest caveat those do not have: this bounds where a tag
/// *starts*, so a tag beginning just inside the limit and declaring a large
/// payload has its own bytes fall outside the warm. That is timing-only (every
/// read is bounded by the buffer either way) and only on a remote volume, but
/// the coupling is partial here rather than complete.
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

/// Tail window warmed and read for the duration fallback: the final
/// `PreviousTagSize` points back at the last tag's header, and its timestamp is
/// the only length signal a file with no `onMetaData` duration has. A last tag
/// larger than this still resolves — the read is against the mmap — it just
/// faults cold on a remote volume, which is timing and never correctness.
pub const TAIL_SCAN_BYTES: usize = 64 << 10;

/// Script-data tags examined for `onMetaData`. It is conventionally the first
/// tag but is not guaranteed to be, and a file may carry `onXMPData`, an
/// encryption `|AdditionalHeader`, or a `onLastSecond` marker beside it.
const MAX_SCRIPT_TAGS: usize = 8;

/// Deepest AMF object nesting accepted. `onMetaData` is two levels
/// (`keyframes.times`) and `colorInfo` is two (`hdrMdcv.redX`); the limit
/// exists because each level costs three bytes of input and one stack frame, so
/// a hostile file could otherwise recurse thousands deep out of a few KiB.
const MAX_AMF_DEPTH: u8 = 8;

/// Total AMF values one script tag may produce. Bytes alone do not bound the
/// *allocation*: a property list costs three bytes per entry and every entry
/// owns a `String`, so a 16 MiB tag of empty keys would mint five million of
/// them. Real payloads are tens of nodes; this is four orders of magnitude of
/// headroom and turns the product of "many entries" and "each allocates" back
/// into a bounded quantity.
const MAX_AMF_NODES: usize = 64 << 10;

/// Longest duration accepted, from `onMetaData` or from the tail timestamp.
/// Both are unvalidated numbers a malformed file can make enormous, and a
/// bitrate divides by this.
const MAX_DURATION_SECS: f64 = 48.0 * 3600.0;

/// `videodatarate` is declared in units of 1024 bits per second — see the
/// module doc's fourth invariant, and `dev/sdr-format-reference.md` §6 for the
/// measurements behind it.
const VIDEODATARATE_SCALE: f64 = 1024.0;

pub(crate) const CONTAINER_LABEL: &str = "FLV (Flash Video)";

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    if !is_flv(data) {
        bail!("not an FLV file (no FLV signature)");
    }
    // "The length of this header in bytes" — usually 9, but the spec says to
    // seek to it rather than assume, and then `PreviousTagSize0` (always 0)
    // precedes the first tag.
    let header_len = u32be(data, 5) as usize;
    let Some(data_start) = header_len.checked_add(4).filter(|s| *s <= data.len()) else {
        bail!("FLV header declares {header_len} bytes, past the end of the file");
    };

    // The head walk is bounded on **both** paths. Under `--full` the fused
    // walk below re-reads the file once, so widening this one here would make
    // `--full` two passes — the thing every streaming backend in this tree
    // exists to avoid.
    let walk = walk_head(data, data_start, HEAD_SCAN_BYTES.min(data.len()));
    let meta = walk.metadata.as_ref();

    let declared_size = meta.and_then(|m| m.number("filesize")).filter(|s| *s > 0.0);
    // A file that holds materially less than its own header declares is a
    // partial download or a still-recording capture. Both are ordinary for FLV,
    // and both make a whole-file byte count the wrong numerator for a rate over
    // the declared runtime — the defect the AVI backend was fixed for.
    let complete = declared_size.is_none_or(|s| data.len() as f64 >= s);

    let duration_secs = meta
        .and_then(|m| m.number("duration"))
        .filter(|d| *d > 0.0 && *d <= MAX_DURATION_SECS)
        .or_else(|| tail_duration(data, data_start, &walk));

    // A tag header is evidence; `onMetaData` is a declaration, so it is only
    // consulted when no video tag named a codec — an encrypted tag chain, or
    // one whose headers could not be read. With neither, the file is refused
    // rather than reporting a placeholder track, which is what the sibling
    // backends do for a file that carries no video.
    let Some(codec) = walk.codec.clone().or_else(|| meta.and_then(metadata_codec)) else {
        bail!("no video stream in the FLV tag chain");
    };
    let mut td = TrackDemux { chunks: walk.chunks, ..TrackDemux::new(codec, NalFormat::AnnexB) };
    // The coded stream first, so nothing declared can overwrite what the
    // bitstream states — ffmpeg's `trust_metadata` gate reaches the same
    // ordering from the other side.
    if let Some((start, end)) = walk.config {
        fill_from_config(&mut td, &data[start..end]);
    }
    if let Some(ci) = &walk.color_info {
        ci.apply(&mut td);
    }
    if td.width == 0 || td.height == 0 {
        let w = meta.and_then(|m| m.number("width")).unwrap_or(0.0);
        let h = meta.and_then(|m| m.number("height")).unwrap_or(0.0);
        if w > 0.0 && h > 0.0 && w <= f64::from(u32::MAX) && h <= f64::from(u32::MAX) {
            (td.width, td.height) = (w as u32, h as u32);
        }
    }
    if td.fps.is_none() {
        td.fps = meta
            .and_then(|m| m.number("framerate"))
            .filter(|f| *f > 0.0 && f.is_finite());
    }

    td.bitrate = if full && complete {
        // The fused `--full` walk sums the exact video payload bytes
        // (`sample::Scan::es_bytes`, applied in main.rs), which is a
        // measurement rather than the muxer's declaration — so leave the rate
        // unset here and let it land, exactly as the MKV `--full` path does.
        //
        // **Gated on the file being whole**, because over a prefix the walk's
        // numerator is the bytes present while the denominator is the declared
        // runtime: measured on `testfiles/sdr/flv_4k_trunc.flv`, 1.35 Mbit/s
        // labelled as an exact video-stream rate against a declared 30. So a
        // short file keeps the muxer's declared rate, which is a header fact
        // and describes the whole title rather than the fragment.
        None
    } else {
        meta.and_then(|m| m.number("videodatarate"))
            .map(|r| r * VIDEODATARATE_SCALE)
            // **The product is what gets checked, not the factor.** A declared
            // rate above `f64::MAX / 1024` multiplies to infinity, which
            // `Bitrate` stores verbatim and `serde` then writes as JSON `null`
            // — breaking the schema's "always a float" contract for
            // `bits_per_sec` on machine consumers. Filtering the input instead
            // is the same shape as clamping two index levels and letting their
            // product escape.
            .filter(|b| *b > 0.0 && b.is_finite())
            .map(Bitrate::video_stream_bps)
            // No declared rate: the whole-container rate, which counts audio
            // and every tag header and is labelled distinctly for it. Withheld
            // over a short file, where the numerator would be the bytes present
            // and the denominator the whole declared runtime.
            .or_else(|| {
                complete.then(|| Bitrate::overall(data.len() as u64, duration_secs)).flatten()
            })
    };

    // A bounded head walk saw only part of the stream, so `--full` would read
    // every chunk that exists and still not have seen it all; the report keeps
    // its sampled marks on for that. Under `--full` the streaming plan below
    // covers the file, so it never applies.
    let bounded_index = !full && !walk.complete && !td.chunks.is_empty();
    let raw_stream = full.then_some(RawFullStream::Flv { data_start });

    Ok(Demux {
        container: CONTAINER_LABEL,
        duration_secs,
        tracks: vec![td],
        ts_stream: None,
        mkv_stream: None,
        raw_stream,
        bounded_index,
    })
}

/// ffmpeg's own probe, which is stricter than the three signature bytes: the
/// version must be below 5 and `DataOffset`'s high byte must be zero (the
/// header is nine bytes, not sixteen million).
pub(crate) fn is_flv(data: &[u8]) -> bool {
    data.len() >= 9 && &data[0..3] == b"FLV" && data[3] < 5 && data[5] == 0 && u32be(data, 5) > 8
}

fn u16be(d: &[u8], at: usize) -> u16 {
    match d.get(at..at + 2) {
        Some(b) => u16::from_be_bytes([b[0], b[1]]),
        None => 0,
    }
}

fn u24be(d: &[u8], at: usize) -> u32 {
    match d.get(at..at + 3) {
        Some(b) => u32::from_be_bytes([0, b[0], b[1], b[2]]),
        None => 0,
    }
}

fn u32be(d: &[u8], at: usize) -> u32 {
    match d.get(at..at + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

// --- the tag walk ------------------------------------------------------------

/// Tag types (`byte & 0x1F`).
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;

/// Fixed part of a tag header, before its payload.
const TAG_HEADER_LEN: usize = 11;
/// The `PreviousTagSize` word that follows every tag.
const BACK_POINTER_LEN: usize = 4;

#[derive(Default)]
struct HeadWalk {
    chunks: Vec<Chunk>,
    /// The codec named by the first video tag.
    codec: Option<Codec>,
    /// Byte range of the decoder configuration record, when one was seen.
    config: Option<(usize, usize)>,
    /// The Enhanced FLV `colorInfo` metadata packet, decoded.
    color_info: Option<ColorInfoPacket>,
    metadata: Option<AmfObject>,
    /// Timestamps of the first and last video tags the walk saw, milliseconds.
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    /// True when the walk consumed the file exactly — every tag accounted for,
    /// nothing left over.
    complete: bool,
}

/// Walk tags from `start`, indexing video payload up to `limit`.
fn walk_head(data: &[u8], start: usize, limit: usize) -> HeadWalk {
    let mut w = HeadWalk::default();
    let mut scripts = 0usize;
    let mut pos = start;
    loop {
        // The chain ends cleanly only by running out exactly at the end of the
        // file; anything else is a bounded stop or a truncation.
        if pos == data.len() {
            w.complete = true;
            return w;
        }
        let Some(tag) = read_tag(data, pos, limit) else { return w };
        match tag.kind {
            TAG_SCRIPT if scripts < MAX_SCRIPT_TAGS && w.metadata.is_none() => {
                scripts += 1;
                w.metadata = parse_script_tag(data, tag.body, tag.end);
            }
            TAG_VIDEO => {
                if w.first_ts.is_none() {
                    w.first_ts = Some(tag.timestamp);
                }
                w.last_ts = Some(tag.timestamp);
                // `Filter == 1` means the payload is encrypted. The spec keeps
                // `onMetaData` in the clear, so metadata still works, but the
                // bitstream must not be handed to a parser as though it were
                // codec bytes.
                if !tag.encrypted {
                    apply_video_tag(data, tag.body, tag.end, &mut w);
                }
            }
            _ => {}
        }
        pos = tag.next;
    }
}

struct Tag {
    kind: u8,
    encrypted: bool,
    timestamp: i64,
    body: usize,
    end: usize,
    next: usize,
}

/// Read one tag header at `pos`. `None` when the header does not fit, the tag
/// runs past the end of the buffer, or the tag begins at or after `limit`.
fn read_tag(data: &[u8], pos: usize, limit: usize) -> Option<Tag> {
    if pos >= limit || pos + TAG_HEADER_LEN > data.len() {
        return None;
    }
    let size = u24be(data, pos + 1) as usize;
    let body = pos + TAG_HEADER_LEN;
    let end = body.checked_add(size)?;
    let next = end.checked_add(BACK_POINTER_LEN)?;
    if next > data.len() {
        return None;
    }
    Some(Tag {
        kind: data[pos] & 0x1F,
        encrypted: data[pos] & 0x20 != 0,
        // The extended byte is the *high* byte of a signed 32-bit millisecond
        // count, not a fourth low byte.
        timestamp: i64::from(
            (i64::from(data[pos + 7]) << 24 | i64::from(u24be(data, pos + 4))) as i32,
        ),
        body,
        end,
        next,
    })
}

/// The Enhanced FLV bit: set in a video tag's first payload byte.
const EX_VIDEO_HEADER: u8 = 0x80;
/// Enhanced `videoPacketType` values this backend acts on.
const PKT_SEQUENCE_START: u8 = 0;
const PKT_CODED_FRAMES: u8 = 1;
const PKT_CODED_FRAMES_X: u8 = 3;
const PKT_METADATA: u8 = 4;
const PKT_MULTITRACK: u8 = 6;
const PKT_MOD_EX: u8 = 7;
/// `videoFrameType` 5: the tag carries a one-byte command rather than a codec's
/// data, and — outside a Metadata packet — no FourCC at all.
const FRAME_TYPE_COMMAND: u8 = 5;
/// Legacy `CodecID` for H.264, the only legacy codec with a config record.
const LEGACY_CODEC_AVC: u8 = 7;

/// Reported name for the `vp08` FourCC. There is no `Codec` variant for VP8 —
/// its bitstream has no parser here — but it shares VP9's configuration record,
/// so `fill_from_config` has to recognise it by name. Naming the constant keeps
/// the two sites from drifting apart silently.
const VP8_LABEL: &str = "VP8";

/// Decode one video tag and fold what it carries into the walk.
fn apply_video_tag(data: &[u8], body: usize, end: usize, w: &mut HeadWalk) {
    // **Bounded by the tag, not the buffer.** A `DataSize` of 0 puts `body`
    // exactly on the 4-byte back-pointer that follows the tag, and a
    // *well-formed* empty video tag writes `00 00 00 0B` there — so the codec
    // would be read as id 0 out of a length field. That is not cosmetic: the
    // codec gate below is first-wins, so one such tag makes every later real
    // video tag inert, and `Codec::Other` has no sampler arm, which silently
    // costs the whole dynamic report (observed on the corpus's Enhanced FLV:
    // the mastering display and MaxCLL vanished). With attacker-chosen
    // back-pointer bytes the identity can be steered to any codec.
    if body >= end {
        return;
    }
    let Some(&first) = data.get(body) else { return };
    if first & EX_VIDEO_HEADER != 0 {
        apply_enhanced_tag(data, body, end, w);
        return;
    }
    let codec_id = first & 0x0F;
    let codec = legacy_codec(codec_id);
    if w.codec.is_none() {
        w.codec = Some(codec);
    }
    if codec_id != LEGACY_CODEC_AVC {
        // Every other legacy codec's payload starts right after the one-byte
        // header and has no NAL structure the sampler could read, so it is
        // named and not indexed.
        return;
    }
    // `AVCPacketType` then a 3-byte composition time, then the payload. The
    // whole five-byte header is checked against the tag's end before any of it
    // is read, so a short tag cannot take its packet type from the following
    // back-pointer.
    let payload = body + 5;
    if payload > end {
        return;
    }
    let packet_type = data[body + 1];
    match packet_type {
        // "This contains the same information that would be stored in an avcC
        // box in an MP4/FLV file" — Adobe's spec, verbatim.
        0 if w.config.is_none() => w.config = Some((payload, end)),
        1 if payload < end => {
            w.chunks.push(Chunk { offset: payload as u64, size: (end - payload) as u64 })
        }
        _ => {}
    }
}

/// The Enhanced (E-RTMP) video tag header: a packet type, an optional chain of
/// `ModEx` blocks, then a four-character codec code.
fn apply_enhanced_tag(data: &[u8], body: usize, end: usize, w: &mut HeadWalk) {
    let mut pos = body;
    let Some(&first) = data.get(pos) else { return };
    let frame_type = (first >> 4) & 0x07;
    let mut packet_type = first & 0x0F;
    pos += 1;
    // `ModEx` prefixes a sized block and then re-states the packet type. The
    // loop is bounded by the tag: each round consumes at least two bytes, and
    // every read is checked against `end` rather than the buffer — a tag whose
    // last payload byte happens to have low nibble 7 would otherwise take its
    // `modExDataSize` out of the following back-pointer.
    while packet_type == PKT_MOD_EX {
        if pos >= end {
            return;
        }
        let size_byte = data[pos];
        pos += 1;
        let mut size = size_byte as usize + 1;
        if size == 256 {
            if pos + 2 > end {
                return;
            }
            size = u16be(data, pos) as usize + 1;
            pos += 2;
        }
        pos = match pos.checked_add(size) {
            Some(p) if p < end => p,
            _ => return,
        };
        packet_type = data[pos] & 0x0F;
        pos += 1;
    }
    // Multitrack (type 6) inserts `videoMultitrackType(4) | videoPacketType(4)`
    // before the FourCC, and what follows the FourCC then depends on that
    // multitrack type — `ManyTracks` interleaves a track id and a size per
    // track, `OneTrack` does not. Descending is deliberately not done: E-RTMP
    // multitrack video is a live-ingest construct with no file fixture to
    // verify against, and blending several tracks' access units into one
    // report would be worse than reporting fewer facts. So the signal is
    // *detected*, the first track's FourCC still names the codec, and nothing
    // from such a tag is indexed — never a silent truncation of a track list
    // into a plausible-looking single track.
    // A `Command` frame (videoFrameType 5) carries a one-byte command instead
    // of a FourCC — but only *outside* a Metadata packet, which is exactly the
    // combination the corpus's own `colorInfo` tag uses (frame type 5, packet
    // type 4). Reading four bytes as a FourCC here names the codec out of a
    // command byte and whatever follows it.
    if packet_type != PKT_METADATA && frame_type == FRAME_TYPE_COMMAND {
        return;
    }
    let multitrack = packet_type == PKT_MULTITRACK;
    if multitrack {
        if pos >= end {
            return;
        }
        packet_type = data[pos] & 0x0F;
        pos += 1;
    }
    // Bounded by the *tag*, not the buffer: every other read in this function
    // is, and a tag whose declared payload ends first would otherwise adopt the
    // 4-byte back-pointer that follows it as the codec identity (observed on a
    // one-byte payload: a reported codec of `0x0000000C`).
    if pos + 4 > end {
        return;
    }
    let fourcc: [u8; 4] = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
    pos += 4;
    if w.codec.is_none() {
        w.codec = Some(enhanced_codec(&fourcc));
    }
    if multitrack {
        return;
    }
    match packet_type {
        PKT_SEQUENCE_START if w.config.is_none() => w.config = Some((pos, end)),
        // **`CodedFrames` carries a 3-byte composition time only for the codecs
        // that have one.** The spec gates `compositionTimeOffset` on the FourCC
        // being `avc1`, `hvc1` or `vvc1`; `av01`, `vp08` and `vp09` go straight
        // to coded data. Skipping it unconditionally shifts every AV1 and VP9
        // access unit forward three bytes and shortens it by three, which for
        // AV1 costs the *whole* dynamic report — the RPU, the HDR10+ T.35 and
        // the CLL/MDCV OBUs all ride metadata OBUs at the head of the unit,
        // and three bytes in is past them. (Reference §6 describes packet type
        // 1 as "with s24 CTS" without the gate, and is wrong.)
        PKT_CODED_FRAMES | PKT_CODED_FRAMES_X => {
            let payload = if packet_type == PKT_CODED_FRAMES && has_composition_time(&fourcc) {
                pos + 3
            } else {
                pos
            };
            if payload < end {
                w.chunks.push(Chunk { offset: payload as u64, size: (end - payload) as u64 });
            }
        }
        PKT_METADATA if w.color_info.is_none() => {
            w.color_info = parse_color_info(data, pos, end);
        }
        _ => {}
    }
}

/// Whether a `CodedFrames` packet of this codec carries the 3-byte
/// `compositionTimeOffset`. The NAL-based codecs reorder pictures and need it;
/// the others do not define the field at all.
fn has_composition_time(fourcc: &[u8; 4]) -> bool {
    matches!(fourcc, b"avc1" | b"hvc1" | b"vvc1")
}

/// The codec `onMetaData.videocodecid` declares: a legacy `CodecID` as a small
/// integer, or an Enhanced `VideoFourCc` as a big-endian `u32` — the corpus's
/// Enhanced file writes 1752589105, which is `hvc1`.
fn metadata_codec(m: &AmfObject) -> Option<Codec> {
    let v = m.number("videocodecid")?;
    if !v.is_finite() || v < 0.0 || v > f64::from(u32::MAX) || v.fract() != 0.0 {
        return None;
    }
    let n = v as u32;
    // The legacy ids are a 4-bit field, so nothing above 15 can be one; a
    // FourCC of printable characters is far above it.
    if n <= 0x0F {
        return Some(legacy_codec(n as u8));
    }
    Some(enhanced_codec(&n.to_be_bytes()))
}

/// Legacy `CodecID`, named from the spec's own table. Only H.264 has a parser
/// here; the rest keep their identity and the container's picture facts.
fn legacy_codec(id: u8) -> Codec {
    match id {
        2 => Codec::Other("Sorenson H.263".into()),
        3 => Codec::Other("Screen video".into()),
        4 => Codec::Other("On2 VP6".into()),
        5 => Codec::Other("On2 VP6 with alpha".into()),
        6 => Codec::Other("Screen video v2".into()),
        LEGACY_CODEC_AVC => Codec::Avc,
        other => Codec::Other(format!("FLV codec {other}")),
    }
}

/// Enhanced FLV `VideoFourCc`. The four this build can parse map to their
/// codecs, the two it cannot are named from the spec's own enum the way
/// [`legacy_codec`] names its unparsed codecs, and anything else keeps the code
/// verbatim.
fn enhanced_codec(fourcc: &[u8; 4]) -> Codec {
    match fourcc {
        b"avc1" => Codec::Avc,
        b"hvc1" => Codec::Hevc,
        b"av01" => Codec::Av1,
        b"vp09" => Codec::Vp9,
        b"vp08" => Codec::Other(VP8_LABEL.into()),
        b"vvc1" => Codec::Other("VVC".into()),
        _ if fourcc.iter().all(|b| (0x20..=0x7E).contains(b)) => {
            Codec::Other(String::from_utf8_lossy(fourcc).trim_end().to_string())
        }
        _ => Codec::Other(format!("0x{:08X}", u32::from_be_bytes(*fourcc))),
    }
}

/// Fill the track from whichever decoder configuration record the codec uses.
fn fill_from_config(td: &mut TrackDemux, rec: &[u8]) {
    match td.codec {
        // The `avcC`/`hvcC` route, shared with the Video for Windows carriages.
        // Both are verbatim ISOBMFF records here, so the check that guards
        // those — is this a configuration record at all — applies unchanged.
        Codec::Avc | Codec::Hevc if bmih::is_config_record(rec) => {
            super::fill_nal_config_fields(td, rec)
        }
        Codec::Av1 => {
            if let Some((depth, chroma, profile)) = super::parse_av1c_record(rec) {
                td.bit_depth = Some(depth);
                td.chroma = Some(chroma.to_string());
                td.codec_profile = Some(profile);
            }
            if let Some(c) = super::color_from_av1c(rec) {
                (td.color, td.color_source) = c;
            }
        }
        // A `vp08`/`vp09` SequenceStart body is a `VPCodecConfigurationRecord`,
        // the same bytes MP4 puts in a `vpcC` box. Reaching it is not optional
        // for VP9: the bitstream names no transfer and no primaries at all, so
        // this record is the only place such a track's colour exists, and
        // without it a BT.2020/PQ stream classifies SDR with nothing else able
        // to correct it.
        //
        // **The VP8 arm names its codec exactly**, rather than matching
        // `Other(_)`, because `parse_vpcc_record`'s only structural test is a
        // leading `0x01` — which is `configurationVersion` in an `avcC` and an
        // `hvcC` too. A wildcard here reads those records' constraint-flag
        // bytes as VP9 fields and fabricates a depth, a chroma format and a
        // full CICP colour description tagged `Container`, i.e. an HDR10
        // verdict invented out of an unrelated record.
        Codec::Vp9 => fill_from_vpcc(td, rec),
        Codec::Other(ref name) if name == VP8_LABEL => fill_from_vpcc(td, rec),
        _ => {}
    }
}

fn fill_from_vpcc(td: &mut TrackDemux, rec: &[u8]) {
    let Some(v) = super::parse_vpcc_record(rec) else { return };
    td.bit_depth = Some(v.bit_depth);
    td.chroma = Some(v.chroma.to_string());
    td.codec_profile = Some(v.profile_str);
    (td.color, td.color_source) = v.color;
}

/// Milliseconds of video, from the timestamps at both ends of the stream.
///
/// Used only when `onMetaData` states no duration, which is the live-capture
/// and still-recording case FLV is most often found in. The span is one frame
/// short by arithmetic rather than by approximation — frame `k` of `N` is
/// presented at `start + k/f`, so first-to-last is `(N-1)/f` while the stream
/// occupies `N/f` — so a known frame rate adds that interval back, the same
/// correction the program-stream backend makes for the same reason. FLV tag
/// timestamps are decode times, which are monotonic, so first and last are the
/// extremes without needing a min/max pass.
fn tail_duration(data: &[u8], data_start: usize, w: &HeadWalk) -> Option<f64> {
    let first = w.first_ts?;
    // A completed walk already has the last tag; otherwise follow the file's
    // final back-pointer to it.
    let last = if w.complete { w.last_ts? } else { last_tag_timestamp(data, data_start)? };
    let ms = last.checked_sub(first).filter(|m| *m > 0)?;
    let secs = ms as f64 / 1000.0;
    (secs > 0.0 && secs <= MAX_DURATION_SECS).then_some(secs)
}

/// The last tag's timestamp, reached by the file's final `PreviousTagSize`.
///
/// Validated rather than trusted: the reconstructed position must hold a tag
/// whose own `DataSize` agrees with the back-pointer, which is what stops a
/// file whose tail is padding from producing a plausible-looking number.
fn last_tag_timestamp(data: &[u8], data_start: usize) -> Option<i64> {
    let prev = u32be(data, data.len().checked_sub(BACK_POINTER_LEN)?) as usize;
    if prev < TAG_HEADER_LEN {
        return None;
    }
    let pos = data.len().checked_sub(BACK_POINTER_LEN)?.checked_sub(prev)?;
    if pos < data_start {
        return None;
    }
    let tag = read_tag(data, pos, data.len())?;
    // `PreviousTagSizeN = 11 + DataSize`, so the two must agree exactly.
    (tag.end - tag.body + TAG_HEADER_LEN == prev && tag.kind == TAG_VIDEO).then_some(tag.timestamp)
}

// --- the `--full` fused walk --------------------------------------------------

/// Walk every tag in the file, handing each video access unit to `on_au` as it
/// is completed and ticking `on_pos` with the walk position.
///
/// The `--full` counterpart of [`walk_head`]: `sample::scan_raw_full` drives it
/// so the index pass and the scan pass are one traversal, and it returns the
/// exact video payload byte count for the bitrate.
///
/// That count is `None` unless the tag chain reached the end of the file. A
/// walk that stopped on a tag declaring more bytes than the file holds measured
/// a prefix, and a prefix's byte count over the whole declared runtime is a
/// rate that is low by exactly the fraction missing.
pub fn walk_tags(
    data: &[u8],
    data_start: usize,
    mut on_pos: impl FnMut(usize),
    mut on_au: impl FnMut(Chunk),
) -> Option<u64> {
    let mut bytes = 0u64;
    let mut w = HeadWalk::default();
    let mut pos = data_start;
    while pos != data.len() {
        let tag = read_tag(data, pos, data.len())?;
        if tag.kind == TAG_VIDEO && !tag.encrypted {
            apply_video_tag(data, tag.body, tag.end, &mut w);
            for c in w.chunks.drain(..) {
                bytes += c.size;
                on_au(c);
            }
        }
        pos = tag.next;
        on_pos(pos);
    }
    Some(bytes)
}

// --- Enhanced FLV colour metadata ---------------------------------------------

/// A decoded `colorInfo` metadata packet.
#[derive(Debug, Default)]
struct ColorInfoPacket {
    primaries: Option<u16>,
    transfer: Option<u16>,
    matrix: Option<u16>,
    bit_depth: Option<u8>,
    mastering: Option<MasteringDisplay>,
    content_light: Option<ContentLight>,
}

impl ColorInfoPacket {
    fn is_empty(&self) -> bool {
        self.primaries.is_none()
            && self.transfer.is_none()
            && self.matrix.is_none()
            && self.bit_depth.is_none()
            && self.mastering.is_none()
            && self.content_light.is_none()
    }

    /// Fold into a track. Container-level signalling, so it wins over the
    /// coded stream's own VUI field by field — the same precedence an MP4
    /// `colr` box or a Matroska `Colour` element has, which is why it is
    /// applied after the configuration record rather than before.
    ///
    /// **There is no range field in `colorConfig`**, deliberately: the spec
    /// defines none, so the range keeps whatever the SPS said.
    fn apply(&self, td: &mut TrackDemux) {
        let (ci, cs) = super::color_from_cicp(
            self.primaries.unwrap_or(2),
            self.transfer.unwrap_or(2),
            self.matrix.unwrap_or(2),
            None,
            crate::model::ColorSource::Container,
        );
        // Each field is taken when the *packet declared it*, not when it
        // decoded to a name: a declared code this build has no label for must
        // still displace the stream's value, or the report would show the SPS's
        // label under a container that said something else. `color_from_cicp`
        // marks that case `ColorSource::UnnamedCode`, which `model::hidden`
        // keeps out of the report — so the value and its provenance stay
        // absent together, and the key sets still match.
        if self.primaries.is_some() {
            td.color.primaries = ci.primaries;
            td.color_source.primaries = cs.primaries;
        }
        if self.transfer.is_some() {
            td.color.transfer = ci.transfer;
            td.color_source.transfer = cs.transfer;
        }
        if self.matrix.is_some() {
            td.color.matrix = ci.matrix;
            td.color_source.matrix = cs.matrix;
        }
        // Depth is a plain value with no provenance channel, so it only fills a
        // gap the configuration record left — which for the codecs that have
        // one it never does.
        if td.bit_depth.is_none() {
            td.bit_depth = self.bit_depth;
        }
        if td.mastering.is_none() {
            td.mastering = self.mastering.clone();
        }
        if td.content_light.is_none() {
            td.content_light = self.content_light;
        }
    }
}

/// Decode a `VideoPacketType.Metadata` body: an AMF name followed by its value.
/// `colorInfo` is the only name the spec defines.
fn parse_color_info(data: &[u8], start: usize, end: usize) -> Option<ColorInfoPacket> {
    let mut r = AmfReader::new(data, start, end);
    // "A series of [name, value] pairs", of which `colorInfo` is the only one
    // the spec defines — so the packet is walked rather than assumed to open
    // with it, and a pair this build does not know is stepped over rather than
    // ending the read.
    let body = loop {
        let name = r.value(0)?;
        let value = r.value(0)?;
        if name.as_str() == Some("colorInfo") {
            break value;
        }
    };
    let mut out = ColorInfoPacket::default();
    if let Some(cfg) = body.object("colorConfig") {
        out.primaries = cfg.cicp("colorPrimaries");
        out.transfer = cfg.cicp("transferCharacteristics");
        out.matrix = cfg.cicp("matrixCoefficients");
        // "SHOULD be 8, 10 or 12". The only depth signal a `vvc1` Enhanced FLV
        // has, since this build carries no VVC configuration-record parser.
        out.bit_depth = cfg
            .number("bitDepth")
            .filter(|d| (8.0..=16.0).contains(d) && d.fract() == 0.0)
            .map(|d| d as u8);
    }
    if let Some(cll) = body.object("hdrCll") {
        // Both fields are cd/m², and both are optional.
        let max_cll = cll.number("maxCLL");
        let max_fall = cll.number("maxFall");
        if max_cll.is_some() || max_fall.is_some() {
            out.content_light = Some(ContentLight::new(
                clamp_u16(max_cll.unwrap_or(0.0)),
                clamp_u16(max_fall.unwrap_or(0.0)),
            ));
        }
    }
    if let Some(md) = body.object("hdrMdcv") {
        // **In nits, not ST.2086's 0.0001 cd/m² units.** The E-RTMP spec
        // departs from ST.2086 here and says so; the model stores nits, so
        // these go in unscaled while an ST.2086 SEI's are divided by 10000.
        let max = md.number("maxLuminance");
        let min = md.number("minLuminance");
        if let (Some(max), Some(min)) = (max, min) {
            if max.is_finite() && min.is_finite() && max > 0.0 && min >= 0.0 {
                out.mastering = Some(MasteringDisplay {
                    max_luminance: max,
                    min_luminance: min,
                    primaries: mastering_primaries(md),
                    primaries_level: None,
                });
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The `hdrMdcv` chromaticities, matched against the known gamuts by the same
/// decoder an MDCV box's coordinates go through, so a name derived here cannot
/// drift from one derived there. The spec's fields are plain CIE xy values,
/// which is what that matcher already takes — no unit conversion, unlike the
/// luminance pair beside them.
fn mastering_primaries(md: &AmfObject) -> Option<String> {
    crate::hdr::primaries_label(
        (md.number("redX")?, md.number("redY")?),
        (md.number("greenX")?, md.number("greenY")?),
        (md.number("blueX")?, md.number("blueY")?),
        (md.number("whitePointX")?, md.number("whitePointY")?),
    )
    .map(str::to_string)
}

fn clamp_u16(v: f64) -> u16 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    v.min(f64::from(u16::MAX)) as u16
}

// --- AMF0 ---------------------------------------------------------------------

/// An AMF0 value, reduced to what a report reads out of one.
#[derive(Debug)]
enum Amf {
    Num(f64),
    Str(String),
    Obj(AmfObject),
    /// Booleans, nulls, dates, arrays and the rest: consumed correctly so the
    /// reader stays in step, then discarded.
    Ignored,
}

/// An AMF object or ecma-array: an ordered property list. A `Vec` rather than a
/// map because these hold a dozen entries and are read a handful of times.
#[derive(Debug, Default)]
struct AmfObject(Vec<(String, Amf)>);

impl Amf {
    fn as_str(&self) -> Option<&str> {
        match self {
            Amf::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn object(&self, key: &str) -> Option<&AmfObject> {
        match self {
            Amf::Obj(o) => o.get(key).and_then(|v| match v {
                Amf::Obj(inner) => Some(inner),
                _ => None,
            }),
            _ => None,
        }
    }
}

impl AmfObject {
    fn get(&self, key: &str) -> Option<&Amf> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn number(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Amf::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// A CICP code point: an index into the ITU-T H.273 tables, which the
    /// E-RTMP spec names as the value space for all three colour fields. Read
    /// as an integer in range, never rounded from an arbitrary double.
    fn cicp(&self, key: &str) -> Option<u16> {
        let n = self.number(key)?;
        (n.is_finite() && n >= 0.0 && n <= f64::from(u16::MAX) && n.fract() == 0.0)
            .then_some(n as u16)
    }
}

/// A bounded AMF0 reader over one tag's payload.
///
/// Every declared length is checked against `end` before it is used, and the
/// node budget bounds what the whole parse may allocate regardless of how the
/// bytes are arranged. Any malformed structure aborts the parse rather than
/// resynchronising: an AMF stream has no framing to resynchronise *to*, so a
/// reader that guessed would report values it invented.
struct AmfReader<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
    budget: usize,
}

impl<'a> AmfReader<'a> {
    fn new(d: &'a [u8], pos: usize, end: usize) -> Self {
        AmfReader { d, pos, end: end.min(d.len()), budget: MAX_AMF_NODES }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let next = self.pos.checked_add(n)?;
        if next > self.end {
            return None;
        }
        let s = &self.d[self.pos..next];
        self.pos = next;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Option<f64> {
        self.take(8).map(|b| {
            f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        })
    }

    fn string(&mut self, long: bool) -> Option<String> {
        let n = if long { self.u32()? as usize } else { self.u16()? as usize };
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    /// One AMF0 value. `None` on any structure the reader cannot follow
    /// exactly, which ends the whole parse.
    fn value(&mut self, depth: u8) -> Option<Amf> {
        self.budget = self.budget.checked_sub(1)?;
        if depth > MAX_AMF_DEPTH {
            return None;
        }
        Some(match self.u8()? {
            0x00 => Amf::Num(self.f64()?),
            0x01 => {
                self.u8()?;
                Amf::Ignored
            }
            0x02 => Amf::Str(self.string(false)?),
            0x03 => Amf::Obj(self.property_list(depth)?),
            0x05 | 0x06 => Amf::Ignored, // null, undefined
            0x07 => {
                self.u16()?; // reference
                Amf::Ignored
            }
            0x08 => {
                // ecma-array. **The count is documented as approximate and is
                // read only to advance past it**; the property list below ends
                // on the terminator, as the spec requires.
                self.u32()?;
                Amf::Obj(self.property_list(depth)?)
            }
            0x0A => {
                // strict-array: the one count the spec *does* define exactly.
                // Still not used to size an allocation — nothing here is
                // preallocated from a declared number — and every element is
                // bounded by `end` and by the node budget.
                let n = self.u32()?;
                for _ in 0..n {
                    self.value(depth + 1)?;
                }
                Amf::Ignored
            }
            0x0B => {
                self.take(10)?; // date: a double plus a timezone offset
                Amf::Ignored
            }
            0x0C => Amf::Str(self.string(true)?),
            0x0F => {
                self.string(true)?; // xml-document
                Amf::Ignored
            }
            0x10 => {
                self.string(false)?; // typed-object's class name
                Amf::Obj(self.property_list(depth)?)
            }
            // 0x09 is the object terminator and never opens a value; 0x0D is
            // "unsupported"; 0x11 switches to AMF3, whose grammar this reader
            // does not implement. All three end the parse rather than letting
            // it drift.
            _ => return None,
        })
    }

    /// A property list, terminated by a zero-length key followed by the
    /// object-end marker.
    fn property_list(&mut self, depth: u8) -> Option<AmfObject> {
        let mut out = Vec::new();
        loop {
            let n = self.u16()? as usize;
            if n == 0 {
                return (self.u8()? == 0x09).then_some(AmfObject(out));
            }
            self.budget = self.budget.checked_sub(1)?;
            // The key is a bare length-prefixed string with no type marker.
            let key = String::from_utf8_lossy(self.take(n)?).into_owned();
            let v = self.value(depth + 1)?;
            out.push((key, v));
        }
    }
}

/// A script-data tag's payload: a name then its value. `onMetaData`'s value is
/// an ecma-array by the spec and an object in some real muxes, so both are
/// accepted; anything else (`onXMPData`, an encryption `|AdditionalHeader`) is
/// declined.
fn parse_script_tag(data: &[u8], body: usize, end: usize) -> Option<AmfObject> {
    let mut r = AmfReader::new(data, body, end);
    let name = r.value(0)?;
    if name.as_str()? != "onMetaData" {
        return None;
    }
    match r.value(0)? {
        Amf::Obj(o) => Some(o),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- synthetic file construction ---------------------------------------

    fn amf_str(s: &str) -> Vec<u8> {
        let mut v = vec![0x02];
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn amf_key(k: &str) -> Vec<u8> {
        let mut v = (k.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(k.as_bytes());
        v
    }

    fn amf_num(n: f64) -> Vec<u8> {
        let mut v = vec![0x00];
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    /// An ecma-array whose declared count is deliberately a lie, since that is
    /// what real writers do and what the parser must not believe.
    fn amf_ecma(props: &[(&str, Vec<u8>)], declared: u32) -> Vec<u8> {
        let mut v = vec![0x08];
        v.extend_from_slice(&declared.to_be_bytes());
        for (k, val) in props {
            v.extend_from_slice(&amf_key(k));
            v.extend_from_slice(val);
        }
        v.extend_from_slice(&[0x00, 0x00, 0x09]);
        v
    }

    fn amf_obj(props: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0x03];
        for (k, val) in props {
            v.extend_from_slice(&amf_key(k));
            v.extend_from_slice(val);
        }
        v.extend_from_slice(&[0x00, 0x00, 0x09]);
        v
    }

    fn tag(kind: u8, timestamp_ms: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&payload.len().to_be_bytes()[5..8]);
        v.extend_from_slice(&timestamp_ms.to_be_bytes()[1..4]);
        v.push((timestamp_ms >> 24) as u8);
        v.extend_from_slice(&[0, 0, 0]); // StreamID
        v.extend_from_slice(payload);
        let total = (TAG_HEADER_LEN + payload.len()) as u32;
        v.extend_from_slice(&total.to_be_bytes());
        v
    }

    fn flv_file(tags: &[Vec<u8>]) -> Vec<u8> {
        let mut v = b"FLV".to_vec();
        v.push(1);
        v.push(0x01); // video only
        v.extend_from_slice(&9u32.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
        for t in tags {
            v.extend_from_slice(t);
        }
        v
    }

    /// `testfiles/sdr/h264.flv`'s configuration record verbatim: an `avcC` for
    /// High profile 320x240.
    const AVCC: [u8; 46] = [
        0x01, 0x64, 0x00, 0x0D, 0xFF, 0xE1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x0D, 0xAC, 0xD9, 0x41,
        0x41, 0xFA, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xF1, 0x42,
        0x99, 0x60, 0x01, 0x00, 0x05, 0x68, 0xEB, 0xEC, 0xB2, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    fn legacy_avc_config() -> Vec<u8> {
        let mut p = vec![0x17, 0x00, 0x00, 0x00, 0x00]; // keyframe, AVC, seq header
        p.extend_from_slice(&AVCC);
        p
    }

    fn legacy_avc_frame(payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x27, 0x01, 0x00, 0x00, 0x00]; // inter, AVC, NALU
        p.extend_from_slice(payload);
        p
    }

    // --- tests --------------------------------------------------------------

    #[test]
    fn a_legacy_h264_flv_reports_the_picture_from_its_config_record() {
        let meta = {
            let mut p = amf_str("onMetaData");
            p.extend_from_slice(&amf_ecma(
                &[
                    ("duration", amf_num(2.08)),
                    ("width", amf_num(320.0)),
                    ("height", amf_num(240.0)),
                    ("framerate", amf_num(25.0)),
                    ("videocodecid", amf_num(7.0)),
                    ("filesize", amf_num(0.0)),
                ],
                // The corpus files' counts happen to be right; this one is a
                // lie, which the walk must survive.
                0,
            ));
            p
        };
        let f = flv_file(&[
            tag(TAG_SCRIPT, 0, &meta),
            tag(TAG_VIDEO, 0, &legacy_avc_config()),
            tag(TAG_VIDEO, 0, &legacy_avc_frame(&[0, 0, 0, 2, 0x41, 0x9A])),
            tag(TAG_VIDEO, 40, &legacy_avc_frame(&[0, 0, 0, 2, 0x41, 0x9A])),
        ]);
        let d = demux(&f, false).expect("demuxes");
        assert_eq!(d.container, CONTAINER_LABEL);
        assert_eq!(d.duration_secs, Some(2.08));
        let t = &d.tracks[0];
        assert_eq!(t.codec, Codec::Avc);
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.codec_profile.as_deref(), Some("High @ L1.3"));
        assert!(matches!(t.nal_format, NalFormat::LengthPrefixed(4)));
        // The configuration tag is not a chunk; the two coded frames are, and
        // each starts past its 5-byte video tag header.
        assert_eq!(t.chunks.len(), 2);
        assert_eq!(t.chunks[0].size, 6);
    }

    #[test]
    fn the_ecma_array_count_is_never_believed() {
        // A writer declaring zero over a populated array is common, and a
        // count-bounded parse would read nothing at all from it.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(7.5))], 0));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert_eq!(demux(&f, false).unwrap().duration_secs, Some(7.5));
        // And one declaring far more than it holds must not run past the
        // terminator into the next tag.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(7.5))], 5000));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert_eq!(demux(&f, false).unwrap().duration_secs, Some(7.5));
    }

    #[test]
    fn videodatarate_is_scaled_by_1024() {
        // `testfiles/sdr/flv_1080p.flv`'s declared value, which recovers the
        // encoder's round 8 Mbit/s target at x1024 and 7,812,500 at x1000.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[("duration", amf_num(30.08)), ("videodatarate", amf_num(7812.5))],
            2,
        ));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        let b = demux(&f, false).unwrap().tracks[0].bitrate.expect("a rate");
        assert_eq!(b.bits_per_sec, 8_000_000.0);
        assert_eq!(b.scope, crate::model::BitrateScope::VideoStream);
    }

    #[test]
    fn a_file_shorter_than_its_declared_size_reports_no_overall_rate() {
        // The live-capture and partial-download case: the header describes the
        // whole title, the bytes are a prefix. A whole-file rate over the
        // declared runtime would be low by exactly the fraction missing.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[("duration", amf_num(21.955)), ("filesize", amf_num(85_623_691.0))],
            2,
        ));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert!(demux(&f, false).unwrap().tracks[0].bitrate.is_none());
        // A declared per-stream rate is a header fact and still stands over the
        // same prefix.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[
                ("duration", amf_num(21.955)),
                ("videodatarate", amf_num(29296.875)),
                ("filesize", amf_num(85_623_691.0)),
            ],
            3,
        ));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        let b = demux(&f, false).unwrap().tracks[0].bitrate.expect("the declared rate");
        assert_eq!(b.bits_per_sec, 30_000_000.0);
    }

    #[test]
    fn a_complete_file_with_no_declared_rate_falls_back_to_the_overall_rate() {
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(2.0))], 1));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        let b = demux(&f, false).unwrap().tracks[0].bitrate.expect("a rate");
        assert_eq!(b.bits_per_sec, f.len() as f64 * 8.0 / 2.0);
        assert_eq!(b.scope, crate::model::BitrateScope::Overall);
    }

    #[test]
    fn the_extended_timestamp_byte_is_the_high_byte() {
        // Past 2^24 ms the extended byte carries the top bits. A file with no
        // declared duration falls back to the tag timestamps, which is the
        // only place the field is read.
        let f = flv_file(&[
            tag(TAG_VIDEO, 0, &legacy_avc_config()),
            tag(TAG_VIDEO, 16_777_216 + 784, &legacy_avc_frame(&[0, 0, 0, 1, 0x41])),
        ]);
        let d = demux(&f, false).unwrap();
        // 16777.216 + 0.784 seconds. Read as a plain u32 over bytes 4..8 the
        // same tag would time at 784 ms, and the duration would be 0.784.
        assert_eq!(d.duration_secs, Some(16778.0));
    }

    #[test]
    fn the_tag_type_is_masked_and_the_filter_bit_declines_the_payload() {
        // TagType 9 with the Filter bit set is 0x29. Unmasked it reads as type
        // 41 and the tag is skipped entirely; masked, the tag is a video tag
        // whose payload is encrypted and must not be parsed as codec bytes.
        let mut enc = tag(TAG_VIDEO, 0, &legacy_avc_config());
        enc[0] = 0x20 | TAG_VIDEO;
        let f = flv_file(&[enc, tag(TAG_VIDEO, 40, &legacy_avc_frame(&[0, 0, 0, 1, 0x41]))]);
        let d = demux(&f, false).unwrap();
        // The encrypted tag contributed no configuration record, so nothing was
        // read out of its payload — but the clear tag still identified the
        // codec, and the timestamps still bounded the duration.
        assert_eq!(d.tracks[0].codec_profile, None);
        assert_eq!(d.tracks[0].codec, Codec::Avc);
    }

    #[test]
    fn an_encrypted_tag_chain_falls_back_to_the_declared_codec_id() {
        // Every video tag encrypted, so no tag header could be read — but
        // `onMetaData` stays in the clear by the spec's own guarantee, and its
        // `videocodecid` is a real declaration rather than a placeholder.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[("duration", amf_num(2.0)), ("videocodecid", amf_num(7.0))],
            2,
        ));
        let mut enc = tag(TAG_VIDEO, 0, &legacy_avc_config());
        enc[0] = 0x20 | TAG_VIDEO;
        assert_eq!(demux(&flv_file(&[tag(TAG_SCRIPT, 0, &p), enc]), false).unwrap().tracks[0].codec,
            Codec::Avc);
        // The Enhanced convention writes the FourCC as a big-endian u32:
        // 1752589105 is `hvc1`, which is what the corpus file declares.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("videocodecid", amf_num(1_752_589_105.0))], 1));
        let mut enc = tag(TAG_VIDEO, 0, &legacy_avc_config());
        enc[0] = 0x20 | TAG_VIDEO;
        assert_eq!(demux(&flv_file(&[tag(TAG_SCRIPT, 0, &p), enc]), false).unwrap().tracks[0].codec,
            Codec::Hevc);
    }

    #[test]
    fn a_file_with_no_readable_video_tag_is_refused() {
        // An audio-only FLV, or one whose tag chain names no codec anywhere.
        // Reporting a placeholder track would be a fact the file does not
        // carry; every sibling backend refuses the same shape.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(2.0))], 1));
        assert!(demux(&flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(8, 0, &[0xAF, 0x00])]), false)
            .is_err());
    }

    #[test]
    fn an_unmapped_legacy_codec_keeps_its_name_and_is_not_indexed() {
        // CodecID 4 is On2 VP6: no parser here, no NAL structure to sample.
        let f = flv_file(&[tag(TAG_VIDEO, 0, &[0x14, 0x00, 0x11, 0x22])]);
        let d = demux(&f, false).unwrap();
        assert_eq!(d.tracks[0].codec, Codec::Other("On2 VP6".into()));
        assert!(d.tracks[0].chunks.is_empty());
    }

    // --- Enhanced FLV -------------------------------------------------------

    /// A complete Enhanced FLV video tag: the extended header byte, the
    /// FourCC, then whatever the packet type carries.
    fn ex_tag(frame_type: u8, packet_type: u8, fourcc: &[u8; 4], rest: &[u8]) -> Vec<u8> {
        let mut p = vec![EX_VIDEO_HEADER | (frame_type << 4) | packet_type];
        p.extend_from_slice(fourcc);
        p.extend_from_slice(rest);
        tag(TAG_VIDEO, 0, &p)
    }

    #[test]
    fn an_enhanced_tag_is_detected_before_the_codec_id_is_read() {
        // The first payload byte of `flv_enhanced_hdr.flv`'s first video tag is
        // 0x90: enhanced, keyframe, SequenceStart. Read as legacy it is
        // frameType 9 / CodecID 0, a codec that does not exist.
        let hvcc = [0x01u8, 0x02, 0x20, 0x00, 0x00, 0x00, 0x90, 0x00];
        let f = flv_file(&[ex_tag(1, PKT_SEQUENCE_START, b"hvc1", &hvcc)]);
        let d = demux(&f, false).unwrap();
        assert_eq!(d.tracks[0].codec, Codec::Hevc);
    }

    #[test]
    fn coded_frames_x_omits_the_composition_time() {
        // Packet type 1 carries a 3-byte CTS; type 3 does not. Both appear in
        // `flv_enhanced_hdr.flv`. Getting it wrong shifts the payload by three
        // bytes, which hands the NAL splitter the tail of the CTS field.
        let f = flv_file(&[
            ex_tag(1, PKT_CODED_FRAMES, b"hvc1", &[0, 0, 0, 0xAA, 0xBB, 0xCC, 0xDD]),
            ex_tag(1, PKT_CODED_FRAMES_X, b"hvc1", &[0xAA, 0xBB, 0xCC, 0xDD]),
        ]);
        let d = demux(&f, false).unwrap();
        let c = &d.tracks[0].chunks;
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].size, 4, "CodedFrames: 3 bytes of CTS skipped");
        assert_eq!(c[1].size, 4, "CodedFramesX: no CTS to skip");
        // Both point at the same four payload bytes — which is the assertion
        // that actually fails if the two offsets are computed alike, since a
        // three-byte shift keeps the *size* right on one of them.
        assert_eq!(&d_slice(&f, c[0]), &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&d_slice(&f, c[1]), &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn only_the_nal_codecs_carry_a_composition_time() {
        // The spec gates `compositionTimeOffset` on the FourCC. Skipping three
        // bytes for `av01` starts the access unit past the metadata OBUs that
        // open it — which is where the RPU, the HDR10+ T.35 and the CLL/MDCV
        // payloads live, so the whole dynamic report goes with them.
        let au = [0xAAu8, 0xBB, 0xCC, 0xDD];
        for (fourcc, expect_cts) in
            [(b"avc1", true), (b"hvc1", true), (b"vvc1", true), (b"av01", false), (b"vp09", false)]
        {
            let mut body = Vec::new();
            if expect_cts {
                body.extend_from_slice(&[0, 0, 0]); // the composition time
            }
            body.extend_from_slice(&au);
            let f = flv_file(&[ex_tag(1, PKT_CODED_FRAMES, fourcc, &body)]);
            let d = demux(&f, false).unwrap();
            let c = d.tracks[0].chunks[0];
            assert_eq!(d_slice(&f, c), au, "{}", String::from_utf8_lossy(fourcc));
        }
    }

    #[test]
    fn an_enhanced_header_reads_no_fourcc_past_the_end_of_its_tag() {
        // Every other read here is bounded by the tag. Bounding this one by the
        // buffer instead adopts the 4-byte back-pointer that follows as the
        // codec identity: a one-byte payload reported `0x0000000C`.
        let d = demux(&flv_file(&[tag(TAG_VIDEO, 0, &[EX_VIDEO_HEADER | (1 << 4)])]), false);
        // No video tag yielded a codec, so the file is refused rather than
        // reporting one invented from bytes outside the tag.
        assert!(d.is_err(), "got {:?}", d.map(|d| d.tracks[0].codec.clone()));
    }

    #[test]
    fn a_command_frame_carries_a_command_byte_where_a_fourcc_would_be() {
        // `videoFrameType == Command` outside a Metadata packet has no FourCC
        // at all. Reading one names the codec out of the command byte.
        let mut p = vec![EX_VIDEO_HEADER | (FRAME_TYPE_COMMAND << 4) | PKT_CODED_FRAMES_X];
        p.extend_from_slice(&[0x00, 0xAA, 0xBB, 0xCC, 0xDD]); // command, then bytes
        let d = demux(&flv_file(&[tag(TAG_VIDEO, 0, &p)]), false);
        assert!(d.is_err(), "got {:?}", d.map(|d| d.tracks[0].codec.clone()));
        // But frame type 5 *with* a Metadata packet is the ordinary case — it
        // is what the corpus's own `colorInfo` tag uses — and must still parse.
        let mut body = amf_str("colorInfo");
        body.extend_from_slice(&amf_obj(&[(
            "colorConfig",
            amf_obj(&[("matrixCoefficients", amf_num(9.0))]),
        )]));
        let f = flv_file(&[
            ex_tag(1, PKT_SEQUENCE_START, b"avc1", &AVCC),
            ex_tag(FRAME_TYPE_COMMAND, PKT_METADATA, b"avc1", &body),
        ]);
        assert_eq!(
            demux(&f, false).unwrap().tracks[0].color.matrix.as_deref(),
            Some("BT.2020 NCL")
        );
    }

    #[test]
    fn a_vp9_sequence_start_is_a_vpcc_record() {
        // VP9's bitstream names no transfer and no primaries at all, so this
        // record is the only place a `vp09` FLV's colour exists. Without it a
        // BT.2020/PQ stream classifies SDR and nothing can correct it.
        let vpcc = [
            0x01, 0x00, 0x00, 0x00, // version 1, flags
            0x02, 0x1F, // profile 2, level 31 (3.1)
            0xA0, // bitDepth 10, chroma 4:2:0, limited range
            0x09, 0x10, 0x09, // BT.2020, PQ, BT.2020 NCL
        ];
        let f = flv_file(&[ex_tag(1, PKT_SEQUENCE_START, b"vp09", &vpcc)]);
        let t = &demux(&f, false).unwrap().tracks[0];
        assert_eq!(t.codec, Codec::Vp9);
        assert_eq!(t.bit_depth, Some(10));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        assert_eq!(t.color.primaries.as_deref(), Some("BT.2020"));
        assert_eq!(t.color.transfer.as_deref(), Some("PQ (SMPTE ST 2084)"));
        assert_eq!(t.color.range.as_deref(), Some("limited"));
        assert_eq!(t.color_source.primaries, Some(crate::model::ColorSource::Container));
    }

    fn d_slice(data: &[u8], c: Chunk) -> Vec<u8> {
        data[c.offset as usize..(c.offset + c.size) as usize].to_vec()
    }

    #[test]
    fn a_mod_ex_block_does_not_move_the_fourcc() {
        // ModEx prefixes a sized block and re-states the packet type. Ignoring
        // it reads the FourCC out of the ModEx payload.
        let mut p = vec![EX_VIDEO_HEADER | (1 << 4) | PKT_MOD_EX];
        p.push(3); // modExDataSize - 1, so four bytes follow
        p.extend_from_slice(b"junk");
        p.push(PKT_CODED_FRAMES_X); // videoPacketModExType(4) | videoPacketType(4)
        p.extend_from_slice(b"hvc1");
        p.extend_from_slice(&[0xAA, 0xBB]);
        let d = demux(&flv_file(&[tag(TAG_VIDEO, 0, &p)]), false).unwrap();
        assert_eq!(d.tracks[0].codec, Codec::Hevc);
        assert_eq!(d.tracks[0].chunks.len(), 1);
        assert_eq!(d_slice(&flv_file(&[tag(TAG_VIDEO, 0, &p)]), d.tracks[0].chunks[0]), vec![
            0xAA, 0xBB
        ]);
    }

    #[test]
    fn a_two_byte_mod_ex_size_is_read_when_the_first_byte_says_255() {
        // `modExDataSize = u8 + 1`, and when that lands on 256 the real size is
        // a following `u16 + 1` instead.
        let mut p = vec![EX_VIDEO_HEADER | (1 << 4) | PKT_MOD_EX];
        p.push(255);
        p.extend_from_slice(&3u16.to_be_bytes()); // four bytes follow
        p.extend_from_slice(b"junk");
        p.push(PKT_CODED_FRAMES_X);
        p.extend_from_slice(b"av01");
        p.extend_from_slice(&[0xAA]);
        let d = demux(&flv_file(&[tag(TAG_VIDEO, 0, &p)]), false).unwrap();
        assert_eq!(d.tracks[0].codec, Codec::Av1);
        assert_eq!(d.tracks[0].chunks.len(), 1);
    }

    /// A `vp09` configuration record declaring BT.709 primaries/transfer/matrix
    /// over **full** range — a configuration whose every field a `colorInfo`
    /// packet can then be seen to displace or leave alone. Built on a real
    /// record rather than a stub because a stub that fails to parse leaves the
    /// track's colour empty, and a displacement test over an empty field
    /// asserts nothing.
    const VPCC_709_FULL: [u8; 10] =
        [0x01, 0x00, 0x00, 0x00, 0x02, 0x1F, 0xA1, 0x01, 0x01, 0x01];

    #[test]
    fn color_info_fills_the_colour_fields_as_container_provenance() {
        // What ffmpeg actually writes: a `colorInfo` carrying nothing but the
        // matrix. `testfiles/sdr/flv_enhanced_hdr.flv` is exactly this.
        let mut body = amf_str("colorInfo");
        body.extend_from_slice(&amf_obj(&[(
            "colorConfig",
            amf_obj(&[("matrixCoefficients", amf_num(9.0))]),
        )]));
        let f = flv_file(&[
            ex_tag(1, PKT_SEQUENCE_START, b"vp09", &VPCC_709_FULL),
            ex_tag(5, PKT_METADATA, b"vp09", &body),
        ]);
        let t = &demux(&f, false).unwrap().tracks[0];
        // The one field the packet named is replaced.
        assert_eq!(t.color.matrix.as_deref(), Some("BT.2020 NCL"));
        assert_eq!(t.color_source.matrix, Some(crate::model::ColorSource::Container));
        // The two it did not name keep what the configuration record said.
        assert_eq!(t.color.primaries.as_deref(), Some("BT.709"));
        assert_eq!(t.color.transfer.as_deref(), Some("BT.709"));
        // And the range survives, because **`colorConfig` has no range field**:
        // a packet that passed one through would report `limited` here, the
        // value `color_from_cicp` produces for a `Some(false)` it was never
        // given.
        assert_eq!(t.color.range.as_deref(), Some("full"));
    }

    #[test]
    fn hdr_mdcv_luminance_is_read_as_nits_not_as_st2086_units() {
        // **The invariant with no real-bytes fixture.** The E-RTMP spec states
        // these are cd/m² directly, departing from ST.2086's 0.0001 cd/m²
        // units. A 1000-nit display with a 0.0001-nit floor is written as
        // 1000 and 0.0001 here, where an ST.2086 SEI writes 10000000 and 1.
        // Dividing by 10000 would report a 0.1-nit display.
        let mut body = amf_str("colorInfo");
        body.extend_from_slice(&amf_obj(&[
            (
                "hdrCll",
                amf_obj(&[("maxCLL", amf_num(1000.0)), ("maxFall", amf_num(400.0))]),
            ),
            (
                "hdrMdcv",
                amf_obj(&[
                    ("redX", amf_num(0.708)),
                    ("redY", amf_num(0.292)),
                    ("greenX", amf_num(0.170)),
                    ("greenY", amf_num(0.797)),
                    ("blueX", amf_num(0.131)),
                    ("blueY", amf_num(0.046)),
                    ("whitePointX", amf_num(0.3127)),
                    ("whitePointY", amf_num(0.3290)),
                    ("maxLuminance", amf_num(1000.0)),
                    ("minLuminance", amf_num(0.0001)),
                ]),
            ),
        ]));
        let f = flv_file(&[
            ex_tag(1, PKT_SEQUENCE_START, b"hvc1", &[0x01, 0x02, 0x20]),
            ex_tag(5, PKT_METADATA, b"hvc1", &body),
        ]);
        let t = &demux(&f, false).unwrap().tracks[0];
        let md = t.mastering.as_ref().expect("a mastering display");
        assert_eq!(md.max_luminance, 1000.0);
        assert_eq!(md.min_luminance, 0.0001);
        assert_eq!(md.primaries.as_deref(), Some("BT.2020"));
        assert_eq!(md.primaries_level, None, "not a Dolby Vision level");
        let cl = t.content_light.expect("content light");
        assert_eq!((cl.max_cll, cl.max_fall), (1000, 400));
    }

    #[test]
    fn a_declared_colour_code_with_no_label_still_displaces_the_streams() {
        // CICP 23 is reserved: the container named a value this build cannot
        // label. Keeping the SPS's label there would show a colour the
        // container contradicted, so the field goes absent — and its
        // provenance goes absent with it, which is what `ColorSource::
        // UnnamedCode` exists to arrange.
        let mut body = amf_str("colorInfo");
        body.extend_from_slice(&amf_obj(&[(
            "colorConfig",
            amf_obj(&[("matrixCoefficients", amf_num(23.0))]),
        )]));
        let f = flv_file(&[
            ex_tag(1, PKT_SEQUENCE_START, b"vp09", &VPCC_709_FULL),
            ex_tag(5, PKT_METADATA, b"vp09", &body),
        ]);
        let t = &demux(&f, false).unwrap().tracks[0];
        // The configuration record really did set BT.709 here, so this is a
        // displacement rather than a fill: keeping the record's label would
        // report a colour the container contradicted.
        assert!(t.color.matrix.is_none(), "got {:?}", t.color.matrix);
        assert_eq!(t.color_source.matrix, Some(crate::model::ColorSource::UnnamedCode));
    }

    #[test]
    fn a_multitrack_packet_names_its_codec_and_indexes_nothing() {
        // The layout after the FourCC depends on the multitrack type, so
        // indexing one as an ordinary coded frame would hand the NAL splitter
        // a track id and a length field. Detect, name, index nothing.
        let mut p = vec![EX_VIDEO_HEADER | (1 << 4) | PKT_MULTITRACK];
        p.push(PKT_CODED_FRAMES_X); // videoMultitrackType(4) | videoPacketType(4)
        p.extend_from_slice(b"hvc1");
        p.extend_from_slice(&[0x00, 0xAA, 0xBB, 0xCC]);
        let d = demux(&flv_file(&[tag(TAG_VIDEO, 0, &p)]), false).unwrap();
        assert_eq!(d.tracks[0].codec, Codec::Hevc, "the first track still names the codec");
        assert!(d.tracks[0].chunks.is_empty(), "no blended access units");
    }

    #[test]
    fn a_metadata_packet_with_another_name_is_declined() {
        let mut body = amf_str("somethingElse");
        body.extend_from_slice(&amf_obj(&[("maxLuminance", amf_num(4000.0))]));
        let f = flv_file(&[
            ex_tag(1, PKT_SEQUENCE_START, b"hvc1", &[0x01, 0x02, 0x20]),
            ex_tag(5, PKT_METADATA, b"hvc1", &body),
        ]);
        let t = &demux(&f, false).unwrap().tracks[0];
        assert!(t.mastering.is_none());
    }

    // --- structure and hostile input ---------------------------------------

    #[test]
    fn a_zero_payload_video_tag_reads_no_codec_from_the_back_pointer() {
        // A `DataSize` of 0 puts the video header exactly on the following
        // 4-byte `PreviousTagSize`, and a *well-formed* empty video tag writes
        // `00 00 00 0B` there — so the codec reads as legacy id 0. The codec
        // gate is first-wins, so this one tag makes every later real video tag
        // inert, and `Codec::Other` has no sampler arm: the whole dynamic
        // report goes with it.
        let f = flv_file(&[
            tag(TAG_VIDEO, 0, &[]),
            tag(TAG_VIDEO, 0, &legacy_avc_config()),
            tag(TAG_VIDEO, 40, &legacy_avc_frame(&[0, 0, 0, 1, 0x41])),
        ]);
        let d = demux(&f, false).expect("demuxes");
        assert_eq!(d.tracks[0].codec, Codec::Avc, "the real tag must still name the codec");
        assert_eq!(d.tracks[0].codec_profile.as_deref(), Some("High @ L1.3"));
        assert_eq!(d.tracks[0].chunks.len(), 1);
        // Steered: with attacker-chosen back-pointer bytes the identity could
        // be set to anything. `07 ..` would read as AVC, `02 ..` as Sorenson.
        let mut steered = flv_file(&[tag(TAG_VIDEO, 0, &[])]);
        let n = steered.len();
        steered[n - 4] = 0x02;
        assert!(demux(&steered, false).is_err(), "no codec may come from outside the tag");
    }

    #[test]
    fn a_config_record_is_not_read_as_a_vpcc_just_because_it_starts_with_one() {
        // `parse_vpcc_record`'s only structural test is a leading `0x01`,
        // which is `configurationVersion` in an `avcC` and an `hvcC` too. An
        // unknown FourCC reaching that parser invents a depth, a chroma format
        // and a full CICP colour description tagged `container` — an HDR10
        // verdict out of an unrelated record.
        let mut rec = vec![0x01u8, 0x00, 0x00, 0x00, 0x02, 0x00, 0x83, 9, 16, 9];
        rec.extend_from_slice(&[0u8; 8]);
        let f = flv_file(&[ex_tag(1, PKT_SEQUENCE_START, b"abcd", &rec)]);
        let t = &demux(&f, false).unwrap().tracks[0];
        assert_eq!(t.codec, Codec::Other("abcd".into()));
        assert!(t.color.primaries.is_none(), "got {:?}", t.color);
        assert!(t.color.transfer.is_none());
        assert!(t.bit_depth.is_none());
        assert!(t.codec_profile.is_none());
        // `vp08` is the one non-`Codec` FourCC that really does carry this
        // record, and it must still be read.
        let f = flv_file(&[ex_tag(1, PKT_SEQUENCE_START, b"vp08", &rec)]);
        let t = &demux(&f, false).unwrap().tracks[0];
        assert_eq!(t.codec, Codec::Other(VP8_LABEL.into()));
        assert_eq!(t.color.transfer.as_deref(), Some("PQ (SMPTE ST 2084)"));
    }

    #[test]
    fn a_videodatarate_that_overflows_when_scaled_yields_no_rate() {
        // The factor was range-checked and the product was not, so a declared
        // rate above `f64::MAX / 1024` multiplied to infinity — which `serde`
        // writes as JSON `null`, breaking the schema's "always a float"
        // guarantee for `bits_per_sec`. The same shape as clamping two index
        // levels and letting their product escape.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[("duration", amf_num(2.0)), ("videodatarate", amf_num(1.8e305))],
            2,
        ));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        let b = demux(&f, false).unwrap().tracks[0].bitrate;
        // It falls through to the whole-container rate, which is finite.
        assert!(b.is_some_and(|b| b.bits_per_sec.is_finite()), "got {b:?}");
        assert_eq!(b.map(|b| b.scope), Some(crate::model::BitrateScope::Overall));
    }

    #[test]
    fn a_zero_length_tag_chain_terminates() {
        // Every tag costs 15 bytes even when empty, so the walk always
        // advances; this pins that a run of them ends rather than spinning.
        let mut f = flv_file(&[tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        for _ in 0..64 {
            f.extend_from_slice(&tag(0, 0, &[]));
        }
        let d = demux(&f, false).expect("demuxes");
        assert_eq!(d.tracks[0].codec, Codec::Avc);
    }

    #[test]
    fn a_tag_declaring_more_than_the_file_holds_stops_the_walk() {
        let mut f = flv_file(&[tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        // A header claiming 16 MiB of payload over nothing.
        f.extend_from_slice(&[TAG_VIDEO, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0]);
        let d = demux(&f, false).expect("demuxes");
        assert_eq!(d.tracks[0].codec_profile.as_deref(), Some("High @ L1.3"));
        // The walk did not reach the end, so no whole-file rate is offered.
        assert!(d.tracks[0].bitrate.is_none());
    }

    #[test]
    fn deeply_nested_amf_is_refused_rather_than_recursed() {
        // Three bytes per level, so a few KiB is thousands of stack frames.
        let mut inner = amf_num(1.0);
        for _ in 0..64 {
            inner = amf_obj(&[("a", inner)]);
        }
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(5.0)), ("deep", inner)], 2));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        // The parse aborts, so no metadata at all — never a partial object of
        // values read from bytes the reader had lost its place in.
        assert_eq!(demux(&f, false).unwrap().duration_secs, None);
    }

    #[test]
    fn an_amf_property_list_cannot_allocate_without_bound() {
        // Three bytes per property against a `String` each: the product is what
        // the node budget bounds. Just over the budget must be refused.
        let props: Vec<(String, Vec<u8>)> =
            (0..MAX_AMF_NODES).map(|_| (String::from("k"), vec![0x05])).collect();
        let mut arr = vec![0x08];
        arr.extend_from_slice(&0u32.to_be_bytes());
        for (k, v) in &props {
            arr.extend_from_slice(&amf_key(k));
            arr.extend_from_slice(v);
        }
        arr.extend_from_slice(&[0x00, 0x00, 0x09]);
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&arr);
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert!(demux(&f, false).unwrap().duration_secs.is_none());
    }

    #[test]
    fn an_unknown_amf_marker_ends_the_parse_rather_than_guessing() {
        let mut p = amf_str("onMetaData");
        let mut arr = vec![0x08];
        arr.extend_from_slice(&1u32.to_be_bytes());
        arr.extend_from_slice(&amf_key("x"));
        arr.push(0x11); // AMF3 switch, whose grammar this reader does not read
        arr.extend_from_slice(&amf_key("duration"));
        arr.extend_from_slice(&amf_num(9.0));
        arr.extend_from_slice(&[0x00, 0x00, 0x09]);
        p.extend_from_slice(&arr);
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert_eq!(demux(&f, false).unwrap().duration_secs, None);
    }

    #[test]
    fn a_strict_array_element_count_cannot_read_past_the_tag() {
        // The one count the spec defines exactly is still a declared number.
        let mut p = amf_str("onMetaData");
        let mut arr = vec![0x08];
        arr.extend_from_slice(&0u32.to_be_bytes());
        arr.extend_from_slice(&amf_key("times"));
        arr.push(0x0A);
        arr.extend_from_slice(&u32::MAX.to_be_bytes()); // four billion elements
        arr.extend_from_slice(&amf_num(1.0));
        arr.extend_from_slice(&[0x00, 0x00, 0x09]);
        p.extend_from_slice(&arr);
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        // Refused, and in bounded time — the elements run out at the tag's end.
        assert!(demux(&f, false).unwrap().duration_secs.is_none());
    }

    #[test]
    fn the_signature_probe_matches_ffmpegs() {
        assert!(is_flv(b"FLV\x01\x01\x00\x00\x00\x09"));
        assert!(!is_flv(b"FLV\x05\x01\x00\x00\x00\x09"), "version must be below 5");
        assert!(!is_flv(b"FLV\x01\x01\x01\x00\x00\x09"), "DataOffset's high byte must be 0");
        assert!(!is_flv(b"FLV\x01\x01\x00\x00\x00\x08"), "DataOffset must exceed 8");
        assert!(!is_flv(b"RIFF\x00\x00\x00\x00AVI "));
        assert!(demux(b"RIFF\x00\x00\x00\x00AVI ", false).is_err());
    }

    #[test]
    fn a_header_declaring_a_data_offset_past_the_file_is_refused() {
        let mut f = b"FLV".to_vec();
        f.push(1);
        f.push(0x01);
        f.extend_from_slice(&0x00FF_FFFFu32.to_be_bytes());
        assert!(demux(&f, false).is_err());
    }

    #[test]
    fn the_full_walk_sums_the_video_payload_and_visits_every_tag() {
        let f = flv_file(&[
            tag(TAG_SCRIPT, 0, &[0x05]),
            tag(TAG_VIDEO, 0, &legacy_avc_config()),
            tag(TAG_VIDEO, 0, &legacy_avc_frame(&[1, 2, 3, 4])),
            tag(TAG_VIDEO, 40, &legacy_avc_frame(&[5, 6, 7])),
        ]);
        let mut aus = Vec::new();
        let mut last = 0;
        let bytes = walk_tags(&f, 13, |p| last = p, |c| aus.push(c));
        assert_eq!(bytes, Some(7), "four payload bytes plus three");
        assert_eq!(aus.len(), 2);
        assert_eq!(last, f.len(), "the walk reached the end of the file");
    }

    #[test]
    fn the_full_walk_measures_nothing_over_a_broken_tag_chain() {
        // The `--full` sum is a measurement, and a measurement over a prefix
        // divided by the whole declared runtime is a wrong number rather than
        // a missing one. `testfiles/sdr/flv_4k_trunc.flv` reported 1.35 Mbit/s
        // as an exact video-stream rate against a declared 30 before this.
        let mut f = flv_file(&[
            tag(TAG_VIDEO, 0, &legacy_avc_config()),
            tag(TAG_VIDEO, 0, &legacy_avc_frame(&[1, 2, 3, 4])),
        ]);
        f.extend_from_slice(&[TAG_VIDEO, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(walk_tags(&f, 13, |_| {}, |_| {}), None);
    }

    #[test]
    fn a_truncated_file_keeps_its_declared_rate_under_full() {
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[
                ("duration", amf_num(21.955)),
                ("videodatarate", amf_num(29296.875)),
                ("filesize", amf_num(85_623_691.0)),
            ],
            3,
        ));
        let f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        // Demux must not hand the rate over to the walk here: the walk would
        // measure the prefix. The declared value describes the whole title.
        let b = demux(&f, true).unwrap().tracks[0].bitrate.expect("the declared rate");
        assert_eq!(b.bits_per_sec, 30_000_000.0);
    }

    #[test]
    fn a_whole_file_hands_its_rate_to_the_full_walk() {
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(&[("duration", amf_num(2.0))], 1));
        let mut f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        let size = f.len() + 9 + amf_num(0.0).len();
        // Rebuild with a `filesize` that matches, so the file is known whole.
        let mut p = amf_str("onMetaData");
        p.extend_from_slice(&amf_ecma(
            &[("duration", amf_num(2.0)), ("filesize", amf_num(size as f64))],
            2,
        ));
        f = flv_file(&[tag(TAG_SCRIPT, 0, &p), tag(TAG_VIDEO, 0, &legacy_avc_config())]);
        assert!(f.len() >= size - 16, "the fixture is close enough to its declared size");
        let d = demux(&f, true).unwrap();
        assert!(d.tracks[0].bitrate.is_none(), "left for the walk's exact sum");
        assert!(matches!(d.raw_stream, Some(RawFullStream::Flv { .. })));
    }
}
