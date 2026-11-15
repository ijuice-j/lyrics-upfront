//! Notification-area icon: the Lyrics UpFront mark while a song plays, the
//! greyed mark while paused or idle. Both come from the exe's own icon
//! resources (see icon_art.rs), loaded once at the tray's size; the icon is
//! only re-sent when play/pause changes (the Python app re-drew and re-set it
//! on every 120ms poll, ~8% of a core plus Explorer's share). The tooltip is
//! always the app's name.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::LoadIconWithScaleDown;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::icon_art;

const TIP: &str = "Lyrics UpFront";

pub const WM_TRAY: u32 = WM_APP + 2;
pub const ID_TOGGLE: usize = 1001;
pub const ID_QUIT: usize = 1002;
pub const ID_SETTINGS: usize = 1003;

pub struct Tray {
    hwnd: HWND,
    icons: [HICON; 2], // [playing, paused]
    playing: bool,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Self {
        // The taskbar's small-icon size; Windows picks the nearest image in
        // the resource and scales it down smoothly.
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16);
        let icons = [load_icon(icon_art::GROUP_ICON, size), load_icon(icon_art::GROUP_ICON_PAUSED, size)];
        let tray = Tray { hwnd, icons, playing: false };
        tray.add();
        tray
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icons[if self.playing { 0 } else { 1 }],
            ..Default::default()
        };
        copy_into(&mut nid.szTip, TIP);
        nid
    }

    /// Also used to re-add the icon after Explorer restarts.
    pub fn add(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &self.data());
        }
    }

    pub fn set_playing(&mut self, playing: bool) {
        if playing == self.playing {
            return;
        }
        self.playing = playing;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data());
        }
    }

    /// A notification bubble from the tray icon.
    pub fn notify(&self, title: &str, text: &str) {
        let mut nid = self.data();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        copy_into(&mut nid.szInfoTitle, title);
        copy_into(&mut nid.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn remove(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
        }
    }
}

fn copy_into(dst: &mut [u16], s: &str) {
    for (i, c) in s.encode_utf16().take(dst.len() - 1).enumerate() {
        dst[i] = c;
    }
}

/// Right-click menu. Runs a modal loop, so the caller must not hold app state.
pub fn show_menu(hwnd: HWND, visible: bool) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, ID_TOGGLE, if visible { w!("Hide overlay") } else { w!("Show overlay") });
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("Settings\u{2026}"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// One of the exe's icon groups at `size` pixels.
fn load_icon(group: u16, size: i32) -> HICON {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        LoadIconWithScaleDown(instance, PCWSTR(group as usize as *const u16), size, size).unwrap_or_default()
    }
}
