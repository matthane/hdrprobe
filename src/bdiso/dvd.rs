//! DVD-Video main-feature location: find `VIDEO_TS`, group the title VOBs by
//! title set, pick the byte-largest set, and resolve it to one contiguous
//! byte range for the program-stream pipeline — the DVD counterpart of the
//! BDMV playlist walk, sharing the UDF walker and the subslice model.
//!
//! What a DVD states and where, verified against a real pressing (the
//! School of Rock ISO the corpus `dvd_vts04` files came from) and against
//! libdvdread (`ifo_types.h`/`ifo_read.c`, the reference implementation —
//! the DVD Forum spec itself is not publicly available):
//!
//! - **A title's video is `VTS_nn_1.VOB` .. `VTS_nn_9.VOB`**, sequential
//!   ≤1 GiB slices of *one* program stream. `VTS_nn_0.VOB` is the title
//!   set's menu and `VIDEO_TS.VOB` the disc menu — both excluded, and the
//!   menu VOB is exactly why the range never simply starts at the first
//!   VOB of the set (on the real disc the menu sits 136 MB before the
//!   feature, non-adjacent). The title VOBs of every set on the real disc
//!   are mutually contiguous — the spec's layout order (`_0.IFO`, `_0.VOB`,
//!   `_1..k.VOB`, `_0.BUP`) makes them so — which is what lets the set
//!   coalesce to one range exactly like a BD clip's 1 GiB UDF extents; a
//!   genuinely scattered set errors honestly, the BDMV rule.
//! - **The main feature is the byte-largest title set.** DVDs have no
//!   playlist ranking to mirror: the VMG's title table (`TT_SRPT`) maps
//!   title numbers to sets but says nothing about which is the feature, and
//!   on every disc observed the feature dwarfs the extras (5.5 GB against
//!   1.9 GB for the next set on the reference pressing). Ties break to the
//!   lowest set number, deterministically.
//! - **The declared feature duration lives in the title set's IFO**, not in
//!   any container field: `VTS_nn_0.IFO` carries the PGC table
//!   (`vts_pgcit`, a sector pointer at byte 0xCC of the `DVDVIDEO-VTS`
//!   header), and each PGC's `playback_time` is BCD `hh:mm:ss:ff` with the
//!   frame-rate flag in the top two bits of the frame byte (`0b11` = 30
//!   fps, `0b01` = 25). The longest PGC is the feature's declared runtime —
//!   MediaInfo's number for the same IFO (6547.500 s on the reference disc,
//!   reproduced exactly). It renders on the `Main feature` line, and —
//!   `main.rs`'s post-pass — **it is the report's duration authority**: a
//!   parsed IFO takes the Duration line and the overall-bitrate denominator
//!   (the MKV/MP4 declared-duration convention), because the PS backend's
//!   measured PTS span is structurally blind here. A cell or layer-break
//!   timestamp reset between the head and tail windows is invisible to
//!   both (the documented concatenation limit), and the real dual-layer
//!   reference pressing measured 33 minutes of a declared 109-minute
//!   feature exactly that way. A missing or unparseable IFO leaves the
//!   measured span standing, with those limits — selection never depends
//!   on the IFO either way.
//!
//! CSS is the AACS analogue and is detected in the PS backend itself (a
//! scrambled video PES in the head window errors honestly), because the
//! signal — `PES_scrambling_control` — is per-packet, not per-filesystem:
//! nothing in the UDF tree distinguishes an encrypted image from a
//! decrypted backup.

use anyhow::{anyhow, bail, Result};

use super::udf::{Entry, UdfVolume};

/// The selected DVD main feature: the absolute byte range of the title VOB
/// set inside the image, plus the facts the `Main feature` line renders.
#[derive(Debug)]
pub struct DvdFeature {
    pub clip_start: u64,
    pub clip_len: u64,
    /// Title set number (`VTS_nn`).
    pub vts: u16,
    /// Title VOBs in the probed set (`VTS_nn_1.VOB` .. `VTS_nn_k.VOB`).
    pub vob_count: usize,
    /// The longest PGC playback time declared by the set's IFO, when one
    /// parsed.
    pub title_duration_secs: Option<f64>,
}

/// IFOs are tens to hundreds of KiB; cap the read so a corrupt File Entry
/// can't gather more (the `MPLS_READ_CAP` convention).
const IFO_READ_CAP: usize = 4 << 20;

/// A VTS holds at most 99 titles; bound the PGC walk accordingly.
const MAX_PGCS: usize = 100;

pub(super) fn locate(vol: &UdfVolume, video_ts: &Entry, data: &[u8]) -> Result<DvdFeature> {
    let entries = vol.read_dir(video_ts)?;

    // Group the *title* VOBs by set: VTS_nn_k.VOB with k >= 1. Menu VOBs
    // (k = 0) and VIDEO_TS.VOB never join a group.
    type Slices = Vec<(u8, Entry, u64)>;
    let mut sets: Vec<(u16, Slices)> = Vec::new();
    for e in &entries {
        if e.is_dir {
            continue;
        }
        let Some((vts, k)) = title_vob_name(&e.name) else { continue };
        let Ok(size) = vol.info_len(e) else { continue };
        match sets.iter_mut().find(|(n, _)| *n == vts) {
            Some((_, v)) => v.push((k, e.clone(), size)),
            None => sets.push((vts, vec![(k, e.clone(), size)])),
        }
    }
    if sets.is_empty() {
        bail!("VIDEO_TS holds no title VOBs (menu-only or empty disc image)");
    }

    // Byte-largest set wins; ties to the lowest set number.
    sets.sort_by_key(|(n, _)| *n);
    let (vts, mut vobs) = sets
        .into_iter()
        .max_by_key(|(n, v)| (v.iter().map(|(_, _, s)| *s).sum::<u64>(), u16::MAX - *n))
        .expect("sets is non-empty");

    // The slices must be consecutive from 1: a missing middle VOB with
    // adjacent survivors would splice two halves of the stream together.
    vobs.sort_by_key(|(k, _, _)| *k);
    for (i, (k, _, _)) in vobs.iter().enumerate() {
        if usize::from(*k) != i + 1 {
            bail!("title set VTS_{vts:02} is missing VTS_{vts:02}_{}.VOB", i + 1);
        }
    }

    // One contiguous range across the whole set, exactly the BD clip rule.
    let mut extents: Vec<(u64, u64)> = Vec::new();
    for (_, e, _) in &vobs {
        extents.extend(vol.extents(e)?);
    }
    let (clip_start, clip_len) = super::coalesce(&extents).ok_or_else(|| {
        anyhow!("title set VTS_{vts:02} is fragmented inside the ISO; not supported")
    })?;
    if clip_start.saturating_add(clip_len) > data.len() as u64 {
        bail!("title set VTS_{vts:02} extends past the end of the image (truncated ISO?)");
    }

    // The set's IFO: the declared feature duration, when it parses.
    let ifo_name = format!("VTS_{vts:02}_0.IFO");
    let title_duration_secs = entries
        .iter()
        .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(&ifo_name))
        .and_then(|e| vol.read_small(e, IFO_READ_CAP).ok())
        .and_then(|ifo| vts_title_duration(&ifo));

    Ok(DvdFeature { clip_start, clip_len, vts, vob_count: vobs.len(), title_duration_secs })
}

/// `VTS_nn_k.VOB` with `k` in 1..=9 → `(nn, k)`; anything else — menu VOBs
/// (`k` = 0), `VIDEO_TS.VOB`, IFOs, BUPs — is `None`.
fn title_vob_name(name: &str) -> Option<(u16, u8)> {
    let upper = name.to_ascii_uppercase();
    let rest = upper.strip_prefix("VTS_")?.strip_suffix(".VOB")?;
    let (nn, k) = rest.split_once('_')?;
    // Two ASCII digits exactly: `parse` alone would admit a leading `+`
    // ("VTS_+4_1.VOB" -> set 4), and the crafted duplicate would then fail
    // the whole probe's consecutive-slice check instead of being ignored.
    if nn.len() != 2 || !nn.bytes().all(|b| b.is_ascii_digit()) || k.len() != 1 {
        return None;
    }
    let vts: u16 = nn.parse().ok()?;
    let k: u8 = k.parse().ok()?;
    (k >= 1).then_some((vts, k))
}

/// The longest PGC playback time in a VTS IFO, in seconds.
///
/// Layout per libdvdread (`ifo_types.h`: `vtsi_mat_t.vts_pgcit` at byte
/// 0xCC, sector-addressed; `pgcit_t.nr_of_pgci_srp`; 8-byte `pgci_srp_t`
/// entries from +8 whose `pgc_start_byte` is PGCIT-relative; `pgc_t`'s
/// `playback_time` at +4), validated byte-for-byte against the reference
/// disc's real IFO, whose one PGC decodes to MediaInfo's exact 6547.500.
fn vts_title_duration(ifo: &[u8]) -> Option<f64> {
    if !ifo.starts_with(b"DVDVIDEO-VTS") {
        return None;
    }
    let pgcit = usize::try_from(be32(ifo, 0xCC)?).ok()?.checked_mul(2048)?;
    let n = usize::from(be16(ifo, pgcit)?);
    // The table's own declared extent: `last_byte` (PGCIT-relative,
    // inclusive) at +4. libdvdread validates every entry against it
    // (`pgc_start_byte + PGC_SIZE <= last_byte + 1`), and so does this walk
    // — without the bound, a corrupt entry count reads bytes *outside* the
    // table as SRPs, and any garbage offset whose time bytes happened to
    // pass the BCD gates would fabricate a runtime the longest-wins fold
    // could prefer over the real one. (This walk reads only the 8-byte PGC
    // head, so 8 is its bound where libdvdread uses the full struct size.)
    let table_end = pgcit
        .checked_add(usize::try_from(be32(ifo, pgcit.checked_add(4)?)?).ok()?)?
        .checked_add(1)?;
    let mut best: Option<f64> = None;
    for i in 0..n.min(MAX_PGCS) {
        // SRPs are sequential: the first one past the table ends the walk.
        let srp = match pgcit.checked_add(8 + i * 8) {
            Some(s) if s.checked_add(8).is_some_and(|e| e <= table_end) => s,
            _ => break,
        };
        // A single corrupt entry skips — the remaining entries may still
        // hold the real PGC (unlike libdvdread, which fails the whole
        // table; a partial read risks nothing here because every candidate
        // is bounds- and BCD-gated).
        let Some(off) = be32(ifo, srp + 4) else { continue };
        let Some(pgc) = pgcit.checked_add(usize::try_from(off).ok()?) else { continue };
        let Some(time_end) = pgc.checked_add(8).filter(|e| *e <= table_end) else { continue };
        let Some(t) = ifo.get(time_end - 4..time_end) else { continue };
        // BCD hh:mm:ss + frame-rate bits over a BCD frame count. Reserved
        // rate codes and non-BCD digits mark a corrupt entry, skipped —
        // decoding them would fabricate a runtime.
        let fps = match t[3] >> 6 {
            0b11 => 30.0,
            0b01 => 25.0,
            _ => continue,
        };
        let (Some(h), Some(m), Some(s), Some(f)) =
            (bcd(t[0]), bcd(t[1]), bcd(t[2]), bcd(t[3] & 0x3F))
        else {
            continue;
        };
        let secs = f64::from(h) * 3600.0 + f64::from(m) * 60.0 + f64::from(s) + f64::from(f) / fps;
        if best.is_none_or(|b| secs > b) {
            best = Some(secs);
        }
    }
    best.filter(|s| *s > 0.0)
}

fn bcd(b: u8) -> Option<u32> {
    let (hi, lo) = (b >> 4, b & 0x0F);
    (hi <= 9 && lo <= 9).then(|| u32::from(hi) * 10 + u32::from(lo))
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal VTS IFO: signature, the PGCIT pointer at 0xCC, and a PGC
    /// table with the given playback times (already-encoded 4-byte values).
    fn ifo_with_pgcs(times: &[[u8; 4]]) -> Vec<u8> {
        let mut d = vec![0u8; 2048 * 2];
        d[..12].copy_from_slice(b"DVDVIDEO-VTS");
        d[0xCC..0xD0].copy_from_slice(&1u32.to_be_bytes()); // PGCIT at sector 1
        let base = 2048;
        d[base..base + 2].copy_from_slice(&(times.len() as u16).to_be_bytes());
        // The table's own inclusive extent (`last_byte`), which the walk
        // bounds every entry against.
        let total = 8 + times.len() * 8 + times.len() * 16;
        d[base + 4..base + 8].copy_from_slice(&(total as u32 - 1).to_be_bytes());
        // PGCs follow the SRP table; each PGC is 8 bytes of header + time.
        let pgc0 = 8 + times.len() * 8;
        for (i, t) in times.iter().enumerate() {
            let srp = base + 8 + i * 8;
            d[srp] = 0x81 + i as u8; // entry id, cosmetic
            let off = (pgc0 + i * 16) as u32;
            d[srp + 4..srp + 8].copy_from_slice(&off.to_be_bytes());
            let pgc = base + pgc0 + i * 16;
            if d.len() < pgc + 8 {
                d.resize(pgc + 8, 0);
            }
            d[pgc + 4..pgc + 8].copy_from_slice(t);
        }
        d
    }

    #[test]
    fn the_real_discs_pgc_time_decodes_to_mediainfos_value() {
        // The reference IFO's one PGC: BCD 01:49:07.15 with the 30 fps flag
        // (top bits 0b11), i.e. bytes 01 49 07 D5 — MediaInfo says 6547.500.
        let ifo = ifo_with_pgcs(&[[0x01, 0x49, 0x07, 0xC0 | 0x15]]);
        assert_eq!(vts_title_duration(&ifo), Some(6547.5));
    }

    #[test]
    fn the_longest_pgc_wins_and_25fps_uses_its_own_divisor() {
        // 00:05:00.00 @30 and 01:30:00.12 @25 (0b01 in the top bits).
        let ifo = ifo_with_pgcs(&[
            [0x00, 0x05, 0x00, 0xC0],
            [0x01, 0x30, 0x00, 0x40 | 0x12],
        ]);
        let d = vts_title_duration(&ifo).unwrap();
        assert!((d - (5400.0 + 12.0 / 25.0)).abs() < 1e-9);
    }

    #[test]
    fn corrupt_entries_are_skipped_never_decoded() {
        // Non-BCD digits (0xAB) and a reserved frame-rate code (0b00) both
        // skip; a table of only such entries yields no duration at all.
        let ifo = ifo_with_pgcs(&[[0xAB, 0x00, 0x00, 0xC0], [0x00, 0x10, 0x00, 0x00]]);
        assert_eq!(vts_title_duration(&ifo), None);
        // And a good entry beside them still parses.
        let ifo = ifo_with_pgcs(&[[0xAB, 0x00, 0x00, 0xC0], [0x00, 0x10, 0x00, 0xC0]]);
        assert_eq!(vts_title_duration(&ifo), Some(600.0));
    }

    #[test]
    fn degenerate_ifos_yield_nothing_and_never_panic() {
        assert_eq!(vts_title_duration(b""), None);
        assert_eq!(vts_title_duration(b"DVDVIDEO-VTS"), None, "no room for the pointer");
        // A PGCIT pointer far outside the read: every access is bounded.
        let mut d = vec![0u8; 2048];
        d[..12].copy_from_slice(b"DVDVIDEO-VTS");
        d[0xCC..0xD0].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert_eq!(vts_title_duration(&d), None);
        // A VMG IFO (different signature) is refused, not misread.
        let mut vmg = ifo_with_pgcs(&[[0x00, 0x10, 0x00, 0xC0]]);
        vmg[..12].copy_from_slice(b"DVDVIDEO-VMG");
        assert_eq!(vts_title_duration(&vmg), None);
    }

    #[test]
    fn the_table_extent_bounds_every_entry() {
        // A corrupt entry count: `last_byte` still bounds the SRP walk, so
        // the bytes past the real table are never decoded as entries — the
        // real PGC survives and nothing is fabricated from table-external
        // bytes (with the MAX_PGCS cap as the outer stop).
        let mut d = ifo_with_pgcs(&[[0x00, 0x10, 0x00, 0xC0]]);
        d[2048..2050].copy_from_slice(&0xFFFFu16.to_be_bytes());
        assert_eq!(vts_title_duration(&d), Some(600.0));

        // A PGC offset pointing outside the table's declared extent is one
        // corrupt entry, skipped — a good sibling still parses.
        let mut d = ifo_with_pgcs(&[[0x00, 0x05, 0x00, 0xC0], [0x00, 0x10, 0x00, 0xC0]]);
        d[2048 + 8 + 4..2048 + 8 + 8].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());
        assert_eq!(vts_title_duration(&d), Some(600.0));
    }

    use super::super::udf::testimg::{self, DirSpec, FileSpec, Opts};
    use super::super::DiscFeature;

    /// A VOB-shaped payload: sector-multiple length (every real VOB is —
    /// DVD sectors are 2048 bytes — and the coalescing depends on it), with
    /// a recognizable first byte.
    fn vob(sectors: usize, mark: u8) -> Vec<u8> {
        let mut d = vec![0u8; sectors * 2048];
        d[0] = mark;
        d
    }

    fn dvd_tree() -> DirSpec {
        DirSpec::named("").dir(
            DirSpec::named("VIDEO_TS")
                .file("VIDEO_TS.IFO", vec![0u8; 2048])
                .file("VIDEO_TS.VOB", vob(2, 0x10)) // disc menu, never a title
                .file("VTS_01_0.IFO", ifo_with_pgcs(&[[0x00, 0x02, 0x00, 0xC0]]))
                .file("VTS_01_1.VOB", vob(2, 0x11)) // decoy extras set
                .file("VTS_02_0.IFO", ifo_with_pgcs(&[[0x01, 0x49, 0x07, 0xD5]]))
                .file("VTS_02_0.VOB", vob(4, 0x20)) // the set's menu, excluded
                .file("VTS_02_1.VOB", vob(10, 0x21))
                .file("VTS_02_2.VOB", vob(6, 0x22)),
        )
    }

    fn locate_dvd(tree: &DirSpec) -> anyhow::Result<super::DvdFeature> {
        let img = testimg::build(tree, &Opts { metadata_partition: false });
        match super::super::locate_feature(&img, None)? {
            DiscFeature::Dvd(f) => Ok(f),
            DiscFeature::Bd(_) => panic!("DVD tree located as BD"),
        }
    }

    #[test]
    fn the_byte_largest_title_set_wins_and_menus_never_join() {
        let tree = dvd_tree();
        let img = testimg::build(&tree, &Opts { metadata_partition: false });
        let DiscFeature::Dvd(f) = super::super::locate_feature(&img, None).unwrap() else {
            panic!("DVD tree located as BD")
        };
        assert_eq!(f.vts, 2);
        assert_eq!(f.vob_count, 2);
        assert_eq!(f.clip_len, (10 + 6) * 2048, "the two title VOBs, coalesced");
        // The range starts at VTS_02_1's own bytes — not the menu VOB's.
        assert_eq!(img[f.clip_start as usize], 0x21);
        assert_eq!(f.title_duration_secs, Some(6547.5), "the IFO's declared runtime");
    }

    #[test]
    fn a_missing_ifo_costs_only_the_declared_duration() {
        let mut tree = dvd_tree();
        tree.dirs[0].files.retain(|f| f.name != "VTS_02_0.IFO");
        let f = locate_dvd(&tree).unwrap();
        assert_eq!(f.vts, 2);
        assert_eq!(f.title_duration_secs, None);
    }

    #[test]
    fn a_fragmented_title_set_errors_honestly() {
        let mut tree = dvd_tree();
        let f = tree.dirs[0].files.iter_mut().find(|f| f.name == "VTS_02_1.VOB").unwrap();
        f.fragment = true;
        let err = locate_dvd(&tree).unwrap_err().to_string();
        assert!(err.contains("fragmented"), "{err}");
    }

    #[test]
    fn a_missing_middle_slice_errors_rather_than_splicing() {
        let mut tree = dvd_tree();
        tree.dirs[0].files.push(FileSpec {
            name: "VTS_03_1.VOB".into(),
            data: vob(20, 0x31),
            fragment: false,
        });
        tree.dirs[0].files.push(FileSpec {
            name: "VTS_03_3.VOB".into(),
            data: vob(20, 0x33),
            fragment: false,
        });
        let err = locate_dvd(&tree).unwrap_err().to_string();
        assert!(err.contains("missing VTS_03_2.VOB"), "{err}");
    }

    /// Regenerates `testfiles/sdr/dvdiso.iso` from the real corpus VOB + IFO
    /// (a decoy title set beside the real VTS 04 split into two sector-
    /// aligned slices). Ordinarily a silent no-op; run explicitly with
    /// `HDRPROBE_WRITE_DVD_FIXTURE=<abs path to testfiles/sdr> cargo test
    /// write_dvd_fixture`. Env-gated so the suite stays path-portable.
    #[test]
    fn write_dvd_fixture_image() {
        let Ok(dir) = std::env::var("HDRPROBE_WRITE_DVD_FIXTURE") else { return };
        let dir = std::path::PathBuf::from(dir);
        let vob_bytes = std::fs::read(dir.join("dvd_vts04.vob")).expect("dvd_vts04.vob");
        let ifo = std::fs::read(dir.join("dvd_vts04_0.ifo")).expect("dvd_vts04_0.ifo");
        assert_eq!(vob_bytes.len() % 2048, 0, "real VOBs are sector multiples");
        let half = (vob_bytes.len() / 2 / 2048) * 2048;
        let (a, b) = vob_bytes.split_at(half);
        let tree = DirSpec::named("").dir(
            DirSpec::named("VIDEO_TS")
                .file("VIDEO_TS.IFO", vec![0u8; 2048])
                .file("VTS_01_0.IFO", ifo_with_pgcs(&[[0x00, 0x01, 0x00, 0xC0]]))
                .file("VTS_01_1.VOB", vob(4, 0x11))
                .file("VTS_04_0.IFO", ifo)
                .file("VTS_04_0.VOB", vob(4, 0x40))
                .file("VTS_04_1.VOB", a.to_vec())
                .file("VTS_04_2.VOB", b.to_vec()),
        );
        let img = testimg::build(&tree, &Opts { metadata_partition: false });
        std::fs::write(dir.join("dvdiso.iso"), img).expect("write dvdiso.iso");
    }

    #[test]
    fn title_vob_names_are_matched_exactly() {
        assert_eq!(title_vob_name("VTS_04_1.VOB"), Some((4, 1)));
        assert_eq!(title_vob_name("vts_12_9.vob"), Some((12, 9)), "case-insensitive");
        assert_eq!(title_vob_name("VTS_04_0.VOB"), None, "menu VOB");
        assert_eq!(title_vob_name("VIDEO_TS.VOB"), None, "disc menu");
        assert_eq!(title_vob_name("VTS_04_0.IFO"), None);
        assert_eq!(title_vob_name("VTS_4_1.VOB"), None, "set number is two digits");
        assert_eq!(title_vob_name("VTS_04_10.VOB"), None, "slice index is one digit");
    }
}
