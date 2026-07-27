//! RealMedia (`.rm`, `.rmvb`) — RealNetworks' container, read against
//! ffmpeg's `libavformat/rmdec.c` (`rm_read_header` and
//! `ff_rm_read_mdpr_codecdata`), which is the only complete public
//! description of the format; every field this backend reads was verified
//! against ffmpeg-muxed RV10/RV20 fixtures and a real-world RV40 `.rmvb`
//! (`testfiles/sdr/rv*.rm*`), cross-checked with MediaInfo and ffprobe.
//!
//! Structurally the ASF shape: a flat chain of big-endian chunks —
//! FourCC + `u32` size + `u16` version — walked from byte 0, with every
//! reported fact declared in the header chunks before `DATA`. So the parse
//! is a bounded head read, `chunks` stays **empty by design** (RealMedia
//! packets interleave and split payloads like ASF's, no video access unit is
//! a byte range, and RealVideo has no bitstream side channel this project
//! samples), and colour is honestly absent — nothing in the container or the
//! codec-private data names primaries, transfer, or matrix, and both
//! MediaInfo and ffprobe report none.
//!
//! The facts and where they live:
//!
//! - **`PROP`** declares the file: average bitrate, packet counts, the
//!   duration in milliseconds, preroll, and the stream count.
//! - **One `MDPR` per stream**: stream number, average bitrate, duration,
//!   description and mime strings (each a `u8` length + bytes), then a
//!   type-specific block. A video stream's block is `[u32 size]["VIDO"]`
//!   `[FourCC][width u16][height u16][2 bytes][4 bytes][fps u32]` — and the
//!   **fps field is 16.16 fixed point** (`fps / 65536`), not an integer or a
//!   pair: ffmpeg computes `av_reduce(..., 0x10000, fps, ...)`, and the
//!   real-world RV40 sample declares 1,571,294 = 23.976. An audio stream's
//!   block opens with `.ra\xfd`; a `MLTI` block wraps one or more nested
//!   codec-data blocks for multirate streams (rule table first, then
//!   `u32`-sized sub-blocks, each parsed like a bare type-specific block).
//! - **Duration**: the video `MDPR`'s own duration when it declares one,
//!   else `PROP`'s — the order ffmpeg applies (`s->duration` is discarded
//!   the moment a stream declares its own). On the RV40 sample the two
//!   differ by 7 ms and ffprobe reports the MDPR value this rule picks.
//! - **Bitrate**: the video `MDPR`'s average bitrate, muxer-declared, at
//!   `video_stream` scope — the value MediaInfo's Video `BitRate` and
//!   ffprobe's stream `bit_rate` both report. `PROP`'s average is the
//!   whole-file declaration (MediaInfo's `OverallBitRate`) and is not
//!   attributed to the video track.
//! - **Depth and chroma are family constants**: every RealVideo generation
//!   is 8-bit 4:2:0. For RV10/RV20 that is normative — they are H.263
//!   designs (§4.1's one pixel format), the same basis as the `H263` arm of
//!   `fill_constant_depth_chroma`. For RV30/RV40 it is a witnessed constant:
//!   the bitstreams are proprietary, but ffmpeg's decoders emit `yuv420p`
//!   alone and no other RealVideo pixel format has ever been observed
//!   (MediaInfo abstains, so the witness is single, and stated here).
//!   Unrecognized `VIDO` FourCCs (e.g. ClearVideo's `CLV1`) keep the honest
//!   FourCC label and no constants.
//! - **`declared_short`**: the first sizable `DATA` chunk declares its own
//!   extent, so a chunk running past EOF is a partial download — the
//!   AVI/ASF/FLV rule. Two escapes keep it honest: live-capture files
//!   legitimately write a zero/tiny `DATA` size (ffmpeg's own header guard
//!   exempts `DATA` from the minimum size), and **ffmpeg's muxer back-patches
//!   the `DATA` size exactly 10 bytes high on every complete file it writes**
//!   (measured on both encoded fixtures), so the comparison carries a small
//!   slack — a real cut misses by megabytes (the 2 MB RV40 sample declares
//!   734 MB), and without the slack every complete ffmpeg remux would read
//!   as a partial download. The slack's own blind spot is accepted: a file
//!   cut within 16 bytes of its declared end reads as complete.
//!
//! An `.rm` carrying only RealAudio errors honestly ("no video stream"), as
//! does the ancient headerless `.ra\xfd` format — both are common, and a
//! report with no video track would say less than the error does.

use anyhow::{bail, Result};

use crate::container::{plausible_fps, Codec, Demux, NalFormat, TrackDemux};
use crate::model::Bitrate;

pub(crate) const CONTAINER_LABEL: &str = "RealMedia";

/// Chunk-count budget for the header walk. Real files carry a handful of
/// header chunks (`PROP`, `CONT`, one `MDPR` per stream, `DATA`); the budget
/// only bounds a crafted chain of tiny chunks.
const MAX_HEADER_CHUNKS: usize = 256;

/// `.RMF` (and the rare `.RMP` streaming variant) at byte 0.
pub(crate) fn is_rm(data: &[u8]) -> bool {
    data.len() >= 8 && (&data[..4] == b".RMF" || &data[..4] == b".RMP")
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// One parsed video stream: the `MDPR` declarations plus the `VIDO` block.
struct VideoStream {
    stream_number: u16,
    avg_bitrate: u32,
    duration_ms: u32,
    fourcc: [u8; 4],
    width: u32,
    height: u32,
    fps: Option<f64>,
}

/// A decoded `VIDO` type-specific block.
struct VideoBlock {
    fourcc: [u8; 4],
    width: u32,
    height: u32,
    fps: Option<f64>,
}

/// ffmpeg's rm muxer back-patches the `DATA` chunk size exactly 10 bytes
/// past the file's true end on every complete file it writes (measured on
/// both encoded fixtures), so the truncation comparison tolerates this much
/// overshoot. A genuinely cut file misses by orders of magnitude more.
const DATA_DECLARED_SLACK: usize = 16;

/// Parse one codec-data block (the bytes after an `MDPR`'s size field, or a
/// `MLTI` sub-block): `Some` for a video (`VIDO`) block, `None` for audio
/// (`.ra\xfd`), logical-fileinfo, or anything unrecognized.
fn parse_video_block(tsd: &[u8]) -> Option<VideoBlock> {
    // Layout per ff_rm_read_mdpr_codecdata: [u32 v]["VIDO"][FourCC]
    // [w u16][h u16][2 bytes bps][4 bytes zero][fps u32 16.16].
    if tsd.len() < 26 || &tsd[4..8] != b"VIDO" {
        return None;
    }
    let fourcc: [u8; 4] = tsd[8..12].try_into().ok()?;
    let width = u32::from(be16(tsd, 12)?);
    let height = u32::from(be16(tsd, 14)?);
    // ffmpeg gates on fps > 0; the value is 16.16 fixed point.
    let fps32 = be32(tsd, 22)?;
    let fps = (fps32 > 0)
        .then(|| f64::from(fps32) / 65536.0)
        .and_then(plausible_fps);
    Some(VideoBlock { fourcc, width, height, fps })
}

/// The RealVideo generation names, as MediaInfo prints them. Anything else
/// falls back to the honest FourCC.
fn codec_label(fourcc: &[u8; 4]) -> Option<&'static str> {
    match fourcc {
        b"RV10" => Some("RealVideo 1"),
        b"RV20" => Some("RealVideo 2"),
        b"RV30" => Some("RealVideo 3"),
        b"RV40" => Some("RealVideo 4"),
        _ => None,
    }
}

pub fn demux(data: &[u8]) -> Result<Demux> {
    if data.len() >= 4 && &data[..4] == b".ra\xfd" {
        bail!("RealAudio stream (no video)");
    }
    if !is_rm(data) {
        bail!("not a RealMedia file: no .RMF header");
    }

    // The file header declares its own size; the chunk chain follows it.
    let header_size = be32(data, 4).unwrap_or(0) as usize;
    if header_size < 8 {
        bail!("RealMedia file header undersized");
    }
    let mut pos = header_size;

    let mut prop_duration_ms: Option<u32> = None;
    let mut videos: Vec<VideoStream> = Vec::new();
    let mut declared_short = false;

    for _ in 0..MAX_HEADER_CHUNKS {
        let Some(tag) = data.get(pos..pos + 4) else { break };
        let Some(size) = be32(data, pos + 4) else { break };
        let size = size as usize;
        let Some(ver) = be16(data, pos + 8) else { break };
        // ffmpeg's own header guards: every chunk but DATA declares at least
        // its 10 header bytes (live captures write DATA size 0), and the
        // object version is 0 or 2.
        if (size < 10 && tag != b"DATA") || (ver != 0 && ver != 2) {
            bail!("malformed RealMedia header chunk");
        }
        let body = pos + 10;
        match tag {
            b"PROP" => {
                // max/avg bitrate, max/avg packet size, packet count, then
                // the duration in ms.
                prop_duration_ms = be32(data, body + 20).filter(|d| *d > 0);
            }
            b"MDPR" => {
                let stream_number = be16(data, body).unwrap_or(0);
                // max bitrate, then the stream's declared average.
                let avg_bitrate = be32(data, body + 6).unwrap_or(0);
                // max/avg packet size, start time, preroll, then the
                // stream's own duration.
                let duration_ms = be32(data, body + 26).unwrap_or(0);
                // Two length-prefixed strings (description, mime), then the
                // type-specific block with its own u32 length.
                let mut p = body + 30;
                for _ in 0..2 {
                    let Some(&len) = data.get(p) else { break };
                    p += 1 + len as usize;
                }
                if let Some(tsd_size) = be32(data, p) {
                    let start = p + 4;
                    let end = start.saturating_add(tsd_size as usize).min(data.len());
                    let tsd = data.get(start..end).unwrap_or(&[]);
                    let mut blocks: Vec<&[u8]> = Vec::new();
                    if tsd.get(..4) == Some(b"MLTI") {
                        // Multirate wrapper: a rule->stream map, then a
                        // count of nested codec-data sub-blocks, each with
                        // its own u32 length (rm_read_multi).
                        if let Some(nrules) = be16(tsd, 4) {
                            let mut q = 6 + 2 * nrules as usize;
                            let n_sub = be16(tsd, q).unwrap_or(0);
                            q += 2;
                            for _ in 0..n_sub.min(64) {
                                let Some(sub_size) = be32(tsd, q) else { break };
                                let s = q + 4;
                                let e = s.saturating_add(sub_size as usize).min(tsd.len());
                                if s >= e {
                                    break;
                                }
                                blocks.push(&tsd[s..e]);
                                q = e;
                            }
                        }
                    } else {
                        blocks.push(tsd);
                    }
                    for block in blocks {
                        if let Some(vb) = parse_video_block(block) {
                            videos.push(VideoStream {
                                stream_number,
                                avg_bitrate,
                                duration_ms,
                                fourcc: vb.fourcc,
                                width: vb.width,
                                height: vb.height,
                                fps: vb.fps,
                            });
                        }
                    }
                }
            }
            b"DATA" => {
                // The chunk declares its own extent; a declaration the file
                // cannot hold is a partial download. Zero/tiny sizes are the
                // live-capture convention and say nothing, and complete
                // ffmpeg muxes systematically declare 10 bytes past EOF
                // (`DATA_DECLARED_SLACK`).
                if size >= 10
                    && pos.saturating_add(size) > data.len() + DATA_DECLARED_SLACK
                {
                    declared_short = true;
                }
                break;
            }
            _ => {}
        }
        // Every chunk advances by its declared size (`CONT` and unknown tags
        // are skipped whole; `PROP`/`MDPR` reads above never pass `size`).
        pos = match pos.checked_add(size.max(10)) {
            Some(next) if next > pos => next,
            _ => break,
        };
    }

    if videos.is_empty() {
        bail!("no video stream (RealAudio-only or unrecognized streams)");
    }

    // The video stream's own duration wins; PROP's whole-file declaration is
    // the fallback (ffmpeg discards it as soon as a stream declares one).
    let duration_secs = videos
        .iter()
        .map(|v| v.duration_ms)
        .max()
        .filter(|d| *d > 0)
        .or(prop_duration_ms)
        .map(|ms| f64::from(ms) / 1000.0);

    let mut tracks = videos
        .into_iter()
        .map(|v| {
            let (codec, constants) = match codec_label(&v.fourcc) {
                Some(name) => (Codec::Other(name.to_string()), true),
                None => (
                    Codec::Other(String::from_utf8_lossy(&v.fourcc).into_owned()),
                    false,
                ),
            };
            TrackDemux {
                track_number: Some(u64::from(v.stream_number)),
                codec_id: Some(super::bmih::fourcc_label(&v.fourcc)),
                width: v.width,
                height: v.height,
                fps: v.fps,
                // The RealVideo family constants (module doc); an unknown
                // FourCC states nothing.
                bit_depth: constants.then_some(8),
                chroma: constants.then(|| "4:2:0".to_string()),
                bitrate: (v.avg_bitrate > 0)
                    .then(|| Bitrate::video_stream_bps(f64::from(v.avg_bitrate))),
                // Placeholder (the mpegv convention): no chunk index exists
                // and the sampler never runs.
                ..TrackDemux::new(codec, NalFormat::AnnexB)
            }
        })
        .collect::<Vec<_>>();

    let mut d = Demux::single(CONTAINER_LABEL, duration_secs, tracks.remove(0));
    d.tracks.extend(tracks);
    d.declared_short = declared_short;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BitrateScope;

    fn chunk(tag: &[u8; 4], ver: u16, body: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(tag);
        c.extend_from_slice(&(body.len() as u32 + 10).to_be_bytes());
        c.extend_from_slice(&ver.to_be_bytes());
        c.extend_from_slice(body);
        c
    }

    fn prop(duration_ms: u32) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [800_000u32, 700_000, 4000, 3000, 100] {
            b.extend_from_slice(&v.to_be_bytes()); // rates, packet sizes, count
        }
        b.extend_from_slice(&duration_ms.to_be_bytes());
        b.extend_from_slice(&[0u8; 4]); // preroll
        b.extend_from_slice(&[0u8; 4]); // index offset (ver 0)
        b.extend_from_slice(&[0u8; 4]); // data offset
        b.extend_from_slice(&2u16.to_be_bytes()); // stream count
        b.extend_from_slice(&0u16.to_be_bytes()); // flags
        chunk(b"PROP", 0, &b)
    }

    fn vido_block(fourcc: &[u8; 4], w: u16, h: u16, fps32: u32) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&34u32.to_be_bytes()); // block's own size field
        t.extend_from_slice(b"VIDO");
        t.extend_from_slice(fourcc);
        t.extend_from_slice(&w.to_be_bytes());
        t.extend_from_slice(&h.to_be_bytes());
        t.extend_from_slice(&[0u8; 2]); // bits per sample
        t.extend_from_slice(&[0u8; 4]); // always zero
        t.extend_from_slice(&fps32.to_be_bytes());
        t.extend_from_slice(&[0u8; 8]); // extradata
        t
    }

    fn mdpr(stream: u16, avg_br: u32, duration_ms: u32, tsd: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&stream.to_be_bytes());
        b.extend_from_slice(&(avg_br + 100_000).to_be_bytes()); // max bitrate
        b.extend_from_slice(&avg_br.to_be_bytes());
        b.extend_from_slice(&4000u32.to_be_bytes()); // max packet
        b.extend_from_slice(&3000u32.to_be_bytes()); // avg packet
        b.extend_from_slice(&0u32.to_be_bytes()); // start time
        b.extend_from_slice(&0u32.to_be_bytes()); // preroll
        b.extend_from_slice(&duration_ms.to_be_bytes());
        b.extend_from_slice(&[5, b'V', b'i', b'd', b'e', b'o']); // desc
        b.extend_from_slice(&[10, b'v', b'i', b'd', b'e', b'o', b'/', b'x', b'-', b'p', b'n']); // mime
        b.extend_from_slice(&(tsd.len() as u32).to_be_bytes());
        b.extend_from_slice(tsd);
        chunk(b"MDPR", 0, &b)
    }

    fn ra_tsd() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b".ra\xfd");
        t.extend_from_slice(&[0u8; 30]);
        t
    }

    fn rmf(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b".RMF");
        f.extend_from_slice(&18u32.to_be_bytes()); // file header size
        f.extend_from_slice(&[0u8; 10]); // version + rest of the file header
        for c in chunks {
            f.extend_from_slice(c);
        }
        f
    }

    /// The real-world RV40 sample's declared rate: 1,571,294 / 65536 = 23.976.
    const FPS_23976: u32 = 1_571_294;

    #[test]
    fn a_video_mdpr_reports_the_declared_stream() {
        let f = rmf(&[
            prop(7_286_044),
            mdpr(0, 192_000, 0, &ra_tsd()),
            mdpr(1, 700_986, 7_286_037, &vido_block(b"RV40", 576, 320, FPS_23976)),
            chunk(b"DATA", 0, &[0u8; 8]),
        ]);
        let d = demux(&f).expect("rm");
        assert_eq!(d.container, CONTAINER_LABEL);
        assert_eq!(d.tracks.len(), 1, "the audio MDPR is not a track");
        let t = &d.tracks[0];
        assert!(matches!(&t.codec, Codec::Other(l) if l == "RealVideo 4"));
        assert_eq!((t.width, t.height), (576, 320));
        assert!((t.fps.unwrap() - 23.976).abs() < 1e-3, "16.16 fixed point");
        assert_eq!(t.track_number, Some(1));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        let b = t.bitrate.unwrap();
        assert_eq!(b.scope, BitrateScope::VideoStream);
        assert!((b.bits_per_sec - 700_986.0).abs() < 0.5);
        // The stream's own duration wins over PROP's (they differ by 7 ms in
        // the real sample; ffprobe reports the MDPR value).
        assert!((d.duration_secs.unwrap() - 7286.037).abs() < 1e-9);
        assert!(t.chunks.is_empty(), "no payload index by design");
        assert!(!d.declared_short);
    }

    #[test]
    fn prop_duration_is_the_fallback_when_the_stream_declares_none() {
        let f = rmf(&[
            prop(2_000),
            mdpr(0, 200_000, 0, &vido_block(b"RV20", 320, 240, 25 << 16)),
            chunk(b"DATA", 0, &[]),
        ]);
        let d = demux(&f).unwrap();
        assert_eq!(d.duration_secs, Some(2.0));
        assert_eq!(d.tracks[0].fps, Some(25.0));
        assert!(matches!(&d.tracks[0].codec, Codec::Other(l) if l == "RealVideo 2"));
    }

    #[test]
    fn an_unknown_vido_fourcc_keeps_the_honest_label_and_no_constants() {
        let f = rmf(&[
            prop(1_000),
            mdpr(0, 100_000, 0, &vido_block(b"CLV1", 320, 240, 25 << 16)),
            chunk(b"DATA", 0, &[]),
        ]);
        let t = &demux(&f).unwrap().tracks[0];
        assert!(matches!(&t.codec, Codec::Other(l) if l == "CLV1"));
        assert_eq!(t.bit_depth, None, "constants are for the known family only");
        assert_eq!(t.chroma, None);
    }

    #[test]
    fn a_multirate_mlti_block_is_unwrapped() {
        // rule map (2 rules -> stream 0), then one nested video sub-block.
        let sub = vido_block(b"RV30", 640, 480, 30 << 16);
        let mut tsd = Vec::new();
        tsd.extend_from_slice(b"MLTI");
        tsd.extend_from_slice(&2u16.to_be_bytes());
        tsd.extend_from_slice(&[0, 0, 0, 0]); // rule -> stream map
        tsd.extend_from_slice(&1u16.to_be_bytes());
        tsd.extend_from_slice(&(sub.len() as u32).to_be_bytes());
        tsd.extend_from_slice(&sub);
        let f = rmf(&[prop(1_000), mdpr(0, 100_000, 0, &tsd), chunk(b"DATA", 0, &[])]);
        let d = demux(&f).unwrap();
        assert_eq!(d.tracks.len(), 1);
        assert!(matches!(&d.tracks[0].codec, Codec::Other(l) if l == "RealVideo 3"));
        assert_eq!((d.tracks[0].width, d.tracks[0].height), (640, 480));
    }

    #[test]
    fn audio_only_and_non_rm_inputs_error_honestly() {
        // RealAudio-only .rm: streams parse, none is video.
        let f = rmf(&[prop(1_000), mdpr(0, 64_000, 0, &ra_tsd()), chunk(b"DATA", 0, &[])]);
        let e = demux(&f).unwrap_err().to_string();
        assert!(e.contains("no video stream"), "{e}");
        // The ancient headerless RealAudio format.
        assert!(demux(b".ra\xfd\x00\x04more").unwrap_err().to_string().contains("RealAudio"));
        // Not RealMedia at all.
        assert!(demux(b"\x1A\x45\xDF\xA3....").is_err());
        assert!(demux(&[]).is_err());
    }

    #[test]
    fn ffmpegs_own_header_guards_are_kept() {
        // A non-DATA chunk declaring less than its own header is malformed.
        let mut f = rmf(&[prop(1_000)]);
        let bad = chunk(b"CONT", 0, &[]);
        f.extend_from_slice(&bad[..4]);
        f.extend_from_slice(&5u32.to_be_bytes()); // size 5 < 10
        f.extend_from_slice(&0u16.to_be_bytes());
        assert!(demux(&f).is_err());
        // An object version other than 0 or 2 is malformed.
        let f2 = rmf(&[chunk(b"PROP", 1, &[0u8; 40])]);
        assert!(demux(&f2).is_err());
    }

    #[test]
    fn a_data_declaration_past_eof_flags_the_file_short() {
        // The real-world shape: a 2 MB cut of a movie whose DATA chunk
        // declares the whole payload (the rv40_spygames.rmvb fixture).
        let mut f = rmf(&[
            prop(7_286_044),
            mdpr(1, 700_986, 7_286_037, &vido_block(b"RV40", 576, 320, FPS_23976)),
        ]);
        f.extend_from_slice(b"DATA");
        f.extend_from_slice(&50_000_000u32.to_be_bytes());
        f.extend_from_slice(&0u16.to_be_bytes());
        f.extend_from_slice(&[0u8; 64]);
        let d = demux(&f).unwrap();
        assert!(d.declared_short);
        // Declared header facts stand — the duration and rate were stated,
        // not measured from the bytes present.
        assert!(d.duration_secs.is_some());
        assert!(d.tracks[0].bitrate.is_some());

        // A zero-size DATA (the live-capture convention) says nothing.
        let f2 = rmf(&[
            prop(1_000),
            mdpr(0, 100_000, 0, &vido_block(b"RV10", 320, 240, 25 << 16)),
            chunk(b"DATA", 0, &[]),
        ]);
        assert!(!demux(&f2).unwrap().declared_short);

        // ffmpeg's muxer declares exactly 10 bytes past EOF on complete
        // files (measured on rv10.rm and rv20.rm): inside the slack, so a
        // complete remux never reads as a partial download.
        let mut f3 = rmf(&[
            prop(1_000),
            mdpr(0, 100_000, 0, &vido_block(b"RV10", 320, 240, 25 << 16)),
        ]);
        let body_len = 64u32;
        f3.extend_from_slice(b"DATA");
        f3.extend_from_slice(&(body_len + 10 + 10).to_be_bytes()); // +10 quirk
        f3.extend_from_slice(&0u16.to_be_bytes());
        f3.extend_from_slice(&vec![0u8; body_len as usize]);
        assert!(!demux(&f3).unwrap().declared_short, "the ffmpeg +10 quirk is not a cut");
    }

    #[test]
    fn zero_and_implausible_fps_are_withheld() {
        let f = rmf(&[
            prop(1_000),
            mdpr(0, 100_000, 0, &vido_block(b"RV40", 320, 240, 0)),
            chunk(b"DATA", 0, &[]),
        ]);
        assert_eq!(demux(&f).unwrap().tracks[0].fps, None, "ffmpeg gates on fps > 0");
        // 40000 fps (16.16 = 40000 << 16) fails the tree-wide plausibility bound.
        let f2 = rmf(&[
            prop(1_000),
            mdpr(0, 100_000, 0, &vido_block(b"RV40", 320, 240, 40_000 << 16)),
            chunk(b"DATA", 0, &[]),
        ]);
        assert_eq!(demux(&f2).unwrap().tracks[0].fps, None);
    }

    #[test]
    fn truncated_and_degenerate_headers_never_panic() {
        let good = rmf(&[
            prop(2_000),
            mdpr(0, 200_000, 0, &vido_block(b"RV20", 320, 240, 25 << 16)),
            chunk(b"DATA", 0, &[]),
        ]);
        for cut in [4, 7, 8, 12, 18, 30, 60, good.len() - 1] {
            let _ = demux(&good[..cut.min(good.len())]);
        }
        // A chunk whose declared size overflows position arithmetic.
        let mut f = rmf(&[]);
        f.extend_from_slice(b"CONT");
        f.extend_from_slice(&u32::MAX.to_be_bytes());
        f.extend_from_slice(&0u16.to_be_bytes());
        let _ = demux(&f);
        // An MDPR whose declared type-specific size runs past EOF: clamped,
        // and the truncated block is simply not a recognizable video block.
        let mut b = mdpr(0, 100_000, 0, &vido_block(b"RV40", 320, 240, 25 << 16));
        b.truncate(b.len() - 20);
        let mut f2 = rmf(&[prop(1_000)]);
        f2.extend_from_slice(&b);
        let _ = demux(&f2);
    }
}
