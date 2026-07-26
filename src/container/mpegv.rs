//! Raw MPEG-1/MPEG-2 video elementary stream (`.m2v`, `.m1v`, `.mpv`).
//!
//! The thinnest backend in the tree, and deliberately so. Everything these
//! streams record about themselves is at the head ([`crate::mpeg2`]), so this
//! fills the General fields from a bounded head read and leaves `chunks` empty,
//! which `sample::scan` reads as "nothing to sample" (see the contract on
//! `TrackDemux::chunks`). No chunk index, no sampler arm, no progress or
//! frontier plumbing.
//!
//! Duration and bitrate are absent for the same reason they are absent from a
//! raw HEVC stream: the format records neither, and the frame count needed to
//! derive one would take a walk over the whole file. `None` is the honest
//! answer, and it is what ffprobe reports for these files too.

use anyhow::{Context, Result};

use crate::container::{Codec, Demux, NalFormat, TrackDemux};

/// Bytes read to find the sequence header. It opens the stream in every mux
/// observed, so this is slack for a capture cut mid-GOP rather than a budget
/// anything normally spends. Must stay `<=` `prefetch::HEAD_WARM` so the
/// generic head warm covers the whole walked span on a network volume, the
/// same coupling `annexb::HEAD_SCAN_BYTES` and `av1::HEAD_SCAN_BYTES` keep.
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

pub fn demux(data: &[u8]) -> Result<Demux> {
    // The stream must *open* on an MPEG video start code. This is a stricter
    // rule than `annexb::demux`'s (which only refuses a head positively
    // identified as another family) and it has to be, because the search below
    // is a scan across 8 MiB rather than a read at a fixed offset: over enough
    // unrelated bytes it eventually finds something that passes
    // `find_sequence_header`'s plausibility checks. Measured across 1.45 GiB of
    // non-MPEG corpus, 5 of 590 `00 00 01 B3` occurrences did. A file reaching
    // this backend by *extension* never passes the sniffer, so before this a
    // `.ogv` renamed `.m2v` reported a fabricated `912x2861`.
    //
    // Requiring a start code at byte 0 costs a stream cut mid-picture, which
    // then gets an honest error instead of a report. A cut at any GOP or
    // picture boundary still opens on a start code and is accepted, and the
    // sniffed route reaches here only on this same verdict, so the two entry
    // paths agree by construction.
    match crate::container::classify_start_code(data) {
        Some(crate::container::StreamFamily::MpegVideoEs) => {}
        Some(other) => {
            anyhow::bail!("not an MPEG video elementary stream: {}", other.label())
        }
        None => anyhow::bail!("not an MPEG video elementary stream: no leading start code"),
    }
    let head = &data[..HEAD_SCAN_BYTES.min(data.len())];
    let s = crate::mpeg2::parse_sequence(head)
        .context("no MPEG-1/2 sequence header found in the head window")?;

    // The stream says which codec it is: 11172-2 defines no extensions, so a
    // `sequence_extension` is present exactly when this is 13818-2. Nothing
    // else in a raw elementary stream carries the distinction.
    let codec = if s.is_mpeg2 { Codec::Mpeg2 } else { Codec::Mpeg1 };
    let container = if s.is_mpeg2 { "raw MPEG-2 Video (ES)" } else { "raw MPEG-1 Video (ES)" };

    let (color, color_source) = s.color;
    let track = TrackDemux {
        width: s.width,
        height: s.height,
        fps: s.fps,
        bit_depth: Some(s.bit_depth),
        chroma: s.chroma.map(str::to_string),
        codec_profile: s.profile_level,
        color,
        color_source,
        // `nal_format` is a placeholder: MPEG access units are start-code
        // delimited but are not NAL units, and the sampler's arm is a no-op.
        ..TrackDemux::new(codec, NalFormat::AnnexB)
    };
    Ok(Demux::single(container, None, track))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/mpeg2.m2v` bytes 0..22 verbatim: sequence header plus the
    /// sequence extension that makes it 13818-2.
    const M2V_HEAD: [u8; 22] = [
        0x00, 0x00, 0x01, 0xB3, 0x14, 0x00, 0xF0, 0x23, 0xFF, 0xFF, 0xE0, 0x18, 0x00, 0x00, 0x01,
        0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00,
    ];

    #[test]
    fn mpeg2_elementary_stream_reports_its_sequence_header() {
        let d = demux(&M2V_HEAD).expect("sequence header");
        assert_eq!(d.container, "raw MPEG-2 Video (ES)");
        let t = &d.tracks[0];
        assert_eq!(t.codec, Codec::Mpeg2);
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.fps, Some(25.0));
        assert_eq!(t.bit_depth, Some(8));
        assert_eq!(t.chroma.as_deref(), Some("4:2:0"));
        assert_eq!(t.codec_profile.as_deref(), Some("Main@Main"));
        // Metadata-only: no chunk index, and neither duration nor bitrate,
        // because a raw elementary stream records neither.
        assert!(t.chunks.is_empty());
        assert!(d.duration_secs.is_none());
        assert!(t.bitrate.is_none());
    }

    #[test]
    fn mpeg1_elementary_stream_is_told_apart_by_the_missing_extension() {
        // `testfiles/sdr/mpeg1.m1v`: the same header, aspect code 1, then a GOP
        // header instead of a sequence extension.
        let mut d = Vec::from(&M2V_HEAD[..12]);
        d[7] = 0x13;
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
        let dm = demux(&d).expect("sequence header");
        assert_eq!(dm.container, "raw MPEG-1 Video (ES)");
        assert_eq!(dm.tracks[0].codec, Codec::Mpeg1);
        assert_eq!(dm.tracks[0].codec_profile, None, "MPEG-1 has no profile field");
    }

    #[test]
    fn bytes_without_a_sequence_header_error_rather_than_report() {
        assert!(demux(&[]).is_err());
        assert!(demux(&[0u8; 4096]).is_err());
        // An HEVC Annex-B head must not be claimed by this backend.
        assert!(demux(&[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF]).is_err());
    }

    #[test]
    fn a_head_that_is_not_an_mpeg_start_code_is_refused_before_the_scan() {
        // This backend is reachable by *extension*, which means nothing checked
        // the bytes first, and the sequence-header search is a scan over 8 MiB
        // that unrelated content can satisfy by chance. So the stream must open
        // on an MPEG video start code. Other container magics carry no start
        // code at byte 0 at all, which is exactly why refusing only positively
        // identified families is not enough here: a Matroska head renamed
        // `.m2v` used to report dimensions invented from its payload.
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
        mkv.extend_from_slice(&[0xAA; 64]);
        mkv.extend_from_slice(&M2V_HEAD); // a plausible header further in
        assert!(demux(&mkv).is_err(), "an EBML head must not reach the scan");

        let mut mp4 = vec![0x00, 0x00, 0x00, 0x18];
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&M2V_HEAD);
        assert!(demux(&mp4).is_err(), "an ISOBMFF head must not reach the scan");

        // A pack header is the program stream's, not this backend's.
        assert!(demux(&[0, 0, 1, 0xBA, 0x44, 0x00, 0x04, 0x00]).is_err());

        // A stream cut at a GOP boundary still opens on an MPEG start code and
        // is accepted, with the sequence header found by the scan.
        let mut cut = Vec::from(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40][..]);
        cut.extend_from_slice(&M2V_HEAD);
        assert!(demux(&cut).is_ok(), "a GOP-boundary cut is legitimate");
    }
}
