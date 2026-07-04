//! Minimal ISOBMFF (MP4/MOV) demuxer. Walks the box tree to recover the video
//! track's codec config (hvcC/av1C), DV config (dvcC/dvvC/dvwC), colour info, and a
//! per-sample byte-range index — from the `stbl` tables for a classic mux, or
//! from the `moof` fragments' `traf`/`tfhd`/`trun` boxes for a fragmented MP4
//! (fMP4/CMAF), whose moov tables are present but empty. Never reads sample
//! payloads here — only their offsets/sizes.

use anyhow::{bail, Context, Result};

use crate::container::{Chunk, Codec, Demux, DvConfig, NalFormat};
use crate::model::{ColorInfo, ContentLight, MasteringDisplay};

struct BoxHdr {
    typ: [u8; 4],
    start: usize,   // abs offset of the box header (first byte of the size field)
    payload: usize, // abs offset of payload start
    end: usize,     // abs offset of box end
}

// Readers are bounds-safe: a truncated/malformed box reads 0 rather than
// panicking. Callers already validate box lengths where the value matters; for
// the rest, 0 yields an empty/short result that downstream code tolerates.
fn read_u32(d: &[u8], o: usize) -> u32 {
    match d.get(o..o + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}
fn read_u16(d: &[u8], o: usize) -> u16 {
    match d.get(o..o + 2) {
        Some(b) => u16::from_be_bytes([b[0], b[1]]),
        None => 0,
    }
}
fn read_u64(d: &[u8], o: usize) -> u64 {
    match d.get(o..o + 8) {
        Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => 0,
    }
}

/// Clamp a box-declared entry count to what the box payload can actually hold,
/// so a corrupt count can't drive a multi-GB allocation or a runaway loop.
fn clamp_count(count: usize, first_entry_off: usize, entry_size: usize, box_end: usize) -> usize {
    let avail = box_end.saturating_sub(first_entry_off) / entry_size;
    count.min(avail)
}

/// Iterate child boxes within [start, end).
fn iter_boxes(d: &[u8], start: usize, end: usize) -> Vec<BoxHdr> {
    let mut out = Vec::new();
    let mut p = start;
    while p + 8 <= end {
        let size32 = read_u32(d, p) as usize;
        let typ = [d[p + 4], d[p + 5], d[p + 6], d[p + 7]];
        let (payload, box_end) = if size32 == 1 {
            if p + 16 > end {
                break;
            }
            let large = read_u64(d, p + 8) as usize;
            (p + 16, p + large)
        } else if size32 == 0 {
            (p + 8, end)
        } else {
            (p + 8, p + size32)
        };
        if box_end > end || box_end <= p {
            break;
        }
        out.push(BoxHdr { typ, start: p, payload, end: box_end });
        p = box_end;
    }
    out
}

fn find<'a>(boxes: &'a [BoxHdr], typ: &[u8; 4]) -> Option<&'a BoxHdr> {
    boxes.iter().find(|b| &b.typ == typ)
}

/// Byte extent `[start, end)` of the top-level `moov` box, read from just the
/// top-level box headers (a handful of reads at box boundaries). Used by the
/// prefetch warmer to stream a tail-located `moov` in one pipelined read on
/// network filesystems; `None` if there is no `moov`.
pub fn moov_extent(data: &[u8]) -> Option<(usize, usize)> {
    let top = iter_boxes(data, 0, data.len());
    find(&top, b"moov").map(|b| (b.payload, b.end))
}

/// Per-fragment bytes warmed from a `sidx`/`mfra` hint: enough for the `moof`'s
/// header tables (a few KiB for a seconds-long fragment), never the mdat.
const MOOF_WARM_BYTES: usize = 16 * 1024;
/// Cap on hinted fragments so a hostile index (`sidx` u16 count, `mfra` u32
/// count) can't queue tens of thousands of warm reads; a partial warm still
/// speeds the front of the moof walk.
const HINT_MAX_FRAGMENTS: usize = 4096;

/// Fragment head ranges `(offset, len)` hinted by a front `sidx` box, for the
/// prefetch warmer. Indexing a fragmented MP4 walks moof → moof via top-level
/// box headers — a serial pointer chase costing one network round-trip per
/// fragment — but a CMAF-style front `sidx` lists every fragment's byte size up
/// front, so the warmer can stream all the moof header regions concurrently
/// before `build_fragment_index` walks them. **Hint-only by design**: the
/// fragment index is still built from the moof boxes themselves, so a missing
/// or lying `sidx` costs nothing but a wasted warm. The search never chases
/// past the first `moof`/`mdat` or `search_limit` (a tail `sidx` couldn't be
/// found without the very pointer chase this avoids).
pub fn sidx_fragment_heads(data: &[u8], search_limit: usize) -> Option<Vec<(u64, usize)>> {
    // Bounded front walk for a top-level sidx; deliberately not `iter_boxes`
    // over the whole file, which would itself fault every box header in.
    let end = data.len();
    let mut p = 0usize;
    let sidx = loop {
        if p + 8 > end || p > search_limit {
            return None;
        }
        let size32 = read_u32(data, p) as usize;
        let typ = [data[p + 4], data[p + 5], data[p + 6], data[p + 7]];
        let (payload, box_end) = if size32 == 1 {
            if p + 16 > end {
                return None;
            }
            (p + 16, p + read_u64(data, p + 8) as usize)
        } else if size32 == 0 {
            (p + 8, end)
        } else {
            (p + 8, p + size32)
        };
        if box_end > end || box_end <= p {
            return None;
        }
        match &typ {
            b"sidx" => break BoxHdr { typ, start: p, payload, end: box_end },
            // Fragments started: a usable global sidx sits in front of them, and
            // walking on would *be* the per-moof pointer chase. An `mdat` is
            // different — one bounded hop — and real muxes put one before the
            // sidx (ffmpeg's global_sidx writes the first fragment as a
            // moov-indexed mdat), so it falls through to the skip.
            b"moof" => return None,
            _ => {}
        }
        p = box_end;
    };

    // sidx (FullBox): reference_ID(4) timescale(4), then EPT + first_offset at
    // 32/64 bits by version, reserved(2), reference_count(2), 12-byte entries.
    let p = sidx.payload;
    let version = *data.get(p)?;
    let (first_offset, count_off) = if version == 0 {
        (read_u32(data, p + 16) as u64, p + 22)
    } else {
        (read_u64(data, p + 20), p + 30)
    };
    let declared = read_u16(data, count_off) as usize;
    let entries = count_off + 2;
    let n = clamp_count(declared, entries, 12, sidx.end).min(HINT_MAX_FRAGMENTS);

    // Fragments chain from the first byte after the sidx box plus first_offset;
    // each entry's referenced_size advances the running offset.
    let mut off = (sidx.end as u64).saturating_add(first_offset);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let word = read_u32(data, entries + i * 12);
        let size = (word & 0x7FFF_FFFF) as u64;
        if size == 0 {
            break; // degenerate entry; stop rather than warm one spot repeatedly
        }
        // reference_type 1 points at a nested sidx, not media; skip its head
        // but keep advancing the chain.
        if word & 0x8000_0000 == 0 {
            out.push((off, (size as usize).min(MOOF_WARM_BYTES)));
        }
        off = off.saturating_add(size);
    }
    (!out.is_empty()).then_some(out)
}

/// Fragment head ranges `(offset, len)` hinted by a tail `mfra` box, for the
/// prefetch warmer — the random-access index most recorders/muxers write when
/// there is no `sidx` (e.g. GPAC). Located in O(1) via the trailing `mfro`
/// box, whose last u32 is the whole `mfra`'s size; each `tfra` child lists
/// absolute `moof` offsets. The same **hint-only** contract as
/// `sidx_fragment_heads`: the fragment index is still built from the moof
/// boxes, so a wrong or missing `mfra` costs nothing but a wasted warm.
pub fn mfra_fragment_heads(data: &[u8]) -> Option<Vec<(u64, usize)>> {
    let len = data.len();
    if len < 16 {
        return None;
    }
    // Trailing mfro: size(4) 'mfro' version/flags(4) mfra_size(4).
    if read_u32(data, len - 16) != 16 || &data[len - 12..len - 8] != b"mfro" {
        return None;
    }
    let mfra_size = read_u32(data, len - 4) as usize;
    let mfra_start = len.checked_sub(mfra_size)?;
    let hdr = iter_boxes(data, mfra_start, len);
    let mfra = find(&hdr, b"mfra")?;

    let mut offsets: Vec<u64> = Vec::new();
    for tfra in iter_boxes(data, mfra.payload, mfra.end).iter().filter(|b| &b.typ == b"tfra") {
        // tfra (FullBox): track_ID(4), reserved + 2-bit lengths of the
        // traf/trun/sample numbers (4), number_of_entry(4), then entries of
        // (time, moof_offset) at 32/64 bits by version plus the three numbers.
        let p = tfra.payload;
        let version = *data.get(p)?;
        let lengths = read_u32(data, p + 8);
        let num_bytes =
            ((lengths >> 4) & 3) as usize + ((lengths >> 2) & 3) as usize + (lengths & 3) as usize + 3;
        let stride = if version == 1 { 16 } else { 8 } + num_bytes;
        let declared = read_u32(data, p + 12) as usize;
        let entries = p + 16;
        let n = clamp_count(declared, entries, stride, tfra.end).min(HINT_MAX_FRAGMENTS);
        for i in 0..n {
            let e = entries + i * stride;
            let moof_off = if version == 1 { read_u64(data, e + 8) } else { read_u32(data, e + 4) as u64 };
            if moof_off > 0 && moof_off < len as u64 {
                offsets.push(moof_off);
            }
        }
    }
    if offsets.is_empty() {
        return None;
    }
    // Several tracks' tfra entries usually name the same moofs; dedupe so the
    // warm list stays one range per fragment (warm_ranges would merge overlaps
    // anyway, but not distant duplicates interleaved by track).
    offsets.sort_unstable();
    offsets.dedup();
    offsets.truncate(HINT_MAX_FRAGMENTS);
    Some(offsets.into_iter().map(|o| (o, MOOF_WARM_BYTES)).collect())
}

/// QuickTime (`.mov`) and MP4 share the ISOBMFF box structure, so one backend
/// reads both. They differ only in the `ftyp` major brand: a QuickTime-native
/// mux stamps `qt  `, and MediaInfo labels such files "QuickTime". Report that
/// distinction when the brand is present; otherwise (including a brandless
/// legacy QuickTime file that opens straight into `moov`) fall back to the
/// generic ISOBMFF label rather than guess.
fn container_label(top: &[BoxHdr], data: &[u8]) -> &'static str {
    match find(top, b"ftyp") {
        Some(ftyp) if data.get(ftyp.payload..ftyp.payload + 4) == Some(b"qt  ") => {
            "QuickTime (MOV)"
        }
        _ => "MP4 (ISOBMFF)",
    }
}

pub fn demux(data: &[u8]) -> Result<Demux> {
    let top = iter_boxes(data, 0, data.len());
    let moov = find(&top, b"moov").context("no moov box (not a valid MP4)")?;
    let moov_boxes = iter_boxes(data, moov.payload, moov.end);

    // movie timescale (mvhd) for duration.
    let mut movie_timescale = 0u32;
    let mut movie_duration = 0u64;
    if let Some(mvhd) = find(&moov_boxes, b"mvhd") {
        let v = data[mvhd.payload];
        if v == 1 {
            movie_timescale = read_u32(data, mvhd.payload + 20);
            movie_duration = read_u64(data, mvhd.payload + 24);
        } else {
            movie_timescale = read_u32(data, mvhd.payload + 12);
            movie_duration = read_u32(data, mvhd.payload + 16) as u64;
        }
    }

    // Collect every video track. Profile 7 dual-track MP4s carry the base layer
    // (BL) and the Dolby Vision enhancement layer (EL, tagged `dvhe`/`dvh1` with a
    // dvcC box) as two separate `trak`s; we merge them into one logical stream.
    let mut tracks: Vec<VideoTrack> = Vec::new();
    for trak in moov_boxes.iter().filter(|b| &b.typ == b"trak") {
        let trak_boxes = iter_boxes(data, trak.payload, trak.end);
        let Some(mdia) = find(&trak_boxes, b"mdia") else { continue };
        let mdia_boxes = iter_boxes(data, mdia.payload, mdia.end);

        // handler must be 'vide'
        let is_video = find(&mdia_boxes, b"hdlr")
            .map(|h| &data[h.payload + 8..h.payload + 12] == b"vide")
            .unwrap_or(false);
        if !is_video {
            continue;
        }

        // Track id from tkhd — a fragment's tfhd names its track by this id.
        let track_id = find(&trak_boxes, b"tkhd")
            .map(|t| {
                let v = *data.get(t.payload).unwrap_or(&0);
                let o = if v == 1 { t.payload + 20 } else { t.payload + 12 };
                read_u32(data, o)
            })
            .unwrap_or(0);

        if let Some(t) =
            parse_video_track(data, &mdia_boxes, movie_timescale, movie_duration, track_id)?
        {
            tracks.push(t);
        }
    }

    if tracks.is_empty() {
        bail!("no video track found in MP4");
    }

    // Fragmented MP4 (fMP4/CMAF): the moov's stbl tables are present but empty —
    // the sample layout lives in the moof fragments' trun boxes. Fill each empty
    // track's index from the fragments; the chunks still point into the mmap, so
    // the zero-copy model holds (data offsets resolve against the enclosing moof
    // or an explicit base offset, never a reassembled buffer).
    if tracks.iter().any(|t| t.chunks.is_empty()) && top.iter().any(|b| &b.typ == b"moof") {
        let (trex, mehd_duration) = parse_mvex(data, &moov_boxes);
        for t in &mut tracks {
            if !t.chunks.is_empty() {
                continue;
            }
            let (chunks, duration_ticks) = build_fragment_index(data, &top, t.track_id, &trex);
            t.chunks = chunks;
            // The summed trun sample durations are the track's exact own duration
            // (the bitrate denominator); the Duration line keeps the mvhd value,
            // falling back to this sum and then the mvex mehd when mvhd is empty.
            if duration_ticks > 0 && t.media_timescale > 0 {
                t.stream_duration_secs =
                    Some(duration_ticks as f64 / t.media_timescale as f64);
            }
            if t.duration_secs.is_none() {
                t.duration_secs = t.stream_duration_secs.or(
                    if mehd_duration > 0 && movie_timescale > 0 {
                        Some(mehd_duration as f64 / movie_timescale as f64)
                    } else {
                        None
                    },
                );
            }
            // fps: prefer the summed sample durations (exact, media timescale);
            // fall back to sample count over the container duration.
            if t.fps.is_none() && !t.chunks.is_empty() {
                t.fps = if duration_ticks > 0 && t.media_timescale > 0 {
                    Some(t.chunks.len() as f64 * t.media_timescale as f64 / duration_ticks as f64)
                } else {
                    t.duration_secs.filter(|d| *d > 0.0).map(|d| t.chunks.len() as f64 / d)
                };
            }
        }
    }

    Ok(assemble_tracks(data, tracks, container_label(&top, data)))
}

/// One parsed video track: its sample description plus a per-sample byte index.
struct VideoTrack {
    sd: SampleDesc,
    chunks: Vec<Chunk>,
    fps: Option<f64>,
    duration_secs: Option<f64>,
    /// The track's *own* playback duration, when it is known exactly — the mdhd
    /// media duration, or for a fragmented mux the summed trun sample durations.
    /// Used as the bitrate denominator (matching MediaInfo, and the same
    /// track-own-duration preference as the MKV `BPS` tag); `duration_secs` may
    /// instead be the whole-presentation mvhd value shown on the Duration line.
    stream_duration_secs: Option<f64>,
    track_id: u32,
    media_timescale: u32,
}

/// Fold one or more video tracks into a single `Demux`. For the common
/// single-track case this is a straight pass-through. For a Profile 7 dual-track
/// pair, the widest track is the base layer (its dimensions/colour describe the
/// picture), the DV config comes from whichever track carries a dvcC/dvvC box (the
/// EL), and both tracks' samples are concatenated so the RPU — which rides the EL —
/// is scanned alongside the base layer.
fn assemble_tracks(data: &[u8], tracks: Vec<VideoTrack>, container: &'static str) -> Demux {
    let primary = tracks
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.sd.width as u64 * t.sd.height as u64)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let p = &tracks[primary];

    // DV config / static HDR from whichever track supplies them.
    let dv_config = tracks.iter().find_map(|t| t.sd.dv_config.clone());
    let mastering = tracks.iter().find_map(|t| t.sd.mastering.clone());
    let content_light = tracks.iter().find_map(|t| t.sd.content_light);
    // Colour: prefer any track whose signalling actually resolved (a bare BL may
    // omit its colr box / carry only an SPS the base parse can't reach).
    let color = tracks
        .iter()
        .find(|t| t.sd.color.transfer.is_some())
        .map(|t| t.sd.color.clone())
        .unwrap_or_else(|| p.sd.color.clone());

    // Concatenate chunks from the base layer and any EL track sharing its NAL
    // length prefix size (mixing sizes would misread the length fields).
    let nal_len = p.sd.nal_len;
    let mut chunks = p.chunks.clone();
    for (i, t) in tracks.iter().enumerate() {
        if i != primary && t.sd.nal_len == nal_len {
            chunks.extend_from_slice(&t.chunks);
        }
    }

    // Last resort for colour: some dual-track BLs carry no colr box and an hvcC
    // whose stored SPS the base parse can't reach. Recover the VUI colour from an
    // in-band SPS in the base-layer samples (the `hev1` case), as TS does.
    let color = if color.transfer.is_none() {
        color_from_stream(data, &p.chunks, p.sd.nal_len).unwrap_or(color)
    } else {
        color
    };

    // The stsz table (or the fragment trun index) gives every sample's encoded
    // size, so the concatenated chunks are the exact video-stream byte count —
    // no sample data was read. Divide by the track's own duration when known.
    let stream_bytes = chunks.iter().map(|c| c.size).sum::<u64>();
    let bitrate = crate::model::Bitrate::video_stream(
        stream_bytes,
        p.stream_duration_secs.or(p.duration_secs),
    );

    // More than one video track means a Profile-7 base/enhancement pair muxed as
    // separate `trak`s (dual track); a single track holds an interleaved or
    // single-layer stream. (`el_present` decides whether this is surfaced.)
    let dv_dual_track = tracks.len() > 1;

    Demux {
        container,
        codec: p.sd.codec.clone(),
        nal_format: NalFormat::LengthPrefixed(nal_len),
        width: p.sd.width,
        height: p.sd.height,
        fps: p.fps,
        duration_secs: p.duration_secs,
        bit_depth: p.sd.bit_depth,
        chroma: p.sd.chroma.clone(),
        codec_profile: p.sd.codec_profile.clone(),
        stereo: tracks.iter().find_map(|t| t.sd.stereo.clone()),
        color,
        dv_config,
        dv_dual_track,
        mastering,
        content_light,
        bitrate,
        chunks,
        sps_chunk: None,
        reassembled: None,
    }
}

fn parse_video_track(
    data: &[u8],
    mdia_boxes: &[BoxHdr],
    movie_timescale: u32,
    movie_duration: u64,
    track_id: u32,
) -> Result<Option<VideoTrack>> {
    // media timescale / duration
    let (media_timescale, media_duration) = match find(mdia_boxes, b"mdhd") {
        Some(mdhd) => {
            let v = data[mdhd.payload];
            if v == 1 {
                (read_u32(data, mdhd.payload + 20), read_u64(data, mdhd.payload + 24))
            } else {
                (read_u32(data, mdhd.payload + 12), read_u32(data, mdhd.payload + 16) as u64)
            }
        }
        None => (0, 0),
    };

    let minf = match find(mdia_boxes, b"minf") {
        Some(b) => b,
        None => return Ok(None),
    };
    let minf_boxes = iter_boxes(data, minf.payload, minf.end);
    let stbl = match find(&minf_boxes, b"stbl") {
        Some(b) => b,
        None => return Ok(None),
    };
    let stbl_boxes = iter_boxes(data, stbl.payload, stbl.end);

    let stsd = find(&stbl_boxes, b"stsd").context("no stsd box")?;
    let sd = parse_stsd(data, stsd)?;

    // Sample index from stbl tables.
    let chunks = build_sample_index(data, &stbl_boxes, sd.codec.clone())?;
    let sample_count = chunks.len() as u64;

    // Duration / fps.
    let stream_duration_secs = if media_timescale > 0 && media_duration > 0 {
        Some(media_duration as f64 / media_timescale as f64)
    } else {
        None
    };
    let duration_secs = stream_duration_secs.or(if movie_timescale > 0 && movie_duration > 0 {
        Some(movie_duration as f64 / movie_timescale as f64)
    } else {
        None
    });
    let fps = match (duration_secs, sample_count) {
        (Some(d), n) if d > 0.0 && n > 0 => Some(n as f64 / d),
        _ => None,
    };

    Ok(Some(VideoTrack {
        sd,
        chunks,
        fps,
        duration_secs,
        stream_duration_secs,
        track_id,
        media_timescale,
    }))
}

struct SampleDesc {
    codec: Codec,
    codec_profile: Option<String>,
    width: u32,
    height: u32,
    bit_depth: Option<u8>,
    chroma: Option<String>,
    nal_len: u8,
    color: ColorInfo,
    dv_config: Option<DvConfig>,
    stereo: Option<String>,
    mastering: Option<MasteringDisplay>,
    content_light: Option<ContentLight>,
}

fn parse_stsd(data: &[u8], stsd: &BoxHdr) -> Result<SampleDesc> {
    // stsd: version(1)+flags(3)+entry_count(4), then entries.
    let entries_start = stsd.payload + 8;
    let entries = iter_boxes(data, entries_start, stsd.end);
    let entry = entries.first().context("empty stsd")?;

    let format = entry.typ;
    let codec = match &format {
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => Codec::Hevc,
        b"avc1" | b"avc3" | b"dva1" | b"dvav" => Codec::Avc,
        b"av01" | b"dav1" => Codec::Av1,
        other => Codec::Other(String::from_utf8_lossy(other).to_string()),
    };

    // VisualSampleEntry: width/height at box offset 32/34; child boxes at 86.
    let entry_box_start = entry.start;
    let width = read_u16(data, entry_box_start + 32) as u32;
    let height = read_u16(data, entry_box_start + 34) as u32;
    let children = iter_boxes(data, entry_box_start + 86, entry.end);

    let mut bit_depth = None;
    let mut chroma = None;
    let mut codec_profile = None;
    let mut nal_len = 4u8;
    let mut color = ColorInfo::default();
    let mut dv_config = None;
    let mut mastering = None;
    let mut content_light = None;
    let mut hvcc_bytes: Option<&[u8]> = None;
    let mut avcc_bytes: Option<&[u8]> = None;
    // A layered-HEVC config box (`lhvC`) beside the base `hvcC` marks MV-HEVC — the
    // multiview form of DV Profile 20 (for 3D / dual-view); its absence is the 2D
    // single-view form. Free to detect: the box is already a sample-entry child.
    let mut layered = false;
    // Stereo view structure from the `vexu` extended-usage box (also a child).
    let mut stereo = None;

    for c in &children {
        match &c.typ {
            b"hvcC" => {
                hvcc_bytes = Some(&data[c.payload..c.end]);
                if let Some(h) = super::parse_hvcc_record(&data[c.payload..c.end]) {
                    bit_depth = Some(h.bit_depth);
                    chroma = Some(h.chroma.to_string());
                    nal_len = h.nal_len;
                    codec_profile = Some(h.profile_str);
                }
            }
            b"avcC" => {
                avcc_bytes = Some(&data[c.payload..c.end]);
                if let Some(a) = super::parse_avcc_record(&data[c.payload..c.end]) {
                    bit_depth = Some(a.bit_depth);
                    chroma = Some(a.chroma.to_string());
                    nal_len = a.nal_len;
                    codec_profile = Some(a.profile_str);
                }
            }
            b"av1C" => {
                if let Some((bd, ch, prof)) = parse_av1c(data, c) {
                    bit_depth = Some(bd);
                    chroma = Some(ch.to_string());
                    codec_profile = Some(prof);
                }
            }
            // dvcC/dvvC carry the DV config for the usual single-view profiles;
            // dvwC is Profile 20 (MV-HEVC, `dvh1` sample entry) — same record layout.
            b"dvcC" | b"dvvC" | b"dvwC" => {
                dv_config = super::parse_dovi_config(&data[c.payload..c.end])
            }
            b"lhvC" => layered = true,
            b"vexu" => stereo = parse_stereo(data, c).or(stereo),
            b"colr" => color = parse_colr(data, c).unwrap_or(color),
            b"mdcv" | b"SmDm" => mastering = parse_mdcv(data, c).or(mastering),
            b"clli" | b"CoLL" => content_light = parse_clli(data, c).or(content_light),
            _ => {}
        }
    }

    // Prefix the base profile ("Main 10, High tier @ L5") to match mediainfo's
    // "Multiview Main 10@L5@High" when the second HEVC layer (`lhvC`) is present.
    if layered {
        if let Some(p) = codec_profile.take() {
            codec_profile = Some(format!("Multiview {p}"));
        }
    }

    // No `colr` box? Recover colour from the SPS in `hvcC` / `avcC`.
    if color.transfer.is_none() {
        if let Some(h) = hvcc_bytes {
            if let Some(c) = super::color_from_hvcc(h) {
                color = c;
            }
        } else if let Some(a) = avcc_bytes {
            if let Some(c) = super::color_from_avcc(a) {
                color = c;
            }
        }
    }

    Ok(SampleDesc {
        codec,
        codec_profile,
        width,
        height,
        bit_depth,
        chroma,
        nal_len,
        color,
        dv_config,
        stereo,
        mastering,
        content_light,
    })
}

/// Decode the stereoscopic view structure from a `vexu` (Video Extended Usage)
/// box: descend to its `eyes` → `stri` (Stereo View Information) child and read
/// the eye-view flags. MV-HEVC (DV Profile 20 for 3D) signals a stereo pair here.
/// A plain container-box walk, all within the sample entry already in hand.
fn parse_stereo(data: &[u8], vexu: &BoxHdr) -> Option<String> {
    let eyes = iter_boxes(data, vexu.payload, vexu.end)
        .into_iter()
        .find(|b| &b.typ == b"eyes")?;
    let stri = iter_boxes(data, eyes.payload, eyes.end)
        .into_iter()
        .find(|b| &b.typ == b"stri")?;
    // stri is a FullBox: version(1)+flags(3), then one byte of eye-view flags —
    // bit0 left, bit1 right, bit2 additional views present, bit3 views reversed.
    let flags = *data.get(stri.payload + 4)?;
    let left = flags & 0x01 != 0;
    let right = flags & 0x02 != 0;
    let additional = flags & 0x04 != 0;
    if additional {
        // More than a plain L/R pair; stri alone can't state the exact count.
        return Some("Multiview 3D (2+ views)".to_string());
    }
    match (left, right) {
        (true, true) => Some("Stereoscopic 3D (2 views)".to_string()),
        (true, false) | (false, true) => Some("Monoscopic (1 view)".to_string()),
        (false, false) => None,
    }
}

/// Recover VUI colour from an in-band SPS in the first few samples of a track.
/// Used when the container carries neither a `colr` box nor an hvcC SPS the base
/// parser can reach — the base layer of some Profile 7 dual-track MP4s.
fn color_from_stream(data: &[u8], chunks: &[Chunk], nal_len: u8) -> Option<ColorInfo> {
    use crate::hevc::nal;
    let mut nals = Vec::new();
    for ch in chunks.iter().take(8) {
        let s = ch.offset as usize;
        let e = (s + ch.size as usize).min(data.len());
        if s >= e {
            continue;
        }
        nals.clear();
        nal::split_length_prefixed(&data[s..e], nal_len, &mut nals);
        for n in &nals {
            if n.nal_type == nal::NAL_SPS {
                if let Some(info) = crate::hevc::sps::parse_sps(&data[s + n.start..s + n.end]) {
                    if let Some(vui) = info.color.as_ref() {
                        return Some(super::color_from_vui(vui));
                    }
                }
            }
        }
    }
    None
}

fn parse_av1c(data: &[u8], b: &BoxHdr) -> Option<(u8, &'static str, String)> {
    if b.end < b.payload {
        return None;
    }
    super::parse_av1c_record(&data[b.payload..b.end])
}

fn parse_colr(data: &[u8], b: &BoxHdr) -> Option<ColorInfo> {
    let p = b.payload;
    if b.end < p + 4 {
        return None;
    }
    let kind = &data[p..p + 4];
    if kind == b"nclx" || kind == b"nclc" {
        if b.end < p + 10 {
            return None;
        }
        let primaries = read_u16(data, p + 4);
        let transfer = read_u16(data, p + 6);
        let matrix = read_u16(data, p + 8);
        let range = if kind == b"nclx" {
            let full = (data[p + 10] & 0x80) != 0;
            Some(if full { "full".to_string() } else { "limited".to_string() })
        } else {
            None
        };
        return Some(ColorInfo {
            primaries: super::cicp_primaries(primaries).map(str::to_string),
            transfer: super::cicp_transfer(transfer).map(str::to_string),
            matrix: super::cicp_matrix(matrix).map(str::to_string),
            range,
        });
    }
    None
}

fn parse_mdcv(data: &[u8], b: &BoxHdr) -> Option<MasteringDisplay> {
    // mdcv: 3x(primary x,y u16), white x,y u16, max(4) min(4) luminance —
    // ST.2086 layout: G/B/R primary order, chromaticities in 0.00002 units.
    let p = b.payload;
    if b.end < p + 24 {
        return None;
    }
    let xy = |o: usize| {
        (read_u16(data, p + o) as f64 / 50000.0, read_u16(data, p + o + 2) as f64 / 50000.0)
    };
    // primaries_label takes R, G, B, white; the box stores G, B, R, white.
    let primaries = crate::hdr::primaries_label(xy(8), xy(0), xy(4), xy(12));
    let max_lum = read_u32(data, p + 16); // units 0.0001 cd/m²
    let min_lum = read_u32(data, p + 20);
    Some(MasteringDisplay {
        max_luminance: max_lum as f64 / 10000.0,
        min_luminance: min_lum as f64 / 10000.0,
        primaries: primaries.map(str::to_string),
        primaries_level: None,
    })
}

fn parse_clli(data: &[u8], b: &BoxHdr) -> Option<ContentLight> {
    let p = b.payload;
    if b.end < p + 4 {
        return None;
    }
    Some(ContentLight::new(read_u16(data, p), read_u16(data, p + 2)))
}

/// Build a per-sample byte-range index from stsc/stco/co64/stsz tables.
fn build_sample_index(data: &[u8], stbl: &[BoxHdr], _codec: Codec) -> Result<Vec<Chunk>> {
    // Sample sizes.
    let stsz = find(stbl, b"stsz").context("no stsz box")?;
    let p = stsz.payload;
    let default_size = read_u32(data, p + 4);
    let sample_count = read_u32(data, p + 8) as usize;
    let sizes: Vec<u32> = if default_size == 0 {
        let n = clamp_count(sample_count, p + 12, 4, stsz.end);
        (0..n).map(|i| read_u32(data, p + 12 + i * 4)).collect()
    } else {
        Vec::new()
    };
    // With an explicit size table, the real sample count is what actually fit.
    let sample_count = if default_size == 0 { sizes.len() } else { sample_count };
    let size_at = |i: usize| if default_size != 0 { default_size } else { sizes[i] };

    // Chunk offsets.
    let chunk_offsets: Vec<u64> = if let Some(stco) = find(stbl, b"stco") {
        let cp = stco.payload;
        let n = clamp_count(read_u32(data, cp + 4) as usize, cp + 8, 4, stco.end);
        (0..n).map(|i| read_u32(data, cp + 8 + i * 4) as u64).collect()
    } else if let Some(co64) = find(stbl, b"co64") {
        let cp = co64.payload;
        let n = clamp_count(read_u32(data, cp + 4) as usize, cp + 8, 8, co64.end);
        (0..n).map(|i| read_u64(data, cp + 8 + i * 8)).collect()
    } else {
        bail!("no stco/co64 box");
    };

    // Sample-to-chunk.
    let stsc = find(stbl, b"stsc").context("no stsc box")?;
    let sp = stsc.payload;
    let stsc_n = clamp_count(read_u32(data, sp + 4) as usize, sp + 8, 12, stsc.end);
    // entries: (first_chunk, samples_per_chunk, sample_desc_index)
    let stsc_entries: Vec<(u32, u32)> = (0..stsc_n)
        .map(|i| {
            let o = sp + 8 + i * 12;
            (read_u32(data, o), read_u32(data, o + 4))
        })
        .collect();

    // Cap the pre-allocation: with a constant sample size the declared count is
    // unvalidated, so a corrupt value must not drive a huge up-front alloc.
    let mut chunks = Vec::with_capacity(sample_count.min(1 << 20));
    let mut sample_idx = 0usize;
    for (ci, &chunk_off) in chunk_offsets.iter().enumerate() {
        let chunk_no = (ci + 1) as u32; // 1-based
        // samples_per_chunk = last stsc entry whose first_chunk <= chunk_no
        let spc = stsc_entries
            .iter()
            .rev()
            .find(|(fc, _)| *fc <= chunk_no)
            .map(|(_, s)| *s)
            .unwrap_or(0);
        let mut off = chunk_off;
        for _ in 0..spc {
            if sample_idx >= sample_count {
                break;
            }
            let sz = size_at(sample_idx) as u64;
            chunks.push(Chunk { offset: off, size: sz });
            off += sz;
            sample_idx += 1;
        }
    }

    Ok(chunks)
}

/// Per-track sample defaults from a `trex` box, applied to fragment samples that
/// carry neither a per-sample value in their `trun` nor a `tfhd` default.
struct TrexDefault {
    track_id: u32,
    sample_duration: u32,
    sample_size: u32,
}

/// Movie-fragment defaults from the moov's `mvex`: the per-track `trex` entries
/// plus the `mehd` whole-presentation duration (movie-timescale ticks, 0 when
/// absent). An fMP4 without an `mvex` yields empty defaults, not an error.
fn parse_mvex(data: &[u8], moov_boxes: &[BoxHdr]) -> (Vec<TrexDefault>, u64) {
    let mut trex = Vec::new();
    let mut mehd_duration = 0u64;
    let Some(mvex) = find(moov_boxes, b"mvex") else {
        return (trex, mehd_duration);
    };
    for c in &iter_boxes(data, mvex.payload, mvex.end) {
        match &c.typ {
            b"mehd" => {
                let v = *data.get(c.payload).unwrap_or(&0);
                mehd_duration = if v == 1 {
                    read_u64(data, c.payload + 4)
                } else {
                    read_u32(data, c.payload + 4) as u64
                };
            }
            // trex: version/flags, track_ID, default_sample_description_index,
            // default_sample_duration, default_sample_size, default_sample_flags.
            b"trex" => trex.push(TrexDefault {
                track_id: read_u32(data, c.payload + 4),
                sample_duration: read_u32(data, c.payload + 12),
                sample_size: read_u32(data, c.payload + 16),
            }),
            _ => {}
        }
    }
    (trex, mehd_duration)
}

/// Track-fragment header: which track this `traf` extends, plus the sample
/// defaults and data-offset base that its `trun` runs resolve against.
struct Tfhd {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_base_is_moof: bool,
    default_sample_duration: Option<u32>,
    default_sample_size: Option<u32>,
}

fn parse_tfhd(data: &[u8], b: &BoxHdr) -> Tfhd {
    let p = b.payload;
    let flags = read_u32(data, p) & 0x00FF_FFFF;
    let track_id = read_u32(data, p + 4);
    let mut off = p + 8;
    let mut base_data_offset = None;
    if flags & 0x1 != 0 {
        base_data_offset = Some(read_u64(data, off));
        off += 8;
    }
    if flags & 0x2 != 0 {
        off += 4; // sample_description_index
    }
    let mut default_sample_duration = None;
    if flags & 0x8 != 0 {
        default_sample_duration = Some(read_u32(data, off));
        off += 4;
    }
    let mut default_sample_size = None;
    if flags & 0x10 != 0 {
        default_sample_size = Some(read_u32(data, off));
    }
    Tfhd {
        track_id,
        base_data_offset,
        default_base_is_moof: flags & 0x0002_0000 != 0,
        default_sample_duration,
        default_sample_size,
    }
}

/// Build the per-sample byte index for one track of a fragmented MP4 by walking
/// every top-level `moof`'s `traf`/`tfhd`/`trun` boxes. Every traf is walked (not
/// just the target track's) because a traf without an explicit base offset chains
/// implicitly off the end of the previous traf's data. Returns the chunks plus
/// the summed per-sample durations in media-timescale ticks (0 when no duration
/// signal exists), so the caller can derive duration/fps when the moov carries
/// neither. Metadata-only: reads box headers and tables, never sample payloads.
fn build_fragment_index(
    data: &[u8],
    top: &[BoxHdr],
    track_id: u32,
    trex: &[TrexDefault],
) -> (Vec<Chunk>, u64) {
    let mut chunks = Vec::new();
    let mut duration_ticks = 0u64;

    for moof in top.iter().filter(|b| &b.typ == b"moof") {
        let moof_boxes = iter_boxes(data, moof.payload, moof.end);
        // Data-offset base for a traf that declares none: the moof start for the
        // first, then the running end of the previous traf's data.
        let mut implicit_base = moof.start as u64;
        for traf in moof_boxes.iter().filter(|b| &b.typ == b"traf") {
            let traf_boxes = iter_boxes(data, traf.payload, traf.end);
            let Some(tfhd_box) = find(&traf_boxes, b"tfhd") else { continue };
            let tfhd = parse_tfhd(data, tfhd_box);
            let tdef = trex.iter().find(|t| t.track_id == tfhd.track_id);
            let default_size =
                tfhd.default_sample_size.or(tdef.map(|t| t.sample_size));
            let default_duration =
                tfhd.default_sample_duration.or(tdef.map(|t| t.sample_duration));
            let is_target = tfhd.track_id == track_id;

            let base = tfhd.base_data_offset.unwrap_or(if tfhd.default_base_is_moof {
                moof.start as u64
            } else {
                implicit_base
            });
            let mut off = base;
            for trun in traf_boxes.iter().filter(|b| &b.typ == b"trun") {
                let p = trun.payload;
                let flags = read_u32(data, p) & 0x00FF_FFFF;
                let declared = read_u32(data, p + 4) as usize;
                let mut ep = p + 8;
                if flags & 0x1 != 0 {
                    // data_offset is signed, relative to the traf's base.
                    let doff = read_u32(data, ep) as i32;
                    off = (base as i64 + doff as i64).max(0) as u64;
                    ep += 4;
                }
                if flags & 0x4 != 0 {
                    ep += 4; // first_sample_flags
                }
                let has_duration = flags & 0x100 != 0;
                let has_size = flags & 0x200 != 0;
                if !has_size && default_size.is_none() {
                    continue; // no way to size these samples
                }
                // Per-sample entry stride: duration, size, flags, cto — each 4
                // bytes when its flag is set, in that order.
                let stride = 4 * ((has_duration as usize)
                    + (has_size as usize)
                    + ((flags & 0x400 != 0) as usize)
                    + ((flags & 0x800 != 0) as usize));
                let count = if stride > 0 {
                    clamp_count(declared, ep, stride, trun.end)
                } else {
                    // All-default samples carry no entry bytes to clamp against;
                    // cap the count so a corrupt value can't drive a huge alloc.
                    declared.min(1 << 20)
                };
                for i in 0..count {
                    let e = ep + i * stride;
                    let duration = if has_duration {
                        read_u32(data, e)
                    } else {
                        default_duration.unwrap_or(0)
                    };
                    let size = if has_size {
                        read_u32(data, e + if has_duration { 4 } else { 0 })
                    } else {
                        default_size.unwrap_or(0)
                    } as u64;
                    if is_target {
                        chunks.push(Chunk { offset: off, size });
                        duration_ticks += duration as u64;
                    }
                    off += size;
                }
            }
            implicit_base = off;
        }
    }
    (chunks, duration_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dv7() -> DvConfig {
        DvConfig {
            profile: 7,
            level: Some(6),
            bl_present: true,
            el_present: true,
            rpu_present: true,
            bl_compatibility_id: Some(0),
        }
    }

    fn track(w: u32, h: u32, nal_len: u8, dv: Option<DvConfig>, chunks: usize) -> VideoTrack {
        VideoTrack {
            sd: SampleDesc {
                codec: Codec::Hevc,
                codec_profile: None,
                width: w,
                height: h,
                bit_depth: Some(10),
                chroma: Some("4:2:0".to_string()),
                nal_len,
                color: ColorInfo::default(),
                dv_config: dv,
                stereo: None,
                mastering: None,
                content_light: None,
            },
            chunks: (0..chunks).map(|i| Chunk { offset: i as u64, size: 1 }).collect(),
            fps: Some(24.0),
            duration_secs: Some(1.0),
            stream_duration_secs: None,
            track_id: 1,
            media_timescale: 0,
        }
    }

    #[test]
    fn single_track_passes_through() {
        let d = assemble_tracks(&[], vec![track(3840, 2160, 4, Some(dv7()), 5)], "MP4 (ISOBMFF)");
        assert_eq!((d.width, d.height), (3840, 2160));
        assert_eq!(d.dv_config.unwrap().profile, 7);
        assert_eq!(d.chunks.len(), 5);
    }

    #[test]
    fn dual_track_takes_bl_dims_el_dvconfig_and_merges_chunks() {
        // BL (4K, no dvcC) listed first; EL (1080p, dvcC) second — as in a real
        // Profile 7 dual-track MP4.
        let bl = track(3840, 2160, 4, None, 3);
        let el = track(1920, 1080, 4, Some(dv7()), 2);
        let d = assemble_tracks(&[], vec![bl, el], "MP4 (ISOBMFF)");
        assert_eq!((d.width, d.height), (3840, 2160), "dims from the base layer");
        assert_eq!(d.dv_config.as_ref().unwrap().profile, 7, "DV config from the EL");
        assert_eq!(d.chunks.len(), 5, "BL + EL samples both scanned for the RPU");
    }

    #[test]
    fn el_with_mismatched_nal_len_is_not_concatenated() {
        // A different length-prefix size would misread the EL's NAL lengths, so it
        // must not be blindly appended; the DV config is still recovered.
        let bl = track(3840, 2160, 4, None, 3);
        let el = track(1920, 1080, 2, Some(dv7()), 2);
        let d = assemble_tracks(&[], vec![bl, el], "MP4 (ISOBMFF)");
        assert_eq!(d.chunks.len(), 3, "mismatched-nal-len EL chunks skipped");
        assert!(d.dv_config.is_some());
        assert!(matches!(d.nal_format, NalFormat::LengthPrefixed(4)));
    }

    #[test]
    fn readers_are_bounds_safe() {
        // A truncated box must read 0, never panic (M8 malformed-file hardening).
        let d = [0xAAu8, 0xBB, 0xCC];
        assert_eq!(read_u32(&d, 0), 0);
        assert_eq!(read_u16(&d, 2), 0);
        assert_eq!(read_u64(&d, 0), 0);
        assert_eq!(read_u16(&d, 0), 0xAABB);
    }

    #[test]
    fn container_label_distinguishes_quicktime_from_mp4() {
        // ftyp with major_brand 'qt  ' → QuickTime; anything else (or no ftyp)
        // → the generic ISOBMFF label.
        let mk = |brand: &[u8; 4]| {
            let mut d = vec![0, 0, 0, 0x10]; // size 16
            d.extend_from_slice(b"ftyp");
            d.extend_from_slice(brand);
            d.extend_from_slice(&[0, 0, 0, 0]); // minor version
            let top = iter_boxes(&d, 0, d.len());
            (d, top)
        };
        let (d, top) = mk(b"qt  ");
        assert_eq!(container_label(&top, &d), "QuickTime (MOV)");
        let (d, top) = mk(b"isom");
        assert_eq!(container_label(&top, &d), "MP4 (ISOBMFF)");
        // No ftyp at all (brandless legacy QuickTime) falls back, never guesses.
        assert_eq!(container_label(&[], &[]), "MP4 (ISOBMFF)");
    }

    #[test]
    fn clamp_count_caps_to_box_payload() {
        // A box claiming a billion entries is capped to what its bytes can hold,
        // so a corrupt count can't drive a huge allocation or a runaway loop.
        assert_eq!(clamp_count(1_000_000_000, 8, 4, 8 + 40), 10);
        assert_eq!(clamp_count(3, 8, 4, 8 + 40), 3, "honest count kept");
        assert_eq!(clamp_count(5, 100, 4, 50), 0, "offset past box end");
    }

    #[test]
    fn build_sample_index_survives_a_lying_stsz_count() {
        // stsz declares 1e9 samples but the box holds only two u32s → no panic,
        // no giant alloc; we index exactly what actually fits.
        // stbl children: stsz (version/flags 0, sample_size 0, count 1e9, then 2
        // sizes), stco (1 offset), stsc (1 entry).
        let mut buf = Vec::new();
        let mut boxes: Vec<BoxHdr> = Vec::new();
        let add = |buf: &mut Vec<u8>, boxes: &mut Vec<BoxHdr>, typ: [u8; 4], body: &[u8]| {
            let start = buf.len();
            let size = (8 + body.len()) as u32;
            buf.extend_from_slice(&size.to_be_bytes());
            buf.extend_from_slice(&typ);
            buf.extend_from_slice(body);
            boxes.push(BoxHdr { typ, start, payload: start + 8, end: buf.len() });
        };
        // stsz: 4 (ver/flags) + 4 (sample_size=0) + 4 (count=1e9) + 2 sizes
        let mut stsz = vec![0, 0, 0, 0, 0, 0, 0, 0];
        stsz.extend_from_slice(&1_000_000_000u32.to_be_bytes());
        stsz.extend_from_slice(&10u32.to_be_bytes());
        stsz.extend_from_slice(&20u32.to_be_bytes());
        add(&mut buf, &mut boxes, *b"stsz", &stsz);
        // stco: ver/flags + count=1 + one offset
        let mut stco = vec![0, 0, 0, 0];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&0u32.to_be_bytes());
        add(&mut buf, &mut boxes, *b"stco", &stco);
        // stsc: ver/flags + count=1 + (first_chunk=1, spc=99, desc=1)
        let mut stsc = vec![0, 0, 0, 0];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&99u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        add(&mut buf, &mut boxes, *b"stsc", &stsc);

        let chunks = build_sample_index(&buf, &boxes, Codec::Hevc).unwrap();
        assert_eq!(chunks.len(), 2, "only the two real sample sizes are indexed");
        assert_eq!(chunks[0].size, 10);
        assert_eq!(chunks[1].size, 20);
    }

    #[test]
    fn vexu_stri_reports_stereoscopic_pair() {
        // The exact vexu → eyes → stri box tree from the Profile 20 MV-HEVC sample:
        // the stri eye-view byte 0x03 = left + right present → a stereo pair.
        let vexu = hex_bytes("0000001d766578750000001565796573000000\
                              0d737472690000000003");
        let hdr = BoxHdr { typ: *b"vexu", start: 0, payload: 8, end: vexu.len() };
        assert_eq!(parse_stereo(&vexu, &hdr).as_deref(), Some("Stereoscopic 3D (2 views)"));

        // A monoscopic file has no vexu; an empty/childless one yields no label.
        let empty = BoxHdr { typ: *b"vexu", start: 0, payload: 8, end: 8 };
        assert_eq!(parse_stereo(&[0; 8], &empty), None);
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Serialize one size-prefixed box (header + body) for synthetic fragments.
    fn boxb(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    fn u32s(vals: &[u32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    #[test]
    fn fragment_index_walks_trun_runs() {
        // moof { traf { tfhd (track 1, default-base-is-moof), trun (data_offset,
        // per-sample sizes 10 and 20) } }, offset from the moof box start.
        let tfhd = boxb(b"tfhd", &u32s(&[0x0002_0000, 1]));
        let trun = boxb(b"trun", &u32s(&[0x201, 2, 100, 10, 20]));
        let traf = boxb(b"traf", &[tfhd, trun].concat());
        let mut buf = boxb(b"free", &[0; 8]); // shift the moof off offset 0
        buf.extend_from_slice(&boxb(b"moof", &traf));
        let moof_start = 16u64;

        let top = iter_boxes(&buf, 0, buf.len());
        let (chunks, ticks) = build_fragment_index(&buf, &top, 1, &[]);
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].offset, chunks[0].size), (moof_start + 100, 10));
        assert_eq!((chunks[1].offset, chunks[1].size), (moof_start + 110, 20));
        assert_eq!(ticks, 0, "no duration signal anywhere");
    }

    #[test]
    fn fragment_index_chains_trafs_and_applies_trex_defaults() {
        // An audio traf (track 2) precedes the video traf (track 1). The video
        // traf declares no base offset, so it chains implicitly off the end of
        // the audio data; its trun has no per-sample fields, so size/duration
        // come from the trex defaults.
        let audio_tfhd = boxb(b"tfhd", &u32s(&[0x0002_0000, 2]));
        let audio_trun = boxb(b"trun", &u32s(&[0x201, 1, 200, 50]));
        let audio_traf = boxb(b"traf", &[audio_tfhd, audio_trun].concat());
        let video_tfhd = boxb(b"tfhd", &u32s(&[0, 1]));
        let video_trun = boxb(b"trun", &u32s(&[0, 3]));
        let video_traf = boxb(b"traf", &[video_tfhd, video_trun].concat());
        let buf = boxb(b"moof", &[audio_traf, video_traf].concat());

        let trex = [TrexDefault { track_id: 1, sample_duration: 100, sample_size: 5 }];
        let top = iter_boxes(&buf, 0, buf.len());
        let (chunks, ticks) = build_fragment_index(&buf, &top, 1, &trex);
        assert_eq!(chunks.len(), 3, "only the target track's samples are indexed");
        // Audio data ran [moof+200, moof+250); video continues from there.
        assert_eq!((chunks[0].offset, chunks[0].size), (250, 5));
        assert_eq!(chunks[2].offset, 260);
        assert_eq!(ticks, 300, "trex default duration summed per sample");
    }

    #[test]
    fn fragment_index_survives_a_lying_trun_count() {
        // trun declares 1e9 samples but holds two size entries → clamped to what
        // the box bytes can hold, no giant alloc, no panic.
        let tfhd = boxb(b"tfhd", &u32s(&[0x0002_0000, 1]));
        let trun = boxb(b"trun", &u32s(&[0x200, 1_000_000_000, 10, 20]));
        let traf = boxb(b"traf", &[tfhd, trun].concat());
        let buf = boxb(b"moof", &traf);

        let top = iter_boxes(&buf, 0, buf.len());
        let (chunks, _) = build_fragment_index(&buf, &top, 1, &[]);
        assert_eq!(chunks.len(), 2, "only the two real entries are indexed");
        assert_eq!(chunks[1].size, 20);
    }

    #[test]
    fn fragment_index_drops_unsizable_runs() {
        // No per-sample sizes, no tfhd default, no trex entry: the samples can't
        // be sized, so the run yields nothing rather than fabricated chunks.
        let tfhd = boxb(b"tfhd", &u32s(&[0x0002_0000, 1]));
        let trun = boxb(b"trun", &u32s(&[0, 4]));
        let traf = boxb(b"traf", &[tfhd, trun].concat());
        let buf = boxb(b"moof", &traf);

        let top = iter_boxes(&buf, 0, buf.len());
        let (chunks, _) = build_fragment_index(&buf, &top, 1, &[]);
        assert!(chunks.is_empty());
    }

    /// sidx body: version/flags, reference_ID, timescale, EPT + first_offset
    /// (32-bit v0 / 64-bit v1), reserved, count, then 12-byte entries.
    fn sidx_body(version: u8, first_offset: u64, entries: &[(bool, u32)]) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        b.extend_from_slice(&u32s(&[1, 90_000])); // reference_ID, timescale
        if version == 0 {
            b.extend_from_slice(&u32s(&[0, first_offset as u32])); // EPT, first_offset
        } else {
            b.extend_from_slice(&0u64.to_be_bytes());
            b.extend_from_slice(&first_offset.to_be_bytes());
        }
        b.extend_from_slice(&(0u16).to_be_bytes()); // reserved
        b.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for &(is_sidx, size) in entries {
            let word = size & 0x7FFF_FFFF | if is_sidx { 0x8000_0000 } else { 0 };
            b.extend_from_slice(&u32s(&[word, 90_000, 0])); // word, duration, SAP
        }
        b
    }

    #[test]
    fn sidx_fragment_heads_chain_from_the_box_end() {
        // ftyp, then a v0 sidx with two media references and a nested-sidx
        // reference between them (skipped, but its size advances the chain).
        let mut buf = boxb(b"ftyp", &[0; 8]);
        let sidx = boxb(b"sidx", &sidx_body(0, 40, &[(false, 100), (true, 60), (false, 200)]));
        let anchor = (buf.len() + sidx.len()) as u64 + 40;
        buf.extend_from_slice(&sidx);

        let heads = sidx_fragment_heads(&buf, buf.len()).unwrap();
        assert_eq!(heads, vec![(anchor, 100), (anchor + 160, 200)]);

        // v1 (64-bit EPT/first_offset) resolves the same chain.
        let mut buf = boxb(b"ftyp", &[0; 8]);
        let sidx = boxb(b"sidx", &sidx_body(1, 0, &[(false, 1 << 24)]));
        let anchor = (buf.len() + sidx.len()) as u64;
        buf.extend_from_slice(&sidx);
        let heads = sidx_fragment_heads(&buf, buf.len()).unwrap();
        assert_eq!(heads, vec![(anchor, MOOF_WARM_BYTES)], "warm len capped to the moof head");
    }

    #[test]
    fn sidx_fragment_heads_survive_a_lying_count_and_stop_at_moof() {
        // Count claims 1000 entries but the box holds one: clamped, no panic.
        let mut body = sidx_body(0, 0, &[(false, 100)]);
        let count_off = body.len() - 12 - 2;
        body[count_off..count_off + 2].copy_from_slice(&1000u16.to_be_bytes());
        let buf = boxb(b"sidx", &body);
        assert_eq!(sidx_fragment_heads(&buf, buf.len()).unwrap().len(), 1);

        // A sidx *after* the first moof is never chased...
        let mut buf = boxb(b"moof", &[0; 8]);
        buf.extend_from_slice(&boxb(b"sidx", &sidx_body(0, 0, &[(false, 100)])));
        assert_eq!(sidx_fragment_heads(&buf, buf.len()), None);

        // ...but an mdat before it is hopped over in one bounded jump
        // (ffmpeg's global_sidx layout: ftyp, moov, mdat, sidx, moof...).
        let mut buf = boxb(b"mdat", &[0; 32]);
        buf.extend_from_slice(&boxb(b"sidx", &sidx_body(0, 0, &[(false, 100)])));
        assert_eq!(sidx_fragment_heads(&buf, buf.len()).unwrap().len(), 1);

        // Zero-size entries stop the chain rather than loop in place.
        let buf = boxb(b"sidx", &sidx_body(0, 0, &[(false, 0), (false, 100)]));
        assert_eq!(sidx_fragment_heads(&buf, buf.len()), None);
    }

    /// tfra body (v0, single-byte traf/trun/sample numbers): track, lengths=0,
    /// count, then (time, moof_offset, 1+1+1 bytes) per entry.
    fn tfra_body(track: u32, moof_offsets: &[u32]) -> Vec<u8> {
        let mut b = u32s(&[0, track, 0, moof_offsets.len() as u32]);
        for &off in moof_offsets {
            b.extend_from_slice(&u32s(&[0, off]));
            b.extend_from_slice(&[1, 1, 1]);
        }
        b
    }

    fn mfra_file(tfras: &[Vec<u8>]) -> Vec<u8> {
        let mut mfra_kids: Vec<u8> = tfras.iter().flat_map(|t| boxb(b"tfra", t)).collect();
        // mfro's size field counts the whole mfra including the mfro itself.
        let mfra_size = 8 + mfra_kids.len() + 16;
        mfra_kids.extend_from_slice(&boxb(b"mfro", &u32s(&[0, mfra_size as u32])));
        let mut file = boxb(b"free", &[0; 100]); // stand-in for moov/moofs/mdat
        file.extend_from_slice(&boxb(b"mfra", &mfra_kids));
        file
    }

    #[test]
    fn mfra_fragment_heads_dedupe_across_tracks() {
        // Two tracks whose tfra entries name overlapping moofs: one warm range
        // per distinct moof, sorted.
        let file = mfra_file(&[tfra_body(1, &[16, 60]), tfra_body(2, &[16, 90])]);
        let heads = mfra_fragment_heads(&file).unwrap();
        let offsets: Vec<u64> = heads.iter().map(|h| h.0).collect();
        assert_eq!(offsets, vec![16, 60, 90]);
        assert!(heads.iter().all(|h| h.1 == MOOF_WARM_BYTES));

        // Entries pointing outside the file are dropped, not warmed.
        let file = mfra_file(&[tfra_body(1, &[16, u32::MAX])]);
        assert_eq!(mfra_fragment_heads(&file).unwrap().len(), 1);

        // No trailing mfro (classic MP4 tail) -> no hint.
        assert_eq!(mfra_fragment_heads(&boxb(b"free", &[0; 64])), None);

        // A lying mfro size that runs past the file start -> no hint.
        let mut file = mfra_file(&[tfra_body(1, &[16])]);
        let n = file.len();
        file[n - 4..].copy_from_slice(&u32s(&[1 << 30]));
        assert_eq!(mfra_fragment_heads(&file), None);
    }

    #[test]
    fn parse_mvex_reads_trex_and_mehd() {
        let mehd = boxb(b"mehd", &u32s(&[0, 178_000]));
        let trex = boxb(b"trex", &u32s(&[0, 1, 1, 1001, 4096, 0]));
        let moov = boxb(b"moov", &boxb(b"mvex", &[mehd, trex].concat()));
        let moov_boxes = iter_boxes(&moov, 8, moov.len());

        let (trex, mehd_duration) = parse_mvex(&moov, &moov_boxes);
        assert_eq!(mehd_duration, 178_000);
        assert_eq!(trex.len(), 1);
        assert_eq!(trex[0].track_id, 1);
        assert_eq!(trex[0].sample_duration, 1001);
        assert_eq!(trex[0].sample_size, 4096);
    }
}
