//! HDR10+ metadata JSON (hdr10plus_tool output). We report the same
//! title-stable fields the SEI path surfaces (profile, application version,
//! window count, target display luminance); the per-scene dynamic values are the
//! HDR10+ analogue of DV L1 and are deliberately not shown.
//!
//! These files are by far the largest sidecar — one metadata object *per frame*,
//! routinely hundreds of MB — yet everything we surface lives in just two objects
//! at the head of the file: the file-level `JSONInfo` and the first `SceneInfo`
//! entry. So instead of the crate's `MetadataJsonRoot::parse` (which deserialises
//! the whole per-frame array, and its `Vec` histograms, only for us to drop all
//! but the first), we're handed a bounded head window and lift just those two
//! objects out of it by a small brace-matching scan, then hand each to the
//! crate's own converters so the reported fields stay identical to the SEI path.

use anyhow::{bail, Result};
use hdr10plus::metadata::Hdr10PlusMetadata;
use hdr10plus::metadata_json::{Hdr10PlusJsonMetadata, JsonInfo};

use crate::dv::rpu::guard;
use crate::model::Hdr10Plus;

use super::Payload;

/// How much of the file to read. `JSONInfo` and `SceneInfo`'s first element sort
/// to the front of the document (serde_json emits object keys alphabetically, and
/// `J`/`S` precede the rest), and each is a small fixed-shape object, so this
/// window captures both with a huge margin even when pretty-printed — while
/// staying bounded no matter how large the file grows.
pub const HEAD_BYTES: usize = 256 * 1024;

pub fn parse(head: &[u8]) -> Result<Payload> {
    let text = String::from_utf8_lossy(head);

    // The first scene object is mandatory; JSONInfo (profile) is best-effort.
    let Some(scene_obj) = first_object_after(&text, "\"SceneInfo\"") else {
        bail!("HDR10+ JSON: no scene metadata in head window");
    };

    // Route the crate calls through the panic guard like every third-party parse.
    let Some((meta, raw_profile)) = guard(|| {
        let scene: Hdr10PlusJsonMetadata = serde_json::from_str(scene_obj).ok()?;
        let meta = Hdr10PlusMetadata::try_from(&scene).ok()?;
        // Profile "A"/"B" comes from the file-level JSONInfo, not a scene.
        let profile = first_object_after(&text, "\"JSONInfo\"")
            .and_then(|o| serde_json::from_str::<JsonInfo>(o).ok())
            .map(|i| i.profile);
        Some((meta, profile))
    }) else {
        bail!("failed to parse HDR10+ scene metadata");
    };

    let profile = raw_profile
        .and_then(|p| p.bytes().next())
        .filter(|b| matches!(b, b'A' | b'B'))
        .map(|b| b as char);

    Ok(Payload::Hdr10Plus(Hdr10Plus {
        application_version: meta.application_version,
        num_windows: meta.num_windows,
        profile,
        target_max_luminance: (meta.targeted_system_display_maximum_luminance > 0)
            .then_some(meta.targeted_system_display_maximum_luminance),
    }))
}

/// Return the first complete JSON object (`{ ... }`) that appears after the
/// literal `key` in `text`, or `None` if `key` is absent or the object runs past
/// the head-window boundary (truncated). Quoted string contents are skipped so a
/// brace inside a value can't unbalance the scan.
fn first_object_after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let after = &text[text.find(key)? + key.len()..];
    let bytes = after.as_bytes();
    let open = bytes.iter().position(|&b| b == b'{')?;

    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&after[open..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}
