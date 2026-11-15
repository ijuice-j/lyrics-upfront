//! Start with Windows: the per-user Run key value "LyricsUpFront", the same
//! value the installer writes, so the two agree. It needs no admin rights and
//! shows up in Settings > Apps > Startup, where the user can also turn it off.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("LyricsUpFront");
/// The value Spotify Lyrics Overlay, the app this one replaces, used.
const LEGACY_VALUE: PCWSTR = w!("LyricOverlay");

fn exists(value: PCWSTR) -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, value, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn is_enabled() -> bool {
    exists(VALUE)
}

/// Add or remove the entry. Returns whether the entry now matches `enabled`.
pub fn set_enabled(enabled: bool) -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE, &mut key) != ERROR_SUCCESS {
            return false;
        }
        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                // Quoted: the install path may contain spaces.
                let command: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
                let bytes = std::slice::from_raw_parts(command.as_ptr() as *const u8, command.len() * 2);
                let _ = RegSetValueExW(key, VALUE, 0, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, VALUE);
        }
        let _ = RegCloseKey(key);
    }
    is_enabled() == enabled
}

/// First run only: if the old app was set to start with Windows, start this
/// one instead, so sign-in does not launch the replaced app (or a missing
/// exe). Returns whether the old entry was found and moved.
pub fn adopt_legacy() -> bool {
    if !exists(LEGACY_VALUE) {
        return false;
    }
    if !set_enabled(true) {
        return false;
    }
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE, &mut key) == ERROR_SUCCESS {
            let _ = RegDeleteValueW(key, LEGACY_VALUE);
            let _ = RegCloseKey(key);
        }
    }
    true
}
