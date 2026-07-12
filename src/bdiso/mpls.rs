//! Minimal Blu-ray MPLS (MoviePlaylist) parser: just enough to rank playlists
//! by duration and name their clips. Reads the header and each PlayItem's
//! fixed-offset fields (clip id, codec id, IN/OUT times), advancing by the
//! item's own length field; STN tables, sub-paths, and multi-angle blocks are
//! skipped whole, never parsed. MPLS is big-endian (unlike UDF; keep the two
//! modules' readers separate).

use anyhow::{bail, Result};

/// One PlayItem's segment: which clip it plays and the 45 kHz edit window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayItem {
    /// Five-character clip id, e.g. `"00055"`; `BDMV/STREAM/<id>.m2ts`.
    pub clip_id: String,
    pub in_45k: u32,
    pub out_45k: u32,
}

#[derive(Debug, Default)]
pub struct Playlist {
    pub items: Vec<PlayItem>,
}

impl Playlist {
    /// Total edit duration over *distinct* segments. Obfuscation decoys loop
    /// one segment hundreds of times; deduping by `(clip, in, out)` collapses
    /// each loop to a single contribution, so decoys can't out-rank the real
    /// feature on duration.
    pub fn duration_secs_deduped(&self) -> f64 {
        let mut seen: Vec<&PlayItem> = Vec::new();
        let mut ticks: u64 = 0;
        for item in &self.items {
            if seen.iter().any(|s| **s == *item) {
                continue;
            }
            seen.push(item);
            ticks += u64::from(item.out_45k.saturating_sub(item.in_45k));
        }
        ticks as f64 / 45_000.0
    }

    /// Distinct clip ids in playback order (first appearance wins).
    pub fn distinct_clips(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for item in &self.items {
            if !out.contains(&item.clip_id.as_str()) {
                out.push(&item.clip_id);
            }
        }
        out
    }
}

// Bounds-safe big-endian readers, mp4.rs discipline: OOB reads 0; the item
// loop is bounded by the buffer and a count cap, so 0 can't run away.
fn read_u16(d: &[u8], o: usize) -> u16 {
    match d.get(o..o + 2) {
        Some(b) => u16::from_be_bytes([b[0], b[1]]),
        None => 0,
    }
}
fn read_u32(d: &[u8], o: usize) -> u32 {
    match d.get(o..o + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// A real playlist holds a handful of PlayItems (seamless branching tops out
/// in the dozens; decoys in the hundreds); anything past this is corrupt.
const MAX_PLAY_ITEMS: usize = 4096;

pub fn parse(data: &[u8]) -> Result<Playlist> {
    if data.len() < 16 || &data[0..4] != b"MPLS" {
        bail!("not an MPLS playlist");
    }
    let version = &data[4..8];
    if !matches!(version, b"0100" | b"0200" | b"0300") {
        bail!("unsupported MPLS version {}", String::from_utf8_lossy(version));
    }
    let pl_start = read_u32(data, 8) as usize;
    // PlayList block: u32 length, 2 reserved bytes, u16 item count, u16 sub-path count.
    if pl_start < 16 || pl_start + 10 > data.len() {
        bail!("PlayList block offset out of range");
    }
    let item_count = (read_u16(data, pl_start + 6) as usize).min(MAX_PLAY_ITEMS);

    let mut items = Vec::new();
    let mut p = pl_start + 10;
    for _ in 0..item_count {
        // PlayItem: u16 length (bytes after the field), clip id at +2 (5
        // ASCII), codec id at +7 ("M2TS"), 2 flag bytes + STC id, IN/OUT
        // 45 kHz u32 at +14/+18. Everything past OUT (UO masks, STN table,
        // angle blocks) is skipped by the length.
        let len = read_u16(data, p) as usize;
        let end = p + 2 + len;
        if len < 20 || end > data.len() {
            break; // truncated/corrupt tail: keep the items already parsed
        }
        let clip_id = &data[p + 2..p + 7];
        let codec_id = &data[p + 7..p + 11];
        if codec_id == b"M2TS" && clip_id.iter().all(|b| b.is_ascii_graphic()) {
            items.push(PlayItem {
                clip_id: String::from_utf8_lossy(clip_id).into_owned(),
                in_45k: read_u32(data, p + 14),
                out_45k: read_u32(data, p + 18),
            });
        }
        p = end;
    }
    Ok(Playlist { items })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Assemble a synthetic MPLS holding the given `(clip_id, in, out)`
    /// segments. Also used by the sibling modules' tests to stuff playlist
    /// payloads into synthetic UDF images.
    pub(crate) fn build_mpls(segments: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"MPLS0200");
        let pl_start = 40u32; // arbitrary gap: offsets are honored, not assumed
        d.extend_from_slice(&pl_start.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes()); // mark start (unused)
        d.extend_from_slice(&0u32.to_be_bytes()); // extension start (unused)
        d.resize(pl_start as usize, 0);
        d.extend_from_slice(&0u32.to_be_bytes()); // PlayList length (unread)
        d.extend_from_slice(&[0, 0]); // reserved
        d.extend_from_slice(&(segments.len() as u16).to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes()); // sub-paths
        for (clip, in_t, out_t) in segments {
            let mut item = Vec::new();
            assert_eq!(clip.len(), 5);
            item.extend_from_slice(clip.as_bytes());
            item.extend_from_slice(b"M2TS");
            item.extend_from_slice(&[0, 0, 0]); // flags + STC id
            item.extend_from_slice(&in_t.to_be_bytes());
            item.extend_from_slice(&out_t.to_be_bytes());
            item.extend_from_slice(&[0u8; 8]); // trailing fields the parser skips
            d.extend_from_slice(&(item.len() as u16).to_be_bytes());
            d.extend_from_slice(&item);
        }
        d
    }

    #[test]
    fn parses_single_item() {
        let pl = parse(&build_mpls(&[("00055", 45_000, 45_000 * 61)])).unwrap();
        assert_eq!(pl.items.len(), 1);
        assert_eq!(pl.items[0].clip_id, "00055");
        assert!((pl.duration_secs_deduped() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn parses_multi_item_and_orders_clips() {
        let pl = parse(&build_mpls(&[
            ("00002", 0, 450_000),
            ("00001", 0, 900_000),
            ("00002", 450_000, 900_000),
        ]))
        .unwrap();
        assert_eq!(pl.items.len(), 3);
        assert_eq!(pl.distinct_clips(), vec!["00002", "00001"]);
        assert!((pl.duration_secs_deduped() - 40.0).abs() < 1e-9);
    }

    #[test]
    fn dedupes_looped_decoy_segments() {
        let mut segs = vec![("00033", 0u32, 450_000u32); 500];
        segs.push(("00034", 0, 90_000));
        let pl = parse(&build_mpls(&segs)).unwrap();
        assert_eq!(pl.items.len(), 501);
        // 500 identical segments collapse to one: 10s + 2s.
        assert!((pl.duration_secs_deduped() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn out_before_in_contributes_zero() {
        let pl = parse(&build_mpls(&[("00001", 900_000, 450_000)])).unwrap();
        assert_eq!(pl.duration_secs_deduped(), 0.0);
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        assert!(parse(b"XPLS0100____________").is_err());
        let mut d = build_mpls(&[("00001", 0, 45_000)]);
        d[4..8].copy_from_slice(b"0400");
        assert!(parse(&d).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn accepts_known_versions() {
        for v in [b"0100", b"0200", b"0300"] {
            let mut d = build_mpls(&[("00001", 0, 45_000)]);
            d[4..8].copy_from_slice(v);
            assert_eq!(parse(&d).unwrap().items.len(), 1);
        }
    }

    #[test]
    fn truncation_keeps_parsed_prefix_and_never_panics() {
        let full = build_mpls(&[("00001", 0, 450_000), ("00002", 0, 450_000)]);
        for cut in 0..full.len() {
            let _ = parse(&full[..cut]); // must not panic
        }
        // Cut inside the second item: the first survives.
        let pl = parse(&full[..full.len() - 4]).unwrap();
        assert_eq!(pl.items.len(), 1);
    }

    #[test]
    fn skips_non_m2ts_items() {
        let mut d = build_mpls(&[("00001", 0, 45_000), ("00002", 0, 45_000)]);
        // Corrupt the second item's codec id ("M2TS" of item 2).
        let pos = d.windows(4).rposition(|w| w == b"M2TS").unwrap();
        d[pos..pos + 4].copy_from_slice(b"XXXX");
        let pl = parse(&d).unwrap();
        assert_eq!(pl.items.len(), 1);
        assert_eq!(pl.items[0].clip_id, "00001");
    }
}
