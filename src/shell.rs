//! Windows shell integration: a right-click "Inspect HDR metadata" context-menu
//! verb for every supported input.
//!
//! `--install-shell` registers a verb under
//! `HKCU\Software\Classes\SystemFileAssociations\.<ext>\shell\hdrprobe` for each
//! extension hdrprobe understands (video + metadata sidecars), so any such file
//! can be inspected from Explorer. `--uninstall-shell` removes them.
//!
//! Design choices that matter:
//! - **HKCU, not HKLM** — per-user, so it needs no elevation and touches no
//!   machine-wide state.
//! - **`SystemFileAssociations\.ext`, not a ProgID** — adds a verb for the
//!   extension without owning or altering the file's default handler.
//! - **`cmd /c "cls & … & pause"`** — hdrprobe is a console app; launched bare
//!   from a verb its window would close before the report could be read, so the
//!   verb runs it under `cmd`, clears the console first (wiping the UNC-cwd
//!   warning `cmd` prints on network files), and pauses after. See
//!   [`command_for`] for the exact quoting.
//! - **Raw advapi32 FFI, no crate** — mirrors the `prefetch` module's direct
//!   `kernel32` calls and avoids a new dependency in the release binary.
//!
//! On Windows 11 the entry appears in the classic menu, reached via "Show more
//! options".

/// Every extension the shell verb is registered for: all video containers plus
/// the metadata sidecars (`.rpu`/`.bin`/`.xml`/`.json`). hdrprobe still content-
/// sniffs sidecars at runtime, so a `.json` that isn't HDR10+ simply reports
/// nothing — the menu entry is harmless on unrelated files.
#[cfg(windows)]
fn all_exts() -> impl Iterator<Item = &'static str> {
    const SIDECAR_EXTS: &[&str] = &["rpu", "bin", "xml", "json"];
    crate::VIDEO_EXTS.iter().chain(SIDECAR_EXTS).copied()
}

/// Build the verb's command string for a given `hdrprobe.exe` path.
///
/// The stored value is `cmd /c "cls & "<exe>" "%1" & pause"`. `cmd /c` strips
/// the first and last quote of its argument whenever the command contains
/// special characters (here `&`), so the outer pair is deliberate padding:
/// after `cmd` removes it, what runs is `cls & "<exe>" "%1" & pause` — clear
/// the console, the quoted exe, the quoted selected file (`%1`), then a pause
/// so the console stays open for reading. The leading `cls` matters on
/// network files: Explorer starts the verb with the file's folder as the
/// working directory, and when that's a UNC path `cmd` opens by printing a
/// three-line "UNC paths are not supported" warning — `cls` wipes it so the
/// report starts at the top of a clean console. Only the verb clears; running
/// hdrprobe from an existing terminal never does.
#[cfg(windows)]
fn command_for(exe: &str) -> String {
    format!("cmd /c \"cls & \"{exe}\" \"%1\" & pause\"")
}

#[cfg(windows)]
mod imp {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{bail, Context, Result};

    type Hkey = *mut c_void;
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as usize as Hkey;
    const KEY_WRITE: u32 = 0x2_0006;
    const REG_SZ: u32 = 1;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegCreateKeyExW(
            h_key: Hkey,
            lp_sub_key: *const u16,
            reserved: u32,
            lp_class: *const u16,
            dw_options: u32,
            sam_desired: u32,
            lp_security_attributes: *mut c_void,
            phk_result: *mut Hkey,
            lpdw_disposition: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            h_key: Hkey,
            lp_value_name: *const u16,
            reserved: u32,
            dw_type: u32,
            lp_data: *const u8,
            cb_data: u32,
        ) -> i32;
        fn RegCloseKey(h_key: Hkey) -> i32;
        fn RegDeleteTreeW(h_key: Hkey, lp_sub_key: *const u16) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Create `HKCU\<subkey>` (and any missing parents) and set a value on it.
    /// `name = None` sets the key's default (unnamed) value.
    fn set_value(subkey: &str, name: Option<&str>, value: &str) -> Result<()> {
        let wsub = wide(subkey);
        let mut hkey: Hkey = std::ptr::null_mut();
        // SAFETY: `wsub` is a valid NUL-terminated wide string that outlives the
        // call; `phk_result` receives an owned handle we close below.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wsub.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_WRITE,
                std::ptr::null_mut(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            bail!("creating registry key HKCU\\{subkey} failed (code {rc})");
        }

        let wval = wide(value);
        let wname = name.map(wide);
        let name_ptr = wname.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
        let cb = (wval.len() * 2) as u32; // byte length, including the NUL
        // SAFETY: `hkey` is valid; `wval`/`wname` outlive the call; `cb` counts
        // exactly the bytes at `wval` including its terminating NUL.
        let rc = unsafe { RegSetValueExW(hkey, name_ptr, 0, REG_SZ, wval.as_ptr().cast::<u8>(), cb) };
        // SAFETY: `hkey` was produced by the matching `RegCreateKeyExW`.
        unsafe { RegCloseKey(hkey) };
        if rc != ERROR_SUCCESS {
            bail!("setting value on HKCU\\{subkey} failed (code {rc})");
        }
        Ok(())
    }

    pub fn install() -> Result<()> {
        let exe = std::env::current_exe().context("locating the hdrprobe executable")?;
        let exe = exe.to_string_lossy().into_owned();
        let command = super::command_for(&exe);
        let icon = format!("{exe},0");

        let mut n = 0;
        for ext in super::all_exts() {
            let base = format!("Software\\Classes\\SystemFileAssociations\\.{ext}\\shell\\hdrprobe");
            set_value(&base, None, "Inspect HDR metadata")?;
            set_value(&base, Some("Icon"), &icon)?;
            set_value(&format!("{base}\\command"), None, &command)?;
            n += 1;
        }

        println!("Registered the hdrprobe context-menu entry for {n} file types.");
        println!("Verb runs: {command}");
        println!("On Windows 11 it's under \"Show more options\" in the right-click menu.");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let mut n = 0;
        for ext in super::all_exts() {
            let sub = wide(&format!("Software\\Classes\\SystemFileAssociations\\.{ext}\\shell\\hdrprobe"));
            // SAFETY: `sub` is a valid NUL-terminated wide string; the hive is a
            // predefined handle. Deletes the verb key and its `command` subkey.
            let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, sub.as_ptr()) };
            if rc == ERROR_SUCCESS {
                n += 1;
            } else if rc != ERROR_FILE_NOT_FOUND {
                bail!("removing registry key for .{ext} failed (code {rc})");
            }
        }
        println!("Removed the hdrprobe context-menu entry ({n} file types).");
        Ok(())
    }
}

#[cfg(windows)]
pub use imp::{install, uninstall};

#[cfg(not(windows))]
pub fn install() -> anyhow::Result<()> {
    anyhow::bail!("shell integration is only available on Windows");
}

#[cfg(not(windows))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!("shell integration is only available on Windows");
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn command_quoting_pads_for_cmd_stripping() {
        // The outer quote pair is padding `cmd /c` strips; what actually runs is
        // cls (wiping cmd's UNC-cwd warning on network files), the quoted exe,
        // the quoted selected file, then pause.
        assert_eq!(
            command_for(r"C:\Program Files\hdrprobe.exe"),
            r#"cmd /c "cls & "C:\Program Files\hdrprobe.exe" "%1" & pause""#
        );
    }

    #[test]
    fn all_exts_covers_video_and_sidecars() {
        let exts: Vec<&str> = all_exts().collect();
        assert!(exts.contains(&"mkv"));
        assert!(exts.contains(&"m2ts"));
        assert!(exts.contains(&"rpu"));
        assert!(exts.contains(&"json"));
        // Video list plus the four sidecar extensions, no duplicates dropped.
        assert_eq!(exts.len(), crate::VIDEO_EXTS.len() + 4);
    }
}
