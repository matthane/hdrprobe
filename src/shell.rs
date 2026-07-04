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
//! - **A sized, cleared, paused console** — hdrprobe is a console app; launched
//!   bare from a verb its window would close before the report could be read,
//!   so the verb runs a `cmd /c "cls & … & pause"` chain: clear the startup
//!   noise (the UNC-cwd warning `cmd` prints on network files), report, pause.
//!   The fresh window is sized to fit a full report without scrolling, and
//!   *how* depends on the console host: Windows Terminal ignores client resize
//!   APIs (`mode con` only reflows the hidden buffer — measured on WT 1.24:
//!   the window stays at profile size), but honours its own `--size` launch
//!   option, so when WT is installed the verb launches through `wt -w new
//!   --size`; without WT a leading `mode con` sizes the classic conhost. See
//!   [`command_for_wt`]/[`command_for`] for the exact quoting.
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

/// The verb window's dimensions: a worst-case report (masthead, all four
/// sections, footnote, pause prompt — about 40 rows; the widest Video line is
/// ~95 columns) fits without scrolling, with headroom for wrapped long paths.
#[cfg(windows)]
const WINDOW_SIZE: (u32, u32) = (110, 45); // (cols, lines)

/// Build the verb's command string for the classic-conhost host (no Windows
/// Terminal installed).
///
/// The stored value is `cmd /c "mode con: cols=C lines=L & cls & "<exe>" "%1"
/// & pause"`. `cmd /c` strips the first and last quote of its argument
/// whenever the command contains special characters (here `&`), so the outer
/// pair is deliberate padding: after `cmd` removes it, what runs is the mode
/// resize (honoured by conhost; Windows Terminal ignores it, which is why the
/// WT host gets [`command_for_wt`] instead), `cls` (wiping the UNC-cwd warning
/// `cmd` prints when Explorer starts the verb in a network folder), the quoted
/// exe, the quoted selected file (`%1`), then a pause so the console stays
/// open for reading. Only the verb's fresh window is resized/cleared; an
/// existing terminal session never sees any of this.
#[cfg(windows)]
fn command_for(exe: &str) -> String {
    let (cols, lines) = WINDOW_SIZE;
    format!("cmd /c \"mode con: cols={cols} lines={lines} & cls & \"{exe}\" \"%1\" & pause\"")
}

/// Build the verb's command string for the Windows Terminal host.
///
/// The stored value is `"<wt>" -w new --size C,L cmd /c "cls & \"<exe>\"
/// \"%1\" & pause"`. WT ignores client resize APIs, so the size rides its own
/// `--size` launch option and `-w new` guarantees a standalone window (never
/// a tab glommed onto an existing one). The inner quotes around the exe and
/// `%1` are backslash-escaped so they survive wt's argv split as literal
/// quotes; wt then hands `cmd` the same `cls & "<exe>" "%1" & pause` chain
/// the conhost verb runs. Verified end-to-end on WT 1.24: the window opens at
/// the requested size and spaced paths parse through all three layers.
///
/// Known limit: wt splits its command line on bare `;`, so a *path*
/// containing a semicolon misparses under this verb (the conhost fallback
/// doesn't split). That's accepted — `%1` can't be escaped statically.
#[cfg(windows)]
fn command_for_wt(wt: &str, exe: &str) -> String {
    let (cols, lines) = WINDOW_SIZE;
    format!(
        "\"{wt}\" -w new --size {cols},{lines} cmd /c \"cls & \\\"{exe}\\\" \\\"%1\\\" & pause\""
    )
}

/// Locate the Windows Terminal launcher: the per-user execution alias at a
/// stable path (present whenever WT is installed with app execution aliases
/// enabled, the default). Resolved at install time; if WT is added or removed
/// later, re-running `--install-shell` re-picks the host.
#[cfg(windows)]
fn wt_path() -> Option<String> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    let wt = format!("{base}\\Microsoft\\WindowsApps\\wt.exe");
    std::path::Path::new(&wt).exists().then_some(wt)
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
        // Prefer the Windows Terminal host when it's installed: it's the
        // Windows 11 default console, and only its own launch option can size
        // the window (see `command_for_wt`).
        let command = match super::wt_path() {
            Some(wt) => super::command_for_wt(&wt, &exe),
            None => super::command_for(&exe),
        };
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
        // the conhost window resize, cls (wiping cmd's UNC-cwd warning on
        // network files), the quoted exe, the quoted selected file, then pause.
        assert_eq!(
            command_for(r"C:\Program Files\hdrprobe.exe"),
            r#"cmd /c "mode con: cols=110 lines=45 & cls & "C:\Program Files\hdrprobe.exe" "%1" & pause""#
        );
    }

    #[test]
    fn wt_command_escapes_inner_quotes_for_argv() {
        // The exe/%1 quotes are backslash-escaped so wt's argv split keeps
        // them literal; the window size rides wt's own --size option since WT
        // ignores mode con.
        assert_eq!(
            command_for_wt(r"C:\WA\wt.exe", r"C:\Program Files\hdrprobe.exe"),
            r#""C:\WA\wt.exe" -w new --size 110,45 cmd /c "cls & \"C:\Program Files\hdrprobe.exe\" \"%1\" & pause""#
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
