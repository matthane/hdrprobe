//! Metadata sidecar files: raw Dolby Vision RPU (`.bin`/`.rpu`), Dolby Vision
//! CM XML, and HDR10+ (hdr10plus_tool) JSON.
//!
//! These carry no picture data — no container, codec, resolution, or byte-range
//! chunks — so they bypass the whole video pipeline (mmap / prefetch / demux /
//! sample, all of which exist to locate video access units and warm NAS
//! windows). Each sidecar parses into the metadata hdrprobe already models and is
//! rendered through the ordinary `Report`, so text / JSON / `-q` output all work
//! unchanged. The heavy lifting reuses the existing aggregators: RPU-bin and DV
//! XML both reduce to `Vec<DoviRpu>` fed through `dv::levels::DvAggregate`.

mod dv_xml;
mod hdr10plus_json;
mod rpu_bin;

use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Result};
use dolby_vision::rpu::dovi_rpu::DoviRpu;

use crate::dv::levels::DvAggregate;
use crate::dv::rpu::parse_hevc_rpu;
use crate::hevc::nal::{self, NalRef};
use crate::model::{ColorInfo, General, Hdr10Plus, Report};

/// A parsed sidecar's payload — exactly one of the metadata sections. The DV
/// section is boxed (it dwarfs the HDR10+ one) to keep the enum small.
enum Payload {
    DolbyVision(Box<crate::model::DolbyVision>),
    Hdr10Plus(Hdr10Plus),
}

/// The picture size L5 active-area *dimensions* are computed against for the DV
/// sidecars, neither of which records a resolution. We assume a UHD master —
/// the near-universal DV mastering resolution, and dovi_tool's own `generate`
/// convention — and label the reported dimensions as assumed. A DV XML needs it
/// to turn L5 *aspect ratios* into offsets at all (libdovi yields zeros without
/// a canvas); a raw RPU bin already bakes L5 offsets in real pixels, so the
/// canvas only sizes them into dimensions. Both callers flag it as assumed.
pub(super) const ASSUMED_CANVAS: (u32, u32) = (3840, 2160);

/// Detect and process a metadata sidecar file. Returns `Ok(None)` when `path`
/// is not a sidecar (the caller then runs the normal video pipeline), `Ok(Some)`
/// when it was recognised and parsed, and `Err` when it is clearly a sidecar of a
/// given kind but could not be parsed.
pub fn try_process(path: &Path) -> Result<Option<Report>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Only sidecar-capable extensions read the file here; real video files fall
    // straight through to the mmap path without a redundant read.
    if !matches!(ext.as_str(), "rpu" | "bin" | "xml" | "json") {
        return Ok(None);
    }

    let started = Instant::now();

    // HDR10+ JSON is the one sidecar we surface without aggregating across frames
    // — only the file-level profile and the first scene, both at the head of the
    // file — and it's also by far the largest (one metadata object per frame,
    // often hundreds of MB). So we read a bounded head window rather than pull the
    // whole per-frame array into memory only to discard it. Every other sidecar
    // reduces to `Vec<DoviRpu>` aggregated over the whole title, so it needs the
    // full file.
    if ext == "json" {
        let mut head = Vec::new();
        read_head(path, hdr10plus_json::HEAD_BYTES, &mut head)?;
        if detect(&ext, &head).is_none() {
            return Ok(None);
        }
        let size = std::fs::metadata(path)?.len();
        let payload = hdr10plus_json::parse(&head)?;
        return Ok(Some(build_report(path, size, "HDR10+ JSON", payload, None, started)));
    }

    let data = std::fs::read(path)?;
    let size = data.len() as u64;

    let Some(kind) = detect(&ext, &data) else {
        return Ok(None);
    };

    // Frame rate is a DV-XML-only global (Level 0) fact; the RPU bin carries none.
    let (container, payload, fps) = match kind {
        Kind::RpuBin => ("Dolby Vision RPU", rpu_bin::parse(&data)?, None),
        Kind::DvXml => {
            let (payload, meta) = dv_xml::parse(&data)?;
            ("Dolby Vision XML", payload, meta.fps)
        }
        // `.json` is handled above via a bounded head read, so it never reaches
        // the whole-file path.
        Kind::Hdr10PlusJson => unreachable!("json is dispatched before the full read"),
    };

    Ok(Some(build_report(path, size, container, payload, fps, started)))
}

#[derive(Clone, Copy)]
enum Kind {
    RpuBin,
    DvXml,
    Hdr10PlusJson,
}

/// Classify by extension, disambiguating collisions by content: `.bin` is shared
/// with raw HEVC (which opens with a VPS/SPS NAL, not the type-62 RPU NAL), and
/// `.xml`/`.json` may be unrelated documents.
fn detect(ext: &str, data: &[u8]) -> Option<Kind> {
    match ext {
        "rpu" => Some(Kind::RpuBin),
        "bin" => looks_like_rpu_bin(data).then_some(Kind::RpuBin),
        "xml" => looks_like_dv_xml(data).then_some(Kind::DvXml),
        "json" => looks_like_hdr10plus_json(data).then_some(Kind::Hdr10PlusJson),
        _ => None,
    }
}

/// A raw RPU stream and a raw HEVC bitstream both start with an Annex-B start
/// code, so type alone can't tell them apart — worse, dovi_tool writes RPUs with
/// no `7C 01` NAL header, so the leading byte is the `rpu_nal_prefix` (0x19), not
/// a type-62 header. The definitive test is whether the first NAL actually parses
/// as an RPU (libdovi validates framing and a CRC32, so false positives are
/// vanishingly unlikely). The parse is panic-guarded like every libdovi call.
fn looks_like_rpu_bin(data: &[u8]) -> bool {
    let mut nals: Vec<NalRef> = Vec::new();
    nal::split_annexb(data, &mut nals);
    nals.iter().take(2).any(|n| parse_hevc_rpu(&data[n.start..n.end]).is_some())
}

/// Whether a directory-scan candidate is a metadata sidecar (a cheap content
/// peek, so an unrelated `.json`/`.xml`/`.bin` in the folder isn't collected).
pub fn is_sidecar_candidate(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "rpu" {
        return true;
    }
    if !matches!(ext.as_str(), "bin" | "xml" | "json") {
        return false;
    }
    // Peek the head only; enough to classify without reading a whole file.
    let mut buf = Vec::new();
    if read_head(path, 8192, &mut buf).is_err() {
        return false;
    }
    detect(&ext, &buf).is_some()
}

fn read_head(path: &Path, max: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    out.clear();
    // `take` + `read_to_end` reads up to `max` bytes across as many syscalls as
    // needed (a single `read` may return short), stopping at EOF for small files.
    f.take(max as u64).read_to_end(out)?;
    Ok(())
}

fn looks_like_dv_xml(data: &[u8]) -> bool {
    contains(data, b"DolbyLabsMDF")
}

fn looks_like_hdr10plus_json(data: &[u8]) -> bool {
    contains(data, b"JSONInfo") || contains(data, b"SceneInfo")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Aggregate a full list of RPUs (from an RPU bin or generated from a DV XML)
/// into the title-stable Dolby Vision section. Shared by both DV sidecars.
///
/// `canvas` is the picture size the L5 active-area dimensions are computed
/// against — the assumed UHD master (`ASSUMED_CANVAS`) for both DV sidecars,
/// since neither records a resolution. Passing `None` (used only by tests /
/// callers with no canvas) shows bare offsets with dimensions omitted.
fn dv_from_rpus(rpus: &[DoviRpu], canvas: Option<(u32, u32)>) -> Result<Payload> {
    let mut agg = DvAggregate::default();
    for rpu in rpus {
        agg.add(rpu);
    }
    finalize_dv(agg, canvas)
}

/// Finalize a populated aggregator into a DV payload. Shared by the raw RPU-bin
/// path (which folds one RPU per frame) and the DV XML path (which folds one
/// representative RPU per shot). `canvas` is the assumed L5 canvas, if any.
fn finalize_dv(mut agg: DvAggregate, canvas: Option<(u32, u32)>) -> Result<Payload> {
    let (cw, ch) = canvas.unwrap_or((0, 0));
    // Metadata-only input: no base layer, so a convention-default compat minor
    // (P8 -> .1) can't be backed by a base-layer VUI and is flagged as assumed.
    agg.mark_metadata_only();
    // Every RPU in the sidecar was accounted for, so this is an exhaustive census,
    // not a sample: pass full=true for the per-level presence / scene-cut counts.
    // There is no container dvcC, it isn't AV1, and it isn't dual-track.
    match agg.finalize(cw, ch, None, true, false, false) {
        Some(mut dv) => {
            if let Some((w, h)) = canvas {
                dv.l5_assumed_canvas = Some([w, h]);
            }
            Ok(Payload::DolbyVision(Box::new(dv)))
        }
        None => bail!("no Dolby Vision RPUs found"),
    }
}

fn build_report(
    path: &Path,
    size: u64,
    container: &str,
    payload: Payload,
    fps: Option<f64>,
    started: Instant,
) -> Report {
    let (dolby_vision, hdr10plus) = match payload {
        Payload::DolbyVision(dv) => (Some(*dv), Hdr10Plus::absent()),
        Payload::Hdr10Plus(hp) => (None, hp),
    };
    Report {
        file: path.display().to_string(),
        size_bytes: size,
        general: General {
            container: container.to_string(),
            codec: String::new(),
            codec_profile: None,
            width: None,
            height: None,
            fps,
            duration_secs: None,
            bitrate: None,
            bit_depth: None,
            chroma: None,
            stereo: None,
            color: ColorInfo::default(),
        },
        hdr: None,
        dolby_vision,
        hdr10plus,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_sniffers_classify_xml_and_json() {
        assert!(looks_like_dv_xml(b"<?xml version=\"1.0\"?><DolbyLabsMDF version=\"5.1.0\">"));
        assert!(!looks_like_dv_xml(b"<?xml version=\"1.0\"?><SomethingElse>"));
        assert!(looks_like_hdr10plus_json(br#"{"JSONInfo":{"HDR10plusProfile":"B"}}"#));
        assert!(!looks_like_hdr10plus_json(b"{\"unrelated\": true}"));
    }

    #[test]
    fn raw_hevc_is_not_detected_as_rpu_bin() {
        // Annex-B SPS NAL (type 33, header byte 0x42) with filler payload — a raw
        // HEVC bitstream, which must fall through to the video path, not the RPU
        // sidecar. The RPU parse rejects it (no valid framing/CRC).
        let mut data = vec![0, 0, 0, 1, 0x42, 0x01];
        data.extend_from_slice(&[0x11; 48]);
        assert!(!looks_like_rpu_bin(&data));
        assert!(detect("bin", &data).is_none());
    }

    #[test]
    fn garbage_rpu_bin_errors_without_panic() {
        // Correct leading framing (start code + rpu_nal_prefix 0x19) but random
        // body: libdovi rejects it, and we surface an error rather than aborting.
        let mut data = vec![0, 0, 0, 1, 0x19];
        data.extend_from_slice(&[0xAB; 48]);
        assert!(rpu_bin::parse(&data).is_err());
    }

    #[test]
    fn malformed_hdr10plus_json_errors_without_panic() {
        assert!(hdr10plus_json::parse(b"{ not valid json").is_err());
        assert!(hdr10plus_json::parse(br#"{"JSONInfo":{},"SceneInfo":[]}"#).is_err());
    }

    #[test]
    fn hdr10plus_head_parse_reads_only_first_scene() {
        // Two scenes with different target luminance; we must report the first
        // (400), proving the bounded scan takes SceneInfo's first element and
        // never needs the rest of the (here, second) array entry.
        let json = br#"{"JSONInfo":{"HDR10plusProfile":"A","Version":"1.0"},"SceneInfo":[{"LuminanceParameters":{"AverageRGB":5,"LuminanceDistributions":{"DistributionIndex":[1,5,10,25,50,75,90,95,99],"DistributionValues":[0,1,2,3,4,5,6,7,8]},"MaxScl":[100,200,300]},"NumberOfWindows":1,"TargetedSystemDisplayMaximumLuminance":400,"SceneFrameIndex":0,"SceneId":0,"SequenceFrameIndex":0},{"LuminanceParameters":{"AverageRGB":9,"LuminanceDistributions":{"DistributionIndex":[1],"DistributionValues":[9]},"MaxScl":[1,2,3]},"NumberOfWindows":1,"TargetedSystemDisplayMaximumLuminance":1000,"SceneFrameIndex":0,"SceneId":1,"SequenceFrameIndex":1}],"SceneInfoSummary":{"SceneFirstFrameIndex":[0],"SceneFrameNumbers":[2]}}"#;
        match hdr10plus_json::parse(json).expect("valid HDR10+ head parses") {
            Payload::Hdr10Plus(hp) => {
                assert!(hp.present);
                assert_eq!(hp.profile, Some('A'));
                assert_eq!(hp.num_windows, Some(1));
                assert_eq!(hp.target_max_luminance, Some(400));
            }
            _ => panic!("expected an HDR10+ payload"),
        }
    }

    #[test]
    fn hdr10plus_first_scene_truncated_in_window_errors() {
        // The head window cut off before the first scene object closed: the scan
        // can't complete it, and we bail cleanly instead of parsing a fragment.
        let truncated = br#"{"JSONInfo":{"HDR10plusProfile":"B","Version":"1.0"},"SceneInfo":[{"LuminanceParameters":{"AverageRGB":5,"Luminance"#;
        assert!(hdr10plus_json::parse(truncated).is_err());
    }

    #[test]
    fn non_sidecar_extension_returns_none() {
        // A `.mp4` never reaches the sidecar reader.
        assert!(detect("mp4", b"anything").is_none());
    }
}
