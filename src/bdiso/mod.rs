//! Blu-ray ISO (BDMV) main-feature location: walk the UDF filesystem to
//! `BDMV/PLAYLIST` and `BDMV/STREAM`, rank the playlists by deduped duration,
//! and resolve the selected feature's largest clip to one contiguous byte
//! range of the image. `main.rs` then hands that subslice to the ordinary
//! TS/M2TS pipeline; every slice-relative mechanism (head/tail windows,
//! streaming positions, bitrate denominators, progress) is correct by
//! construction, and no HDR/DV fact is read here. Decrypted images only: an
//! AACS-encrypted clip fails the TS sync-lock gate and errors honestly.

mod mpls;
mod udf;

use std::collections::HashMap;
use std::fs::File;

use anyhow::{anyhow, bail, Context, Result};

pub use udf::is_udf_iso;

use crate::container::ts;
use crate::prefetch;

/// Playlists are KiB-scale; cap the read so a corrupt File Entry can't gather
/// megabytes per playlist.
const MPLS_READ_CAP: usize = 4 << 20;
/// Bytes of the clip head handed to the TS sync-lock gate.
const DETECT_WINDOW: u64 = 64 << 10;
/// Duration ties inside this band fall through to the byte tie-break.
const DURATION_TIE_SECS: f64 = 1.0;

/// The selected main feature. `clip_start`/`clip_len` are the absolute byte
/// range of the probed m2ts inside the image (extents coalesced).
#[derive(Debug)]
pub struct MainFeature {
    pub clip_start: u64,
    pub clip_len: u64,
    /// Selected playlist file name, e.g. `"00800.mpls"`.
    pub playlist: String,
    /// The playlist's own edit duration (deduped segments).
    pub playlist_duration_secs: f64,
    /// Probed clip file name, e.g. `"00055.m2ts"`.
    pub clip: String,
    /// 1-based position of the probed clip among the playlist's distinct
    /// clips in playback order.
    pub clip_index: usize,
    pub clip_count: usize,
}

pub fn locate_main_feature(data: &[u8], warm: Option<&File>) -> Result<MainFeature> {
    let vol = udf::UdfVolume::open(data).context("reading UDF volume structure")?;
    // Every file entry and directory the walk faults lives in the metadata
    // partition (UDF 2.50); stream it in one pipelined read on remote volumes.
    if let Some(file) = warm {
        prefetch::warm_ranges(file, vol.metadata_extents());
    }

    let root = vol.root()?;
    let entries = vol.read_dir(&root)?;
    let find = |list: &[udf::Entry], name: &str| -> Option<udf::Entry> {
        list.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned()
    };
    let has_aacs = find(&entries, "AACS").is_some();
    let Some(bdmv) = find(&entries, "BDMV").filter(|e| e.is_dir) else {
        if find(&entries, "VIDEO_TS").is_some() {
            bail!("DVD-Video ISO; only Blu-ray (BDMV) ISOs are supported");
        }
        bail!("no BDMV directory; not a Blu-ray ISO");
    };
    let bdmv_entries = vol.read_dir(&bdmv)?;
    let playlist_dir = find(&bdmv_entries, "PLAYLIST")
        .filter(|e| e.is_dir)
        .ok_or_else(|| anyhow!("no BDMV/PLAYLIST directory"))?;
    let stream_dir = find(&bdmv_entries, "STREAM")
        .filter(|e| e.is_dir)
        .ok_or_else(|| anyhow!("no BDMV/STREAM directory"))?;

    // Clip id ("00055") -> (entry, size) from BDMV/STREAM.
    let mut clips: HashMap<String, (udf::Entry, u64)> = HashMap::new();
    for e in vol.read_dir(&stream_dir)? {
        if e.is_dir {
            continue;
        }
        let Some(id) = clip_id_of(&e.name) else { continue };
        let Ok(size) = vol.info_len(&e) else { continue };
        clips.insert(id, (e, size));
    }
    let clip_sizes: HashMap<String, u64> =
        clips.iter().map(|(id, (_, size))| (id.clone(), *size)).collect();

    // Parse every playlist (tiny files; malformed ones are skipped, not
    // fatal). Warmed in one batch on remote volumes before the reads fault.
    let mut mpls_entries: Vec<udf::Entry> = vol
        .read_dir(&playlist_dir)?
        .into_iter()
        .filter(|e| !e.is_dir && has_ext(&e.name, ".mpls"))
        .collect();
    mpls_entries.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(file) = warm {
        let mut ranges = Vec::new();
        for e in &mpls_entries {
            if let Ok(extents) = vol.extents(e) {
                ranges.extend(extents.into_iter().map(|(o, l)| (o, l.min(MPLS_READ_CAP as u64) as usize)));
            }
        }
        prefetch::warm_ranges(file, ranges);
    }
    let mut cands = Vec::new();
    for e in &mpls_entries {
        if let Ok(bytes) = vol.read_small(e, MPLS_READ_CAP) {
            if let Ok(playlist) = mpls::parse(&bytes) {
                if !playlist.items.is_empty() {
                    cands.push(Candidate { name: e.name.clone(), playlist });
                }
            }
        }
    }

    let sel = select_main(&cands, &clip_sizes)?;

    let (clip_entry, _) = &clips[&sel.clip_id];
    let extents = vol.extents(clip_entry)?;
    let (clip_start, clip_len) = coalesce(&extents)
        .ok_or_else(|| anyhow!("main-feature m2ts is fragmented inside the ISO; not supported"))?;
    if clip_start.saturating_add(clip_len) > data.len() as u64 {
        bail!("main-feature m2ts extends past the end of the image (truncated ISO?)");
    }

    // The decrypted gate: AACS leaves only the first 16 bytes of each
    // 6144-byte aligned unit in the clear, so the 5-packet sync-stride lock
    // fails on an encrypted clip. Directory presence alone is never the
    // verdict: decrypted backups keep their AACS directory.
    let head = &data[clip_start as usize..(clip_start + clip_len.min(DETECT_WINDOW)) as usize];
    if ts::detect_layout(head).is_none() {
        if has_aacs {
            bail!("AACS-encrypted Blu-ray ISO; probe a decrypted backup");
        }
        bail!("main-feature clip {} is not a recognizable M2TS stream", clip_entry.name);
    }

    Ok(MainFeature {
        clip_start,
        clip_len,
        playlist: sel.name,
        playlist_duration_secs: sel.duration_secs,
        clip: clip_entry.name.clone(),
        clip_index: sel.clip_index,
        clip_count: sel.clip_count,
    })
}

struct Candidate {
    name: String,
    playlist: mpls::Playlist,
}

struct Selection {
    name: String,
    duration_secs: f64,
    clip_id: String,
    clip_index: usize,
    clip_count: usize,
}

/// The main-title heuristic: longest deduped duration wins, ties (within
/// `DURATION_TIE_SECS`) broken by total referenced clip bytes, then by the
/// lowest playlist name. Playlists referencing clips absent from STREAM are
/// dropped (robustness and decoy filtering), and identical PlayItem sequences
/// collapse to the lowest-numbered playlist. The probe clip is the winner's
/// largest referenced clip.
fn select_main(cands: &[Candidate], clip_sizes: &HashMap<String, u64>) -> Result<Selection> {
    let mut order: Vec<&Candidate> = cands.iter().collect();
    order.sort_by(|a, b| a.name.cmp(&b.name));
    let mut kept: Vec<&Candidate> = Vec::new();
    for c in order {
        if c.playlist.items.iter().any(|i| !clip_sizes.contains_key(&i.clip_id)) {
            continue;
        }
        if kept.iter().any(|k| k.playlist.items == c.playlist.items) {
            continue;
        }
        kept.push(c);
    }

    let score = |c: &Candidate| -> (f64, u64) {
        let bytes = c.playlist.distinct_clips().iter().map(|id| clip_sizes[*id]).sum();
        (c.playlist.duration_secs_deduped(), bytes)
    };
    let mut best: Option<(&Candidate, f64, u64)> = None;
    for c in kept {
        let (d, b) = score(c);
        let wins = match best {
            None => true,
            Some((_, bd, bb)) => {
                if (d - bd).abs() > DURATION_TIE_SECS {
                    d > bd
                } else {
                    b > bb // equal falls through: kept is name-sorted, first wins
                }
            }
        };
        if wins {
            best = Some((c, d, b));
        }
    }
    let (best, duration_secs, _) =
        best.ok_or_else(|| anyhow!("no usable playlist in BDMV/PLAYLIST"))?;

    let distinct = best.playlist.distinct_clips();
    let mut pick = 0usize;
    for (i, id) in distinct.iter().enumerate() {
        if clip_sizes[*id] > clip_sizes[distinct[pick]] {
            pick = i;
        }
    }
    Ok(Selection {
        name: best.name.clone(),
        duration_secs,
        clip_id: distinct[pick].to_string(),
        clip_index: pick + 1,
        clip_count: distinct.len(),
    })
}

/// One contiguous `(start, len)` from file-ordered extents, or `None` when a
/// gap remains. UDF caps a single extent near 1 GiB, so a feature-length clip
/// is many exactly-adjacent extents; a genuinely scattered file stays `None`.
fn coalesce(extents: &[(u64, u64)]) -> Option<(u64, u64)> {
    let (&(start, first_len), rest) = extents.split_first()?;
    let mut end = start.checked_add(first_len)?;
    for &(off, len) in rest {
        if off != end {
            return None;
        }
        end = end.checked_add(len)?;
    }
    (end > start).then_some((start, end - start))
}

/// `"00055.m2ts"` -> `"00055"`; `None` for non-m2ts names.
fn clip_id_of(name: &str) -> Option<String> {
    has_ext(name, ".m2ts").then(|| name[..name.len() - 5].to_ascii_uppercase())
}

fn has_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

#[cfg(test)]
mod tests {
    use super::udf::testimg::{DirSpec, Opts};
    use super::*;

    /// Clip payload the TS sync-lock accepts: M2TS framing, 0x47 at offset 4
    /// of each 192-byte packet.
    fn m2ts_bytes(packets: usize) -> Vec<u8> {
        let mut d = vec![0u8; packets * 192];
        for p in 0..packets {
            d[p * 192 + 4] = 0x47;
        }
        d
    }

    fn mpls(segments: &[(&str, u32, u32)]) -> Vec<u8> {
        super::mpls::tests::build_mpls(segments)
    }

    /// A disc with a short extras playlist and a longer two-clip feature.
    fn feature_tree() -> DirSpec {
        DirSpec::named("").dir(
            DirSpec::named("BDMV")
                .dir(
                    DirSpec::named("PLAYLIST")
                        .file("00000.mpls", mpls(&[("00001", 0, 45_000 * 60)]))
                        .file(
                            "00800.mpls",
                            mpls(&[("00010", 0, 45_000 * 3600), ("00011", 0, 45_000 * 1800)]),
                        ),
                )
                .dir(
                    DirSpec::named("STREAM")
                        .file("00001.m2ts", m2ts_bytes(8))
                        .file("00010.m2ts", m2ts_bytes(64))
                        .file("00011.m2ts", m2ts_bytes(16)),
                ),
        )
    }

    fn locate(tree: &DirSpec, metadata: bool) -> Result<MainFeature> {
        let img = super::udf::testimg::build(tree, &Opts { metadata_partition: metadata });
        locate_main_feature(&img, None)
    }

    #[test]
    fn selects_longest_playlist_and_largest_clip() {
        for metadata in [false, true] {
            let tree = feature_tree();
            let img = super::udf::testimg::build(&tree, &Opts { metadata_partition: metadata });
            let f = locate_main_feature(&img, None).unwrap();
            assert_eq!(f.playlist, "00800.mpls");
            assert_eq!(f.clip, "00010.m2ts");
            assert_eq!((f.clip_index, f.clip_count), (1, 2));
            assert!((f.playlist_duration_secs - 5400.0).abs() < 1e-9);
            assert_eq!(f.clip_len, 64 * 192);
            // The range really is the clip: M2TS sync at offset 4.
            assert_eq!(img[f.clip_start as usize + 4], 0x47);
        }
    }

    #[test]
    fn loop_decoy_loses_to_the_real_feature() {
        let mut tree = feature_tree();
        // A decoy looping one 10-second segment 400 times (67 min raw).
        let decoy: Vec<(&str, u32, u32)> = vec![("00001", 0, 450_000); 400];
        tree.dirs[0].dirs[0].files.push(super::udf::testimg::FileSpec {
            name: "00999.mpls".into(),
            data: mpls(&decoy),
            fragment: false,
        });
        let f = locate(&tree, false).unwrap();
        assert_eq!(f.playlist, "00800.mpls");
    }

    #[test]
    fn playlist_referencing_missing_clip_is_dropped() {
        let mut tree = feature_tree();
        tree.dirs[0].dirs[0].files.push(super::udf::testimg::FileSpec {
            name: "00001.mpls".into(),
            data: mpls(&[("99999", 0, 45_000 * 7200)]),
            fragment: false,
        });
        let f = locate(&tree, false).unwrap();
        assert_eq!(f.playlist, "00800.mpls");
    }

    #[test]
    fn identical_playlists_collapse_to_the_lowest_name() {
        let mut tree = feature_tree();
        let dup = tree.dirs[0].dirs[0].files[1].data.clone();
        tree.dirs[0].dirs[0].files.push(super::udf::testimg::FileSpec {
            name: "00900.mpls".into(),
            data: dup,
            fragment: false,
        });
        let f = locate(&tree, false).unwrap();
        assert_eq!(f.playlist, "00800.mpls");
    }

    #[test]
    fn encrypted_clip_with_aacs_dir_reports_encryption() {
        let mut tree = feature_tree();
        tree.dirs.push(DirSpec::named("AACS"));
        // Garbage clip bytes: the sync lock fails.
        for f in &mut tree.dirs[0].dirs[1].files {
            f.data = vec![0u8; f.data.len()];
        }
        let err = locate(&tree, false).unwrap_err().to_string();
        assert!(err.contains("AACS-encrypted"), "{err}");
    }

    #[test]
    fn garbage_clip_without_aacs_reports_the_clip() {
        let mut tree = feature_tree();
        for f in &mut tree.dirs[0].dirs[1].files {
            f.data = vec![0u8; f.data.len()];
        }
        let err = locate(&tree, false).unwrap_err().to_string();
        assert!(err.contains("not a recognizable M2TS"), "{err}");
    }

    #[test]
    fn decrypted_backup_with_aacs_dir_still_probes() {
        let mut tree = feature_tree();
        tree.dirs.push(DirSpec::named("AACS"));
        assert!(locate(&tree, false).is_ok());
    }

    #[test]
    fn dvd_and_non_bd_isos_error_honestly() {
        let dvd = DirSpec::named("").dir(DirSpec::named("VIDEO_TS"));
        let err = locate(&dvd, false).unwrap_err().to_string();
        assert!(err.contains("DVD-Video"), "{err}");

        let other = DirSpec::named("").dir(DirSpec::named("DATA"));
        let err = locate(&other, false).unwrap_err().to_string();
        assert!(err.contains("no BDMV"), "{err}");
    }

    #[test]
    fn fragmented_main_clip_errors_honestly() {
        let mut tree = feature_tree();
        tree.dirs[0].dirs[1].files[1].fragment = true;
        let err = locate(&tree, false).unwrap_err().to_string();
        assert!(err.contains("fragmented"), "{err}");
    }

    #[test]
    fn coalesce_merges_adjacent_and_rejects_gaps() {
        assert_eq!(coalesce(&[(100, 50), (150, 50)]), Some((100, 100)));
        assert_eq!(coalesce(&[(100, 50)]), Some((100, 50)));
        assert_eq!(coalesce(&[(100, 50), (200, 50)]), None);
        assert_eq!(coalesce(&[]), None);
    }

    #[test]
    fn tie_break_prefers_more_bytes_then_lowest_name() {
        let sizes: HashMap<String, u64> =
            [("00001".to_string(), 100u64), ("00002".to_string(), 900u64)].into();
        let cands = vec![
            Candidate {
                name: "00500.mpls".into(),
                playlist: super::mpls::parse(&mpls(&[("00001", 0, 45_000 * 60)])).unwrap(),
            },
            Candidate {
                name: "00600.mpls".into(),
                playlist: super::mpls::parse(&mpls(&[("00002", 0, 45_000 * 60)])).unwrap(),
            },
        ];
        let sel = select_main(&cands, &sizes).unwrap();
        assert_eq!(sel.name, "00600.mpls"); // same duration, more bytes
        assert_eq!(sel.clip_id, "00002");
    }
}
