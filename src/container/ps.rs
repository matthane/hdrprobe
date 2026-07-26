//! MPEG program stream and MPEG-1 system stream: `.mpg`, `.mpeg`, `.vob`,
//! `.m2p`, `.evo`.
//!
//! Read against **ITU-T H.222.0 (10/2014) | ISO/IEC 13818-1**, the ITU-published
//! equivalent of the MPEG-2 Systems standard, which is free from ITU and covers
//! the program stream normatively: §2.5.3.3 (Table 2-39, the pack header),
//! §2.4.3.6 (Table 2-21, the PES packet) and §2.4.3.7 (Table 2-22, the stream_id
//! assignments). The MPEG-1 pack and PES layouts are ISO/IEC 11172-1's and are
//! not in H.222.0; those two come from ffmpeg's `mpeg.c` plus validation against
//! real files, and are marked at their code sites.
//!
//! The system layer carries almost no video metadata. Everything in the
//! codec/picture/colour half comes from the video elementary stream reassembled
//! out of PES payloads — the same division the transport-stream backend already
//! has, and the reason this backend is a walker plus a router rather than a
//! parser.
//!
//! Four facts shape the code, each of which a later change would otherwise undo
//! silently.
//!
//! **The stream id names a stream number, not a codec.** Table 2-22 gives
//! `1110 xxxx` (0xE0..0xEF) as "video stream number xxxx" for H.262, 11172-2,
//! 14496-2, H.264 **and** H.265 alike, so the low nibble is an index and nothing
//! more. ffmpeg's habit of writing H.264 at 0xE2 (`H264_ID` in `mpeg.h`) is a
//! muxer convention, not a rule — its own demuxer does not rely on it, and
//! neither may this one. The codec is therefore decided from the reassembled
//! bytes ([`classify_es`]), never from the id.
//!
//! **The elementary stream must be routed by a census, not by its first byte.**
//! `container::classify_start_code` answers only for a buffer that *opens* on a
//! start-code prefix, and below `0x80` its reading is deliberately permissive
//! (see `looks_like_nal_header`). A reassembled elementary stream frequently
//! satisfies neither condition, because the first video PES packet in the window
//! usually continues a picture that began before it. Measured over 1999
//! sector-boundary cuts of the corpus retail DVD, the reassembled stream opens
//! **mid-picture with no start code at all in 1915 of them** (96%, so first-byte
//! routing has no verdict to give), on a sub-`0x80` slice code in 1 (which the
//! discriminator's AVC reading admits, so the verdict it gives is Annex-B and
//! wrong), and on a sequence or GOP code in the remaining 83.
//!
//! [`classify_es`] instead scans for start codes across the whole reassembled
//! head and decides on the **set** it finds, which answers all 1999 and is sound
//! rather than probabilistic: emulation prevention means a conforming
//! H.264/H.265 stream cannot contain `00 00 01` followed by a byte with bit 7
//! set, so a single such code refutes Annex-B outright.
//!
//! **`PES_packet_length` is the only legal way to advance.** Audio and private
//! payloads contain `00 00 01 xx` patterns; a byte scan that resumes inside a
//! packet reads them as structure. On the corpus's retail DVD a raw scan reports
//! seven Program Stream Maps on a disc that has none. The walk therefore steps
//! packet to packet and only ever rescans after a *failed* structural read.
//!
//! **The SCR is not a duration, and neither is the PTS span on its own.**
//! Duration comes from the video presentation timestamps ([`pts_span`]) plus the
//! last picture's own display time ([`whole_frame_duration`]); the pack clock's
//! role is to detect a discontinuity, never to measure.

use anyhow::{bail, Result};

use crate::container::{Chunk, Codec, Demux, NalFormat, TrackDemux};
use crate::model::Bitrate;

/// Bytes from the head the metadata walk may cover. The video sequence header
/// or SPS rides the first access unit of the first video PES, which on every
/// file observed sits within the first few packs — a retail DVD puts it at byte
/// 2084 — so this is slack for a mid-file cut rather than a budget anything
/// normally spends. Must stay `<=` `prefetch::HEAD_WARM` so the generic head
/// warm covers the whole walked span on a network volume, the same coupling
/// `annexb::HEAD_SCAN_BYTES`, `av1::HEAD_SCAN_BYTES` and
/// `mpegv::HEAD_SCAN_BYTES` keep.
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

/// Bytes from the tail scanned for the last presentation timestamp. A program
/// stream has no duration field, so — as for a transport stream's PCR — the
/// close of the timeline comes from a bounded trailing window. Sized to hold
/// many packs at any realistic mux rate (a DVD's ceiling of 13.2 Mbit/s fills
/// this in 2.5 s of playing time). The prefetch warms exactly this trailing
/// window for program streams; keep the two in sync.
pub const TAIL_SCAN_BYTES: usize = 4 << 20; // 4 MiB

/// A PTS is a 33-bit value at 90 kHz (H.222.0 §2.4.3.7, equation 2-11), so it
/// wraps every `2^33 / 90000` seconds — about 26 h 30 min.
const PTS_MODULUS: u64 = 1 << 33;

/// Largest duration this backend reports rather than rejecting as an undetected
/// second wrap. A deliberately conservative floor under the clock's own range,
/// not that range itself: `2^33 / 90000` is 95443.7 s (26 h 30 m 43 s), and the
/// 43-minute gap is the only thing separating "wrapped once" from "went
/// backwards". The transport backend uses the identical bound for the identical
/// reason.
const MAX_SPAN_SECS: f64 = 26.0 * 3600.0;

/// Floor on the margin allowed between a window's presentation span and its own
/// pack clock (see [`Walk::pts_within`]). Absolute rather than proportional, so
/// a short window whose two clocks differ by a fixed decoder-buffer delay is not
/// rejected for it.
const SPAN_SLACK_SECS: f64 = 2.0;

// --- stream ids, H.222.0 Table 2-22 -----------------------------------------

const SID_PROGRAM_END: u8 = 0xB9;
const SID_PACK: u8 = 0xBA;
const SID_VIDEO_FIRST: u8 = 0xE0;
const SID_VIDEO_LAST: u8 = 0xEF;

/// `1110 xxxx` — Table 2-22's video range. The low nibble is the stream
/// *number*; the row covers every video codec the standard admits, so this says
/// "video", never which codec.
fn is_video_sid(sid: u8) -> bool {
    (SID_VIDEO_FIRST..=SID_VIDEO_LAST).contains(&sid)
}

/// Which pack layer the file uses. The two are told apart by the bits after the
/// pack start code and differ in length, field order and clock resolution, so
/// nothing downstream may assume one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// ISO/IEC 11172-1 system stream: a 12-byte pack, `'0010'`, a 33-bit
    /// 90 kHz SCR and no stuffing. **ffmpeg writes this for `-f mpeg`**, so a
    /// `.mpg` holding MPEG-2 video is routinely an MPEG-1 *system* stream —
    /// the layers version independently.
    Mpeg1System,
    /// ITU-T H.222.0 program stream: a 14-byte pack, `'01'`, a 42-bit SCR
    /// (33-bit 90 kHz base plus a 9-bit 27 MHz extension) and up to 7 stuffing
    /// bytes. `-f vob` writes this.
    Mpeg2Program,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::Mpeg1System => MPEG1_SYSTEM_LABEL,
            Variant::Mpeg2Program => MPEG2_PROGRAM_LABEL,
        }
    }
}

// The three `Demux::container` values this backend emits. They are constants
// rather than inline strings because `main.rs` matches on them to drop the
// PTS-span duration for a truncated stdin prefix: the coupling is by string
// equality, so a rename here would silently disable that suppression and start
// printing a prefix's duration and overall bitrate as if they described the
// whole stream.
pub(crate) const MPEG2_PROGRAM_LABEL: &str = "MPEG-2 Program Stream";
pub(crate) const MPEG1_SYSTEM_LABEL: &str = "MPEG-1 System Stream";
/// A program stream with no pack layer at all: a mid-file cut, or a bare PES
/// stream (which ffmpeg's own probe recognises as its own shape). The walk is
/// identical; only the label differs, because calling it MPEG-1 or MPEG-2 would
/// state a fact no byte in the file carries.
pub(crate) const PES_ONLY_LABEL: &str = "MPEG PES stream";

pub fn demux(data: &[u8]) -> Result<Demux> {
    // This backend is reachable by *extension*, so nothing has checked the
    // bytes, and its walk is a scan rather than a read at a fixed offset. The
    // census is the gate (the same lesson `mpegv::demux`'s leading start-code
    // check records): without it a file merely *named* `.vob` would be walked
    // for 8 MiB and could report whatever its bytes happened to satisfy.
    let head_end = HEAD_SCAN_BYTES.min(data.len());
    if !looks_like_program_stream(&data[..CENSUS_SPAN.min(data.len())]) {
        bail!("not an MPEG program stream (no pack or PES structure in the head)");
    }

    let head = walk(data, 0, head_end, true);
    if head.streams.is_empty() {
        // Name the range, because the likeliest way to reach this is an HD DVD
        // `.evo` whose video really is present but rides the extended stream id
        // 0xFD, whose PES-extension substream layout this walker does not
        // decode. "No video elementary stream" alone would read as a claim
        // about the file that its bytes contradict.
        bail!(
            "no video elementary stream in the ordinary stream-id range \
             (0xE0..0xEF) in the program stream head window; extended stream \
             ids such as 0xFD are not decoded"
        );
    }

    // The tail supplies the close of the presentation timeline. A file that
    // fits inside the head window has already been walked end to end, so its
    // own maxima are the answer and no second read is needed.
    //
    // The two windows must be **disjoint**. Clamping the tail's start to the
    // head's end is what makes them so: without it, any file between
    // `HEAD_SCAN_BYTES` and `HEAD_SCAN_BYTES + TAIL_SCAN_BYTES` has a tail that
    // re-reads bytes the head already walked, so the tail's clock necessarily
    // starts before the head's ended and the concatenation guard below fires on
    // an ordinary file. That band is ~10-15 s of DVD-rate content and ~a minute
    // at VCD rate, i.e. exactly the short clips this backend is most often
    // pointed at, all of which silently lost their duration and bitrate.
    let tail_start = data.len().saturating_sub(TAIL_SCAN_BYTES).max(head_end);
    let tail = (data.len() > head_end).then(|| walk(data, tail_start, data.len(), false));
    let span = pts_span(&head, tail.as_ref(), tail_start == head_end);

    let container = head.variant.map_or(PES_ONLY_LABEL, Variant::label);
    let mut tracks = Vec::with_capacity(head.streams.len());
    for mut es in head.streams {
        // No container box names the codec (H.222.0 Table 2-22's video row
        // covers every codec it admits), and a program stream map is almost
        // never present, so the reassembled bytes decide.
        let Some(codec) = classify_es(&es.buf) else { continue };

        es.finish();
        let sps = crate::container::best_sps(&es.buf, &es.chunks, &codec);
        let sps_chunk = sps.as_ref().map(|s| s.chunk);
        let (width, height, bit_depth, chroma, codec_profile, (color, color_source), fps) =
            crate::container::sps_fields(sps);

        let mut td = TrackDemux {
            track_number: Some(es.sid as u64),
            width,
            height,
            fps,
            bit_depth,
            chroma,
            codec_profile,
            // A program stream carries no colour description of its own: every
            // field here came from the coded stream.
            color_source,
            color,
            chunks: es.chunks,
            sps_chunk,
            ..TrackDemux::new(codec, NalFormat::AnnexB)
        };
        match td.codec {
            // The MPEG codecs have no SPS; their metadata rides headers the
            // gap-fillers find in the same reassembled buffer. There is no
            // container-side copy of those headers in a program stream, so the
            // chunk scan is the whole path and `headers` is empty.
            Codec::Mpeg1 | Codec::Mpeg2 => {
                crate::container::fill_mpeg2_stream_fields(&mut td, &es.buf)
            }
            Codec::Mpeg4Part2 => {
                crate::container::fill_mpeg4part2_stream_fields(&mut td, &[], &es.buf)
            }
            _ => {}
        }
        td.reassembled = Some(es.buf);
        tracks.push(td);
    }

    if tracks.is_empty() {
        bail!("no recognized video codec in the program stream's elementary streams");
    }

    // The duration needs the frame rate, which only exists once a track has
    // been parsed, and the bitrate needs the duration — hence this third pass
    // rather than filling both inside the loop above.
    let duration_secs = whole_frame_duration(span, tracks.first().and_then(|t| t.fps));
    if let [t] = tracks.as_mut_slice() {
        // An overall rate only: the byte count includes audio, subtitles and
        // every layer of packet overhead. `program_mux_rate` is deliberately
        // never read for this — H.222.0 §2.5.3.4 defines it as the rate "at
        // which the P-STD receives the program stream during the pack in which
        // it is included" and lets it vary pack to pack, i.e. a per-pack
        // ceiling; a retail DVD writes 32964, which is 13.2 Mbit/s against an
        // actual 6.6. With more than one video stream even the overall rate is
        // unattributable, so it is left unset rather than repeated per track.
        t.bitrate = Bitrate::overall(data.len() as u64, duration_secs);
    }

    Ok(Demux {
        container,
        duration_secs,
        tracks,
        ts_stream: None,
        mkv_stream: None,
        raw_stream: None,
        // The chunks above index the bounded head window and there is no
        // streaming plan behind them, so `--full` reads every chunk that exists
        // without having seen the whole stream. Saying so keeps the report's
        // sampled footnote on, which matters for an AVC- or HEVC-carrying
        // program stream whose per-GOP SEIs really are sampled. Closing this
        // properly is the plan's D7 work — a windowed walk in the `ts_stream`
        // shape — which is cross-cutting and not Phase 3's.
        bounded_index: true,
    })
}

// --- duration ---------------------------------------------------------------

/// Turn a presentation span into a duration by adding the last picture's own
/// display time.
///
/// **The span is one frame short of the duration, by arithmetic rather than by
/// approximation.** Frame `k` of `N` is presented at `start + k/f`, so the first
/// picture's timestamp to the last picture's is `(N-1)/f` while the stream
/// occupies `N/f`. Reporting the bare span makes every duration one frame low
/// and — because the bitrate divides by it — every overall rate correspondingly
/// high, which is 2% on a two-second clip.
///
/// Adding the interval reproduces MediaInfo *exactly* on all four corpus
/// program streams (it reaches the same number from the other direction, by
/// counting frames and dividing by the rate): 1.96 s + 1/25 = 2.000 against its
/// 2.000 on three of them, and 60.8107 s + 1/29.97 = 60.844 against its 60.844
/// on the retail DVD.
///
/// Without a frame rate there is nothing to add and the bare span stands, which
/// is the honest fallback rather than a second guess.
fn whole_frame_duration(span: Option<f64>, fps: Option<f64>) -> Option<f64> {
    let span = span?;
    Some(match fps {
        Some(f) if f > 0.0 => span + 1.0 / f,
        _ => span,
    })
}

/// The **video PTS span**: the smallest presentation timestamp in the head
/// window to the largest in the tail. Minimum and maximum rather than first and
/// last because a PTS is a presentation time and B-frame reordering makes it
/// non-monotonic in stream order.
///
/// **The SCR span is not a fallback**, and that is a measured decision rather
/// than a stylistic one. The pack clock times the *arrival* of bytes at the
/// decoder, so it closes when the mux ends while the PTS closes when the last
/// picture is shown. Measured against ffprobe: **−11.0% on the two-second
/// corpus clips** (1.780 s of pack clock against a 2.0 s stream), −0.47% on the
/// 60 s retail DVD, and **+3.2% on an ffmpeg-muxed VOB** (12.381 s against
/// 12.000 s). Neither the size of the error nor even its direction is
/// predictable, which is also why there is no span-comparison cross-check
/// between the two clocks: any tolerance wide enough to accept that spread is
/// far too wide to catch anything.
///
/// What the SCR *is* used for is spotting a clock reset, which is the one way a
/// plausible-looking span can be wholly wrong. Two checks, both cheap: a
/// backward step inside either window, and a tail whose clock starts before the
/// head's ended.
///
/// **Concatenation detection is real but partial, and the limit is structural.**
/// The second check catches `cat a.vob b.vob` only when `b`'s clock at its own
/// tail window is still below `a`'s clock at the end of the head window — which
/// at DVD rate means only when `b` runs shorter than roughly 17 s. Append
/// anything longer and both windows are internally monotonic, the head sits
/// wholly inside `a` and the tail wholly inside `b`, and nothing either one can
/// see gives the join away; the reported duration is then `b`'s alone. The
/// reverse order *is* caught, and so is any concatenation small enough to fit
/// inside the head window. Catching the general case needs a third probe point
/// or a clock-against-byte-position rate band, both of which cost a read this
/// backend does not take — and ffprobe and MediaInfo report the same number on
/// the same file, so this is a bound on head-and-tail probing rather than a
/// divergence from the field.
///
/// The other residual limitation is the transport backend's exactly: a
/// discontinuity wholly inside the unread middle is invisible, and a reset that
/// happens to land on a larger value still reads as plausible.
fn pts_span(head: &Walk, tail: Option<&Walk>, contiguous: bool) -> Option<f64> {
    if head.scr_backward || tail.is_some_and(|t| t.scr_backward) {
        return None;
    }
    if let (Some(t), Some(head_last)) = (tail, head.scr_last) {
        if t.scr_first.is_some_and(|f| f < head_last) {
            return None;
        }
    }
    // A single stray timestamp anywhere in either window would otherwise set
    // the answer, because the span is a min/max rather than the transport
    // backend's first-and-last pair. One non-conforming packet was measured
    // turning a real file into "25 h 55 m at 1.26 kb/s".
    //
    // The pack clock is the check: over the *same bytes* it and the
    // presentation clock measure the same interval, so their spans differ only
    // by the decoder buffer delay, which the P-STD model bounds at well under a
    // second. Observed excess on real files is at most 0.2 s. This is also
    // precisely why the same comparison is useless against concatenation, where
    // both clocks agree to 0.08%: it catches an impossible span, not a
    // plausible-but-wrong one.
    if !head.pts_within(head.scr_span()) {
        return None;
    }
    if let Some(t) = tail {
        if !t.pts_within(t.scr_span()) {
            return None;
        }
    }
    let first = head.pts_min?;
    let last = match tail {
        None => head.pts_max,
        // `contiguous` means the two windows meet, so between them they have
        // read the whole file and the head's own maximum is real evidence about
        // where the stream ends. That is the case for a file only slightly
        // larger than the head window, whose tail is a sliver that may hold no
        // video timestamp at all — before this it lost its duration entirely.
        Some(t) if contiguous => t.pts_max.or(head.pts_max),
        // With unread bytes between the windows the head knows nothing about
        // the end, so a tail that found no timestamp means no duration rather
        // than a duration describing the head window alone.
        Some(t) => t.pts_max,
    }?;
    let span = if last >= first { last - first } else { last + PTS_MODULUS - first };
    let secs = span as f64 / 90_000.0;
    (secs > 0.0 && secs <= MAX_SPAN_SECS).then_some(secs)
}

// --- the pack / PES walk ----------------------------------------------------

/// One video elementary stream being reassembled out of its PES payloads.
struct EsOut {
    sid: u8,
    buf: Vec<u8>,
    chunks: Vec<Chunk>,
    /// Offset in `buf` at which the access unit under construction began.
    au_start: u64,
    started: bool,
}

impl EsOut {
    /// Append one PES payload. `au_boundary` marks a packet carrying a PTS,
    /// which H.222.0 §2.4.3.7 defines as referring to "the access unit
    /// containing the first picture start code that commences in this PES
    /// packet" — so such a packet begins an access unit.
    ///
    /// Timestamps are not required on every access unit, so a run of untimed
    /// packets merges into the preceding chunk. That under-segments and never
    /// misaligns: every chunk still *starts* on an access unit, which is what
    /// the header parsers and the Annex-B splitter need. Cutting at every PES
    /// packet instead would be the misaligned choice — a DVD fragments one
    /// picture across dozens of ~2 KiB packets.
    ///
    /// **The cut lands on the payload's first start code, not on its first
    /// byte, and that is a safety property rather than a nicety.** §2.4.3.7 says
    /// the timestamp refers to "the access unit containing the first picture
    /// start code that **commences in** this PES packet" — which says nothing
    /// about that code sitting at offset 0. The guarantee that it does is
    /// `data_alignment_indicator`, and §2.4.3.7 is explicit that when the flag
    /// is clear "it is not defined whether any such alignment occurs or not".
    /// Measured: on the corpus retail DVD all 27 timestamped video packets do
    /// begin on a start code, but on ffmpeg-written program streams only 1 to 3
    /// of 42 to 50 do — and every file observed, disc included, clears the flag.
    ///
    /// Cutting at the payload's first byte would therefore misalign routinely,
    /// and misalignment is not cosmetic: `split_annexb` treats a chunk's own
    /// offset 0 as an implicit NAL boundary, so a chunk starting mid-slice mints
    /// a NAL out of compressed payload and the SEI readers parse it.
    /// Demonstrated on a Dolby Vision stream rewrapped with a timestamp on every
    /// packet, where the report grew a **signalled** mastering display of
    /// 396272 cd/m² and a MaxCLL of 44200 out of nothing. Bytes before the start
    /// code belong to the access unit still open, which is where they go; a
    /// payload holding no start code at all begins no access unit whatever its
    /// header claims, so it does not cut.
    fn push(&mut self, payload: &[u8], au_boundary: bool) {
        let cut = au_boundary.then(|| first_start_code(payload)).flatten();
        match cut {
            Some(k) if self.started => {
                self.buf.extend_from_slice(&payload[..k]);
                self.close();
                self.buf.extend_from_slice(&payload[k..]);
            }
            _ => self.buf.extend_from_slice(payload),
        }
        self.started = true;
    }

    fn close(&mut self) {
        let end = self.buf.len() as u64;
        if end > self.au_start {
            self.chunks.push(Chunk { offset: self.au_start, size: end - self.au_start });
            self.au_start = end;
        }
    }

    /// Close the access unit still accumulating at the end of the walk.
    ///
    /// The transport backend deliberately drops its trailing partial unit,
    /// because there the count feeds a bitrate and no terminating PES start
    /// bounds it. Here the buffer is a bounded metadata window that feeds no
    /// rate, and dropping the tail would leave a stream carrying no timestamps
    /// at all — an ffmpeg-written H.264 program stream is exactly that — with
    /// no chunks whatsoever, hence no metadata.
    fn finish(&mut self) {
        self.close();
    }
}

/// Offset of the first start-code prefix in a PES payload, which is where an
/// access unit commencing in this packet actually begins. Covers the four-byte
/// form too: `00 00 00 01` contains `00 00 01` at offset 1, and starting a chunk
/// on the three-byte prefix is what [`crate::hevc::nal::split_annexb`] expects.
fn first_start_code(payload: &[u8]) -> Option<usize> {
    payload.windows(3).position(|w| w == [0, 0, 1])
}

/// What one bounded pass over the pack/PES structure observed.
#[derive(Default)]
struct Walk {
    /// Video streams in ascending stream-id order, which is the report order.
    streams: Vec<EsOut>,
    variant: Option<Variant>,
    scr_first: Option<u64>,
    scr_last: Option<u64>,
    /// The pack clock stepped backwards, so this window straddles a reset.
    scr_backward: bool,
    pts_min: Option<u64>,
    pts_max: Option<u64>,
}

impl Walk {
    fn note_scr(&mut self, scr: u64) {
        if let Some(prev) = self.scr_last {
            if scr < prev {
                self.scr_backward = true;
            }
        }
        self.scr_first.get_or_insert(scr);
        self.scr_last = Some(scr);
    }

    fn note_pts(&mut self, pts: u64) {
        self.pts_min = Some(self.pts_min.map_or(pts, |m| m.min(pts)));
        self.pts_max = Some(self.pts_max.map_or(pts, |m| m.max(pts)));
    }

    /// Seconds of pack clock this window covers, `None` with fewer than two
    /// packs — a pack-less stream has no second opinion to offer and the
    /// timestamp check below is skipped rather than failed.
    fn scr_span(&self) -> Option<f64> {
        let (a, b) = (self.scr_first?, self.scr_last?);
        Some((b.saturating_sub(a)) as f64 / 27_000_000.0)
    }

    /// Whether this window's presentation span is credible against its own pack
    /// clock. The presentation span may legitimately exceed the arrival span a
    /// little — the last picture is shown after the last byte arrives — so the
    /// margin is one-sided and generous: half again, or two seconds, whichever
    /// is larger. Real files were measured at most 0.2 s over.
    fn pts_within(&self, scr_span: Option<f64>) -> bool {
        let (Some(scr), Some(lo), Some(hi)) = (scr_span, self.pts_min, self.pts_max) else {
            return true;
        };
        let pts = hi.saturating_sub(lo) as f64 / 90_000.0;
        pts <= scr + (0.5 * scr).max(SPAN_SLACK_SECS)
    }

    fn stream(&mut self, sid: u8) -> &mut EsOut {
        let i = match self.streams.binary_search_by_key(&sid, |s| s.sid) {
            Ok(i) => i,
            Err(i) => {
                self.streams.insert(
                    i,
                    EsOut { sid, buf: Vec::new(), chunks: Vec::new(), au_start: 0, started: false },
                );
                i
            }
        };
        &mut self.streams[i]
    }
}

/// Walk `[start, end)` packet by packet, collecting the clocks and — when
/// `collect_es` — the video payloads.
///
/// A window that does not begin at byte 0 is anchored on confirmed structure
/// first, because an arbitrary offset lands inside a payload whose bytes look
/// like structure.
///
/// **Stepping by `PES_packet_length` is right for every stream id, including
/// the eight that carry no optional PES header at all.** H.222.0 Table 2-21
/// excludes `0xBC` program_stream_map, `0xBE` padding_stream, `0xBF`
/// private_stream_2, `0xF0` ECM, `0xF1` EMM, `0xF2` DSM-CC, `0xF8` H.222.1
/// type E and `0xFF` program_stream_directory from the `'10'`-marker header —
/// Note 3 under Table 2-22 puts it plainly: for those, "no syntax is specified
/// after PES_packet_length field". But the length field itself is at the same
/// offset for all of them, so the walk needs no special case; only a reader
/// that wanted their *payload* would. (An earlier draft of the format reference
/// listed just two of the eight, which would have been enough to make such a
/// reader parse compressed bytes as a header.)
fn walk(data: &[u8], start: usize, end: usize, collect_es: bool) -> Walk {
    let mut w = Walk::default();
    let end = end.min(data.len());
    let Some(mut pos) = (if start == 0 { Some(0) } else { find_anchor(data, start, end) }) else {
        return w;
    };

    while pos + 4 <= end {
        if data[pos] != 0 || data[pos + 1] != 0 || data[pos + 2] != 1 {
            pos += 1;
            continue;
        }
        let sid = data[pos + 3];
        if sid == SID_PACK {
            match parse_pack(data, pos, end) {
                Some((variant, next, scr)) => {
                    w.variant.get_or_insert(variant);
                    w.note_scr(scr);
                    pos = next;
                }
                // Not a pack after all, so these bytes are payload emulation
                // or corruption. Resuming the byte scan is right precisely
                // because the structural read failed.
                None => pos += 1,
            }
            continue;
        }
        if sid == SID_PROGRAM_END {
            pos += 4;
            continue;
        }
        let Some(len) = read_u16(data, pos + 4) else { break };
        // H.222.0 §2.4.3.7: a `PES_packet_length` of 0 is permitted only for
        // video inside a *transport* stream. Zero here means the structure is
        // broken, and advancing by it would not terminate.
        if len == 0 {
            break;
        }
        let body = pos + 6;
        let next = body.saturating_add(len);
        if collect_es && is_video_sid(sid) && body < end {
            let limit = next.min(end);
            if let Some((off, pts)) = pes_payload(data, body, limit) {
                if let Some(p) = pts {
                    w.note_pts(p);
                }
                if off < limit {
                    let payload = &data[off..limit];
                    w.stream(sid).push(payload, pts.is_some());
                }
            }
        } else if is_video_sid(sid) && body < end {
            // Timestamps only: the tail window needs no payload copies.
            if let Some((_, Some(p))) = pes_payload(data, body, next.min(end)) {
                w.note_pts(p);
            }
        }
        pos = next;
    }
    w
}

/// Find a byte offset at or after `from` where the pack/PES structure resumes.
///
/// A single valid-looking header is not enough — `00 00 01 BA` occurs inside
/// compressed payload — so the candidate must also chain: walking forward by
/// the declared extents has to land on a start code [`ANCHOR_CONFIRMATIONS`]
/// times running. Measured over 1200 arbitrary start offsets across three real
/// program streams, that produced no false anchor at all.
///
/// **A PES packet anchors as well as a pack.** Restricting this to packs looks
/// safer — a pack header carries six marker bits of structural evidence (five for the MPEG-1 form) and a
/// PES header carries none — but it silently denies a duration to every
/// pack-less stream over `HEAD_SCAN_BYTES`, which is a shape this backend
/// explicitly supports (see [`PES_ONLY_LABEL`]). The chain confirmation is what
/// actually earns the anchor in both cases: each step must land exactly on a
/// start-code prefix, which is a 1-in-16-million coincidence per step before
/// the length arithmetic even has to agree.
fn find_anchor(data: &[u8], from: usize, end: usize) -> Option<usize> {
    // Clamped here rather than trusted from the caller: everything below raw-
    // indexes against it, while `parse_pack` beside them is `get`-checked, and
    // that asymmetry is exactly how a second caller would introduce a panic.
    let end = end.min(data.len());
    let mut pos = from;
    while pos + 4 <= end {
        // `0xB9` and up is the whole program-stream start-code space: §2.5.3.2
        // (`MPEG_program_end_code`), §2.5.3.4 (`pack_start_code`), §2.5.3.5
        // (the system header) and then Table 2-22 from `0xBC` on. Video-layer
        // codes sit below `0xB9` and cannot begin a packet.
        if data[pos] == 0
            && data[pos + 1] == 0
            && data[pos + 2] == 1
            && data[pos + 3] >= SID_PROGRAM_END
            && chains(data, pos, end)
        {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

/// Consecutive structural steps a tail anchor must survive.
const ANCHOR_CONFIRMATIONS: usize = 3;

/// Whether the structure at `pos` chains: each step advances by the packet's own
/// declared extent and must land on another start-code prefix.
///
/// A step whose declared extent runs past `end` **fails**. The window ends at
/// the file's own end, so a packet claiming bytes beyond it is malformed, and
/// treating the overshoot as "the structure held as far as the window goes"
/// would let two plausible steps plus one wild length anchor the tail walk
/// inside compressed payload — where every timestamp it then read would be
/// garbage.
fn chains(data: &[u8], pos: usize, end: usize) -> bool {
    let end = end.min(data.len());
    let mut p = pos;
    for _ in 0..ANCHOR_CONFIRMATIONS {
        if p + 4 > end || data[p] != 0 || data[p + 1] != 0 || data[p + 2] != 1 {
            return false;
        }
        let sid = data[p + 3];
        p = if sid == SID_PACK {
            match parse_pack(data, p, end) {
                Some((_, next, _)) => next,
                None => return false,
            }
        } else if sid == SID_PROGRAM_END {
            p + 4
        } else {
            match read_u16(data, p + 4) {
                Some(len) if len > 0 && p + 6 + len <= end => p + 6 + len,
                _ => return false,
            }
        };
    }
    // Landing exactly on the window end is a pass: the last packet ended where
    // the file does.
    p == end || (p + 3 <= end && data[p] == 0 && data[p + 1] == 0 && data[p + 2] == 1)
}

fn read_u16(data: &[u8], at: usize) -> Option<usize> {
    let b = data.get(at..at + 2)?;
    Some(((b[0] as usize) << 8) | b[1] as usize)
}

/// Parse a pack header at `pos`, returning its variant, the offset just past it,
/// and the system clock reference in 27 MHz units.
///
/// Every marker bit is checked. They are the only structural evidence that these
/// bytes really are a pack rather than payload that happens to start
/// `00 00 01 BA`, and the tail anchor depends on that discrimination.
fn parse_pack(data: &[u8], pos: usize, end: usize) -> Option<(Variant, usize, u64)> {
    let d = *data.get(pos + 4)?;
    if d & 0xC0 == 0x40 {
        // H.222.0 Table 2-39. 14 bytes plus up to 7 stuffing bytes:
        //   '01'(2) SCR_base[32..30](3) m SCR_base[29..15](15) m
        //   SCR_base[14..0](15) m SCR_ext(9) m mux_rate(22) m m
        //   reserved(5) pack_stuffing_length(3)
        let b = data.get(pos + 4..pos + 14)?;
        if b[0] & 0x04 == 0
            || b[2] & 0x04 == 0
            || b[4] & 0x04 == 0
            || b[5] & 0x01 == 0
            || b[8] & 0x03 != 0x03
        {
            return None;
        }
        let base = (((b[0] >> 3) & 0x07) as u64) << 30
            | ((b[0] & 0x03) as u64) << 28
            | (b[1] as u64) << 20
            | (((b[2] >> 3) & 0x1F) as u64) << 15
            | ((b[2] & 0x03) as u64) << 13
            | (b[3] as u64) << 5
            | ((b[4] >> 3) & 0x1F) as u64;
        let ext = (((b[4] & 0x03) as u64) << 7) | (b[5] >> 1) as u64;
        let next = pos.checked_add(14 + (b[9] & 0x07) as usize)?;
        (next <= end).then_some((Variant::Mpeg2Program, next, base * 300 + ext))
    } else if d & 0xF0 == 0x20 {
        // ISO/IEC 11172-1 §2.4.3.2, via ffmpeg `mpeg.c` and validated against
        // both corpus MPEG-1 system streams. 12 bytes, no stuffing:
        //   '0010'(4) SCR[32..30](3) m SCR[29..15](15) m SCR[14..0](15) m
        //   m mux_rate(22) m
        let b = data.get(pos + 4..pos + 12)?;
        if b[0] & 0x01 == 0
            || b[2] & 0x01 == 0
            || b[4] & 0x01 == 0
            || b[5] & 0x80 == 0
            || b[7] & 0x01 == 0
        {
            return None;
        }
        let scr = (((b[0] >> 1) & 0x07) as u64) << 30
            | (b[1] as u64) << 22
            | ((b[2] >> 1) as u64) << 15
            | (b[3] as u64) << 7
            | (b[4] >> 1) as u64;
        let next = pos.checked_add(12)?;
        // The MPEG-1 clock has no extension field; scaling to 27 MHz keeps one
        // unit for both variants so nothing downstream has to ask which it has.
        (next <= end).then_some((Variant::Mpeg1System, next, scr * 300))
    } else {
        None
    }
}

/// Offset of the elementary-stream bytes inside a PES packet body, plus the
/// presentation timestamp when one is present. `body` points just past the
/// 6-byte `00 00 01 <sid> <length>` prefix.
///
/// Two completely different layouts share this entry point, exactly as ffmpeg's
/// `mpegps_read_pes_header` handles them, because a `.mpg` may carry either.
fn pes_payload(data: &[u8], body: usize, end: usize) -> Option<(usize, Option<u64>)> {
    let c = *data.get(body)?;
    if c & 0xC0 == 0x80 {
        // H.222.0 Table 2-21: '10'(2) scrambling(2) priority alignment
        // copyright original | PTS_DTS_flags(2) ESCR ES_rate trick copy_info
        // CRC extension | PES_header_data_length(8)
        let flags = *data.get(body + 1)?;
        let header_len = *data.get(body + 2)? as usize;
        let off = body.checked_add(3)?.checked_add(header_len)?;
        // PTS_DTS_flags '10' or '11'; '01' is forbidden and carries no PTS.
        // Bounded by `end`, not by the buffer: a packet whose declared
        // `PES_packet_length` is too short to hold the timestamp it claims
        // would otherwise assemble one out of the *next* packet's bytes and
        // feed it to the duration.
        // `header_len >= 5` mirrors ffmpeg's `header_len -= 5; if (header_len
        // < 0) goto error_redo`: a packet claiming a timestamp its own declared
        // header is too short to hold is reading its payload, not a timestamp.
        let pts = (flags & 0x80 != 0 && header_len >= 5)
            .then(|| parse_timestamp(&data[..end], body + 3))
            .flatten();
        Some((off.min(end), pts))
    } else {
        // ISO/IEC 11172-1's PES layer. The dispatch ladder is ffmpeg's
        // (`mpeg.c::mpegps_read_pes_header`) and cannot be derived from
        // H.222.0, which does not define this form.
        let mut p = body;
        // Stuffing bytes, bounded by the packet's own declared extent — which
        // is what ffmpeg does. A "no more than 16" limit is often quoted for
        // 11172-1, but that standard is not available here and H.222.0 does not
        // define this header form at all, so the packet bound is the one this
        // code can actually source.
        while p < end && data[p] == 0xFF {
            p += 1;
        }
        let mut c = *data.get(p)?;
        if c & 0xC0 == 0x40 {
            // STD buffer scale and size: two bytes, then re-read.
            p = p.checked_add(2)?;
            c = *data.get(p)?;
        }
        if c & 0xE0 == 0x20 {
            let pts = parse_timestamp(&data[..end], p);
            // A 5-byte PTS, and 5 more for the DTS when the low bit of the
            // '0011' nibble is set.
            p = p.checked_add(if c & 0x10 != 0 { 10 } else { 5 })?;
            Some((p.min(end), pts))
        } else if c == 0x0F {
            // The no-timestamp case is a single byte, not a length-bearing
            // header. ffmpeg writes exactly this for H.264 in a program
            // stream, so it is a live path rather than a spec curiosity.
            Some((p.checked_add(1)?.min(end), None))
        } else {
            None
        }
    }
}

/// Decode a 33-bit 90 kHz timestamp from its 5-byte marker-interleaved form
/// (H.222.0 Table 2-21: `'0010'` PTS[32..30] m PTS[29..15] m PTS[14..0] m).
///
/// The three marker bits are checked, for the same reason [`parse_pack`] checks
/// its eight: they are the only structural evidence that these five bytes are a
/// timestamp rather than payload, and the value they carry is the one the whole
/// head/tail machinery exists to get right or refuse.
fn parse_timestamp(data: &[u8], at: usize) -> Option<u64> {
    let b = data.get(at..at + 5)?;
    if b[0] & 0x01 == 0 || b[2] & 0x01 == 0 || b[4] & 0x01 == 0 {
        return None;
    }
    Some(
        (((b[0] & 0x0E) as u64) << 29)
            | ((b[1] as u64) << 22)
            | (((b[2] >> 1) as u64) << 15)
            | ((b[3] as u64) << 7)
            | ((b[4] >> 1) as u64),
    )
}

// --- codec routing ----------------------------------------------------------

/// Bytes of a reassembled elementary stream the codec census reads. The
/// decisive evidence is at the head of the first access unit in every stream
/// observed; this is slack, and it bounds the work on a stream that carries no
/// start codes at all.
const ES_CENSUS_SPAN: usize = 1 << 20; // 1 MiB

/// Decide which codec a reassembled elementary stream carries, from the set of
/// start codes it contains.
///
/// The rule that makes this sound rather than probabilistic: **H.264 and H.265
/// use emulation prevention**, so a conforming Annex-B stream cannot contain the
/// byte sequence `00 00 01` anywhere except at a NAL start, and a NAL header's
/// `forbidden_zero_bit` must be 0. A single `00 00 01` followed by a byte with
/// bit 7 set therefore refutes Annex-B outright, however many plausible NAL
/// headers sit beside it. That is why this scans the stream instead of reading
/// its first bytes — which usually continue a picture and are not a start code
/// at all (the module doc has the measurement) — and why the MPEG readings are
/// tried first:
/// the reverse order lets an MPEG stream's tens of thousands of slice start
/// codes eventually yield a byte run that decodes as a valid SPS, which is the
/// documented way a retail DVD once reported a full HEVC profile.
///
/// Within the MPEG family the split follows the reserved-code table: `B0`, `B1`
/// and `B6` are MPEG-4 Part 2's visual object codes and reserved in MPEG-2,
/// `B7` and `B8` are MPEG-2's and reserved in Part 2, and `B3`/`B5` are
/// genuinely shared. Deciding on the first *unambiguous* code rather than on
/// `B3` is what keeps a Part 2 stream out of the MPEG-2 parser.
fn classify_es(es: &[u8]) -> Option<Codec> {
    let span = &es[..es.len().min(ES_CENSUS_SPAN)];
    let mut saw_high = false;
    let mut i = 0;
    while i + 4 <= span.len() {
        if span[i] != 0 || span[i + 1] != 0 || span[i + 2] != 1 {
            i += 1;
            continue;
        }
        let v = span[i + 3];
        match v {
            // MPEG-4 Part 2's visual object sequence, its end, and its VOP —
            // all reserved in MPEG-2. The VOP code rides every frame, so this
            // arm fires on any Part 2 stream within a few thousand bytes.
            //
            // **The VOL range `20`..`2F` is deliberately NOT here.** It is a
            // Part 2 start code, but it is also squarely inside MPEG-2's
            // `slice_start_code` span (`01`..`AF`, ISO/IEC 13818-2 Table 6-1),
            // where the value is `slice_vertical_position` — so slice `0x20` is
            // macroblock row 32, i.e. line 512, which every format above
            // standard definition reaches. A 576i, 720p or 1080i program stream
            // whose head holds a picture and its slices but no GOP header would
            // otherwise be named "MPEG-4 Visual" and parsed by the wrong
            // gap-filler. `src/mpeg4part2.rs` records the same overlap as the
            // reason its own parser demands dimensions.
            0xB0 | 0xB1 | 0xB6 => return Some(Codec::Mpeg4Part2),
            // Sequence end and group-of-pictures in MPEG-2. Part 2 *does*
            // define both — as the studio profile's slice and extension codes,
            // per ffmpeg's `mpeg4videodefs.h`; ISO/IEC 14496-2 itself is not
            // available here to confirm the pairing, so treat that as one
            // implementation witness. It costs nothing in practice: any studio
            // stream emits its `B0` visual object sequence code first, so this
            // arm is reachable only by a studio stream cut mid-picture, a shape
            // for which no file is known to exist anywhere.
            0xB7 | 0xB8 => return Some(mpeg_family(span)),
            _ => {
                saw_high |= v >= 0x80;
                i += 3;
                continue;
            }
        }
    }
    if saw_high {
        // Only shared codes (`B3`, `B5`, slices) were seen, so ask the parsers
        // which one actually reads: a sequence header first, then a visual
        // object layer.
        if crate::mpeg2::parse_sequence(span).is_some() {
            return Some(mpeg_family(span));
        }
        return crate::mpeg4part2::parse_visual(span).map(|_| Codec::Mpeg4Part2);
    }
    annexb_codec(span)
}

/// MPEG-1 or MPEG-2, told apart the only way a raw stream allows: ISO/IEC
/// 11172-2 defines no extensions at all, so a `sequence_extension` is present
/// exactly when the stream is 13818-2. Falls back to MPEG-2 only when the
/// sequence header itself did not parse, which is also the case in which no
/// field will be filled.
fn mpeg_family(es: &[u8]) -> Codec {
    let span = &es[..es.len().min(ES_CENSUS_SPAN)];
    match crate::mpeg2::parse_sequence(span) {
        Some(s) if !s.is_mpeg2 => Codec::Mpeg1,
        _ => Codec::Mpeg2,
    }
}

/// Which Annex-B codec a stream with no MPEG-layer start codes carries, decided
/// by which SPS parser accepts it. HEVC is tried first because its sequence
/// parameter set is the more structured of the two and so the less likely to
/// accept unrelated bytes.
fn annexb_codec(es: &[u8]) -> Option<Codec> {
    let whole = [Chunk { offset: 0, size: es.len() as u64 }];
    [Codec::Hevc, Codec::Avc]
        .into_iter()
        .find(|codec| crate::container::best_sps(es, &whole, codec).is_some())
}

// --- the head census --------------------------------------------------------

/// Start-code counts over a head window, in ffmpeg's own terms.
#[derive(Debug, Default, PartialEq, Eq)]
struct Census {
    sys: u32,
    pspack: u32,
    priv1: u32,
    vid: u32,
    audio: u32,
    invalid: u32,
}

/// Head bytes the census reads, wherever it is called from. One span, because
/// ffmpeg's thresholds below are calibrated against its probe buffer and
/// applying them over a span eight times larger would be applying different
/// thresholds. `container::classify_start_code` uses this too, so the sniffed
/// and extension-matched paths cannot reach different verdicts on one file.
pub(crate) const CENSUS_SPAN: usize = 1 << 20; // 1 MiB, ffmpeg's probe ceiling

/// Whether a head looks like a program stream.
///
/// **Transcribed from ffmpeg's `mpegps_probe` (`libavformat/mpeg.c`)** rather
/// than reinvented, on the reasoning recorded in the format reference: its
/// thresholds encode two decades of misidentification reports, and a fresh set
/// would rediscover them one bug at a time. All four score-producing conditions
/// are kept verbatim and reduced to "did any of them fire", which is exactly
/// ffmpeg's `score > 0`.
///
/// **That reduction is not free, and the first condition is where it costs.**
/// ffmpeg weighs its score against every other demuxer's, so a low score is a
/// guess it expects to lose; here there is no competition and any nonzero score
/// is an accept. The first condition is the weak one — in ffmpeg it assigns
/// `AVPROBE_SCORE_EXTENSION / 2` and falls through, and its own comments record
/// it firing on MP3 (`mp3_misidentified_2.mp3 has ... audio:6`). It is kept
/// anyway because it is the branch that admits genuinely short PES streams,
/// which the other three reject, and because the cost of a wrong accept here is
/// bounded: this gate only decides whether to attempt the walk, and a file with
/// no video elementary stream in it fails with an honest message a few
/// milliseconds later. It never produces a report.
///
/// The other deliberate departure is bounds discipline. ffmpeg reads a few
/// bytes past a start code without checking, relying on its probe buffer's
/// padding; every read here is checked, and a truncated candidate simply fails
/// its test.
pub(crate) fn looks_like_program_stream(buf: &[u8]) -> bool {
    let c = census(buf);
    // /* invalid VDR files nd short PES streams */
    if c.vid + c.audio > c.invalid + 1 {
        return true;
    }
    if c.sys > c.invalid && c.sys * 9 <= c.pspack * 10 {
        return true;
    }
    if c.pspack > c.invalid && (c.priv1 + c.vid + c.audio) * 10 >= c.pspack * 9 {
        return true;
    }
    // A bare PES stream: one media kind, no system layer, no packs.
    (c.vid > 0) ^ (c.audio > 0)
        && (c.audio > 4 || c.vid > 1)
        && c.sys == 0
        && c.pspack == 0
        && buf.len() > 2048
        && c.vid + c.audio > c.invalid
}

/// Count the start codes ffmpeg's probe counts, with its payload skips: an
/// audio or private packet's payload is stepped over so its compressed bytes
/// cannot emulate more structure, and a video packet suppresses nested PES
/// tests until its declared end.
fn census(buf: &[u8]) -> Census {
    let mut c = Census::default();
    let mut endpes = 0usize;
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if buf[i] != 0 || buf[i + 1] != 0 || buf[i + 2] != 1 {
            i += 1;
            continue;
        }
        let sid = buf[i + 3];
        let at = i + 3;
        let len = read_u16(buf, at + 1).unwrap_or(0);
        let pes = endpes <= at && check_pes(buf, at);
        match sid {
            0xBB => c.sys += 1,
            SID_PACK if check_pack(buf, at) => c.pspack += 1,
            _ if is_video_sid(sid) && pes => {
                endpes = at.saturating_add(len);
                c.vid += 1;
            }
            0xC0..=0xDF if pes => {
                c.audio += 1;
                i = at.saturating_add(len);
            }
            0xBD if pes => {
                c.priv1 += 1;
                i = at.saturating_add(len);
            }
            // 0x1fd, VC-1 behind the extended stream id.
            0xFD if pes => c.vid += 1,
            _ if is_video_sid(sid) || (0xC0..=0xDF).contains(&sid) || sid == 0xBD => c.invalid += 1,
            _ => {}
        }
        i += 1;
    }
    c
}

/// ffmpeg's `check_pack_header`, bounds-checked. `at` indexes the stream id, so
/// the byte tested is the one after it.
fn check_pack(buf: &[u8], at: usize) -> bool {
    match buf.get(at + 1) {
        Some(&b) => b & 0xC0 == 0x40 || b & 0xF0 == 0x20,
        None => false,
    }
}

/// ffmpeg's `check_pes`, bounds-checked. `at` indexes the stream id; `p[0]` in
/// ffmpeg's arithmetic is that byte.
fn check_pes(buf: &[u8], at: usize) -> bool {
    let g = |k: usize| buf.get(at + k).copied();
    // The MPEG-2 reading: the '10' marker, a legal PTS_DTS_flags value, and —
    // when timestamps are claimed — a matching marker nibble on the first
    // timestamp byte.
    let pes2 = g(3).is_some_and(|b| b & 0xC0 == 0x80)
        && g(4).is_some_and(|b| b & 0xC0 != 0x40)
        && g(4).is_some_and(|b| {
            b & 0xC0 == 0x00 || g(6).is_some_and(|t| (b & 0xC0) >> 2 == t & 0xF0)
        });

    // The MPEG-1 reading: skip stuffing, optionally a buffer bound, then check
    // the marker bits of whichever timestamp form is claimed.
    let mut p = at + 3;
    // Bounded by the buffer, as in ffmpeg. A start code needs two zero bytes so
    // it can never sit inside a run of 0xFF, which is what keeps the whole
    // census linear despite this inner loop.
    while buf.get(p) == Some(&0xFF) {
        p += 1;
    }
    let Some(&c) = buf.get(p) else { return pes2 };
    if c & 0xC0 == 0x40 {
        p += 2;
    }
    let Some(&c) = buf.get(p) else { return pes2 };
    let m = |k: usize| buf.get(p + k).copied().unwrap_or(0);
    let pes1 = if c & 0xF0 == 0x20 {
        m(0) & m(2) & m(4) & 1 == 1
    } else if c & 0xF0 == 0x30 {
        m(0) & m(2) & m(4) & m(5) & m(7) & m(9) & 1 == 1
    } else {
        c == 0x0F
    };
    pes1 || pes2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DVD navigation packs ride this id; the walk must step over them by
    /// length rather than resyncing inside their start-code-rich payload.
    const SID_PRIVATE_2: u8 = 0xBF;

    /// `testfiles/sdr/mpeg2ps.vob` bytes 0..14 verbatim: an ITU-T H.222.0
    /// pack header with a zero SCR and no stuffing.
    const MPEG2_PACK: [u8; 14] =
        [0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x86, 0x66, 0xCF, 0xF8];

    /// `testfiles/sdr/mpeg1.mpg` bytes 0..12 verbatim: an ISO/IEC 11172-1 pack
    /// header, which is what `ffmpeg -f mpeg` writes.
    const MPEG1_PACK: [u8; 12] =
        [0x00, 0x00, 0x01, 0xBA, 0x21, 0x00, 0x01, 0x00, 0x01, 0xC3, 0x33, 0x67];

    #[test]
    fn both_pack_forms_decode_with_every_marker_checked() {
        let (v, next, scr) = parse_pack(&MPEG2_PACK, 0, MPEG2_PACK.len()).expect("mpeg-2 pack");
        assert_eq!(v, Variant::Mpeg2Program);
        assert_eq!(next, 14);
        assert_eq!(scr, 0);

        let (v, next, scr) = parse_pack(&MPEG1_PACK, 0, MPEG1_PACK.len()).expect("mpeg-1 pack");
        assert_eq!(v, Variant::Mpeg1System);
        assert_eq!(next, 12);
        assert_eq!(scr, 0);
    }

    #[test]
    fn a_cleared_marker_bit_refutes_a_pack() {
        // Each marker in turn: clearing exactly one must reject the header,
        // which is what stops payload emulation from anchoring a tail walk.
        for (i, mask) in [(4, 0x04), (6, 0x04), (8, 0x04), (9, 0x01), (12, 0x03)] {
            let mut p = MPEG2_PACK;
            p[i] &= !mask;
            assert!(parse_pack(&p, 0, p.len()).is_none(), "byte {i} marker {mask:#04x}");
        }
        for (i, mask) in [(4, 0x01), (6, 0x01), (8, 0x01), (9, 0x80), (11, 0x01)] {
            let mut p = MPEG1_PACK;
            p[i] &= !mask;
            assert!(parse_pack(&p, 0, p.len()).is_none(), "mpeg-1 byte {i} marker {mask:#04x}");
        }
    }

    #[test]
    fn a_pack_header_cut_by_the_window_is_refused_not_read_past() {
        for n in 0..MPEG2_PACK.len() {
            assert!(parse_pack(&MPEG2_PACK[..n], 0, n).is_none(), "truncated to {n}");
        }
        for n in 0..MPEG1_PACK.len() {
            assert!(parse_pack(&MPEG1_PACK[..n], 0, n).is_none(), "mpeg-1 truncated to {n}");
        }
    }

    #[test]
    fn pack_stuffing_length_moves_the_next_packet() {
        let mut p = Vec::from(MPEG2_PACK);
        p[13] = 0xF8 | 3; // three stuffing bytes
        p.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        let (_, next, _) = parse_pack(&p, 0, p.len()).expect("pack");
        assert_eq!(next, 17, "stuffing bytes belong to the pack header");
    }

    /// The MPEG-2 PES form, holding a PTS of 0.54 s — `testfiles/sdr/mpeg2.mpg`'s
    /// first video timestamp.
    fn mpeg2_pes(pts: Option<u64>) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00];
        match pts {
            Some(t) => {
                v.extend_from_slice(&[0x80, 0x80, 0x05]);
                v.push(0x21 | (((t >> 30) as u8 & 0x07) << 1));
                v.push((t >> 22) as u8);
                v.push((((t >> 15) as u8) << 1) | 1);
                v.push((t >> 7) as u8);
                v.push(((t as u8) << 1) | 1);
            }
            None => v.extend_from_slice(&[0x80, 0x00, 0x00]),
        }
        let n = v.len() - 6;
        v[4] = (n >> 8) as u8;
        v[5] = n as u8;
        v
    }

    #[test]
    fn mpeg2_pes_header_yields_its_payload_offset_and_timestamp() {
        let pts = (0.54 * 90_000.0) as u64;
        let p = mpeg2_pes(Some(pts));
        let (off, got) = pes_payload(&p, 6, p.len()).expect("pes");
        assert_eq!(got, Some(pts));
        assert_eq!(off, p.len(), "payload begins after the optional header");

        let p = mpeg2_pes(None);
        let (off, got) = pes_payload(&p, 6, p.len()).expect("pes");
        assert_eq!(got, None);
        assert_eq!(off, p.len());
    }

    #[test]
    fn mpeg1_pes_ladder_covers_all_three_branches() {
        // Stuffing, then a buffer bound, then a 5-byte PTS.
        let pts = 90_000u64;
        let mut ts = vec![0x21 | (((pts >> 30) as u8 & 0x07) << 1)];
        ts.push((pts >> 22) as u8);
        ts.push((((pts >> 15) as u8) << 1) | 1);
        ts.push((pts >> 7) as u8);
        ts.push(((pts as u8) << 1) | 1);
        let mut p = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0xFF, 0xFF, 0x5F, 0xE0];
        p.extend_from_slice(&ts);
        p.push(0xAA);
        let (off, got) = pes_payload(&p, 6, p.len()).expect("mpeg-1 pes");
        assert_eq!(got, Some(pts), "PTS survives stuffing and the STD buffer bound");
        assert_eq!(off, p.len() - 1);

        // The single-byte no-timestamp form, which is what ffmpeg writes for
        // H.264 in a program stream.
        let p = [0x00, 0x00, 0x01, 0xE2, 0x07, 0xDF, 0x0F, 0x00, 0x00, 0x00, 0x01, 0x67];
        let (off, got) = pes_payload(&p, 6, p.len()).expect("0x0F form");
        assert_eq!(got, None);
        assert_eq!(off, 7, "one byte of header, then the elementary stream");

        // A byte matching no branch is not a PES header at all.
        let p = [0x00, 0x00, 0x01, 0xE0, 0x00, 0x02, 0x1A, 0x00];
        assert!(pes_payload(&p, 6, p.len()).is_none());
    }

    #[test]
    fn timestamps_are_read_across_their_marker_bits() {
        // 0x54 0x00 0x01 0x00 0x01 would be 0x5400010001 read flat; the marker
        // bits make it 0.0 s, and a naive read would be off by a huge factor.
        let b = [0x21, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(parse_timestamp(&b, 0), Some(0));
        // The full 33-bit range round-trips.
        for t in [1u64, 90_000, 8_589_934_591] {
            let e = [
                0x21 | (((t >> 30) as u8 & 0x07) << 1),
                (t >> 22) as u8,
                (((t >> 15) as u8) << 1) | 1,
                (t >> 7) as u8,
                ((t as u8) << 1) | 1,
            ];
            assert_eq!(parse_timestamp(&e, 0), Some(t), "{t}");
        }
        assert_eq!(parse_timestamp(&[0x21, 0x00], 0), None, "a cut timestamp reads as absent");
    }

    /// A minimal program stream: one pack, then video PES packets carrying the
    /// given payloads, each with a PTS.
    fn stream(variant: Variant, sid: u8, aus: &[(u64, &[u8])]) -> Vec<u8> {
        let mut v = match variant {
            Variant::Mpeg2Program => Vec::from(MPEG2_PACK),
            Variant::Mpeg1System => Vec::from(MPEG1_PACK),
        };
        for (pts, payload) in aus {
            let mut p = mpeg2_pes(Some(*pts));
            p[3] = sid;
            p.extend_from_slice(payload);
            let n = p.len() - 6;
            p[4] = (n >> 8) as u8;
            p[5] = n as u8;
            v.extend_from_slice(&p);
        }
        v
    }

    /// `testfiles/sdr/mpeg2.m2v` bytes 0..22: a sequence header plus the
    /// sequence extension that makes it 13818-2.
    const M2V_HEAD: [u8; 22] = [
        0x00, 0x00, 0x01, 0xB3, 0x14, 0x00, 0xF0, 0x23, 0xFF, 0xFF, 0xE0, 0x18, 0x00, 0x00, 0x01,
        0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00,
    ];

    #[test]
    fn a_program_stream_reports_its_video_track() {
        let d = stream(Variant::Mpeg2Program, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        let dm = demux(&d).expect("program stream");
        assert_eq!(dm.container, "MPEG-2 Program Stream");
        assert_eq!(dm.tracks.len(), 1);
        let t = &dm.tracks[0];
        assert_eq!(t.codec, Codec::Mpeg2);
        assert_eq!(t.track_number, Some(0xE0));
        assert_eq!((t.width, t.height), (320, 240));
        // Two pictures one second apart span 1 s and occupy 1 s + one 25 fps
        // frame interval, which is the number a duration means.
        assert_eq!(dm.duration_secs, Some(1.04));
    }

    #[test]
    fn a_span_becomes_a_duration_by_one_frame_interval() {
        assert_eq!(whole_frame_duration(Some(1.96), Some(25.0)), Some(2.0));
        // No rate to add: the bare span stands rather than a second guess.
        assert_eq!(whole_frame_duration(Some(1.96), None), Some(1.96));
        // A nonsense rate must not divide by zero into an infinite duration.
        assert_eq!(whole_frame_duration(Some(1.96), Some(0.0)), Some(1.96));
        assert_eq!(whole_frame_duration(None, Some(25.0)), None);
    }

    #[test]
    fn the_pack_variant_names_the_container_and_not_the_codec() {
        // ffmpeg's `-f mpeg` writes an ISO/IEC 11172-1 system stream that
        // carries MPEG-2 video; reporting "MPEG-1" for the codec would be the
        // classic misreading, and the two layers must stay independent.
        let d = stream(Variant::Mpeg1System, 0xE0, &[(0, &M2V_HEAD), (45_000, &M2V_HEAD)]);
        let dm = demux(&d).expect("system stream");
        assert_eq!(dm.container, "MPEG-1 System Stream");
        assert_eq!(dm.tracks[0].codec, Codec::Mpeg2);
        assert_eq!(dm.duration_secs, Some(0.54));
    }

    #[test]
    fn each_video_stream_id_becomes_its_own_track() {
        let mut d = stream(Variant::Mpeg2Program, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        let e1 = stream(Variant::Mpeg2Program, 0xE1, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        d.extend_from_slice(&e1[MPEG2_PACK.len()..]);
        let dm = demux(&d).expect("two video streams");
        assert_eq!(dm.tracks.len(), 2);
        assert_eq!(dm.tracks[0].track_number, Some(0xE0));
        assert_eq!(dm.tracks[1].track_number, Some(0xE1));
        // An overall rate counts audio and overhead, so with more than one
        // video track there is nothing it can honestly be attributed to.
        assert!(dm.tracks.iter().all(|t| t.bitrate.is_none()));
    }

    #[test]
    fn a_slice_start_code_is_not_read_as_an_mpeg4_visual_object_layer() {
        // MPEG-2's slice codes run 01..AF and overlap MPEG-4 Part 2's VOL range
        // 20..2F, so a stream taller than 512 lines emits slice 0x20 as a matter
        // of course. Treating that as a Part 2 verdict names the wrong codec and
        // hands the bytes to the wrong gap-filler; the sequence header two codes
        // earlier is what actually decides.
        let mut es = Vec::from(&M2V_HEAD[..]);
        es.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x12, 0x34]); // picture
        for slice in 0x01u8..=0x24 {
            es.extend_from_slice(&[0x00, 0x00, 0x01, slice, 0xAA, 0xBB]);
        }
        assert_eq!(classify_es(&es), Some(Codec::Mpeg2), "slice 0x20 is not a VOL");

        // And a real Part 2 stream still classifies without that range, because
        // its VOP start code rides every frame.
        let es = [0x00, 0x00, 0x01, 0xB6, 0x00, 0x00, 0x01, 0x20, 0x00, 0xC4];
        assert_eq!(classify_es(&es), Some(Codec::Mpeg4Part2));
    }

    #[test]
    fn annex_b_nal_headers_in_the_vol_range_stay_annex_b() {
        // The `20`..`2F` range is where H.264's `nal_ref_idc == 1` slices and
        // H.265's IRAP NAL headers land, so reading it as a VideoObjectLayer
        // hijacked those streams too — and Part 2's parser then fabricated a
        // resolution, frame rate, chroma and colour from their bytes.
        let mut avc = vec![0x00, 0x00, 0x00, 0x01, 0x21, 0x9A, 0x35, 0x82]; // a slice
        avc.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0D]);
        avc.extend_from_slice(&[0xAC, 0xD9, 0x41, 0x41, 0xFB, 0x01, 0x10, 0x00]);
        avc.extend_from_slice(&[0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03]);
        avc.extend_from_slice(&[0x20, 0xF1, 0x42, 0x99, 0x60]);
        assert_eq!(classify_es(&avc), Some(Codec::Avc));

        // HEVC: an IRAP NAL header (`nal_unit_type = byte >> 1`, so 0x26 is
        // type 19) ahead of the sequence parameter set.
        let mut hevc = vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x06];
        hevc.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01]);
        hevc.extend_from_slice(&[0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00]);
        hevc.extend_from_slice(&[0x03, 0x00, 0x00, 0x03, 0x00, 0x5D, 0xA0, 0x02]);
        hevc.extend_from_slice(&[0x80, 0x80, 0x2D, 0x16, 0x59, 0x59, 0xA4, 0x93]);
        hevc.extend_from_slice(&[0x2B, 0x9A, 0x80, 0x80, 0x80, 0x82, 0x00, 0x00]);
        assert_eq!(classify_es(&hevc), Some(Codec::Hevc));
    }

    #[test]
    fn an_access_unit_is_cut_at_a_start_code_and_never_mid_slice() {
        // A timestamped packet whose payload begins part-way through the
        // previous access unit must not open a chunk there: `split_annexb`
        // reads a chunk's offset 0 as a NAL boundary, so a mid-slice cut mints
        // a NAL out of compressed payload and the SEI readers parse it as
        // signalled mastering-display and content-light metadata.
        let mut es = EsOut { sid: 0xE0, buf: Vec::new(), chunks: Vec::new(), au_start: 0, started: false };
        es.push(&[0x00, 0x00, 0x01, 0xB3, 0xAA], true);
        // Four bytes of the open unit, then the next one's start code.
        es.push(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x01, 0xB3, 0xBB], true);
        es.finish();
        assert_eq!(es.chunks.len(), 2);
        assert_eq!(es.chunks[0].size, 9, "the trailing four bytes close the first unit");
        for c in &es.chunks {
            let at = c.offset as usize;
            assert_eq!(&es.buf[at..at + 3], &[0, 0, 1], "every chunk opens on a start code");
        }

        // A payload with no start code at all begins no access unit, whatever
        // its header claims, so it does not cut.
        let mut es = EsOut { sid: 0xE0, buf: Vec::new(), chunks: Vec::new(), au_start: 0, started: false };
        es.push(&[0x00, 0x00, 0x01, 0xB3, 0xAA], true);
        es.push(&[0xDE, 0xAD, 0xBE, 0xEF], true);
        es.finish();
        assert_eq!(es.chunks.len(), 1);
    }

    #[test]
    fn a_stray_timestamp_cannot_set_the_duration() {
        // The span is a min/max over a whole window, so one non-conforming
        // packet would otherwise decide it. The pack clock measures the same
        // bytes and refuses an impossible presentation span.
        let mut h = Walk::default();
        h.note_scr(0);
        h.note_scr(27_000_000 * 10); // ten seconds of arrival
        h.note_pts(0);
        h.note_pts(9 * 90_000);
        assert_eq!(pts_span(&h, None, false), Some(9.0), "a credible span survives");

        h.note_pts(90_000 * 90_000); // one stray timestamp, 25 hours out
        assert_eq!(pts_span(&h, None, false), None);

        // A window with no pack clock has no second opinion, so the check is
        // skipped rather than failed — that is the pack-less PES shape.
        let mut h = Walk::default();
        h.note_pts(0);
        h.note_pts(90_000 * 90_000);
        assert!(pts_span(&h, None, false).is_some());
    }

    #[test]
    fn a_timestamp_with_a_cleared_marker_is_refused() {
        let t = 90_000u64;
        let good = [
            0x21 | (((t >> 30) as u8 & 0x07) << 1),
            (t >> 22) as u8,
            (((t >> 15) as u8) << 1) | 1,
            (t >> 7) as u8,
            ((t as u8) << 1) | 1,
        ];
        assert_eq!(parse_timestamp(&good, 0), Some(t));
        for i in [0usize, 2, 4] {
            let mut b = good;
            b[i] &= !0x01;
            assert!(parse_timestamp(&b, 0).is_none(), "marker in byte {i}");
        }
    }

    #[test]
    fn a_declared_header_too_short_for_its_timestamp_reads_no_timestamp() {
        // `PTS_DTS_flags` says a timestamp is present but the declared header
        // has no room for one, so the five bytes read would be the packet's own
        // payload. ffmpeg rejects the same shape by decrementing its header
        // budget and bailing when it goes negative.
        for header_len in 0u8..5 {
            let mut p = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, header_len];
            p.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // a well-formed PTS
            let n = p.len() - 6;
            p[4] = (n >> 8) as u8;
            p[5] = n as u8;
            let (_, pts) = pes_payload(&p, 6, p.len()).expect("pes");
            assert_eq!(pts, None, "header_data_length {header_len}");
        }
        // Five bytes is exactly enough, and then it reads.
        let mut p = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05];
        p.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]);
        let n = p.len() - 6;
        p[4] = (n >> 8) as u8;
        p[5] = n as u8;
        assert_eq!(pes_payload(&p, 6, p.len()).expect("pes").1, Some(0));
    }

    #[test]
    fn a_pes_packet_too_short_for_its_timestamp_reads_no_timestamp() {
        // The packet declares 3 bytes of body but claims a PTS. Reading past
        // the declared extent would assemble one out of the next packet.
        let mut d = Vec::from(MPEG2_PACK);
        let at = d.len();
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x03, 0x80, 0x80, 0x05]);
        d.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // a valid PTS, but past the length
        let end = at + 6 + 3;
        assert_eq!(pes_payload(&d, at + 6, end), Some((end, None)));
    }

    /// Build an H.222.0 pack header carrying `scr` in 27 MHz units — the
    /// inverse of [`parse_pack`]'s MPEG-2 branch, so a walk over many packs can
    /// be given a real moving clock. Anchored against the corpus bytes by the
    /// assertion in `a_tail_window_never_overlaps_the_head`.
    fn mpeg2_pack(scr: u64) -> [u8; 14] {
        let (base, ext) = (scr / 300, scr % 300);
        [
            0x00,
            0x00,
            0x01,
            0xBA,
            0x40 | (((base >> 30) as u8 & 0x07) << 3) | 0x04 | ((base >> 28) as u8 & 0x03),
            (base >> 20) as u8,
            (((base >> 15) as u8 & 0x1F) << 3) | 0x04 | ((base >> 13) as u8 & 0x03),
            (base >> 5) as u8,
            ((base as u8 & 0x1F) << 3) | 0x04 | ((ext >> 7) as u8 & 0x03),
            ((ext as u8 & 0x7F) << 1) | 0x01,
            0x86,
            0x66,
            0xCF,
            0xF8,
        ]
    }

    #[test]
    fn a_tail_window_never_overlaps_the_head() {
        // The encoder is only trustworthy if it reproduces real bytes.
        assert_eq!(mpeg2_pack(0), MPEG2_PACK, "the pack encoder must match the corpus header");
        for scr in [1u64, 27_000_000, 299, 8_589_934_591] {
            let p = mpeg2_pack(scr);
            assert_eq!(parse_pack(&p, 0, p.len()).map(|(_, _, s)| s), Some(scr), "{scr}");
        }

        // A file just past the head window used to get a tail window that
        // re-read bytes the head had already walked. The tail's clock then
        // necessarily started before the head's ended, so the concatenation
        // guard fired on an ordinary file and its duration and bitrate both
        // vanished. The band is every file between 8 and 12 MiB — ~10-15 s of
        // DVD-rate content, and about a minute at VCD rate.
        let mut d = Vec::with_capacity(HEAD_SCAN_BYTES + TAIL_SCAN_BYTES);
        let (mut pts, mut scr) = (0u64, 0u64);
        while d.len() < HEAD_SCAN_BYTES + (TAIL_SCAN_BYTES / 2) {
            d.extend_from_slice(&mpeg2_pack(scr));
            let mut p = mpeg2_pes(Some(pts));
            p.extend_from_slice(&M2V_HEAD);
            p.extend_from_slice(&[0x55; 8192]);
            let n = p.len() - 6;
            p[4] = (n >> 8) as u8;
            p[5] = n as u8;
            d.extend_from_slice(&p);
            pts += 3600; // 25 fps
            scr += 27_000_000 / 25;
        }
        assert!(
            d.len() > HEAD_SCAN_BYTES && d.len() - TAIL_SCAN_BYTES < HEAD_SCAN_BYTES,
            "the fixture must land inside the overlap band"
        );
        let dm = demux(&d).expect("a file in the head/tail overlap band");
        assert!(dm.duration_secs.is_some(), "the windows must not overlap");
        assert!(dm.tracks[0].bitrate.is_some());
    }

    #[test]
    fn a_pack_less_stream_still_anchors_its_tail() {
        // The pack-less shape is one this backend reports, so its tail window
        // has to anchor on a PES packet; requiring a pack header denied every
        // such file over the head window a duration.
        let mut d = Vec::new();
        for pts in [0u64, 90_000, 180_000] {
            let mut p = mpeg2_pes(Some(pts));
            p.extend_from_slice(&M2V_HEAD);
            let n = p.len() - 6;
            p[4] = (n >> 8) as u8;
            p[5] = n as u8;
            d.extend_from_slice(&p);
        }
        let mut junk = vec![0xAB; 23];
        junk.extend_from_slice(&d);
        let w = walk(&junk, 1, junk.len(), false);
        assert_eq!(w.pts_max, Some(180_000), "a PES packet anchors the walk");
        assert!(w.variant.is_none(), "and no pack means no pack variant");
    }

    #[test]
    fn a_bounded_index_keeps_the_sampled_footnote_on() {
        // `--full` promises every access unit was read; this backend's chunks
        // cover a bounded head window only, so it must say so or the report
        // silently drops the mark that says otherwise.
        let d = stream(Variant::Mpeg2Program, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        assert!(demux(&d).expect("program stream").bounded_index);
    }

    #[test]
    fn non_video_stream_ids_are_stepped_over_by_length() {
        // A navigation pack's `private_stream_2` payload is start-code rich. It
        // is not enough that the video buffer stays clean — routing it in would
        // only create a second `EsOut`, keyed by its own id. What this pins is
        // that the walk *steps over* it by `PES_packet_length`: a byte scan
        // would read the `00 00 01 B3` inside as a packet of its own, take the
        // following bytes as a 16-bit length, and skip past the real video
        // packets entirely.
        let mut d = stream(Variant::Mpeg2Program, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        let mut nav = vec![0x00, 0x00, 0x01, SID_PRIVATE_2, 0x00, 0x10];
        nav.extend_from_slice(&[0x00, 0x00, 0x01, 0xB3, 0xFF, 0xFF, 0xFF, 0xFF]);
        nav.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0xAA, 0xBB, 0xCC, 0xDD]);
        let at = MPEG2_PACK.len();
        d.splice(at..at, nav);
        let dm = demux(&d).expect("program stream with a navigation pack");
        assert_eq!(dm.tracks.len(), 1);
        assert_eq!((dm.tracks[0].width, dm.tracks[0].height), (320, 240));
    }

    #[test]
    fn a_backward_clock_yields_no_duration() {
        let mut h = Walk::default();
        h.note_scr(27_000_000);
        h.note_scr(0);
        assert!(h.scr_backward);
        h.note_pts(0);
        h.note_pts(90_000);
        assert_eq!(pts_span(&h, None, false), None, "a reset inside one window");

        // Two windows each internally monotonic, but the tail's clock starts
        // before the head's ended: `cat a.vob b.vob`.
        let mut head = Walk::default();
        head.note_scr(0);
        head.note_scr(27_000_000);
        head.note_pts(0);
        let mut tail = Walk::default();
        tail.note_scr(0);
        tail.note_scr(27_000_000);
        tail.note_pts(180_000);
        assert_eq!(pts_span(&head, Some(&tail), false), None, "a reset between windows");

        // The same shape with a monotonic clock is a real duration.
        let mut tail = Walk::default();
        tail.note_scr(54_000_000);
        tail.note_pts(180_000);
        assert_eq!(pts_span(&head, Some(&tail), false), Some(2.0));
    }

    #[test]
    fn an_absurd_or_absent_span_yields_no_duration() {
        // One timestamp is not a span.
        let mut h = Walk::default();
        h.note_pts(0);
        assert_eq!(pts_span(&h, None, false), None, "a zero span");

        // Longer than the 33-bit clock can mean: a second undetected wrap.
        let mut h = Walk::default();
        h.note_pts(0);
        let mut t = Walk::default();
        t.note_pts((MAX_SPAN_SECS as u64 + 600) * 90_000);
        assert_eq!(pts_span(&h, Some(&t), false), None);

        // A single rollover inside the clock's range is a real duration, not an
        // error: the wrap is 26 h 30 min, so a span that crosses zero and lands
        // under the bound still describes a timeline that can exist.
        let mut h = Walk::default();
        h.note_pts(PTS_MODULUS - 90_000);
        let mut t = Walk::default();
        t.note_pts(90_000);
        assert_eq!(pts_span(&h, Some(&t), false), Some(2.0), "one rollover");

        // No timestamps at all: the pack clock is not a substitute, however
        // many packs it counted.
        let mut h = Walk::default();
        h.note_scr(0);
        h.note_scr(27_000_000 * 60);
        assert_eq!(pts_span(&h, None, false), None);
    }

    #[test]
    fn a_timestampless_tail_falls_back_only_when_the_windows_meet() {
        let mut head = Walk::default();
        head.note_pts(0);
        head.note_pts(90_000);
        let empty = Walk::default();

        // Windows that meet have read the whole file between them, so the
        // head's own maximum is evidence. This is the file barely over the head
        // window, whose tail sliver may carry no video timestamp at all.
        assert_eq!(pts_span(&head, Some(&empty), true), Some(1.0));

        // With unread bytes between them the head knows nothing about the end,
        // and a duration describing only the head window would be a wrong
        // number rather than a missing one.
        assert_eq!(pts_span(&head, Some(&empty), false), None);
    }

    #[test]
    fn the_census_refuses_bytes_that_are_not_a_program_stream() {
        assert!(!looks_like_program_stream(&[]));
        assert!(!looks_like_program_stream(&[0u8; 4096]));
        // An HEVC Annex-B head.
        assert!(!looks_like_program_stream(&[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF]));
        // A Matroska head with a plausible pack further in must not pass on
        // the strength of one start code.
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
        mkv.extend_from_slice(&[0xAA; 512]);
        mkv.extend_from_slice(&MPEG2_PACK);
        assert!(!looks_like_program_stream(&mkv));
        assert!(demux(&mkv).is_err());
    }

    #[test]
    fn the_census_accepts_both_real_pack_forms() {
        let d = stream(Variant::Mpeg2Program, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        assert!(looks_like_program_stream(&d));
        let d = stream(Variant::Mpeg1System, 0xE0, &[(0, &M2V_HEAD), (90_000, &M2V_HEAD)]);
        assert!(looks_like_program_stream(&d));
    }

    #[test]
    fn the_elementary_stream_census_beats_first_byte_routing() {
        // A picture start code opens the payload: its `0x00` passes the HEVC
        // NAL reading, so first-byte routing sends this to Annex-B. The census
        // sees the sequence header's `0xB3` and the GOP's `0xB8`, neither of
        // which can occur in an emulation-prevented stream.
        let mut es = vec![0x00, 0x00, 0x01, 0x00, 0x12, 0x34, 0x56];
        es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
        es.extend_from_slice(&M2V_HEAD);
        assert_eq!(classify_es(&es), Some(Codec::Mpeg2));
        assert!(
            matches!(
                crate::container::classify_start_code(&es),
                Some(crate::container::StreamFamily::AnnexB)
            ),
            "the first-byte discriminator really does misread these bytes"
        );
    }

    #[test]
    fn the_elementary_stream_census_separates_the_mpeg_families() {
        // MPEG-4 Part 2: a visual object sequence start code, reserved in
        // MPEG-2.
        let es = [0x00, 0x00, 0x01, 0xB0, 0x01, 0x00, 0x00, 0x01, 0xB5, 0x89];
        assert_eq!(classify_es(&es), Some(Codec::Mpeg4Part2));

        // MPEG-1: the same sequence header without the extension that would
        // make it 13818-2, followed by a GOP header.
        let mut es = Vec::from(&M2V_HEAD[..12]);
        es[7] = 0x13;
        es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
        assert_eq!(classify_es(&es), Some(Codec::Mpeg1));

        // Annex-B: no MPEG-layer code anywhere, and a parsable SPS.
        let mut es = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0D];
        es.extend_from_slice(&[0xAC, 0xD9, 0x41, 0x41, 0xFB, 0x01, 0x10, 0x00]);
        es.extend_from_slice(&[0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03]);
        es.extend_from_slice(&[0x20, 0xF1, 0x42, 0x99, 0x60]);
        assert_eq!(classify_es(&es), Some(Codec::Avc));

        // Bytes carrying no header any of them recognises name no codec.
        assert_eq!(classify_es(&[0xAA; 64]), None);
    }

    #[test]
    fn a_zero_pes_length_stops_the_walk_rather_than_spinning() {
        let mut d = Vec::from(MPEG2_PACK);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00]);
        d.extend_from_slice(&M2V_HEAD);
        let w = walk(&d, 0, d.len(), true);
        assert!(w.streams.is_empty(), "a forbidden zero length ends the walk");
    }

    #[test]
    fn a_tail_window_anchors_on_confirmed_structure() {
        let d = stream(
            Variant::Mpeg2Program,
            0xE0,
            &[(0, &M2V_HEAD), (90_000, &M2V_HEAD), (180_000, &M2V_HEAD)],
        );
        // Junk in front, so the anchor must find the pack rather than assume
        // the window starts on one.
        let mut junk = vec![0xAB; 37];
        junk.extend_from_slice(&d);
        let w = walk(&junk, 1, junk.len(), false);
        assert_eq!(w.pts_max, Some(180_000));

        // A lone pack-looking byte run with nothing chaining behind it is not
        // an anchor.
        let mut lone = vec![0xAB; 16];
        lone.extend_from_slice(&MPEG2_PACK);
        lone.extend_from_slice(&[0xCD; 64]);
        assert!(find_anchor(&lone, 1, lone.len()).is_none());
    }
}
