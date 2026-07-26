//! ASF (Advanced Systems Format) — `.wmv`, `.asf`, and a `.wma` that carries
//! video.
//!
//! A flat tree of `GUID` + `u64 size` objects, all little-endian, walked from
//! byte 0. The Header Object declares its own size, sits first by definition,
//! and holds every field a report wants, so the whole parse is a bounded head
//! read: nothing descriptive lives at the tail. Like [`super::mpegv`] this
//! backend leaves `chunks` empty — see the "no payload index" note at the end
//! for why that is a design decision here rather than an omission.
//!
//! Five facts about the format are invariants a later change would otherwise
//! undo quietly. Each is pinned by a test.
//!
//! **GUIDs are mixed-endian and must be generated, never hand-typed.** The
//! first three fields are little-endian and the last eight bytes are verbatim,
//! so `75B22630-668E-11CF-A6D9-00AA0062CE6C` is stored as `30 26 B2 75 8E 66 CF
//! 11 A6 D9 00 AA 00 62 CE 6C`. Transcribing that by hand is how the reference
//! notes three GUIDs were mistyped while validating this section against real
//! files, each of which reported an object as absent from a file that plainly
//! contains it. [`guid`] does the byte shuffle at compile time from the
//! canonical field values, so every constant below reads as its published
//! string and cannot be transposed.
//!
//! **The object tree must be walked; the first occurrence of a GUID is the
//! wrong object.** Real files are multi-stream and the audio stream is
//! routinely first: in `testfiles/sdr/vc1_asf_fishes.wmv` the first
//! `Stream Properties Object` is the audio track, and reading video fields at
//! its offsets yields garbage. There is likewise **one Extended Stream
//! Properties Object per stream**, so its `Data Bitrate` and
//! `Average Time Per Frame` are correlated to a video track by `Stream Number`,
//! never taken from the first one found. Both objects are reached here by
//! descending the tree — top-level children of the Header Object, and the
//! Header Extension Object's own nested children — and matched on their stream
//! number.
//!
//! **Play Duration is offset by Preroll and the subtraction is not optional.**
//! Spec §3.2: "player software must subtract the value in the preroll field
//! from the play duration". Microsoft's own encoder writes a 5000 ms preroll on
//! both reference files and ffmpeg writes 3100 ms on a two-second clip, so
//! skipping it overstates that clip by 155% and a 12.5-second one by 40%.
//! The units differ between the two fields — 100-nanosecond ticks against
//! milliseconds — which is the other half of the trap.
//!
//! **`Maximum Bitrate` is not a bitrate this report may use.** It is a
//! whole-file ceiling including audio and packetisation, and the corpus files
//! declare 1153074 against a real 1045000. The genuine per-video-stream rate is
//! the Extended Stream Properties `Data Bitrate`, which the spec defines as the
//! leak rate of the data portion "excluding all ASF Data Packet overhead";
//! `Stream Bitrate Properties` is the fallback and is ~1% higher because the
//! spec says it *should* include that overhead (measured: 1054970 against
//! 1045000 on the same stream). MediaInfo reports the Data Bitrate, and so does
//! this.
//!
//! **ASF records no colour, and `biBitCount` is not a bit depth.** The video
//! media type has no colour field in any published extension, so every colour
//! value a report shows for an ASF came out of the codec's own extradata — the
//! VC-1 sequence header for `WVC1`, nothing at all for `WMV1`/`WMV2`/`WMV3`.
//! `biBitCount` is display bits per pixel (24 on both corpus files, over 4:2:0
//! 8-bit video) and [`super::bmih`] refuses to expose it.
//!
//! **No payload index, deliberately.** ASF Data Packets carry a
//! length-encoded payload header with multiple payloads per packet, and
//! payloads split across packets, so a video access unit is not a byte range in
//! the file the way an AVI chunk or an FLV tag is — indexing them means writing
//! a real ASF demuxer. Nothing in the report needs it: VC-1 publishes its
//! sequence header into codec-private data by every carriage spec that defines
//! one (which is why [`super::fill_vc1_stream_fields`] has no chunk fallback
//! anywhere), the Windows Media codecs signal nothing in band, and the bitrate
//! and duration are stated outright. So `chunks` stays empty and the sampler
//! never runs, exactly as `TrackDemux::chunks` documents.

use anyhow::{bail, Result};

use crate::model::Bitrate;

use super::{bmih, Codec, Demux, NalFormat, TrackDemux};

/// A 16-byte ASF GUID, built from the canonical string's own field values.
///
/// `Data1`/`Data2`/`Data3` are stored little-endian and the trailing eight
/// bytes verbatim — `struct.pack('<IHH', d1, d2, d3) + bytes[8:16]`. Doing the
/// shuffle here rather than in each literal is the whole point: see the module
/// doc's first invariant.
const fn guid(d1: u32, d2: u16, d3: u16, tail: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], tail[0], tail[1], tail[2], tail[3],
        tail[4], tail[5], tail[6], tail[7],
    ]
}

const HEADER_OBJECT: [u8; 16] =
    guid(0x75B2_2630, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const FILE_PROPERTIES: [u8; 16] =
    guid(0x8CAB_DCA1, 0xA947, 0x11CF, [0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const STREAM_PROPERTIES: [u8; 16] =
    guid(0xB7DC_0791, 0xA9B7, 0x11CF, [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const HEADER_EXTENSION: [u8; 16] =
    guid(0x5FBF_03B5, 0xA92E, 0x11CF, [0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const STREAM_BITRATE_PROPERTIES: [u8; 16] =
    guid(0x7BF8_75CE, 0x468D, 0x11D1, [0x8D, 0x82, 0x00, 0x60, 0x97, 0xC9, 0xA2, 0xB2]);
const EXTENDED_STREAM_PROPERTIES: [u8; 16] =
    guid(0x14E6_A5CB, 0xC672, 0x4332, [0x83, 0x99, 0xA9, 0x69, 0x52, 0x06, 0x5B, 0x5A]);
const VIDEO_MEDIA: [u8; 16] =
    guid(0xBC19_EFC0, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);

/// Every object header is a GUID plus its own size, and the size counts them.
const OBJECT_HEADER_LEN: u64 = 24;

/// Bound on objects read from one level of the tree. Sizes must be `>= 24` and
/// the walk advances by them, so a level is already bounded by its own byte
/// span; this only keeps a file that declares a multi-gigabyte header from
/// spending millions of iterations proving there is nothing in it. Real files
/// carry a handful per level (seven top-level children, seven nested).
const MAX_OBJECTS: usize = 4096;

/// Deepest nesting the object walk will follow.
///
/// The format has exactly one nesting level — the Header Extension Object's
/// children — so anything past the second is already non-conforming. The bound
/// exists because the walk is *recursive* and a Header Extension costs only 46
/// bytes: without it, a file nesting them is a stack frame per 46 bytes, and a
/// megabyte of them overflows the stack. That is an abort, not a catchable
/// panic, so it escapes both the `catch_unwind` guard and the tool's 0/1/2 exit
/// contract entirely.
const MAX_DEPTH: u8 = 4;

/// Bound on stream records read from a `Stream Bitrate Properties` object's
/// declared count. Stream numbers are 1..=127, so a conforming file cannot
/// exceed that; the count is a `u16` and is clamped against the object's own
/// remaining bytes as well.
const MAX_BITRATE_RECORDS: usize = 128;

/// Longest duration accepted from the header arithmetic. `Play Duration` is an
/// unvalidated `u64` of 100-nanosecond ticks, so a malformed file can compute
/// 58,000 years and a bitrate dividing by it then reports 0 b/s. Two days is
/// generous for a streaming container.
const MAX_DURATION_SECS: f64 = 48.0 * 3600.0;

/// How far the declared `File Size` may exceed the bytes actually present
/// before the file is treated as short. ffmpeg applies the same 5% test before
/// trusting `Play Duration`, and it is what catches a partial download: a
/// truncated file's header still declares the whole runtime, so a duration
/// taken from it is right about the title and wrong about the file.
const SIZE_TOLERANCE_DIVISOR: u64 = 20;

pub(crate) const CONTAINER_LABEL: &str = "ASF (Windows Media)";

pub fn demux(data: &[u8]) -> Result<Demux> {
    if !is_asf(data) {
        bail!("not an ASF file (no ASF_Header_Object GUID)");
    }
    // The Header Object's size counts its own 24-byte header plus the 6 bytes
    // of object count and reserved fields before the children.
    let declared = u64le(data, 16);
    if declared < 30 {
        bail!("ASF Header Object declares {declared} bytes, less than its own header");
    }
    // Reserved Field 2 "must be set to the value 0x02. If the value is not
    // 0x02, the software should fail to source the content" (spec §3.1). Both
    // corpus files write 1 and 2; the check costs nothing and is the spec's own
    // instruction.
    if data.get(29) != Some(&0x02) {
        bail!("ASF Header Object reserved field 2 is not 0x02");
    }
    let header_end = (declared as usize).min(data.len());

    let mut h = Header::default();
    walk(data, 30, header_end, 0, &mut h);

    let file_size_ok = h
        .file
        .as_ref()
        .is_some_and(|f| f.size_plausible(data.len() as u64));
    // ffmpeg trusts `Play Duration` only when the Broadcast flag is clear and
    // the declared file size matches reality; a live/broadcast header carries
    // no meaningful length at all, and a truncated download's is about the
    // title rather than the file.
    let duration_secs = h.file.as_ref().filter(|_| file_size_ok).and_then(FileProps::duration);

    let mut videos = h.video;
    // Report order is stream number, which is the file's own identifier for a
    // stream and is not necessarily header order — the corpus files list audio
    // (stream 1) before video (stream 2), and a mux with two video streams is
    // free to interleave them.
    videos.sort_by_key(|v| v.number);

    let mut tracks = Vec::new();
    for v in &videos {
        let Some(bh) = bmih::parse(&data[v.bmih..v.bmih_end]) else { continue };
        let extradata = &data[(v.bmih + bmih::HEADER_LEN).min(v.bmih_end)..v.bmih_end];
        let codec = bmih::codec_from_fourcc(&bh.compression)
            .unwrap_or_else(|| Codec::Other(fourcc_label(&bh.compression)));

        let mut td = TrackDemux {
            track_number: Some(v.number as u64),
            width: bh.width,
            height: bh.height,
            // The spec says the outer Encoded Image Width/Height "should be
            // equal" to the BITMAPINFOHEADER's, and ffmpeg's two demuxers
            // disagree about which to prefer. The header is what players show
            // and what MediaInfo reports, so it wins; the outer pair is kept
            // only to notice a disagreement in tests.
            ..TrackDemux::new(codec, NalFormat::AnnexB)
        };
        fill_from_extradata(&mut td, extradata);

        let esp = h.esp.iter().find(|e| e.number == v.number);
        if td.fps.is_none() {
            td.fps = esp.and_then(|e| e.fps());
        }
        td.bitrate = esp
            .and_then(|e| (e.data_bitrate > 0).then(|| f64::from(e.data_bitrate)))
            // `Stream Bitrate Properties` is a true per-stream average too, so
            // it keeps the `video_stream` label; it simply includes the packet
            // overhead the Data Bitrate excludes.
            .or_else(|| {
                h.sbp
                    .iter()
                    .find(|(n, br)| *n == v.number && *br > 0)
                    .map(|(_, br)| f64::from(*br))
            })
            .map(Bitrate::video_stream_bps)
            // Neither object is mandatory — an `ffmpeg`-written ASF has
            // neither — and then the only honest answer is the whole-container
            // rate, which counts audio and packet headers and is labelled
            // distinctly for it. Withheld over a short file for the reason the
            // AVI backend withholds it: the numerator would be the bytes
            // present and the denominator the whole declared runtime.
            .or_else(|| {
                file_size_ok.then(|| Bitrate::overall(data.len() as u64, duration_secs)).flatten()
            });
        // The families whose depth and chroma are format constants (WMV3's
        // ST 421 pair, the WMV1/WMV2 and MS-MPEG-4 witnessed constants),
        // filled only where the extradata read above left both absent. ASF
        // indexes no payload, so this is also the only depth/chroma source a
        // `WMV1`/`WMV2` track can ever have here.
        super::fill_constant_depth_chroma(&mut td);
        tracks.push(td);
    }

    if tracks.is_empty() {
        bail!("no video stream in the ASF header");
    }

    Ok(Demux {
        container: CONTAINER_LABEL,
        duration_secs,
        tracks,
        ts_stream: None,
        mkv_stream: None,
        raw_stream: None,
        bounded_index: false,
    })
}

/// The ASF_Header_Object GUID at byte 0 — 16 bytes of magic, which is why no
/// further structural probe is needed to identify the format.
pub(crate) fn is_asf(data: &[u8]) -> bool {
    data.len() >= 24 && data[..16] == HEADER_OBJECT
}

// --- object walking ----------------------------------------------------------

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

#[derive(Default)]
struct Header {
    file: Option<FileProps>,
    video: Vec<VideoStream>,
    esp: Vec<StreamRate>,
    /// `Stream Bitrate Properties` records: stream number and average bitrate.
    sbp: Vec<(u8, u32)>,
}

struct FileProps {
    declared_size: u64,
    /// `Play Duration`, in 100-nanosecond units.
    play_duration: u64,
    /// `Preroll`, in **milliseconds** — a different unit from the field above,
    /// which is the trap.
    preroll_ms: u64,
    broadcast: bool,
}

impl FileProps {
    /// The presentation length, preroll removed, or `None` when the header
    /// cannot state one: a broadcast header has no length, and the subtraction
    /// can legitimately underflow on a file whose preroll exceeds its content.
    fn duration(&self) -> Option<f64> {
        if self.broadcast {
            return None;
        }
        let ms = (self.play_duration / 10_000).checked_sub(self.preroll_ms)?;
        let secs = ms as f64 / 1000.0;
        (secs > 0.0 && secs <= MAX_DURATION_SECS).then_some(secs)
    }

    /// Whether the bytes present account for what the header declares. A file
    /// larger than declared is fine (trailing bytes hurt nothing); one
    /// materially smaller is a partial download.
    fn size_plausible(&self, actual: u64) -> bool {
        self.declared_size > 0
            && actual >= self.declared_size - self.declared_size / SIZE_TOLERANCE_DIVISOR
    }
}

/// A video `Stream Properties Object`: its stream number and the extent of the
/// `BITMAPINFOHEADER` inside its type-specific data.
struct VideoStream {
    number: u8,
    bmih: usize,
    bmih_end: usize,
}

/// One `Extended Stream Properties Object`, reduced to the two fields a video
/// track wants from it.
struct StreamRate {
    number: u8,
    data_bitrate: u32,
    /// `Average Time Per Frame`, in 100-nanosecond units; 0 means unknown.
    avg_time_per_frame: u64,
}

impl StreamRate {
    fn fps(&self) -> Option<f64> {
        if self.avg_time_per_frame == 0 {
            return None;
        }
        // `Average Time Per Frame` is 100-nanosecond ticks per frame, so a
        // value of 1 computes ten million frames per second and a value of
        // `2^63` computes a positive float that renders `0.000 fps`. Both ends
        // are the shared bound's business (`container::plausible_fps`).
        super::plausible_fps(10_000_000.0 / self.avg_time_per_frame as f64)
    }
}

/// Walk one level of the object tree, collecting what a video report needs.
///
/// Descends into the Header Extension Object, which is the only nesting the
/// format has and the only place `Extended Stream Properties` can appear.
fn walk(data: &[u8], mut pos: usize, end: usize, depth: u8, out: &mut Header) {
    if depth > MAX_DEPTH {
        return;
    }
    for _ in 0..MAX_OBJECTS {
        if pos + OBJECT_HEADER_LEN as usize > end {
            return;
        }
        let size = u64le(data, pos + 16);
        // A size below the object header would make the walk stand still, and
        // one past the parent's end is a lie about a region this level does not
        // own. Either way the level is unreadable from here on.
        if size < OBJECT_HEADER_LEN || size > (end - pos) as u64 {
            return;
        }
        let obj_end = pos + size as usize;
        match &data[pos..pos + 16] {
            g if *g == FILE_PROPERTIES => {
                if obj_end >= pos + 104 {
                    out.file = Some(FileProps {
                        declared_size: u64le(data, pos + 40),
                        play_duration: u64le(data, pos + 64),
                        preroll_ms: u64le(data, pos + 80),
                        broadcast: u32le(data, pos + 88) & 0x01 != 0,
                    });
                }
            }
            g if *g == STREAM_PROPERTIES => {
                if let Some(v) = parse_stream_properties(data, pos, obj_end) {
                    out.video.push(v);
                }
            }
            g if *g == STREAM_BITRATE_PROPERTIES => {
                parse_stream_bitrates(data, pos, obj_end, &mut out.sbp);
            }
            g if *g == EXTENDED_STREAM_PROPERTIES => {
                if obj_end >= pos + 84 {
                    out.esp.push(StreamRate {
                        number: (u16le(data, pos + 72) & 0x7F) as u8,
                        data_bitrate: u32le(data, pos + 40),
                        avg_time_per_frame: u64le(data, pos + 76),
                    });
                }
            }
            g if *g == HEADER_EXTENSION => {
                // 24 bytes of object header, a reserved GUID, a reserved u16,
                // then `Header Extension Data Size` bounding the children.
                let child_bytes = u32le(data, pos + 42) as usize;
                let child_start = pos + 46;
                if child_start <= obj_end {
                    let child_end = child_start.saturating_add(child_bytes).min(obj_end);
                    walk(data, child_start, child_end, depth + 1, out);
                }
            }
            _ => {}
        }
        pos = obj_end;
    }
}

/// A video `Stream Properties Object`, or `None` when it describes another
/// media type or is too short to hold a `BITMAPINFOHEADER`.
fn parse_stream_properties(data: &[u8], pos: usize, obj_end: usize) -> Option<VideoStream> {
    // 24 stream type GUID, 40 error correction GUID, 56 time offset,
    // 64 type-specific data length, 68 error correction data length,
    // 72 flags, 74 reserved, 78 type-specific data.
    if obj_end < pos + 78 || data.get(pos + 24..pos + 40)? != VIDEO_MEDIA {
        return None;
    }
    let tsd_len = u32le(data, pos + 64) as usize;
    let tsd_end = pos.checked_add(78)?.checked_add(tsd_len)?.min(obj_end);
    // Video type-specific data: 0 encoded width, 4 encoded height, 8 reserved
    // flags, 9 format data size, 11 the BITMAPINFOHEADER.
    let bmih = pos + 89;
    if bmih >= tsd_end {
        return None;
    }
    // `Format Data Size` is the declared length of the header plus its
    // extradata. Bounded by the type-specific data either way, so a lie in
    // either field can only shorten the slice.
    let format_data = u16le(data, pos + 87) as usize;
    let bmih_end = bmih.saturating_add(format_data).min(tsd_end);
    Some(VideoStream {
        // Bits 0-6 of the flags word; bit 15 marks encrypted *payload* data,
        // which never reaches this backend — the header, and so the extradata
        // parsed out of it, is always in the clear.
        number: (u16le(data, pos + 72) & 0x7F) as u8,
        bmih,
        bmih_end,
    })
}

/// `Stream Bitrate Properties`: a `u16` record count then that many
/// `u16 flags` + `u32 average bitrate` pairs.
fn parse_stream_bitrates(data: &[u8], pos: usize, obj_end: usize, out: &mut Vec<(u8, u32)>) {
    let declared = u16le(data, pos + 24) as usize;
    // Clamped against the bytes the object actually holds as well as the
    // format's own 127-stream ceiling: the count is a declared quantity and
    // this is the only thing standing between it and the read loop.
    let room = obj_end.saturating_sub(pos + 26) / 6;
    for i in 0..declared.min(room).min(MAX_BITRATE_RECORDS) {
        let at = pos + 26 + i * 6;
        out.push(((u16le(data, at) & 0x7F) as u8, u32le(data, at + 2)));
    }
}

/// Fill a track's codec-derived fields from the `BITMAPINFOHEADER` extradata.
///
/// This is the whole bitstream side of an ASF report: there is no chunk index
/// to fall back on, which costs nothing for the codecs ASF actually carries
/// (see the module doc's last invariant).
fn fill_from_extradata(td: &mut TrackDemux, extradata: &[u8]) {
    match td.codec {
        Codec::Avc | Codec::Hevc if bmih::is_config_record(extradata) => {
            super::fill_nal_config_fields(td, extradata)
        }
        // Part 2's VOS/VOL header set is copied into codec-private data by the
        // muxers that have it; the helper reads it there and has nothing else
        // to try here.
        Codec::Mpeg4Part2 => super::fill_mpeg4part2_stream_fields(td, extradata, &[]),
        // `WVC1` puts the Advanced Profile sequence header here, and `WMV3`
        // puts a STRUCT_C that carries only the profile. Note the extradata
        // does **not** begin at the start code: both Microsoft-authored corpus
        // files write one leading byte before `00 00 01 0F`, which is why the
        // parser locates the EBDU rather than reading from offset 0 — the same
        // thing ffmpeg's `vc1_decode_init` does.
        Codec::Vc1 => super::fill_vc1_stream_fields(td, extradata),
        _ => {}
    }
}

/// A FourCC as a codec label, falling back to hex for a non-printable one so a
/// report never emits control characters. Matches the AVI backend's rendering
/// of the same field.
fn fourcc_label(f: &[u8; 4]) -> String {
    if f.iter().all(|b| (0x20..=0x7E).contains(b)) && f.iter().any(|b| *b != b' ') {
        return String::from_utf8_lossy(f).trim_end().to_string();
    }
    format!("0x{:08X}", u32::from_le_bytes(*f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/vc1_asf_fishes.wmv`'s video extradata verbatim: one
    /// leading byte, the Advanced Profile sequence header EBDU, then the entry
    /// point EBDU.
    const WVC1_EXTRADATA: [u8; 31] = [
        0x25, 0x00, 0x00, 0x01, 0x0F, 0xCB, 0xA0, 0x0F, 0xF0, 0x8F, 0x8A, 0x0F, 0xF8, 0x23, 0xF1,
        0x80, 0x85, 0x08, 0x19, 0xFE, 0x3C, 0xFB, 0x9C, 0x00, 0x00, 0x01, 0x0E, 0x5A, 0x67, 0xF8,
        0x40,
    ];

    // --- synthetic file construction ---------------------------------------

    fn obj(id: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&((body.len() + 24) as u64).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn file_properties(size: u64, play: u64, preroll_ms: u64, flags: u32) -> Vec<u8> {
        let mut b = vec![0u8; 80]; // +24..+104 of the object
        b[16..24].copy_from_slice(&size.to_le_bytes()); // +40 File Size
        b[40..48].copy_from_slice(&play.to_le_bytes()); // +64 Play Duration
        b[56..64].copy_from_slice(&preroll_ms.to_le_bytes()); // +80 Preroll
        b[64..68].copy_from_slice(&flags.to_le_bytes()); // +88 Flags
        obj(FILE_PROPERTIES, &b)
    }

    fn bitmapinfoheader(fourcc: &[u8; 4], w: i32, h: i32, extradata: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[0..4].copy_from_slice(&((40 + extradata.len()) as u32).to_le_bytes());
        b[4..8].copy_from_slice(&w.to_le_bytes());
        b[8..12].copy_from_slice(&h.to_le_bytes());
        b[14..16].copy_from_slice(&24u16.to_le_bytes()); // biBitCount, never a depth
        b[16..20].copy_from_slice(fourcc);
        b.extend_from_slice(extradata);
        b
    }

    fn stream_properties(kind: [u8; 16], number: u8, format_data: &[u8]) -> Vec<u8> {
        // +24..+78 is two GUIDs, a time offset and four length/flag fields.
        let mut b = Vec::new();
        b.extend_from_slice(&kind); // +24 stream type
        b.extend_from_slice(&[0u8; 16]); // +40 error correction type
        b.extend_from_slice(&0u64.to_le_bytes()); // +56 time offset
        let tsd_len = 11 + format_data.len();
        b.extend_from_slice(&(tsd_len as u32).to_le_bytes()); // +64
        b.extend_from_slice(&0u32.to_le_bytes()); // +68
        b.extend_from_slice(&(number as u16).to_le_bytes()); // +72 flags
        b.extend_from_slice(&0u32.to_le_bytes()); // +74 reserved
        b.extend_from_slice(&320u32.to_le_bytes()); // +78 encoded width
        b.extend_from_slice(&240u32.to_le_bytes()); // +82 encoded height
        b.push(2); // +86 reserved flags
        b.extend_from_slice(&(format_data.len() as u16).to_le_bytes()); // +87
        b.extend_from_slice(format_data); // +89
        obj(STREAM_PROPERTIES, &b)
    }

    fn extended_stream_properties(number: u8, data_bitrate: u32, atpf: u64) -> Vec<u8> {
        let mut b = vec![0u8; 64]; // +24..+88
        b[16..20].copy_from_slice(&data_bitrate.to_le_bytes()); // +40
        b[48..50].copy_from_slice(&(number as u16).to_le_bytes()); // +72
        b[52..60].copy_from_slice(&atpf.to_le_bytes()); // +76
        obj(EXTENDED_STREAM_PROPERTIES, &b)
    }

    fn header_extension(children: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 16]); // +24 reserved GUID
        b.extend_from_slice(&6u16.to_le_bytes()); // +40 reserved
        b.extend_from_slice(&(children.len() as u32).to_le_bytes()); // +42
        b.extend_from_slice(children);
        obj(HEADER_EXTENSION, &b)
    }

    fn stream_bitrate_properties(records: &[(u8, u32)]) -> Vec<u8> {
        let mut b = (records.len() as u16).to_le_bytes().to_vec();
        for (n, br) in records {
            b.extend_from_slice(&(*n as u16).to_le_bytes());
            b.extend_from_slice(&br.to_le_bytes());
        }
        obj(STREAM_BITRATE_PROPERTIES, &b)
    }

    /// Assemble a Header Object over `children`, then pad the file out to
    /// `total_len` so the declared File Size can be satisfied.
    fn asf_file(children: &[u8], count: u32, total_len: usize) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&count.to_le_bytes()); // +24 header object count
        body.push(0x01); // +28 reserved 1
        body.push(0x02); // +29 reserved 2
        body.extend_from_slice(children);
        let mut f = obj(HEADER_OBJECT, &body);
        f.resize(total_len.max(f.len()), 0);
        f
    }

    /// The corpus's `vc1_asf_fishes.wmv` reduced to the objects this backend
    /// reads, with its real numbers: audio as stream 1 and video as stream 2,
    /// a 5000 ms preroll, and both bitrate objects populated.
    fn fishes_like() -> Vec<u8> {
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(1_661_574, 174_800_000, 5000, 0x2));
        children.extend_from_slice(&header_extension(
            &[
                extended_stream_properties(1, 96_024, 1_987_297),
                extended_stream_properties(2, 1_045_000, 400_000),
            ]
            .concat(),
        ));
        children.extend_from_slice(&stream_properties(
            guid(0xF869_9E40, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]),
            1,
            &[0u8; 36],
        ));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        children.extend_from_slice(&stream_bitrate_properties(&[(1, 98_104), (2, 1_054_970)]));
        asf_file(&children, 5, 1_661_574)
    }

    // --- tests --------------------------------------------------------------

    #[test]
    fn guids_are_generated_with_the_mixed_endian_byte_order() {
        // The published string `75B22630-668E-11CF-A6D9-00AA0062CE6C` stored
        // as ASF stores it. Hand-typing this is what the module doc warns
        // about; the constant is generated, and this pins the shuffle.
        assert_eq!(
            HEADER_OBJECT,
            [
                0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62,
                0xCE, 0x6C
            ]
        );
        // And the one whose Data1..Data3 are not all byte-symmetric, so a
        // transposition would show.
        assert_eq!(
            EXTENDED_STREAM_PROPERTIES,
            [
                0xCB, 0xA5, 0xE6, 0x14, 0x72, 0xC6, 0x32, 0x43, 0x83, 0x99, 0xA9, 0x69, 0x52, 0x06,
                0x5B, 0x5A
            ]
        );
    }

    #[test]
    fn a_multi_stream_file_reports_the_video_stream_not_the_first_one() {
        let d = demux(&fishes_like()).expect("demuxes");
        assert_eq!(d.container, CONTAINER_LABEL);
        assert_eq!(d.tracks.len(), 1, "one video track, the audio stream is not one");
        let t = &d.tracks[0];
        assert_eq!(t.track_number, Some(2), "the video stream's own number");
        assert_eq!((t.width, t.height), (512, 288));
        assert_eq!(t.codec, Codec::Vc1);
    }

    #[test]
    fn the_duration_has_preroll_subtracted() {
        // 17.480 s of Play Duration with a 5000 ms preroll is a 12.480 s
        // presentation — the value ffprobe and MediaInfo both report for the
        // real file. Without the subtraction it reads 17.480, 40% long.
        let d = demux(&fishes_like()).expect("demuxes");
        assert_eq!(d.duration_secs, Some(12.480));
    }

    #[test]
    fn the_bitrate_prefers_the_extended_stream_properties_rate() {
        let d = demux(&fishes_like()).expect("demuxes");
        let b = d.tracks[0].bitrate.expect("a rate");
        // The Data Bitrate for stream *2*, not stream 1's 96024, and not the
        // Stream Bitrate Properties' 1054970 (which includes packet overhead).
        assert_eq!(b.bits_per_sec, 1_045_000.0);
        assert_eq!(b.scope, crate::model::BitrateScope::VideoStream);
    }

    #[test]
    fn stream_bitrate_properties_is_the_fallback_when_there_is_no_esp() {
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(1_661_574, 174_800_000, 5000, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        children.extend_from_slice(&stream_bitrate_properties(&[(1, 98_104), (2, 1_054_970)]));
        let d = demux(&asf_file(&children, 3, 1_661_574)).expect("demuxes");
        let b = d.tracks[0].bitrate.expect("a rate");
        assert_eq!(b.bits_per_sec, 1_054_970.0);
        assert_eq!(b.scope, crate::model::BitrateScope::VideoStream);
    }

    #[test]
    fn a_minimal_asf_with_no_rate_objects_falls_back_to_the_overall_rate() {
        // `testfiles/sdr/wmv2.wmv`'s shape: one stream, no Extended Stream
        // Properties and no Stream Bitrate Properties at all.
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[0xC8, 0xC3, 0xB4, 0x80]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(99_935, 51_000_000, 3100, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let d = demux(&asf_file(&children, 2, 99_935)).expect("demuxes");
        assert_eq!(d.duration_secs, Some(2.0), "5.100 s of play less 3100 ms of preroll");
        let b = d.tracks[0].bitrate.expect("a rate");
        assert_eq!(b.bits_per_sec, 99_935.0 * 8.0 / 2.0);
        assert_eq!(b.scope, crate::model::BitrateScope::Overall);
        // An unmapped FourCC keeps its identity and the container's facts.
        assert_eq!(d.tracks[0].codec, Codec::Other("WMV2".into()));
        assert_eq!((d.tracks[0].width, d.tracks[0].height), (320, 240));
        // ASF records no colour and WMV2 signals none either.
        assert!(d.tracks[0].color.primaries.is_none());
        assert!(d.tracks[0].color.transfer.is_none());
        assert!(d.tracks[0].color.matrix.is_none());
    }

    /// A one-stream file whose codec signals no frame rate of its own, so the
    /// only thing that can produce one is `Average Time Per Frame`. Built with
    /// `WMV2` deliberately: the corpus's VC-1 sequence header states 25 fps,
    /// which is also what its `ATPF` computes, so a WVC1 fixture cannot tell
    /// the two sources apart and a test written over one asserts nothing.
    fn atpf_only(atpf: u64) -> Vec<u8> {
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[0xC8, 0xC3, 0xB4, 0x80]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&header_extension(&extended_stream_properties(1, 0, atpf)));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        asf_file(&children, 3, 4096)
    }

    #[test]
    fn the_frame_rate_comes_from_average_time_per_frame() {
        // 400000 ticks of 100 ns is 40 ms, i.e. 25 fps — what MediaInfo
        // reports for the real file (ffprobe's 50/1 is its own field-rate
        // doubling, and `avg_frame_rate` there is 0/0).
        assert_eq!(demux(&atpf_only(400_000)).unwrap().tracks[0].fps, Some(25.0));
    }

    #[test]
    fn an_absurd_average_time_per_frame_yields_no_rate() {
        // One 100-ns tick per frame is ten million fps: a misread field, not a
        // fast stream. Nothing else here states a rate, so the answer is none.
        assert_eq!(demux(&atpf_only(1)).unwrap().tracks[0].fps, None);
        // Zero is the field's own "unknown".
        assert_eq!(demux(&atpf_only(0)).unwrap().tracks[0].fps, None);
    }

    #[test]
    fn the_coded_streams_own_rate_outranks_average_time_per_frame() {
        // The AVI backend's ordering, for the same reason: a container's
        // declared rate is the muxer's word and the sequence header is the
        // bitstream's. The corpus VC-1 header states 25 fps; an ATPF claiming
        // 10 must not displace it.
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children
            .extend_from_slice(&header_extension(&extended_stream_properties(1, 0, 1_000_000)));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        assert_eq!(demux(&asf_file(&children, 3, 4096)).unwrap().tracks[0].fps, Some(25.0));
    }

    #[test]
    fn the_vc1_sequence_header_is_found_past_the_leading_extradata_byte() {
        let d = demux(&fishes_like()).expect("demuxes");
        let t = &d.tracks[0];
        // Only reachable by locating the `00 00 01 0F` EBDU at offset 1:
        // reading the extradata from offset 0 finds no start code at all.
        assert_eq!(t.codec_profile.as_deref(), Some("Advanced@L1"));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        // VC-1's spec defaults: BT.709 primaries and transfer over a BT.601
        // matrix, which is ASF's only colour signal of any kind.
        assert_eq!(t.color.primaries.as_deref(), Some("BT.709"));
        assert_eq!(t.color.matrix.as_deref(), Some("BT.601 (NTSC)"));
    }

    #[test]
    fn a_wmv3_stream_reports_its_struct_c_profile() {
        // `WMV3` is VC-1 Simple or Main, whose codec-private data is a 32-bit
        // STRUCT_C rather than a sequence header — so the EBDU search finds
        // nothing and the profile came back empty, although the project has had
        // the STRUCT_C parser since Phase 2 (reachable only from MP4's `dvc1`).
        // These four bytes are `vc1.rs`'s own known-good Main-profile record.
        let strf = bitmapinfoheader(b"WMV3", 320, 240, &[0x40, 0x01, 0x00, 0x01]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let t = &demux(&asf_file(&children, 2, 4096)).expect("demuxes").tracks[0];
        assert_eq!(t.codec, Codec::Vc1);
        assert_eq!(t.codec_profile.as_deref(), Some("Main"));
        // No level: STRUCT_C carries none, which is the shape SCHEMA.md
        // documents for these profiles. Depth is the format constant.
        assert_eq!(t.bit_depth, Some(8));
        // The fix is in the shared helper, so all three Video for Windows
        // carriages get it; four bytes that are *not* a STRUCT_C must still
        // yield nothing rather than a guess.
        let strf = bitmapinfoheader(b"WMV3", 320, 240, &[0x40, 0x00, 0x00, 0x01]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let t = &demux(&asf_file(&children, 2, 4096)).expect("demuxes").tracks[0];
        assert_eq!(t.codec_profile, None);
    }

    #[test]
    fn a_truncated_file_reports_neither_duration_nor_an_overall_rate() {
        // The header still declares the whole title's length; the bytes are
        // not there. A rate over them would be low by exactly the fraction
        // missing, which is the defect the AVI backend was fixed for.
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(99_935, 51_000_000, 3100, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let d = demux(&asf_file(&children, 2, 4096)).expect("demuxes");
        assert_eq!(d.duration_secs, None);
        assert!(d.tracks[0].bitrate.is_none());
    }

    #[test]
    fn a_broadcast_header_states_no_duration() {
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[]);
        let mut children = Vec::new();
        // Broadcast is flags bit 0. Live headers write a Play Duration that
        // describes nothing.
        children.extend_from_slice(&file_properties(4096, 51_000_000, 0, 0x1));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let d = demux(&asf_file(&children, 2, 4096)).expect("demuxes");
        assert_eq!(d.duration_secs, None);
    }

    #[test]
    fn a_preroll_longer_than_the_play_duration_underflows_to_no_duration() {
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[]);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 10_000_000, 5000, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let d = demux(&asf_file(&children, 2, 4096)).expect("demuxes");
        assert_eq!(d.duration_secs, None, "1000 ms of play less a 5000 ms preroll");
    }

    #[test]
    fn a_header_extension_child_cannot_reach_past_its_declared_data_size() {
        // The nested walk is bounded by `Header Extension Data Size`, not by
        // the enclosing object: a child beyond it belongs to nothing and must
        // not be read. Built by declaring a data size that stops short of the
        // second ESP.
        let first = extended_stream_properties(1, 111, 400_000);
        let second = extended_stream_properties(2, 222, 400_000);
        let mut kids = first.clone();
        kids.extend_from_slice(&second);
        let mut he = Vec::new();
        he.extend_from_slice(&[0u8; 16]);
        he.extend_from_slice(&6u16.to_le_bytes());
        he.extend_from_slice(&(first.len() as u32).to_le_bytes()); // only the first
        he.extend_from_slice(&kids);
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&obj(HEADER_EXTENSION, &he));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        let d = demux(&asf_file(&children, 3, 4096)).expect("demuxes");
        // Stream 2's ESP was out of bounds, so no rate for it was collected
        // and the overall fallback applies.
        assert_eq!(d.tracks[0].bitrate.map(|b| b.scope), Some(crate::model::BitrateScope::Overall));
    }

    #[test]
    fn nested_header_extensions_are_depth_bounded_rather_than_recursed() {
        // The walk descends into a Header Extension, and one costs 46 bytes,
        // so an unbounded walk is one stack frame per 46 bytes of input — a
        // stack *overflow*, which aborts the process outside the tool's 0/1/2
        // exit contract rather than raising a catchable panic. The format
        // nests exactly one level, so anything past a handful is hostile.
        let strf = bitmapinfoheader(b"WMV2", 320, 240, &[]);
        let mut nest = stream_properties(VIDEO_MEDIA, 1, &strf);
        for _ in 0..20_000 {
            nest = header_extension(&nest);
        }
        let mut children = file_properties(4096, 20_000_000, 0, 0x2);
        children.extend_from_slice(&nest);
        // The stream is buried past the depth bound, so it is never found and
        // the file is refused — in bounded stack, which is the point.
        assert!(demux(&asf_file(&children, 2, 4096)).is_err());
    }

    #[test]
    fn an_object_declaring_less_than_its_own_header_stops_the_walk() {
        // A size below the 24-byte object header cannot advance the walk, so
        // the level is unreadable from there on. Observed by putting a valid
        // object *after* the malformed one: reaching it would mean the walk
        // resynchronised, which nothing in the format lets it do.
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        children.extend_from_slice(&FILE_PROPERTIES);
        children.extend_from_slice(&2u64.to_le_bytes()); // a size of two bytes
        children.extend_from_slice(&stream_bitrate_properties(&[(2, 1_054_970)]));
        let d = demux(&asf_file(&children, 4, 4096)).expect("demuxes");
        // The Stream Bitrate Properties beyond the bad object was never read,
        // so the rate falls back to the whole-container one.
        assert_eq!(
            d.tracks[0].bitrate.map(|b| b.scope),
            Some(crate::model::BitrateScope::Overall)
        );
    }

    #[test]
    fn a_bitrate_record_count_beyond_the_objects_bytes_is_clamped() {
        // A declared count of 60000 over an object holding two records must
        // read two. Asserted on the *record count* rather than on the bitrate
        // that comes out: an unclamped loop still finds the two real records
        // among the 59,998 zeroed ones it invents, so a value assertion cannot
        // tell the two implementations apart.
        let mut b = 60_000u16.to_le_bytes().to_vec();
        for (n, br) in [(1u8, 98_104u32), (2, 1_054_970)] {
            b.extend_from_slice(&(n as u16).to_le_bytes());
            b.extend_from_slice(&br.to_le_bytes());
        }
        let o = obj(STREAM_BITRATE_PROPERTIES, &b);
        let mut out = Vec::new();
        parse_stream_bitrates(&o, 0, o.len(), &mut out);
        assert_eq!(out, vec![(1, 98_104), (2, 1_054_970)]);

        // And end to end, the right record still wins.
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        children.extend_from_slice(&o);
        let d = demux(&asf_file(&children, 3, 4096)).expect("demuxes");
        assert_eq!(d.tracks[0].bitrate.map(|b| b.bits_per_sec), Some(1_054_970.0));
    }

    #[test]
    fn two_video_streams_are_two_tracks_ordered_by_stream_number() {
        let strf = bitmapinfoheader(b"WVC1", 512, 288, &WVC1_EXTRADATA);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        // Written out of order on purpose.
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 3, &strf));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 2, &strf));
        let d = demux(&asf_file(&children, 3, 4096)).expect("demuxes");
        assert_eq!(
            d.tracks.iter().map(|t| t.track_number).collect::<Vec<_>>(),
            vec![Some(2), Some(3)]
        );
    }

    #[test]
    fn a_non_asf_head_and_a_bad_reserved_field_are_both_refused() {
        assert!(demux(&[]).is_err());
        assert!(demux(b"RIFF\0\0\0\0AVI ").is_err());
        assert!(!is_asf(b"RIFF\0\0\0\0AVI "));
        // The spec's own instruction: reserved field 2 must be 0x02.
        let mut f = fishes_like();
        f[29] = 0x03;
        assert!(demux(&f).is_err());
    }

    #[test]
    fn an_avc_config_record_in_the_extradata_is_read() {
        // ASF can carry H.264 in a Video for Windows wrapper. There is no
        // chunk index to fall back on, so the configuration record is the only
        // route — and it carries the whole picture description.
        let avcc: [u8; 34] = [
            0x01, 0x64, 0x00, 0x0D, 0xFF, 0xE1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x0D, 0xAC, 0xD9,
            0x41, 0x41, 0xFA, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20,
            0xF1, 0x42, 0x99, 0x60, 0x01, 0x00,
        ];
        let strf = bitmapinfoheader(b"H264", 320, 240, &avcc);
        let mut children = Vec::new();
        children.extend_from_slice(&file_properties(4096, 20_000_000, 0, 0x2));
        children.extend_from_slice(&stream_properties(VIDEO_MEDIA, 1, &strf));
        let d = demux(&asf_file(&children, 2, 4096)).expect("demuxes");
        let t = &d.tracks[0];
        assert_eq!(t.codec, Codec::Avc);
        assert!(matches!(t.nal_format, NalFormat::LengthPrefixed(4)));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.codec_profile.as_deref(), Some("High @ L1.3"));
    }
}
