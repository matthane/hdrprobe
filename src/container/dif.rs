//! Raw DV (DIF) — `.dv` / `.dif`: the tape formats' own byte stream, IEC
//! 61834 (consumer DV) and SMPTE 314M/370M (DVCPRO / DVCPRO50 / DVCPRO HD).
//!
//! The stream is fixed-size frames of 80-byte DIF blocks with no container
//! around them, which makes this the most arithmetic backend in the tree:
//! every reported fact comes from the file length plus a bounded head read —
//! the two discriminators sit in the first 480 bytes, and the aspect flag's
//! pack search tops out near byte 108,250, all well inside one frame.
//! A frame is `difseg_size` DIF sequences of 150 blocks (12000 bytes), so the
//! frame size, dimensions, frame rate and chroma are all system constants
//! keyed by two discriminators in the header DIF block — the DSF bit
//! (byte 3 bit 7: 0 = 525-60, 1 = 625-50) and the video-source pack's `stype`
//! (byte 451 low 5 bits: 0x00/0x01 = DV25, 0x04 = DVCPRO50, 0x14/0x18 =
//! DVCPRO HD). The constant table is transcribed from ffmpeg's
//! `libavcodec/dv_profile.c` (`dv_profiles[]`), whose rows cite the defining
//! specs, and every row this backend can produce was verified against
//! ffmpeg-encoded fixtures read back by ffprobe (`testfiles/sdr/dv_*.dv`).
//!
//! Duration is `file_len / frame_size` whole frames over the system rate, and
//! the bitrate is the file's bytes over that — both exact on the default
//! path, so `--full` changes nothing here. The rate's scope is **`overall`**,
//! not `video_stream`: DV interleaves its audio *inside* the video frame (9
//! audio DIF blocks per 150-block sequence), so the whole file is the whole
//! mux. ffprobe's stream `bit_rate` and MediaInfo's `OverallBitRate` are this
//! same number (`frame_size × 8 × fps`, e.g. 28771229 for 525-60 DV25);
//! MediaInfo's separate video-only rate (24.4 Mb/s) is an internal constant
//! this backend does not reproduce.
//!
//! Facts that are easy to get wrong, each pinned by a test:
//!
//! - **APT is not IEC-vs-DVCPRO evidence on 525-60.** ffmpeg's own encoder
//!   writes `APT = 1` on a consumer IEC NTSC encode (it keys APT on
//!   `pix_fmt != yuv420p`, not on the family), and both families are 4:1:1
//!   there anyway. APT decides exactly one thing, the same one thing it
//!   decides in `ff_dv_frame_profile`: a 625-50 DV25 stream with `APT != 0`
//!   is SMPTE 314M DVCPRO (4:1:1) rather than IEC 61834 (4:2:0).
//! - **`stype` is read at the fixed offset 451**, byte 3 of the VAUX
//!   video-source pack, exactly as ffmpeg reads it — no pack search. The one
//!   escape is ffmpeg's QuickTime 3 fallback (trac #217): a frame whose
//!   VAUX bytes are unwritten (`0xFF`) still names its system via DSF, so
//!   `stype_byte == 0xFF` resolves to the DV25 IEC row for the DSF.
//! - **The 16:9 flag lives in the VAUX video-*control* pack (id 0x61), which
//!   is searched, not fixed**: ffmpeg probes two per-sequence positions
//!   (even sequences at `80*5 + 48 + 5`, odd at `80*3 + 8`, both plus
//!   `seq * 12000`) because the two block layouts place it differently. The
//!   `0x07` aspect code counts as 16:9 only when `APT == 0` — IEC and 314M
//!   assign that code differently — and a frame with no findable pack
//!   reports no aspect at all, never a guessed 4:3.
//! - **Scan type is deliberately unreported.** The reference tools disagree
//!   (ffprobe says `field_order=unknown` on every raw-DV fixture; MediaInfo
//!   asserts Interlaced/BFF), the VSC pack's FF/FS bits are the only
//!   candidate signal, and no primary spec for their semantics is on hand —
//!   so per the signalled-only rule the field stays empty rather than
//!   mirroring one tool's assertion.
//! - **No colour is signalled anywhere this tree reads** (like MJPEG and
//!   ASF): the VAUX packs carry no primaries/transfer/matrix, so the Color
//!   line stays honestly empty and bit depth is the family constant 8.
//!
//! One ffmpeg behaviour is deliberately not mirrored: the wrong-DSF PAL hack
//! (trac #8333/#2177: `dsf == 0` but the 50/60 bit at byte 451 bit 5 set)
//! fires only when the caller hands `av_dv_frame_profile` a whole 144000-byte
//! buffer, which ffmpeg's own raw-.dv demuxer never does — its header pass
//! reads `DV_PROFILE_BYTES` and its packet pass reads the *previous* verdict's
//! frame size. A head probe cannot satisfy it either, so such a file reports
//! as 525-60 here exactly as ffprobe reports it.
//!
//! A second ffmpeg behaviour deliberately not mirrored: its header pass
//! *scans* for the header state (`dv_read_header`'s while loop), accepting a
//! damaged or offset tape capture with leading junk. This backend requires
//! the header DIF block at byte 0, the same rule `mpegv` applies to its start
//! code — an extension-reached backend is parsing unvalidated bytes, and a
//! capture cut mid-frame gets an honest error rather than a report whose
//! frame arithmetic would need an offset correction nothing else exercises.
//!
//! `chunks` stays empty by design (the `mpegv`/`asf` contract): DV has no
//! bitstream side channel this project samples — no SEI, no RPU, no T.35.

use anyhow::{bail, Result};

use crate::container::{Codec, Demux, NalFormat, TrackDemux};
use crate::model::Bitrate;

/// Container label; also keyed in `main::suppress_prefix_derived_facts`,
/// because the duration is derived from the payload length and a stdin
/// prefix's length describes the buffer, not the file.
pub(crate) const CONTAINER_LABEL: &str = "raw DV (DIF)";

/// Minimum head: the six DIF blocks of sequence 0 (header, two subcode,
/// three VAUX — 480 bytes), which hold both discriminators; the `stype` byte
/// sits at offset 451 inside VAUX block 2. This is the same span ffmpeg's
/// header pass reads (`DV_PROFILE_BYTES`).
const MIN_HEAD: usize = 6 * 80;

/// The header DIF block's magic: ID bytes `1F 07 00` (header section,
/// sequence 0, block 0) followed by the header pack id, `0x3F` for 525-60 or
/// `0xBF` for 625-50 — i.e. ffmpeg's `(state & 0xffffff7f) == 0x1f07003f`.
pub(crate) fn is_dif(data: &[u8]) -> bool {
    data.len() >= 4
        && data[0] == 0x1F
        && data[1] == 0x07
        && data[2] == 0x00
        && (data[3] & 0x7F) == 0x3F
}

/// One row of the system-constant table (`dv_profiles[]` in ffmpeg's
/// `dv_profile.c`, verified against encoded fixtures).
struct DvSystem {
    frame_size: usize,
    width: u32,
    height: u32,
    /// Frame rate as a rational, kept exact so 525-60's `30000/1001` renders
    /// as 29.970 and the duration arithmetic reproduces ffprobe's to the
    /// millisecond.
    fps_num: u32,
    fps_den: u32,
    chroma: &'static str,
    /// The SMPTE trade name for the professional variants, rendered as the
    /// codec profile; `None` for consumer DV25, which has no variant name
    /// either reference tool prints.
    variant: Option<&'static str>,
}

/// Resolve the system from the two discriminators, mirroring
/// `ff_dv_frame_profile`'s order: the 625-50 APT special case, then the
/// `(dsf, stype)` table, then the QuickTime 3 unwritten-VAUX fallback.
fn system_for(dsf: bool, stype_byte: u8, apt: u8) -> Option<DvSystem> {
    let stype = stype_byte & 0x1F;
    // SMPTE 314M 625-50 DV25 ("576i50 25Mbps 4:1:1 is a special case").
    if dsf && stype == 0 && apt != 0 {
        return Some(DvSystem {
            frame_size: 144000,
            width: 720,
            height: 576,
            fps_num: 25,
            fps_den: 1,
            chroma: "4:1:1",
            variant: Some("DVCPRO"),
        });
    }
    let row = match (dsf, stype) {
        // IEC 61834 / SMPTE 314M — 525/60 DV25 (4:1:1 in both families).
        (false, 0x00) => (120000, 720, 480, 30000, 1001, "4:1:1", None),
        // IEC 61834 — 625/50 DV25; 0x01 is the IEC 61883-5 signalling of the
        // same system.
        (true, 0x00 | 0x01) => (144000, 720, 576, 25, 1, "4:2:0", None),
        // SMPTE 314M 50 Mbps.
        (false, 0x04) => (240000, 720, 480, 30000, 1001, "4:2:2", Some("DVCPRO50")),
        (true, 0x04) => (288000, 720, 576, 25, 1, "4:2:2", Some("DVCPRO50")),
        // SMPTE 370M 100 Mbps. The widths are the *coded* widths (1280/1440
        // at 1080, 960 at 720), which is what ffprobe and MediaInfo both
        // report; display geometry comes from the 16:9 flag like every row.
        (false, 0x14) => (480000, 1280, 1080, 30000, 1001, "4:2:2", Some("DVCPRO HD")),
        (true, 0x14) => (576000, 1440, 1080, 25, 1, "4:2:2", Some("DVCPRO HD")),
        (false, 0x18) => (240000, 960, 720, 60000, 1001, "4:2:2", Some("DVCPRO HD")),
        (true, 0x18) => (288000, 960, 720, 50, 1, "4:2:2", Some("DVCPRO HD")),
        _ => {
            // QuickTime 3 wrote frames with the VAUX section unwritten
            // (every byte 0xFF); DSF still names the DV25 system (ffmpeg
            // trac #217). Any other unknown stype is refused — inventing
            // dimensions for a reserved code would be a guess.
            if stype_byte != 0xFF {
                return None;
            }
            if dsf {
                (144000, 720, 576, 25, 1, "4:2:0", None)
            } else {
                (120000, 720, 480, 30000, 1001, "4:1:1", None)
            }
        }
    };
    let (frame_size, width, height, fps_num, fps_den, chroma, variant) = row;
    Some(DvSystem { frame_size, width, height, fps_num, fps_den, chroma, variant })
}

/// VAUX video-control pack id (IEC 61834-4 pack 0x61, `DV_VIDEO_CONTROL`).
const VIDEO_CONTROL_PACK: u8 = 0x61;

/// Find the video-control pack: ffmpeg's `dv_extract_pack` positions — even
/// DIF sequences carry it at `80*5 + 48 + 5`, odd at `80*3 + 8`, each plus
/// `seq * 12000`; the first ten sequences are probed. Returns the pack's
/// first three bytes' worth of view when found.
fn video_control_pack(data: &[u8]) -> Option<&[u8]> {
    for seq in 0..10usize {
        let offs = if seq % 2 == 1 { 80 * 3 + 8 + seq * 12000 } else { 80 * 5 + 48 + 5 + seq * 12000 };
        if offs + 2 < data.len() && data[offs] == VIDEO_CONTROL_PACK {
            return Some(&data[offs..offs + 3]);
        }
    }
    None
}

pub fn demux(data: &[u8]) -> Result<Demux> {
    if !is_dif(data) {
        bail!("not a DV (DIF) stream: no header DIF block at byte 0");
    }
    if data.len() < MIN_HEAD {
        bail!("DV (DIF) header truncated: {} bytes, need {}", data.len(), MIN_HEAD);
    }

    let dsf = data[3] & 0x80 != 0;
    let apt = data[4] & 0x07;
    let stype_byte = data[451];
    let Some(sys) = system_for(dsf, stype_byte, apt) else {
        bail!(
            "unrecognized DV system (dsf={}, stype=0x{:02X})",
            u8::from(dsf),
            stype_byte & 0x1F
        );
    };

    // Whole frames only: a trailing partial frame is a cut, not presentable
    // content. This is a deliberate difference from the reference tools,
    // which stretch the duration fractionally over the partial frame
    // (ffprobe 0.0834 s and MediaInfo 0.083 for a 2.5-frame cut, against
    // 0.0667 here); on every whole-frame file the three agree exactly.
    let frames = data.len() / sys.frame_size;
    let duration_secs =
        (frames > 0).then(|| frames as f64 * sys.fps_den as f64 / sys.fps_num as f64);

    // The 16:9 flag, exactly as ffmpeg's `dv_extract_video_info` reads it:
    // aspect code 0x02 always means 16:9, and IEC (APT 0) additionally
    // assigns 0x07 to it. No pack, no aspect.
    let display_aspect = video_control_pack(data).map(|vsc| {
        let code = vsc[2] & 0x07;
        if code == 0x02 || (apt == 0 && code == 0x07) {
            (16, 9)
        } else {
            (4, 3)
        }
    });

    let track = TrackDemux {
        width: sys.width,
        height: sys.height,
        fps: Some(sys.fps_num as f64 / sys.fps_den as f64),
        // 8-bit is the family constant across DV25/50/100 (SMPTE 314M/370M),
        // like ProRes's and MPEG-2's family depths.
        bit_depth: Some(8),
        chroma: Some(sys.chroma.to_string()),
        display_aspect,
        codec_profile: sys.variant.map(str::to_string),
        // The numerator counts the same whole frames the duration does, so a
        // file cut mid-frame reports exactly the nominal system rate
        // (frame_size × 8 × fps — both reference tools' number) instead of
        // an inflated one: dividing the whole file's bytes by the floored
        // duration overstated a 2.5-frame cut by 25%.
        bitrate: Bitrate::overall((frames * sys.frame_size) as u64, duration_secs),
        // `nal_format` is a placeholder (the `mpegv` convention): DIF blocks
        // are not NAL units and the sampler never runs — `chunks` is empty.
        ..TrackDemux::new(Codec::Other("DV".to_string()), NalFormat::AnnexB)
    };
    Ok(Demux::single(CONTAINER_LABEL, duration_secs, track))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BitrateScope;

    /// A synthetic head: header DIF block magic + DSF/APT + the stype byte,
    /// zero elsewhere, padded to `len` with 0xAA so nothing else reads as a
    /// pack id.
    fn head(dsf: bool, stype_byte: u8, apt: u8, len: usize) -> Vec<u8> {
        let mut d = vec![0u8; MIN_HEAD.min(len)];
        d[0] = 0x1F;
        d[1] = 0x07;
        d[2] = 0x00;
        d[3] = if dsf { 0xBF } else { 0x3F };
        d[4] = 0xF8 | (apt & 0x07); // reserved-high bits set, as real files write
        if d.len() > 451 {
            d[448] = 0x60; // video-source pack id, cosmetic
            d[451] = stype_byte;
        }
        d.resize(len, 0xAA);
        d
    }

    #[test]
    fn ntsc_dv25_reports_the_system_constants() {
        // Two whole 525-60 frames; layout verified against
        // testfiles/sdr/dv_ntsc.dv (bytes 0..5 = 1F 07 00 3F F9).
        let d = demux(&head(false, 0x00, 1, 240000)).expect("dv25 ntsc");
        assert_eq!(d.container, CONTAINER_LABEL);
        let t = &d.tracks[0];
        assert!(matches!(&t.codec, Codec::Other(l) if l == "DV"));
        assert_eq!((t.width, t.height), (720, 480));
        assert!((t.fps.unwrap() - 30000.0 / 1001.0).abs() < 1e-9);
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:1:1"));
        assert_eq!(t.codec_profile, None, "consumer DV25 has no variant name");
        assert!((d.duration_secs.unwrap() - 2.0 * 1001.0 / 30000.0).abs() < 1e-9);
        // ffprobe's number for this system: frame_size × 8 × fps.
        let b = t.bitrate.unwrap();
        assert_eq!(b.scope, BitrateScope::Overall);
        assert!((b.bits_per_sec - 28_771_228.77).abs() < 1.0);
        assert!(t.chunks.is_empty(), "DV has nothing to sample");
    }

    #[test]
    fn pal_apt_splits_iec_from_dvcpro() {
        // The one thing APT decides: 625-50 DV25 chroma. Verified against
        // dv_pal.dv (APT 0) and dv_pal_dvcpro.dv (APT 1).
        let iec = demux(&head(true, 0x00, 0, 144000)).unwrap();
        assert_eq!(iec.tracks[0].chroma.as_deref(), Some("4:2:0"));
        assert_eq!(iec.tracks[0].codec_profile, None);
        assert_eq!((iec.tracks[0].width, iec.tracks[0].height), (720, 576));
        assert_eq!(iec.tracks[0].fps, Some(25.0));

        let pro = demux(&head(true, 0x00, 1, 144000)).unwrap();
        assert_eq!(pro.tracks[0].chroma.as_deref(), Some("4:1:1"));
        assert_eq!(pro.tracks[0].codec_profile.as_deref(), Some("DVCPRO"));

        // …and NOT on 525-60, where ffmpeg's own IEC encodes write APT 1 and
        // both families are 4:1:1: the verdict must be identical either way.
        let a0 = demux(&head(false, 0x00, 0, 120000)).unwrap();
        let a1 = demux(&head(false, 0x00, 1, 120000)).unwrap();
        assert_eq!(a0.tracks[0].chroma, a1.tracks[0].chroma);
        assert_eq!(a0.tracks[0].codec_profile, a1.tracks[0].codec_profile);

        // stype 0x01 (IEC 61883-5) is the same PAL IEC system.
        let iec5 = demux(&head(true, 0x01, 1, 144000)).unwrap();
        assert_eq!(iec5.tracks[0].chroma.as_deref(), Some("4:2:0"));
    }

    #[test]
    fn dvcpro50_and_hd_rows_match_their_fixtures() {
        // Verified against dv50_ntsc.dv, dvcprohd_720p.dv, dvcprohd_1080i.dv.
        let dv50 = demux(&head(false, 0x04, 1, 240000)).unwrap();
        let t = &dv50.tracks[0];
        assert_eq!((t.width, t.height), (720, 480));
        assert_eq!(t.chroma.as_deref(), Some("4:2:2"));
        assert_eq!(t.codec_profile.as_deref(), Some("DVCPRO50"));
        assert!((dv50.duration_secs.unwrap() - 1001.0 / 30000.0).abs() < 1e-9);

        let hd720 = demux(&head(false, 0x18, 1, 240000)).unwrap();
        let t = &hd720.tracks[0];
        assert_eq!((t.width, t.height), (960, 720));
        assert!((t.fps.unwrap() - 60000.0 / 1001.0).abs() < 1e-9);
        assert_eq!(t.codec_profile.as_deref(), Some("DVCPRO HD"));

        let hd1080i60 = demux(&head(false, 0x14, 1, 480000)).unwrap();
        assert_eq!((hd1080i60.tracks[0].width, hd1080i60.tracks[0].height), (1280, 1080));

        let hd1080i50 = demux(&head(true, 0x14, 1, 576000)).unwrap();
        assert_eq!((hd1080i50.tracks[0].width, hd1080i50.tracks[0].height), (1440, 1080));

        let hd720p50 = demux(&head(true, 0x18, 1, 288000)).unwrap();
        assert_eq!(hd720p50.tracks[0].fps, Some(50.0));
    }

    #[test]
    fn unwritten_vaux_falls_back_to_dv25_by_dsf_and_reserved_stype_is_refused() {
        // QuickTime 3 frames leave the VAUX section 0xFF (ffmpeg trac #217).
        let qt = demux(&head(false, 0xFF, 1, 120000)).unwrap();
        assert_eq!((qt.tracks[0].width, qt.tracks[0].height), (720, 480));
        let qt_pal = demux(&head(true, 0xFF, 0, 144000)).unwrap();
        assert_eq!(qt_pal.tracks[0].chroma.as_deref(), Some("4:2:0"));

        // A reserved stype that is not the unwritten pattern gets an honest
        // error, not invented dimensions.
        assert!(demux(&head(false, 0x0A, 1, 120000)).is_err());
    }

    #[test]
    fn the_video_control_pack_signals_the_aspect_or_nothing() {
        // Aspect code 0x02 is 16:9 in both families.
        let mut d = head(false, 0x00, 1, 120000);
        d[453] = 0x61;
        d[455] = 0x02;
        assert_eq!(demux(&d).unwrap().tracks[0].display_aspect, Some((16, 9)));

        // 0x07 counts as 16:9 only under APT 0 (IEC); under APT 1 it is 4:3.
        let mut iec = head(false, 0x00, 0, 120000);
        iec[453] = 0x61;
        iec[455] = 0x07;
        assert_eq!(demux(&iec).unwrap().tracks[0].display_aspect, Some((16, 9)));
        let mut pro = head(false, 0x00, 1, 120000);
        pro[453] = 0x61;
        pro[455] = 0x07;
        assert_eq!(demux(&pro).unwrap().tracks[0].display_aspect, Some((4, 3)));

        // Code 0 is 4:3; no findable pack is no aspect at all.
        let mut d43 = head(false, 0x00, 1, 120000);
        d43[453] = 0x61;
        d43[455] = 0x00;
        assert_eq!(demux(&d43).unwrap().tracks[0].display_aspect, Some((4, 3)));
        assert_eq!(demux(&head(false, 0x00, 1, 120000)).unwrap().tracks[0].display_aspect, None);

        // The odd-sequence position (80*3 + 8 + 12000): the pack search must
        // probe both layouts, not just the even one.
        let mut odd = head(false, 0x00, 1, 120000);
        odd[80 * 3 + 8 + 12000] = 0x61;
        odd[80 * 3 + 8 + 12000 + 2] = 0x02;
        assert_eq!(demux(&odd).unwrap().tracks[0].display_aspect, Some((16, 9)));
    }

    #[test]
    fn duration_counts_whole_frames_and_a_short_head_reports_no_timeline() {
        // 2.5 frames: the trailing partial frame is a cut, not content — and
        // the bitrate numerator must count the same whole frames, or the cut
        // file reports an inflated rate (dividing all 300000 bytes by the
        // floored duration read 25% high; the reference tools hold the rate
        // at the nominal system constant on such files).
        let d = demux(&head(false, 0x00, 1, 300000)).unwrap();
        assert!((d.duration_secs.unwrap() - 2.0 * 1001.0 / 30000.0).abs() < 1e-9);
        let rate = d.tracks[0].bitrate.unwrap().bits_per_sec;
        assert!((rate - 28_771_228.77).abs() < 1.0, "cut file must report the nominal rate");

        // A complete header but less than one frame: dimensions yes,
        // duration and bitrate no.
        let d = demux(&head(false, 0x00, 1, MIN_HEAD)).unwrap();
        assert_eq!((d.tracks[0].width, d.tracks[0].height), (720, 480));
        assert!(d.duration_secs.is_none());
        assert!(d.tracks[0].bitrate.is_none());
    }

    #[test]
    fn non_dif_bytes_are_refused() {
        assert!(demux(&[]).is_err());
        assert!(demux(&[0x1F, 0x07, 0x00]).is_err());
        // Right magic shape, wrong header pack id.
        assert!(demux(&head(false, 0x00, 1, 120000).iter().enumerate().map(|(i, &b)| if i == 3 { 0x3E } else { b }).collect::<Vec<_>>()).is_err());
        // A header DIF block cut before the stype byte.
        let mut short = head(false, 0x00, 1, MIN_HEAD);
        short.truncate(400);
        assert!(demux(&short).is_err());
        // Unrelated container magics.
        assert!(demux(&[0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00]).is_err());
        assert!(!is_dif(b"\x00\x00\x01\xBA"));
    }
}
