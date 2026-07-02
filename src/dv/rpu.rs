//! Thin wrapper over libdovi (`dolby_vision`) RPU parsing.
//!
//! libdovi — and the sibling `hdr10plus` crate — parse untrusted bytes and can
//! *panic* (not merely return `Err`) on adversarially malformed input; e.g. a
//! corrupt NLQ record aborts inside `rpu_data_mapping.rs`. Because one run may
//! scan thousands of samples across many files (a directory scan must survive a
//! single corrupt file), every call into those crates goes through [`guard`],
//! which isolates the panic with `catch_unwind` and turns it into `None`. This
//! relies on the release profile *not* using `panic = "abort"`.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};

use dolby_vision::rpu::dovi_rpu::DoviRpu;

thread_local! {
    /// Set while inside a [`guard`] call so the process-wide panic hook stays
    /// quiet for the *expected*, already-handled malformed-input panics.
    static SILENCED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is inside a [`guard`] call. The panic hook
/// installed in `main` consults this to suppress noise from isolated,
/// already-handled parser panics while still surfacing genuine bugs.
pub fn panic_silenced() -> bool {
    SILENCED.with(|c| c.get())
}

/// Run an untrusted third-party parser, converting both an `Err` and a panic
/// into `None`. Use for every call into libdovi / `hdr10plus`.
pub fn guard<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    SILENCED.with(|c| c.set(true));
    let r = panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(None);
    SILENCED.with(|c| c.set(false));
    r
}

/// Parse an HEVC UNSPEC62 NAL (input includes the 2-byte NAL header) into a
/// `DoviRpu`. Returns `None` on any parse/validation failure *or* panic.
pub fn parse_hevc_rpu(nal_with_header: &[u8]) -> Option<DoviRpu> {
    guard(|| DoviRpu::parse_unspec62_nalu(nal_with_header).ok())
}

/// Parse an AV1 ITU-T T.35 Dolby metadata OBU payload into a `DoviRpu`. The
/// input starts at `itu_t_t35_country_code` (0xB5); libdovi unwraps the EMDF.
pub fn parse_av1_rpu(obu_payload: &[u8]) -> Option<DoviRpu> {
    guard(|| DoviRpu::parse_itu_t35_dovi_metadata_obu(obu_payload).ok())
}

/// Parse an AVC (H.264) Dolby Vision RPU NAL into a `DoviRpu`. The DV RPU rides
/// in an *unspecified* AVC NAL (1-byte header) whose payload is the RPU EBSP
/// beginning with the `rpu_nal_prefix` byte `0x19`. libdovi has no AVC-specific
/// entry point, but its parsing is codec-agnostic once the NAL header is off:
/// strip the 1-byte header, clear emulation prevention to an RBSP, and hand it
/// to `parse_rpu`, which locates the `0x19` prefix and validates the CRC.
/// Returns `None` on any parse/validation failure *or* panic.
pub fn parse_avc_rpu(nal_with_header: &[u8]) -> Option<DoviRpu> {
    if nal_with_header.len() < 2 {
        return None;
    }
    let rbsp = crate::bits::ebsp_to_rbsp(&nal_with_header[1..]);
    guard(|| DoviRpu::parse_rpu(&rbsp).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_avc_wrapped_rpu() {
        // A real DV RPU payload (from a dovi_tool `.bin`) wrapped in an AVC
        // unspecified NAL: 1-byte header 0x1C (type 28) followed by the RPU EBSP
        // beginning with the 0x19 rpu_nal_prefix. This exercises the full AVC RPU
        // decode path: strip the header, clear emulation prevention, validate CRC.
        let nal = [
            0x1c, 0x19, 0x08, 0x09, 0x08, 0x40, 0x61, 0x36, 0x50, 0x6f, 0x00, 0x3f, 0xf8, 0x01,
            0xff, 0xc0, 0x0f, 0xff, 0xd0, 0x00, 0x00, 0x08, 0x00, 0x00, 0x06, 0x80, 0x00, 0x00,
            0x40, 0x00, 0x00, 0x34, 0x00, 0x00, 0x03, 0x02, 0x00, 0x00, 0x03, 0x01, 0xa2, 0x56,
            0x60, 0x00, 0x03, 0x5e, 0xa2, 0x56, 0x6f, 0x9f, 0xce, 0xb1, 0xc2, 0x56, 0x64, 0x4c,
            0xa0, 0x00, 0x00, 0x10, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80,
            0x00, 0x00, 0x03, 0x01, 0xc3, 0x62, 0x24, 0x30, 0x18, 0x60, 0xa5, 0xe3, 0x08, 0xe0,
            0x51, 0x40, 0x00, 0x00, 0x1a, 0x63, 0xe5, 0xaf, 0xff, 0xf0, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x06, 0x02, 0x00, 0xf8, 0x0e, 0x15, 0x1c, 0x30,
            0x08, 0x00, 0x5e, 0xa3, 0xa5, 0x00, 0xc0, 0x28, 0x21, 0x60, 0x67, 0xe5, 0x52, 0x78,
            0x00, 0x80, 0x04, 0x00, 0x01, 0x80, 0x56, 0x46, 0xfb, 0x10, 0x00, 0xf2, 0x50, 0x01,
            0x00, 0x08, 0x00, 0x03, 0x00, 0xb0, 0x1e, 0x00, 0xa0, 0x01, 0xff, 0x60, 0x02, 0x00,
            0x10, 0x00, 0x04, 0x02, 0x80, 0x00, 0x00, 0x03, 0x01, 0x17, 0x08, 0xc0, 0x09, 0x06,
            0x03, 0xe8, 0x00, 0x01, 0x04, 0x1b, 0x01, 0x13, 0x30, 0x30, 0x1c, 0x00, 0x40, 0x03,
            0x10, 0x80, 0xb0, 0x80, 0x18, 0x00, 0x80, 0x08, 0x00, 0x80, 0x08, 0x00, 0x80, 0x04,
            0x12, 0x00, 0x50, 0xb0, 0x11, 0x00, 0x00, 0x07, 0xfc, 0x00, 0x04, 0x33, 0x9b, 0xed,
            0x35, 0x80,
        ];
        let rpu = parse_avc_rpu(&nal).expect("valid AVC-wrapped RPU");
        // The RPU header carries a Dolby Vision profile; single-layer P9 uses the
        // cross-compatible baseline (rpu profile 1), like profile 8.
        assert!(rpu.dovi_profile <= 8);
    }
}
