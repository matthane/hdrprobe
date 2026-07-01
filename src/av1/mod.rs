//! AV1 bitstream handling: OBU walking for Dolby Vision (Profile 10) and
//! static-HDR / HDR10+ metadata. Demux-only; pictures are never decoded.

pub mod obu;
pub mod seq;
