//! Read-only UDF walker over a mmapped ISO image: just enough of ECMA-167 /
//! UDF 2.50 to find `BDMV/PLAYLIST` and `BDMV/STREAM`, list their entries, and
//! resolve a file's byte extents. Handles both the plain type-1 partition map
//! (UDF 1.02/2.01 images) and UDF 2.50's Metadata Partition (BD-ROM's shape:
//! file entries and directories live in a metadata *file* whose extents remap
//! onto the physical partition). UDF is little-endian (explicit LE reads,
//! never native), and every descriptor-declared count is clamped or bounded
//! (`mp4.rs` discipline): a corrupt image must yield `Err`, never a panic or a
//! runaway loop.

use anyhow::{anyhow, bail, Result};

/// ISO images address descriptors in 2048-byte sectors; UDF requires the
/// logical block size to match (enforced against the LVD in `open`).
const SECTOR: u64 = 2048;

/// Volume Recognition Sequence scan window (sectors 16..). Bridge images put
/// ISO9660 `CD001` descriptors ahead of the `BEA01`/`NSR` sequence, so scan a
/// window rather than requiring `NSR` at a fixed sector.
const VRS_SCAN_SECTORS: u64 = 64;

const MAX_VDS_SECTORS: u64 = 1024;
const MAX_FIDS_PER_DIR: usize = 65_536;
const MAX_EXTENTS_PER_FILE: usize = 65_536;
const MAX_AED_HOPS: usize = 64;
const MAX_ICB_INDIRECTIONS: usize = 4;
/// Directory data / small-file gather cap (BDMV directories and playlists are
/// KiB-scale; anything larger is corrupt or not ours to read).
const MAX_GATHER: usize = 8 << 20;

// Bounds-safe little-endian readers: OOB reads 0. Every value that gates a
// loop or an allocation is additionally clamped/capped at its use site.
fn read_u16(d: &[u8], o: usize) -> u16 {
    match d.get(o..o + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}
fn read_u32(d: &[u8], o: usize) -> u32 {
    match d.get(o..o + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}
fn read_u64(d: &[u8], o: usize) -> u64 {
    match d.get(o..o + 8) {
        Some(b) => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => 0,
    }
}

/// Verify a 16-byte descriptor tag (checksum byte 4 = sum of the other 15
/// header bytes) and return its tag identifier; `None` for anything invalid.
fn tag_id(d: &[u8], off: usize) -> Option<u16> {
    let t = d.get(off..off + 16)?;
    let mut sum = 0u8;
    for (i, b) in t.iter().enumerate() {
        if i != 4 {
            sum = sum.wrapping_add(*b);
        }
    }
    if sum != t[4] || (t[0] == 0 && t[1] == 0) {
        return None;
    }
    Some(u16::from_le_bytes([t[0], t[1]]))
}

/// long_ad / short_ad, normalized: `raw_len` keeps the extent-type bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LongAd {
    raw_len: u32,
    lbn: u32,
    prn: u16,
}

impl LongAd {
    fn parse_long(d: &[u8], o: usize) -> LongAd {
        LongAd { raw_len: read_u32(d, o), lbn: read_u32(d, o + 4), prn: read_u16(d, o + 8) }
    }
    fn parse_short(d: &[u8], o: usize, host_prn: u16) -> LongAd {
        LongAd { raw_len: read_u32(d, o), lbn: read_u32(d, o + 4), prn: host_prn }
    }
    fn len(&self) -> u64 {
        u64::from(self.raw_len & 0x3FFF_FFFF)
    }
    /// 0 recorded, 1 allocated-unrecorded, 2 unallocated, 3 continuation AED.
    fn extent_type(&self) -> u8 {
        (self.raw_len >> 30) as u8
    }
}

enum PartitionRef {
    /// Type-1 map: absolute start sector of the physical partition.
    Physical { start: u32 },
    /// UDF 2.50 metadata partition: logical blocks remap through the metadata
    /// file's extents, `(first_meta_block, abs_byte, block_count)` each.
    Metadata { extents: Vec<(u32, u64, u32)> },
}

/// A directory child as listed by `read_dir`; opaque beyond name and kind.
/// Hand it back to the volume for sizes/extents/content.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    icb: LongAd,
}

pub struct UdfVolume<'a> {
    data: &'a [u8],
    parts: Vec<PartitionRef>,
    fsd_icb: LongAd,
}

/// Volume Recognition Sequence check: an `NSR02`/`NSR03` descriptor in the
/// sector-16 window marks an ECMA-167 (UDF) volume. This is the cheap gate
/// `main.rs` uses before committing to the ISO path.
pub fn is_udf_iso(data: &[u8]) -> bool {
    for s in 16..VRS_SCAN_SECTORS {
        let off = (s * SECTOR) as usize;
        let Some(id) = data.get(off + 1..off + 6) else { return false };
        if id == b"NSR02" || id == b"NSR03" {
            return true;
        }
    }
    false
}

impl<'a> UdfVolume<'a> {
    /// Bounds-checked absolute byte slice.
    fn slice(&self, off: u64, len: u64) -> Result<&'a [u8]> {
        let end = off.checked_add(len).ok_or_else(|| anyhow!("extent overflows"))?;
        if end > self.data.len() as u64 {
            bail!("extent [{off}, {end}) outside the image");
        }
        Ok(&self.data[off as usize..end as usize])
    }

    fn resolve(&self, prn: u16, lbn: u32) -> Result<u64> {
        match self.parts.get(prn as usize) {
            Some(PartitionRef::Physical { start }) => {
                Ok((u64::from(*start) + u64::from(lbn)) * SECTOR)
            }
            Some(PartitionRef::Metadata { extents }) => {
                for (first, abs, blocks) in extents {
                    if lbn >= *first && lbn - *first < *blocks {
                        return Ok(abs + u64::from(lbn - *first) * SECTOR);
                    }
                }
                bail!("logical block {lbn} outside the metadata partition");
            }
            None => bail!("unknown partition reference {prn}"),
        }
    }

    pub fn open(data: &'a [u8]) -> Result<UdfVolume<'a>> {
        if !is_udf_iso(data) {
            bail!("not a UDF volume (no NSR descriptor in the volume recognition sequence)");
        }
        let sectors = data.len() as u64 / SECTOR;
        let mut vol = UdfVolume { data, parts: Vec::new(), fsd_icb: LongAd { raw_len: 0, lbn: 0, prn: 0 } };

        // Anchor Volume Descriptor Pointer: sector 256 primarily; the spec's
        // alternate anchors (N-1, N-257) rescue an image with a damaged head.
        let mut vds: Option<(u32, u32)> = None; // (location, length bytes)
        for anchor in [256u64, sectors.saturating_sub(257), sectors.saturating_sub(1)] {
            let off = anchor * SECTOR;
            if anchor >= 256 && tag_id(data, off as usize) == Some(2) {
                let o = off as usize;
                vds = Some((read_u32(data, o + 20), read_u32(data, o + 16)));
                break;
            }
        }
        let (vds_loc, vds_len) = vds.ok_or_else(|| anyhow!("no anchor volume descriptor"))?;

        // Volume Descriptor Sequence: collect the Partition Descriptor(s) and
        // the Logical Volume Descriptor; stop at the Terminating Descriptor.
        let mut pds: Vec<(u16, u32)> = Vec::new(); // (partition_number, start sector)
        let mut lvd: Option<&[u8]> = None;
        for i in 0..(u64::from(vds_len) / SECTOR).min(MAX_VDS_SECTORS) {
            let off = (u64::from(vds_loc) + i) * SECTOR;
            let Ok(sec) = vol.slice(off, SECTOR) else { break };
            match tag_id(sec, 0) {
                Some(5) => pds.push((read_u16(sec, 22), read_u32(sec, 188))),
                Some(6) => lvd = Some(sec),
                Some(8) | None => break,
                _ => {}
            }
        }
        let lvd = lvd.ok_or_else(|| anyhow!("no logical volume descriptor"))?;
        if read_u32(lvd, 212) != SECTOR as u32 {
            bail!("unsupported UDF logical block size {}", read_u32(lvd, 212));
        }
        vol.fsd_icb = LongAd::parse_long(lvd, 248);

        // Partition maps, in table order (long_ad partition reference numbers
        // index this order). Metadata maps are resolved in a second pass: the
        // metadata file's own File Entry lives in the physical partition.
        let map_count = read_u32(lvd, 268).min(64) as usize;
        let map_end = (440 + read_u32(lvd, 264) as usize).min(lvd.len());
        let mut meta_pending: Vec<(usize, u16, u32, u32)> = Vec::new(); // (part idx, phys num, file loc, mirror loc)
        let mut p = 440usize;
        for _ in 0..map_count {
            if p + 2 > map_end {
                break;
            }
            let (typ, len) = (lvd[p], lvd[p + 1] as usize);
            if len < 2 || p + len > map_end {
                break;
            }
            match typ {
                1 if len >= 6 => {
                    let num = read_u16(lvd, p + 4);
                    let start = pds
                        .iter()
                        .find(|(n, _)| *n == num)
                        .map(|(_, s)| *s)
                        .ok_or_else(|| anyhow!("partition map names unknown partition {num}"))?;
                    vol.parts.push(PartitionRef::Physical { start });
                }
                2 if len >= 64 && &lvd[p + 5..p + 28] == b"*UDF Metadata Partition" => {
                    meta_pending.push((
                        vol.parts.len(),
                        read_u16(lvd, p + 38),
                        read_u32(lvd, p + 40),
                        read_u32(lvd, p + 44),
                    ));
                    vol.parts.push(PartitionRef::Metadata { extents: Vec::new() });
                }
                2 => bail!(
                    "unsupported UDF partition map type '{}'",
                    String::from_utf8_lossy(&lvd[p + 5..p + 28]).trim_end()
                ),
                _ => bail!("unsupported UDF partition map type {typ}"),
            }
            p += len;
        }

        for (idx, phys_num, file_loc, mirror_loc) in meta_pending {
            let host_prn = vol
                .parts
                .iter()
                .position(|part| {
                    matches!(part, PartitionRef::Physical { start }
                        if pds.iter().any(|(n, s)| *n == phys_num && s == start))
                })
                .ok_or_else(|| anyhow!("metadata partition names unknown partition {phys_num}"))?
                as u16;
            let extents = vol
                .metadata_file_extents(host_prn, file_loc)
                .or_else(|_| vol.metadata_file_extents(host_prn, mirror_loc))?;
            vol.parts[idx] = PartitionRef::Metadata { extents };
        }
        Ok(vol)
    }

    /// The metadata file's recorded extents as `(first_meta_block, abs_byte,
    /// blocks)`: the metadata partition's block-remap table.
    fn metadata_file_extents(&self, host_prn: u16, lbn: u32) -> Result<Vec<(u32, u64, u32)>> {
        let fe = self.file_entry(LongAd { raw_len: 1, lbn, prn: host_prn })?;
        let mut out = Vec::new();
        let mut next_block: u32 = 0;
        match self.body(&fe)? {
            Body::Inline(_) => bail!("inline metadata file"),
            Body::Extents(extents) => {
                for (abs, len) in extents {
                    let blocks = (len / SECTOR) as u32;
                    out.push((next_block, abs, blocks));
                    next_block = next_block
                        .checked_add(blocks)
                        .ok_or_else(|| anyhow!("metadata file too large"))?;
                }
            }
        }
        if out.is_empty() {
            bail!("empty metadata file");
        }
        Ok(out)
    }

    /// Read a File Entry / Extended File Entry block, following a bounded
    /// chain of Indirect Entries (strategy-4096 volumes).
    fn file_entry(&self, mut icb: LongAd) -> Result<Fe<'a>> {
        for _ in 0..MAX_ICB_INDIRECTIONS {
            let off = self.resolve(icb.prn, icb.lbn)?;
            let block = self.slice(off, SECTOR)?;
            match tag_id(block, 0) {
                Some(261) => return Ok(Fe { block, efe: false, host_prn: icb.prn }),
                Some(266) => return Ok(Fe { block, efe: true, host_prn: icb.prn }),
                Some(259) => icb = LongAd::parse_long(block, 36),
                other => bail!("expected a file entry, found tag {other:?}"),
            }
        }
        bail!("indirect ICB chain too deep");
    }

    /// A file body: either data embedded in the File Entry, or recorded
    /// extents as absolute `(offset, len)` byte ranges in file order
    /// (uncoalesced; adjacency is the caller's concern).
    fn body(&self, fe: &Fe<'a>) -> Result<Body<'a>> {
        let (l_ea_off, l_ad_off, area_base) = if fe.efe { (208, 212, 216) } else { (168, 172, 176) };
        let l_ea = read_u32(fe.block, l_ea_off) as usize;
        let l_ad = read_u32(fe.block, l_ad_off) as usize;
        let start = area_base + l_ea;
        let area = fe
            .block
            .get(start..start.checked_add(l_ad).ok_or_else(|| anyhow!("bad AD area"))?)
            .ok_or_else(|| anyhow!("allocation descriptors outside the file entry"))?;
        let ad_form = read_u16(fe.block, 34) & 7; // icb_tag flags
        if ad_form == 3 {
            return Ok(Body::Inline(area));
        }
        let ad_size = match ad_form {
            0 => 8,  // short_ad: partition of the hosting file entry
            1 => 16, // long_ad
            other => bail!("unsupported allocation descriptor form {other}"),
        };

        let mut out = Vec::new();
        let mut area = area;
        let mut host_prn = fe.host_prn;
        'chain: for _ in 0..MAX_AED_HOPS {
            let mut p = 0;
            while p + ad_size <= area.len() {
                if out.len() >= MAX_EXTENTS_PER_FILE {
                    bail!("too many extents");
                }
                let ad = if ad_size == 8 {
                    LongAd::parse_short(area, p, host_prn)
                } else {
                    LongAd::parse_long(area, p)
                };
                p += ad_size;
                if ad.len() == 0 {
                    break 'chain;
                }
                match ad.extent_type() {
                    0 => out.push((self.resolve(ad.prn, ad.lbn)?, ad.len())),
                    3 => {
                        // Continuation: an Allocation Extent Descriptor block.
                        let off = self.resolve(ad.prn, ad.lbn)?;
                        let block = self.slice(off, SECTOR)?;
                        if tag_id(block, 0) != Some(258) {
                            bail!("bad allocation extent descriptor");
                        }
                        let len = (read_u32(block, 20) as usize).min(block.len() - 24);
                        area = &block[24..24 + len];
                        host_prn = ad.prn;
                        continue 'chain;
                    }
                    _ => bail!("unrecorded extent in file body"),
                }
            }
            break;
        }
        Ok(Body::Extents(out))
    }

    /// Gather a body into an owned buffer (directory data, playlist files).
    fn gather(&self, fe: &Fe<'a>, cap: usize) -> Result<Vec<u8>> {
        let info_len = read_u64(fe.block, 56);
        let take = info_len.min(cap as u64);
        match self.body(fe)? {
            Body::Inline(d) => Ok(d.get(..take.min(d.len() as u64) as usize).unwrap_or(d).to_vec()),
            Body::Extents(extents) => {
                let mut out = Vec::with_capacity(take as usize);
                for (off, len) in extents {
                    if out.len() as u64 >= take {
                        break;
                    }
                    let want = (take - out.len() as u64).min(len);
                    out.extend_from_slice(self.slice(off, want)?);
                }
                Ok(out)
            }
        }
    }

    /// The root directory as a synthetic entry for `read_dir`.
    pub fn root(&self) -> Result<Entry> {
        let off = self.resolve(self.fsd_icb.prn, self.fsd_icb.lbn)?;
        let fsd = self.slice(off, SECTOR)?;
        if tag_id(fsd, 0) != Some(256) {
            bail!("no file set descriptor");
        }
        Ok(Entry { name: String::new(), is_dir: true, icb: LongAd::parse_long(fsd, 400) })
    }

    /// List a directory's children (deleted and parent entries skipped).
    pub fn read_dir(&self, dir: &Entry) -> Result<Vec<Entry>> {
        if !dir.is_dir {
            bail!("not a directory");
        }
        let fe = self.file_entry(dir.icb)?;
        let buf = self.gather(&fe, MAX_GATHER)?;
        let mut out = Vec::new();
        let mut p = 0usize;
        while p + 38 <= buf.len() && out.len() < MAX_FIDS_PER_DIR {
            if tag_id(&buf, p) != Some(257) {
                break;
            }
            let chars = buf[p + 18];
            let l_fi = buf[p + 19] as usize;
            let l_iu = read_u16(&buf, p + 36) as usize;
            let total = (38 + l_iu + l_fi + 3) & !3;
            let Some(name_bytes) = buf.get(p + 38 + l_iu..p + 38 + l_iu + l_fi) else { break };
            // Skip parent (bit 3) and deleted (bit 2) entries.
            if chars & 0x0C == 0 && l_fi > 0 {
                out.push(Entry {
                    name: decode_ostaname(name_bytes),
                    is_dir: chars & 0x02 != 0,
                    icb: LongAd::parse_long(&buf, p + 20),
                });
            }
            p += total;
        }
        Ok(out)
    }

    /// The file's recorded size (File Entry `information_length`).
    pub fn info_len(&self, f: &Entry) -> Result<u64> {
        Ok(read_u64(self.file_entry(f.icb)?.block, 56))
    }

    /// Absolute `(offset, len)` byte extents in file order, uncoalesced.
    pub fn extents(&self, f: &Entry) -> Result<Vec<(u64, u64)>> {
        let fe = self.file_entry(f.icb)?;
        match self.body(&fe)? {
            Body::Inline(_) => bail!("file data is embedded in its file entry"),
            Body::Extents(e) => Ok(e),
        }
    }

    /// Read a small file whole (size-capped): playlists.
    pub fn read_small(&self, f: &Entry, cap: usize) -> Result<Vec<u8>> {
        let fe = self.file_entry(f.icb)?;
        self.gather(&fe, cap)
    }

    /// The metadata partition's extents (empty on a type-1-only volume), for
    /// the prefetch warm: every file entry and directory the walk will fault
    /// lives inside them.
    pub fn metadata_extents(&self) -> Vec<(u64, usize)> {
        let mut out = Vec::new();
        for part in &self.parts {
            if let PartitionRef::Metadata { extents } = part {
                for (_, abs, blocks) in extents {
                    out.push((*abs, (u64::from(*blocks) * SECTOR).min(MAX_GATHER as u64) as usize));
                }
            }
        }
        out
    }
}

struct Fe<'a> {
    block: &'a [u8],
    efe: bool,
    host_prn: u16,
}

enum Body<'a> {
    Inline(&'a [u8]),
    Extents(Vec<(u64, u64)>),
}

/// OSTA compressed unicode: byte 0 is the compression id, 8 = one byte per
/// character (latin-1), 16 = UTF-16BE pairs.
fn decode_ostaname(d: &[u8]) -> String {
    match d.first() {
        Some(8) => d[1..].iter().map(|&b| b as char).collect(),
        Some(16) => {
            let units: Vec<u16> =
                d[1..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::new(),
    }
}

/// Synthetic UDF image builder for tests: hand-rolls the descriptor chain the
/// walker consumes (VRS, AVDP, VDS, FSD, FEs, FIDs) into an in-memory image a
/// couple hundred KiB long. Type-1-only images use short_ad file data and
/// extent-recorded directories; metadata-partition images use long_ad file
/// data and inline directories, so both descriptor forms are exercised.
#[cfg(test)]
pub(crate) mod testimg {
    use super::SECTOR;

    pub(crate) struct FileSpec {
        pub name: String,
        pub data: Vec<u8>,
        /// Split the data into two extents with a one-block hole between them.
        pub fragment: bool,
    }

    #[derive(Default)]
    pub(crate) struct DirSpec {
        pub name: String,
        pub dirs: Vec<DirSpec>,
        pub files: Vec<FileSpec>,
    }

    impl DirSpec {
        pub(crate) fn named(name: &str) -> DirSpec {
            DirSpec { name: name.to_string(), ..Default::default() }
        }
        pub(crate) fn file(mut self, name: &str, data: Vec<u8>) -> DirSpec {
            self.files.push(FileSpec { name: name.to_string(), data, fragment: false });
            self
        }
        pub(crate) fn dir(mut self, d: DirSpec) -> DirSpec {
            self.dirs.push(d);
            self
        }
    }

    pub(crate) struct Opts {
        /// Build the UDF 2.50 shape: FSD/FEs/directories in a metadata
        /// partition, file data via long_ad. Otherwise plain type-1.
        pub metadata_partition: bool,
    }

    const PART_START: u64 = 300; // absolute sector of the physical partition
    const VDS_LOC: u32 = 32;

    struct Img {
        data: Vec<u8>,
    }

    impl Img {
        fn write(&mut self, abs_byte: u64, bytes: &[u8]) {
            let end = abs_byte as usize + bytes.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[abs_byte as usize..end].copy_from_slice(bytes);
        }
    }

    /// 16-byte descriptor tag + payload; checksum computed, CRC left zero
    /// (the walker verifies the checksum only).
    fn tagged(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; 16 + payload.len()];
        d[0..2].copy_from_slice(&id.to_le_bytes());
        d[2..4].copy_from_slice(&3u16.to_le_bytes()); // descriptor version
        d[16..].copy_from_slice(payload);
        let mut sum = 0u8;
        for (i, b) in d[..16].iter().enumerate() {
            if i != 4 {
                sum = sum.wrapping_add(*b);
            }
        }
        d[4] = sum;
        d
    }

    fn long_ad(len: u32, lbn: u32, prn: u16) -> [u8; 16] {
        let mut d = [0u8; 16];
        d[0..4].copy_from_slice(&len.to_le_bytes());
        d[4..8].copy_from_slice(&lbn.to_le_bytes());
        d[8..10].copy_from_slice(&prn.to_le_bytes());
        d
    }

    fn short_ad(len: u32, lbn: u32) -> [u8; 8] {
        let mut d = [0u8; 8];
        d[0..4].copy_from_slice(&len.to_le_bytes());
        d[4..8].copy_from_slice(&lbn.to_le_bytes());
        d
    }

    /// A File Entry (tag 261) with the given ICB file type, information
    /// length, and allocation area (`ad_form` = icb flags low bits).
    fn file_entry(file_type: u8, info_len: u64, ad_form: u16, ads: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 160]; // payload: icb_tag @0 (20 bytes), fields per ECMA 4/14.9
        p[4..6].copy_from_slice(&4u16.to_le_bytes()); // strategy 4
        p[11] = file_type;
        p[18..20].copy_from_slice(&ad_form.to_le_bytes());
        p[40..48].copy_from_slice(&info_len.to_le_bytes()); // information_length @ FE 56
        p[152..156].copy_from_slice(&0u32.to_le_bytes()); // L_EA @ FE 168
        p[156..160].copy_from_slice(&(ads.len() as u32).to_le_bytes()); // L_AD @ FE 172
        p.extend_from_slice(ads);
        tagged(261, &p)
    }

    fn fid(name: &[u8], icb: [u8; 16], is_dir: bool, parent: bool, comp16: bool) -> Vec<u8> {
        let encoded: Vec<u8> = if name.is_empty() {
            Vec::new()
        } else if comp16 {
            std::iter::once(16u8)
                .chain(name.iter().flat_map(|&b| [0u8, b]))
                .collect()
        } else {
            std::iter::once(8u8).chain(name.iter().copied()).collect()
        };
        let mut p = vec![0u8; 22]; // payload up to the name (FID fields from byte 16)
        p[2] = (u8::from(is_dir) * 0x02) | (u8::from(parent) * 0x08); // characteristics @18
        p[3] = encoded.len() as u8; // L_FI
        p[4..20].copy_from_slice(&icb); // ICB long_ad @20
        p[20..22].copy_from_slice(&0u16.to_le_bytes()); // L_IU
        p.extend_from_slice(&encoded);
        let mut d = tagged(257, &p);
        while d.len() % 4 != 0 {
            d.push(0);
        }
        d
    }

    /// Build the image. Layout: VRS @16, VDS @32 (PD, LVD, TD), AVDP @256,
    /// physical partition @300. Returns the raw image bytes.
    pub(crate) fn build(root: &DirSpec, opts: &Opts) -> Vec<u8> {
        let mut img = Img { data: Vec::new() };

        // Volume recognition sequence.
        for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
            let mut d = vec![0u8; 7];
            d[1..6].copy_from_slice(*id);
            img.write((16 + i as u64) * SECTOR, &d);
        }

        // Two allocators: metadata volumes place FEs/directories/FSD in the
        // metadata partition's lb space (lb N lives at phys lb 1+N; the
        // metadata file's one extent starts at phys lb 1, its FE at phys 0)
        // and file data in the physical space past it; type-1 volumes bump
        // everything from phys lb 0.
        const META_BLOCKS: u32 = 64;
        let mut phys_next: u32 = if opts.metadata_partition { 1 + META_BLOCKS } else { 0 };
        let mut meta_next: u32 = 0;
        let (fe_prn, data_prn) = if opts.metadata_partition { (1u16, 0u16) } else { (0, 0) };

        // Recursively place the tree; returns the directory's ICB long_ad.
        fn place(
            img: &mut Img,
            node: &DirSpec,
            opts: &Opts,
            phys_next: &mut u32,
            meta_next: &mut u32,
            fe_prn: u16,
            data_prn: u16,
        ) -> [u8; 16] {
            let fe_alloc = if opts.metadata_partition { &mut *meta_next } else { &mut *phys_next };
            let self_lb = *fe_alloc;
            *fe_alloc += 1;

            let mut fids: Vec<u8> =
                fid(b"", long_ad(2048, 0, fe_prn), true, true, false); // parent placeholder

            for sub in &node.dirs {
                let icb = place(img, sub, opts, phys_next, meta_next, fe_prn, data_prn);
                fids.extend_from_slice(&fid(sub.name.as_bytes(), icb, true, false, false));
            }
            for f in &node.files {
                // Allocate and write the file data in the physical partition.
                let blocks = (f.data.len() as u32).div_ceil(SECTOR as u32).max(1);
                let mut ads: Vec<u8> = Vec::new();
                let mut written = 0usize;
                let pieces: Vec<u32> = if f.fragment && blocks >= 2 {
                    vec![blocks / 2, blocks - blocks / 2]
                } else {
                    vec![blocks]
                };
                for (i, piece) in pieces.iter().enumerate() {
                    if i > 0 {
                        *phys_next += 1; // the fragmentation hole
                    }
                    let lb = *phys_next;
                    *phys_next += piece;
                    let take =
                        (f.data.len() - written).min((*piece as usize) * SECTOR as usize);
                    img.write(
                        (PART_START + u64::from(lb)) * SECTOR,
                        &f.data[written..written + take],
                    );
                    written += take;
                    let byte_len = if i + 1 == pieces.len() {
                        (f.data.len() - (written - take)) as u32
                    } else {
                        piece * SECTOR as u32
                    };
                    if opts.metadata_partition {
                        ads.extend_from_slice(&long_ad(byte_len, lb, data_prn));
                    } else {
                        ads.extend_from_slice(&short_ad(byte_len, lb));
                    }
                }
                let ad_form = u16::from(opts.metadata_partition); // 1 long, 0 short
                let fe = file_entry(5, f.data.len() as u64, ad_form, &ads);
                let fe_alloc =
                    if opts.metadata_partition { &mut *meta_next } else { &mut *phys_next };
                let fe_lb = *fe_alloc;
                *fe_alloc += 1;
                let fe_pos = if opts.metadata_partition {
                    (PART_START + 1 + u64::from(fe_lb)) * SECTOR
                } else {
                    (PART_START + u64::from(fe_lb)) * SECTOR
                };
                img.write(fe_pos, &fe);
                // Exercise both name encodings: 16-bit for names with a '+'.
                let comp16 = f.name.contains('+');
                fids.extend_from_slice(&fid(
                    f.name.as_bytes(),
                    long_ad(2048, fe_lb, fe_prn),
                    false,
                    false,
                    comp16,
                ));
            }

            // Directory data: inline for metadata volumes, extent for type-1.
            let self_pos = if opts.metadata_partition {
                (PART_START + 1 + u64::from(self_lb)) * SECTOR
            } else {
                (PART_START + u64::from(self_lb)) * SECTOR
            };
            let fe = if opts.metadata_partition {
                file_entry(4, fids.len() as u64, 3, &fids) // inline
            } else {
                let blocks = (fids.len() as u32).div_ceil(SECTOR as u32).max(1);
                let lb = *phys_next;
                *phys_next += blocks;
                img.write((PART_START + u64::from(lb)) * SECTOR, &fids);
                file_entry(4, fids.len() as u64, 0, &short_ad(fids.len() as u32, lb))
            };
            img.write(self_pos, &fe);
            long_ad(2048, self_lb, fe_prn)
        }

        let root_icb =
            place(&mut img, root, opts, &mut phys_next, &mut meta_next, fe_prn, data_prn);

        // File Set Descriptor at (fe partition) lb = next free FE block.
        let fsd_lb = if opts.metadata_partition { meta_next } else { phys_next };
        let mut fsd_payload = vec![0u8; 464];
        fsd_payload[384..400].copy_from_slice(&root_icb); // root ICB @ FSD 400
        let fsd = tagged(256, &fsd_payload);
        let fsd_pos = if opts.metadata_partition {
            (PART_START + 1 + u64::from(fsd_lb)) * SECTOR
        } else {
            (PART_START + u64::from(fsd_lb)) * SECTOR
        };
        img.write(fsd_pos, &fsd);

        // Metadata volumes: the metadata file's FE at phys lb 0, one extent
        // covering phys lbs 1..1+META_BLOCKS (all FE/dir/FSD blocks above).
        if opts.metadata_partition {
            let ads = short_ad(META_BLOCKS * SECTOR as u32, 1);
            let fe = file_entry(250, u64::from(META_BLOCKS) * SECTOR, 0, &ads);
            img.write(PART_START * SECTOR, &fe);
        }

        // VDS: PD + LVD + TD.
        let mut pd = vec![0u8; 484];
        pd[6..8].copy_from_slice(&0u16.to_le_bytes()); // partition_number @22
        pd[172..176].copy_from_slice(&(PART_START as u32).to_le_bytes()); // start @188
        pd[176..180].copy_from_slice(&4096u32.to_le_bytes()); // length @192
        img.write(u64::from(VDS_LOC) * SECTOR, &tagged(5, &pd));

        let mut maps: Vec<u8> = vec![1, 6, 0, 0, 0, 0]; // type 1, vol seq 0, partition 0
        if opts.metadata_partition {
            let mut m = vec![0u8; 64];
            m[0] = 2;
            m[1] = 64;
            m[5..28].copy_from_slice(b"*UDF Metadata Partition");
            m[38..40].copy_from_slice(&0u16.to_le_bytes()); // partition number
            m[40..44].copy_from_slice(&0u32.to_le_bytes()); // metadata file @ phys lb 0
            m[44..48].copy_from_slice(&0u32.to_le_bytes()); // mirror (same)
            maps.extend_from_slice(&m);
        }
        let mut lvd = vec![0u8; 424 + maps.len()];
        lvd[196..200].copy_from_slice(&(SECTOR as u32).to_le_bytes()); // block size @212
        let fsd_ad = long_ad(2048, fsd_lb, fe_prn);
        lvd[232..248].copy_from_slice(&fsd_ad); // contents use @248
        lvd[248..252].copy_from_slice(&(maps.len() as u32).to_le_bytes()); // map table len @264
        lvd[252..256]
            .copy_from_slice(&(1 + u32::from(opts.metadata_partition)).to_le_bytes()); // n maps @268
        lvd[424..].copy_from_slice(&maps); // maps @440
        img.write((u64::from(VDS_LOC) + 1) * SECTOR, &tagged(6, &lvd));
        img.write((u64::from(VDS_LOC) + 2) * SECTOR, &tagged(8, &[0u8; 496]));

        // AVDP at 256 → VDS.
        let mut avdp = vec![0u8; 16];
        avdp[0..4].copy_from_slice(&(16u32 * SECTOR as u32).to_le_bytes()); // VDS length
        avdp[4..8].copy_from_slice(&VDS_LOC.to_le_bytes());
        img.write(256 * SECTOR, &tagged(2, &avdp));

        // Round the image up to whole sectors.
        let sectors = (img.data.len() as u64).div_ceil(SECTOR);
        img.data.resize((sectors * SECTOR) as usize, 0);
        img.data
    }
}

#[cfg(test)]
mod tests {
    use super::testimg::{DirSpec, Opts};
    use super::*;

    fn sample_tree() -> DirSpec {
        DirSpec::named("")
            .dir(
                DirSpec::named("BDMV")
                    .dir(DirSpec::named("PLAYLIST").file("00000.mpls", vec![0xAA; 300]))
                    .dir(
                        DirSpec::named("STREAM")
                            .file("00001.m2ts", vec![0x47; 6000])
                            .file("small+.m2ts", vec![0x11; 100]),
                    ),
            )
            .file("readme.txt", vec![0x55; 10])
    }

    fn build_variant(metadata: bool) -> Vec<u8> {
        testimg::build(&sample_tree(), &Opts { metadata_partition: metadata })
    }

    fn walk_names(vol: &UdfVolume, dir: &Entry) -> Vec<String> {
        vol.read_dir(dir).unwrap().iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn walks_type1_volume() {
        let img = build_variant(false);
        let vol = UdfVolume::open(&img).unwrap();
        let root = vol.root().unwrap();
        assert_eq!(walk_names(&vol, &root), vec!["BDMV", "readme.txt"]);
        let bdmv = vol.read_dir(&root).unwrap().into_iter().find(|e| e.name == "BDMV").unwrap();
        assert!(bdmv.is_dir);
        let stream = vol
            .read_dir(&bdmv)
            .unwrap()
            .into_iter()
            .find(|e| e.name == "STREAM")
            .unwrap();
        let clips = vol.read_dir(&stream).unwrap();
        assert_eq!(clips.len(), 2);
        let clip = &clips[0];
        assert_eq!(clip.name, "00001.m2ts");
        assert_eq!(vol.info_len(clip).unwrap(), 6000);
        let extents = vol.extents(clip).unwrap();
        assert_eq!(extents.iter().map(|(_, l)| l).sum::<u64>(), 6000);
        // The data really lives there.
        let (off, _) = extents[0];
        assert_eq!(img[off as usize], 0x47);
    }

    #[test]
    fn walks_metadata_partition_volume() {
        let img = build_variant(true);
        let vol = UdfVolume::open(&img).unwrap();
        assert!(!vol.metadata_extents().is_empty());
        let root = vol.root().unwrap();
        assert_eq!(walk_names(&vol, &root), vec!["BDMV", "readme.txt"]);
        let bdmv = vol.read_dir(&root).unwrap().remove(0);
        let names = walk_names(&vol, &bdmv);
        assert_eq!(names, vec!["PLAYLIST", "STREAM"]);
        let playlist =
            vol.read_dir(&bdmv).unwrap().into_iter().find(|e| e.name == "PLAYLIST").unwrap();
        let mpls = vol.read_dir(&playlist).unwrap().remove(0);
        assert_eq!(vol.read_small(&mpls, 1 << 20).unwrap(), vec![0xAA; 300]);
    }

    #[test]
    fn decodes_comp16_names() {
        let img = build_variant(false);
        let vol = UdfVolume::open(&img).unwrap();
        let root = vol.root().unwrap();
        let bdmv = vol.read_dir(&root).unwrap().remove(0);
        let stream =
            vol.read_dir(&bdmv).unwrap().into_iter().find(|e| e.name == "STREAM").unwrap();
        let names = walk_names(&vol, &stream);
        assert!(names.contains(&"small+.m2ts".to_string()), "{names:?}");
    }

    #[test]
    fn fragmented_file_reports_two_extents() {
        let mut tree = sample_tree();
        tree.dirs[0].dirs[1].files[0].fragment = true;
        let img = testimg::build(&tree, &Opts { metadata_partition: false });
        let vol = UdfVolume::open(&img).unwrap();
        let root = vol.root().unwrap();
        let bdmv = vol.read_dir(&root).unwrap().remove(0);
        let stream =
            vol.read_dir(&bdmv).unwrap().into_iter().find(|e| e.name == "STREAM").unwrap();
        let clip = vol.read_dir(&stream).unwrap().remove(0);
        let extents = vol.extents(&clip).unwrap();
        assert_eq!(extents.len(), 2);
        // Non-adjacent by construction (one-block hole).
        assert_ne!(extents[0].0 + extents[0].1.div_ceil(SECTOR) * SECTOR, extents[1].0);
        // Gathering still returns the full content.
        assert_eq!(vol.read_small(&clip, 1 << 20).unwrap().len(), 6000);
    }

    #[test]
    fn rejects_non_udf() {
        assert!(!is_udf_iso(&[]));
        assert!(!is_udf_iso(&vec![0u8; 1 << 20]));
        assert!(UdfVolume::open(&vec![0u8; 1 << 20]).is_err());
    }

    #[test]
    fn truncation_never_panics() {
        let img = build_variant(true);
        let mut cut = 0usize;
        while cut < img.len() {
            let sub = &img[..cut];
            if let Ok(vol) = UdfVolume::open(sub) {
                if let Ok(root) = vol.root() {
                    if let Ok(entries) = vol.read_dir(&root) {
                        for e in entries {
                            let _ = vol.read_dir(&e);
                            let _ = vol.extents(&e);
                            let _ = vol.info_len(&e);
                        }
                    }
                }
            }
            cut += 4096;
        }
    }

    #[test]
    fn byte_corruption_never_panics() {
        // Flip a byte at a spread of offsets; every walk must stay Err-or-ok.
        let img = build_variant(false);
        let mut work = img.clone();
        let mut pos = 0usize;
        while pos < work.len() {
            work[pos] ^= 0xFF;
            if let Ok(vol) = UdfVolume::open(&work) {
                if let Ok(root) = vol.root() {
                    if let Ok(entries) = vol.read_dir(&root) {
                        for e in entries {
                            let _ = vol.read_dir(&e);
                            let _ = vol.extents(&e);
                        }
                    }
                }
            }
            work[pos] ^= 0xFF;
            pos += 971; // prime stride for coverage without 500k iterations
        }
    }
}
