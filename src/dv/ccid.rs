//! Dolby Vision base-layer cross-compatibility ID (CCID) spec tables.
//!
//! One authoritative transcription of the two tables every DV colour/profile
//! inference in this crate used to hand-roll separately:
//!
//! - **Table A**, profile -> CCID ([`profile_ccid`]): which cross-compatibility
//!   IDs a bitstream profile admits, and which profiles fix it definitionally.
//! - **Table B**, CCID -> VUI ([`vui_rows`]): the five-part VUI tuple
//!   (range, primaries, transfer, matrix, chroma siting) each CCID's base layer
//!   carries, as *CICP code points* — never display labels. Names are resolved
//!   through the same `container::cicp_*` decoders the signalled path uses, so
//!   a derived label and a signalled label can never drift apart.
//!
//! Sources, both consulted (they diverge; see the per-item notes):
//!
//! - **v1.5**, 12 December 2024 — Table 1 p9, Table 2 p11, Notes to profiles
//!   p12-14, Table 6 (Annex I) p23.
//! - **v1.4**, 2 October 2023 — Table 1 p9.
//! - **v1.3.2**, 16 September 2019 — Table 1 p8, CCID-to-VUI table p9-10,
//!   Notes to profiles p10-11, Table 6 (Annex I) p20.
//!
//! The admitted-CCID sets are the **union across revisions**, because real
//! content was authored under each: v1.5 narrowed profile 8 from {1,2,4} to
//! {1,4} and profile 10 from {0,1,2,4} to {0,1,4}, yet 8.2 SDR-compatible
//! muxes are everywhere. Narrowing to the newest revision would make hdrprobe
//! refuse to name streams that are legal under the revision they were authored
//! against.
//!
//! **Doc erratum, deliberately not followed.** v1.3.2's "Notes to profiles"
//! (p10, p11) gives the profile 4 EL and profile 5 BL VUI as `2,2,2,1,0`,
//! transposed against its own table and its own prose (`1,2,2,2,0`, decoded on
//! p9 as "full-range, unspecified, unspecified, unspecified, and center-left
//! siting"). v1.5 fixed the note. The tables are authoritative here.
//!
//! **Enhancement-layer rows are not in [`vui_rows`]** (which describes base
//! layers only), but they exist and are pinned by the tests below: v1.3.2 gives
//! the profile 4 EL as `1,2,2,2,0` — byte-identical to a profile 5 base layer,
//! which is why a P4 enhancement layer read in isolation is indistinguishable
//! from P5 — and both revisions give the profile 7 EL as `0,9,16,9,2`, the same
//! as its base layer.

use crate::container::{cicp_matrix, cicp_primaries, cicp_transfer};
use crate::model::ColorInfo;

/// H.265 Annex E "unspecified" code point, shared by primaries, transfer and
/// matrix. Also stands in for the spec's `NA` in the profile 10 CCID-0 row:
/// AV1 has no distinct unspecified signalling, so the row means "these fields
/// carry nothing meaningful" either way (v1.5 Table 2 footnote [d]).
const UNSPEC: u16 = 2;

/// `video_full_range_flag` values, in the position Table 2 prints them.
const LIMITED: u8 = 0;
const FULL: u8 = 1;

/// The cross-compatibility ID(s) a Dolby Vision bitstream profile admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileCcid {
    /// The profile defines exactly one CCID, so a stream carrying no
    /// declaration still has a known one.
    Fixed(u8),
    /// The profile admits several; only a container declaration or the base
    /// layer's own signalling can pick between them.
    Variable(&'static [u8]),
}

/// Table A: the CCID(s) a profile admits, `None` for profile IDs the spec
/// reserves (11-19, 21-99) or does not define.
///
/// Fixed entries come from the single-valued CCID column of Table 1 (current
/// profiles) and Table 6 (Annex I, "not supported for new applications"):
/// 0 -> 2, 1 -> 0, 2 -> 2, 3 -> 0, 4 -> 2, 5 -> 0, 6 -> 1, 7 -> 6, 9 -> 2.
/// Variable entries are the cross-revision union described in the module docs.
pub fn profile_ccid(profile: u8) -> Option<ProfileCcid> {
    use ProfileCcid::{Fixed, Variable};
    Some(match profile {
        // Annex I legacy profiles (v1.3.2 Table 6 p20, v1.5 Table 6 p23).
        0 => Fixed(2),  // dvav.per, AVC 1:1/4
        1 => Fixed(0),  // dvav.pen, AVC 1:1
        2 => Fixed(2),  // dvhe.der, 8-bit HEVC 1:1/4
        3 => Fixed(0),  // dvhe.den, 8-bit HEVC 1:1
        // Current in v1.3.2/v1.4 Table 1, moved to v1.5's Annex I; CCID 2 in
        // all three.
        4 => Fixed(2),  // dvhe.04, 10-bit HEVC 1:1/4
        5 => Fixed(0),  // dvhe.05, 10-bit HEVC single-layer
        6 => Fixed(1),  // dvhe.dth, 10-bit HEVC 1:1/4
        7 => Fixed(6),  // dvhe.07, Blu-ray dual layer
        // v1.3.2/v1.4 Table 1: "1, 2, or 4"; v1.5 narrowed to "1 or 4". 8.2 is
        // widely deployed, so keep the union. (8.3 and 8.5 are separately
        // withdrawn — see `deprecated_combination`.)
        8 => Variable(&[1, 2, 4]),
        9 => Fixed(2),  // dvav.09, 8-bit AVC single-layer
        // v1.4 Table 1: "0, 1, 2, or 4"; v1.5 narrowed to "0, 1 or 4".
        10 => Variable(&[0, 1, 2, 4]),
        // v1.4 Table 1: "0"; v1.5 widened to "0 or 4".
        20 => Variable(&[0, 4]),
        _ => return None,
    })
}

/// The CCID a profile's definition fixes, `None` when the profile admits
/// several (or is undefined). This is the *spec* rung of CCID resolution: it
/// needs no stream evidence at all, only the profile ID, which is why a raw
/// Profile 5 elementary stream with no dvcC still has a real compatibility id
/// rather than a convention default.
pub fn spec_ccid(profile: u8) -> Option<u8> {
    match profile_ccid(profile)? {
        ProfileCcid::Fixed(id) => Some(id),
        ProfileCcid::Variable(_) => None,
    }
}

/// The human name for a CCID, from Table 2's "Type of cross-compatibility"
/// column plus the prose definition of each id (v1.5 p10-11). CCIDs 3, 5, 7 and
/// 15 are reserved and 6 is "Ultra HD Blu-ray Disc HDR (per Blu-ray Disc
/// Association standard)" — the same CTA-861.3 HDR10 base as id 1 with disc
/// constraints on top, which is why the L6/MaxCLL gating treats them as one
/// HDR10 family.
pub fn compatibility_label(ccid: u8) -> Option<&'static str> {
    Some(match ccid {
        0 => "no cross-compatibility",
        1 => "HDR10-compatible",
        2 => "SDR-compatible",
        4 => "HLG-compatible",
        6 => "Ultra HD Blu-ray-compatible",
        _ => return None,
    })
}

/// Whether this profile/CCID pairing is one Dolby has withdrawn — the rows of
/// Annex I ("Profiles not supported for new applications") that name a
/// *combination* rather than a whole profile. Profile 8 is current; only two of
/// its pairings are not.
///
/// - **8.3** — v1.3.2 Table 6 p20. Dropped from v1.5's Annex I because CCID 3
///   became "Reserved for Dolby Vision proprietary, non-SDR and non-HDR base
///   layer", so the pairing cannot be legal under either revision.
/// - **8.5** — v1.3.2 Table 6 p20 and v1.5 Table 6 p23, withdrawn in both.
///
/// Whole profiles Annex I lists (0, 1, 2, 3, 4, 6) are deliberately *not* here:
/// hdrprobe reports plenty of Profile 4 and 7 content and "legacy" is not a
/// defect. This flags only the pairings, which no encoder should now produce.
pub fn deprecated_combination(profile: u8, ccid: u8) -> bool {
    matches!((profile, ccid), (8, 3) | (8, 5))
}

/// The base signal a decoder without a Dolby Vision decoder actually receives,
/// named in hdrprobe's own `hdr.format` vocabulary. Table 2's "Type of
/// cross-compatibility" column, rendered as a format tag rather than as the
/// prose name [`compatibility_label`] gives the same column. `None` for CCID 0,
/// whose base is Dolby's proprietary IPT-PQ-C2 and viewable by nothing, and for
/// every reserved or unknown id.
pub fn base_signal(ccid: u8) -> Option<&'static str> {
    Some(match ccid {
        1 | 6 => "HDR10",
        2 => "SDR",
        4 => "HLG",
        // 0 has no viewable base; 3, 5, 7 and 15 are reserved.
        _ => return None,
    })
}

/// Whether the base layer's actual transfer characteristic is Dolby's
/// proprietary "PQ with reshaping" rather than the plain PQ its VUI names.
///
/// v1.5 Table 2 footnote [b]: although a transfer characteristic of 16
/// "generally indicates perceptual quantization (PQ)", "in the context of Dolby
/// Vision CCID=0 when color_matrix is 15 ... the actual proprietary transfer
/// characteristic, even when signaled with 16, is 'PQ with reshaping'".
/// Repeated for profile 20 (v1.5 p13) and for profile 5 (v1.3.2 p11).
pub fn pq_with_reshaping(ccid: u8) -> bool {
    ccid == 0
}

/// Whether a Dolby Vision title's base layer is the CTA-861.3 HDR10 signal that
/// MaxCLL/MaxFALL and an ST.2086 mastering display actually describe: CCID 1, or
/// 6 (Ultra HD Blu-ray, the same CTA-861.3 base with disc constraints on top).
/// No other base has a consumer for that static metadata — CCID 0 has no
/// viewable base at all, HLG is scene-referred, and an SDR base signals none.
///
/// `ccid` is the resolved id. `None` means nothing resolved it, which after the
/// declared, spec and inferred rungs is reachable only for a Profile 8 whose
/// carriage declared no id and whose base layer does not separate CCID 1, 2 and
/// 4 — genuinely unresolvable, since the profile admits all three. Those keep
/// the historical default, which the `assumed` rung's `8.1` label already
/// states: profiles 7 and 8 assume an HDR10 base, everything else does not.
pub fn hdr10_base(ccid: Option<u8>, profile: Option<u8>) -> bool {
    match ccid {
        Some(id) => id == 1 || id == 6,
        None => matches!(profile, Some(7 | 8)),
    }
}

/// One row of Table B (v1.5 "Table 2: Cross-compatibility ID to VUI mapping",
/// v1.3.2's unnumbered CCID-to-VUI table): the five-part VUI in the spec's own
/// print order — range, colour primaries, transfer characteristic, matrix
/// coefficients, chroma sample location type. Note range comes *first*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vui {
    pub range: u8,
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub chroma_loc: u8,
}

const fn vui(range: u8, primaries: u16, transfer: u16, matrix: u16, chroma_loc: u8) -> Vui {
    Vui { range, primaries, transfer, matrix, chroma_loc }
}

impl Vui {
    /// Whether this row is consistent with what a stream actually signalled.
    ///
    /// A field the stream leaves unsignalled cannot exclude the row — that is
    /// the whole point of a *defined* VUI, and profiles 4/5 routinely signal
    /// nothing but range. A field the stream does signal must match exactly.
    /// Both sides are compared as decoder labels, never as raw codes, so the
    /// comparison shares one value space with the signalled report path.
    ///
    /// **Range is deliberately not compared.** The table prints one range per
    /// row, but real muxes set `video_full_range_flag` independently of the
    /// colour description — corpus evidence: `dv_84.mov` and `dv_84_alt.mov`
    /// are both declared 8.4 (CCID 4, whose row is limited) yet signal full and
    /// limited respectively. A field that unreliable must not veto a match the
    /// three colour-description fields agree on.
    fn consistent_with(&self, cc: &ColorInfo) -> bool {
        let agree = |signalled: Option<&str>, defined: Option<&str>| match signalled {
            None => true,
            Some(s) => defined == Some(s),
        };
        agree(cc.primaries.as_deref(), cicp_primaries(self.primaries))
            && agree(cc.transfer.as_deref(), cicp_transfer(self.transfer))
            && agree(cc.matrix.as_deref(), cicp_matrix(self.matrix))
    }
}

// ---------------------------------------------------------------------------
// Table B rows, base layers. Every row below is one cell of the spec's
// CCID-to-VUI table; nothing here is interpolated.
// ---------------------------------------------------------------------------

/// CCID 0, profile 5. v1.5 lists two options: "Preferred" (as of v1.4) and
/// "Original". The preferred form's BT.2020 primaries and matrix code point 15
/// are a post-2023 clarification (SMPTE ST 2128:2023 standardized IPT-PQ-C2 as
/// code point 15); older content signalled the original all-unspecified form,
/// which decomposes to the same colour because ST 2128 defines IPT-PQ-C2
/// relative to BT.2020 primaries.
const CCID0_P5: [Vui; 2] = [vui(FULL, 9, 16, 15, 0), vui(FULL, UNSPEC, UNSPEC, UNSPEC, 0)];
/// CCID 0, profile 10 (AV1). Second row is the spec's `1,NA,NA,NA,1`.
const CCID0_P10: [Vui; 2] = [vui(FULL, 9, 16, 15, 1), vui(FULL, UNSPEC, UNSPEC, UNSPEC, 1)];
/// CCID 0, profile 20 (MV-HEVC). One row, top-left siting.
const CCID0_P20: [Vui; 1] = [vui(FULL, 9, 16, 15, 2)];
/// CCID 0, v1.3.2's profile-generic row, for the legacy CCID-0 profiles 1 and 3.
const CCID0_LEGACY: [Vui; 1] = [vui(FULL, UNSPEC, UNSPEC, UNSPEC, 0)];
/// CCID 1 (CTA HDR10), profile 8 — and v1.3.2's profile-generic row, so it also
/// covers the legacy profile 6.
const CCID1_P8: [Vui; 1] = [vui(LIMITED, 9, 16, 9, 0)];
/// CCID 1, profile 10: same tuple, AV1's CSP_VERTICAL siting.
const CCID1_P10: [Vui; 1] = [vui(LIMITED, 9, 16, 9, 1)];
/// CCID 2 (SDR). v1.5 states it for profile 9; v1.3.2 states it generically, so
/// it is equally the base layer of legacy profiles 0, 2 and 4.
const CCID2_BL: [Vui; 1] = [vui(LIMITED, 1, 1, 1, 0)];
/// CCID 4 (HLG), ARIB form — profiles 8, 10 and 20.
const CCID4_ARIB: Vui = vui(LIMITED, 9, 18, 9, 2);
/// CCID 4, profile 8: the ARIB form plus the DVB BT.2020 form, whose transfer
/// 14 pairs with an `alternative_transfer_characteristic` SEI carrying
/// `preferred_transfer_function = 18` at every RAP (ETSI TS 101 154 v2.5.1).
const CCID4_P8: [Vui; 2] = [CCID4_ARIB, vui(LIMITED, 9, 14, 9, 0)];
/// CCID 4 elsewhere (profiles 10 and 20): ARIB only, no DVB row.
const CCID4_ARIB_ONLY: [Vui; 1] = [CCID4_ARIB];
/// CCID 6 (Ultra HD Blu-ray), profile 7 base layer.
const CCID6_P7: [Vui; 1] = [vui(LIMITED, 9, 16, 9, 2)];

/// Whether the profile/CCID pairing is one the tables define at all.
fn admits(profile: u8, ccid: u8) -> bool {
    match profile_ccid(profile) {
        Some(ProfileCcid::Fixed(id)) => id == ccid,
        Some(ProfileCcid::Variable(ids)) => ids.contains(&ccid),
        None => false,
    }
}

/// The Table B rows for a profile's base layer under a given CCID. Empty when
/// the spec pairs no VUI with that combination — including every pairing
/// Table A does not admit in the first place, which is why the catch-all arms
/// below can stay profile-generic (v1.3.2 states most CCID rows without naming
/// a profile, so the arms mirror the source).
pub fn vui_rows(profile: u8, ccid: u8) -> &'static [Vui] {
    if !admits(profile, ccid) {
        return &[];
    }
    match (profile, ccid) {
        (5, 0) => &CCID0_P5,
        (10, 0) => &CCID0_P10,
        (20, 0) => &CCID0_P20,
        (_, 0) => &CCID0_LEGACY,
        (10, 1) => &CCID1_P10,
        (_, 1) => &CCID1_P8,
        (_, 2) => &CCID2_BL,
        (8, 4) => &CCID4_P8,
        (_, 4) => &CCID4_ARIB_ONLY,
        (_, 6) => &CCID6_P7,
        _ => &[],
    }
}

/// The VUI a profile's base layer carries under a given CCID, for filling in
/// what a stream did not signal. The first of [`vui_rows`], which makes the
/// choice **deliberately lossy in one place**: CCID 4 has two rows for profile
/// 8, and this returns the ARIB one (transfer 18). The DVB variant (transfer 14)
/// exists only as an explicitly signalled 14 paired with an alt-transfer SEI, so
/// it can never be the right answer to "what did this stream leave out" — if 14
/// is signalled there is nothing to fill. Both rows stay in the table because
/// the reverse lookup needs them.
pub fn defined_vui(profile: u8, ccid: u8) -> Option<Vui> {
    vui_rows(profile, ccid).first().copied()
}

/// Deduce the CCID from the base layer's *signalled* VUI, for a profile whose
/// definition does not fix one ([`spec_ccid`] resolves those without reading a
/// stream at all). Returns a value only when exactly one admitted CCID has a
/// Table B row consistent with the signal — a stream that signals too little to
/// separate two candidates yields `None`, never a pick.
///
/// This generalizes what used to be a Profile 10-only reverse lookup. The
/// generalization is sound because each profile's candidate rows are separated
/// by fields real streams do carry: profile 8's four rows differ in transfer
/// alone (16 -> 1, 1 -> 2, 18 -> 4, 14 -> 4, the DVB variant), and profile 10's
/// CCID-0 rows are full-range where every other candidate is limited.
pub fn infer_ccid(profile: u8, cc: &ColorInfo) -> Option<u8> {
    let ProfileCcid::Variable(candidates) = profile_ccid(profile)? else { return None };
    let mut found = None;
    for &ccid in candidates {
        if vui_rows(profile, ccid).iter().any(|row| row.consistent_with(cc)) {
            if found.is_some() {
                return None;
            }
            found = Some(ccid);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::cicp_range;

    fn color(primaries: Option<&str>, transfer: Option<&str>, matrix: Option<&str>, range: Option<&str>) -> ColorInfo {
        ColorInfo {
            primaries: primaries.map(str::to_string),
            transfer: transfer.map(str::to_string),
            matrix: matrix.map(str::to_string),
            range: range.map(str::to_string),
        }
    }

    /// v1.5 Table 1 (p9) and Table 6 (Annex I, p23); v1.3.2 Table 1 (p8) and
    /// Table 6 (p20); v1.4 Table 1 (p9). Every profile ID either table names,
    /// with the cross-revision union where the revisions disagree.
    #[test]
    fn table_1_and_table_6_profile_to_ccid_rows_match_the_spec() {
        use ProfileCcid::{Fixed, Variable};
        let expected: &[(u8, ProfileCcid)] = &[
            (0, Fixed(2)),
            (1, Fixed(0)),
            (2, Fixed(2)),
            (3, Fixed(0)),
            (4, Fixed(2)),
            (5, Fixed(0)),
            (6, Fixed(1)),
            (7, Fixed(6)),
            (8, Variable(&[1, 2, 4])),
            (9, Fixed(2)),
            (10, Variable(&[0, 1, 2, 4])),
            (20, Variable(&[0, 4])),
        ];
        for &(profile, ref want) in expected {
            assert_eq!(profile_ccid(profile).as_ref(), Some(want), "profile {profile}");
        }
        // Reserved profile IDs carry no CCID at all.
        for profile in [11, 12, 19, 21, 50, 99] {
            assert_eq!(profile_ccid(profile), None, "profile {profile} is reserved");
        }
        // The spec rung answers only for the definitionally fixed profiles.
        assert_eq!(spec_ccid(4), Some(2));
        assert_eq!(spec_ccid(5), Some(0));
        assert_eq!(spec_ccid(7), Some(6));
        assert_eq!(spec_ccid(9), Some(2));
        assert_eq!(spec_ccid(8), None);
        assert_eq!(spec_ccid(10), None);
        assert_eq!(spec_ccid(20), None);
        assert_eq!(spec_ccid(11), None);
    }

    /// Annex I's withdrawn profile/CCID *combinations*, and only those: the
    /// legacy whole profiles it also lists stay reportable.
    #[test]
    fn deprecated_combinations_are_the_two_annex_i_pairings() {
        assert!(deprecated_combination(8, 3), "v1.3.2 Table 6 p20");
        assert!(deprecated_combination(8, 5), "v1.3.2 Table 6 p20, v1.5 Table 6 p23");
        for ccid in [1u8, 2, 4] {
            assert!(!deprecated_combination(8, ccid), "8.{ccid} is current");
        }
        // Annex I's legacy profiles are not flagged: the corpus reports them.
        for (profile, ccid) in [(4u8, 2u8), (7, 6), (0, 2), (1, 0), (2, 2), (3, 0), (6, 1)] {
            assert!(!deprecated_combination(profile, ccid), "profile {profile} is legacy, not withdrawn");
        }
    }

    /// Table 2's cross-compatibility labels, including CCID 6 — every Profile 7
    /// title carries it, and it used to report as an unnamed id.
    #[test]
    fn compatibility_labels_cover_every_defined_ccid() {
        assert_eq!(compatibility_label(0), Some("no cross-compatibility"));
        assert_eq!(compatibility_label(1), Some("HDR10-compatible"));
        assert_eq!(compatibility_label(2), Some("SDR-compatible"));
        assert_eq!(compatibility_label(4), Some("HLG-compatible"));
        assert_eq!(compatibility_label(6), Some("Ultra HD Blu-ray-compatible"));
        // Reserved ids are named by nothing, never guessed.
        for reserved in [3, 5, 7, 15] {
            assert_eq!(compatibility_label(reserved), None, "CCID {reserved} is reserved");
        }
    }

    /// The same Table 2 column rendered as an `hdr.format` tag. Kept beside
    /// `compatibility_label` so the two renderings of one column cannot drift.
    #[test]
    fn base_signal_tags_match_the_cross_compatibility_column() {
        assert_eq!(base_signal(0), None, "CCID 0's base is viewable by nothing");
        assert_eq!(base_signal(1), Some("HDR10"));
        assert_eq!(base_signal(2), Some("SDR"));
        assert_eq!(base_signal(4), Some("HLG"));
        assert_eq!(base_signal(6), Some("HDR10"), "Ultra HD Blu-ray is the same CTA-861.3 base");
        for reserved in [3, 5, 7, 15] {
            assert_eq!(base_signal(reserved), None, "CCID {reserved} is reserved");
        }
        // The HDR10 gate and the tag agree by construction.
        for ccid in 0u8..=15 {
            assert_eq!(
                hdr10_base(Some(ccid), None),
                base_signal(ccid) == Some("HDR10"),
                "CCID {ccid}"
            );
        }
        // Reshaping is CCID 0 and nothing else.
        for ccid in 0u8..=15 {
            assert_eq!(pq_with_reshaping(ccid), ccid == 0, "CCID {ccid}");
        }
    }

    /// v1.5 "Table 2: Cross-compatibility ID to VUI mapping" (p11), cell by
    /// cell, plus v1.3.2's CCID-to-VUI table (p9-10) where it adds a row.
    #[test]
    fn table_2_ccid_to_vui_rows_match_the_spec() {
        // CCID 0 - none, Dolby Vision proprietary 10-bit.
        assert_eq!(vui_rows(5, 0), &[vui(1, 9, 16, 15, 0), vui(1, 2, 2, 2, 0)]);
        assert_eq!(vui_rows(10, 0), &[vui(1, 9, 16, 15, 1), vui(1, 2, 2, 2, 1)]);
        assert_eq!(vui_rows(20, 0), &[vui(1, 9, 16, 15, 2)]);
        // CCID 1 - HDR10.
        assert_eq!(vui_rows(8, 1), &[vui(0, 9, 16, 9, 0)]);
        assert_eq!(vui_rows(10, 1), &[vui(0, 9, 16, 9, 1)]);
        // CCID 2 - SDR.
        assert_eq!(vui_rows(9, 2), &[vui(0, 1, 1, 1, 0)]);
        // CCID 4 - HLG: the ARIB row for every profile, plus profile 8's DVB row.
        assert_eq!(vui_rows(8, 4), &[vui(0, 9, 18, 9, 2), vui(0, 9, 14, 9, 0)]);
        assert_eq!(vui_rows(10, 4), &[vui(0, 9, 18, 9, 2)]);
        assert_eq!(vui_rows(20, 4), &[vui(0, 9, 18, 9, 2)]);
        // CCID 6 - Ultra HD Blu-ray.
        assert_eq!(vui_rows(7, 6), &[vui(0, 9, 16, 9, 2)]);
        // Combinations the spec pairs with no VUI.
        assert!(vui_rows(5, 1).is_empty());
        assert!(vui_rows(9, 15).is_empty());
    }

    /// The enhancement-layer rows, kept checkable even though `vui_rows`
    /// describes base layers only. v1.3.2 p9: profile 4's EL is `1,2,2,2,0` —
    /// identical to a profile 5 base layer. v1.5 Table 2 p11: profile 7's EL is
    /// `0,9,16,9,2`, the same as its base layer.
    #[test]
    fn enhancement_layer_rows_are_recorded() {
        let p4_el = vui(1, 2, 2, 2, 0);
        let p5_bl_original = vui_rows(5, 0)[1];
        assert_eq!(p4_el, p5_bl_original, "a P4 EL is indistinguishable from a P5 base layer");
        let p7_el = vui(0, 9, 16, 9, 2);
        assert_eq!(p7_el, vui_rows(7, 6)[0], "P7's EL VUI matches its BL VUI");
    }

    /// The gate behind every MaxCLL/mastering suppression rule, including the
    /// one case no corpus file reaches: a Profile 8 that declares no id and
    /// whose base layer does not separate CCID 1, 2 and 4. The profile admits
    /// all three, so nothing can resolve it, and the historical default — a
    /// profile 7 or 8 assumes an HDR10 base, nothing else does — has to be kept
    /// explicitly rather than assumed to fall out of the table.
    #[test]
    fn the_hdr10_base_gate_keeps_its_profile_8_fallback() {
        // Resolved ids answer on their own, whatever the profile.
        assert!(hdr10_base(Some(1), Some(8)));
        assert!(hdr10_base(Some(6), Some(7)));
        assert!(!hdr10_base(Some(0), Some(5)));
        assert!(!hdr10_base(Some(2), Some(9)));
        assert!(!hdr10_base(Some(4), Some(8)));
        // A resolved id beats the fallback it disagrees with: an 8.4 is an HLG
        // base even though the fallback would have assumed HDR10 for a bare 8.
        assert!(!hdr10_base(Some(4), Some(8)));
        // Unresolved: the retained heuristic.
        assert!(hdr10_base(None, Some(7)));
        assert!(hdr10_base(None, Some(8)));
        assert!(!hdr10_base(None, Some(4)));
        assert!(!hdr10_base(None, Some(5)));
        assert!(!hdr10_base(None, Some(10)));
        assert!(!hdr10_base(None, Some(20)));
        assert!(!hdr10_base(None, None));
    }

    /// Table rows must decode through the same `cicp_*` decoders the signalled
    /// report path uses, so a derived and a signalled label can never drift.
    #[test]
    fn rows_decode_to_the_shared_report_labels() {
        let p5 = vui_rows(5, 0)[0];
        assert_eq!(cicp_primaries(p5.primaries), Some("BT.2020"));
        assert_eq!(cicp_transfer(p5.transfer), Some("PQ (SMPTE ST 2084)"));
        assert_eq!(cicp_matrix(p5.matrix), Some("IPT-PQ-C2"));
        assert_eq!(cicp_range(p5.range == FULL), "full");
        // The "original" rows carry H.265 "unspecified", which decodes to no
        // label at all — the reason a raw Profile 5 stream reports bare "full".
        let p5_original = vui_rows(5, 0)[1];
        assert_eq!(cicp_primaries(p5_original.primaries), None);
        assert_eq!(cicp_transfer(p5_original.transfer), None);
        assert_eq!(cicp_matrix(p5_original.matrix), None);
        // Profile 9's SDR base layer is Rec.709 throughout.
        let p9 = vui_rows(9, 2)[0];
        assert_eq!(cicp_primaries(p9.primaries), Some("BT.709"));
        assert_eq!(cicp_transfer(p9.transfer), Some("BT.709"));
        assert_eq!(cicp_matrix(p9.matrix), Some("BT.709"));
        assert_eq!(cicp_range(p9.range == FULL), "limited");
    }

    #[test]
    fn infers_profile_10_from_an_explicit_base_layer_signal() {
        let hdr10 = color(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(10, &hdr10), Some(1));
        let hlg = color(Some("BT.2020"), Some("HLG (ARIB STD-B67)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(10, &hlg), Some(4));
        let ipt = color(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("IPT-PQ-C2"), Some("full"));
        assert_eq!(infer_ccid(10, &ipt), Some(0));
        // An SDR gamma transfer excludes every other candidate row.
        let sdr = color(None, Some("BT.709"), None, None);
        assert_eq!(infer_ccid(10, &sdr), Some(2));
    }

    #[test]
    fn infers_profile_8_including_the_dvb_hlg_variant() {
        let hdr10 = color(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(8, &hdr10), Some(1));
        let arib = color(Some("BT.2020"), Some("HLG (ARIB STD-B67)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(8, &arib), Some(4));
        // Transfer 14 is profile 8's DVB HLG row, *not* the SDR gamma family it
        // would read as under a profile-blind rule.
        let dvb = color(Some("BT.2020"), Some("BT.2020 (10-bit)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(8, &dvb), Some(4));
        let sdr = color(Some("BT.709"), Some("BT.709"), Some("BT.709"), Some("limited"));
        assert_eq!(infer_ccid(8, &sdr), Some(2));
        // Profile 8 does not admit CCID 0, so an IPT matrix names nothing.
        let ipt = color(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), Some("IPT-PQ-C2"), Some("full"));
        assert_eq!(infer_ccid(8, &ipt), None);
    }

    #[test]
    fn a_signal_that_separates_nothing_infers_nothing() {
        // Silent stream: every candidate row survives.
        assert_eq!(infer_ccid(10, &ColorInfo::default()), None);
        assert_eq!(infer_ccid(8, &ColorInfo::default()), None);
        // PQ alone cannot exclude a CCID-0 base: IPT-PQ-C2 is itself
        // PQ-encoded, and its signalling convention leaves the rest unspecified.
        let pq_only = color(None, Some("PQ (SMPTE ST 2084)"), None, None);
        assert_eq!(infer_ccid(10, &pq_only), None);
        // Primaries + transfer, still no matrix or range to separate CCID 0.
        let pq_2020 = color(Some("BT.2020"), Some("PQ (SMPTE ST 2084)"), None, None);
        assert_eq!(infer_ccid(10, &pq_2020), None);
        // A fixed-CCID profile is the spec rung's business, not inference.
        let p5_like = color(None, None, None, Some("full"));
        assert_eq!(infer_ccid(5, &p5_like), None);
        assert_eq!(infer_ccid(7, &p5_like), None);
        // Undefined profile IDs infer nothing.
        assert_eq!(infer_ccid(11, &p5_like), None);
    }

    /// The signalled matrix is what separates a CCID-0 base from an HDR10 one,
    /// and it carries that weight on its own: a matrix naming BT.2020 NCL
    /// contradicts both CCID-0 rows (IPT-PQ-C2, or nothing at all) whether or
    /// not the stream also tagged its primaries.
    #[test]
    fn the_matrix_alone_separates_an_ipt_base_from_an_hdr10_one() {
        let pq_no_primaries = color(None, Some("PQ (SMPTE ST 2084)"), Some("BT.2020 NCL"), Some("limited"));
        assert_eq!(infer_ccid(10, &pq_no_primaries), Some(1));
        let ipt_no_primaries = color(None, Some("PQ (SMPTE ST 2084)"), Some("IPT-PQ-C2"), Some("full"));
        assert_eq!(infer_ccid(10, &ipt_no_primaries), Some(0));
    }

    /// Every profile/CCID pairing the tables define, and the ones they do not.
    #[test]
    fn admitted_pairings_follow_the_profile_tables() {
        assert!(admits(5, 0) && !admits(5, 1));
        assert!(admits(8, 1) && admits(8, 2) && admits(8, 4));
        assert!(!admits(8, 0) && !admits(8, 6));
        assert!(admits(7, 6) && !admits(7, 1));
        assert!(admits(20, 0) && admits(20, 4) && !admits(20, 1));
        assert!(!admits(11, 0));
    }
}
