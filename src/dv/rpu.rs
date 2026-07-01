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
