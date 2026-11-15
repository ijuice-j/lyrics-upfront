//! The Settings window, opened from the tray menu.
//!
//! Standard Windows controls (Common Controls v6 via the embedded manifest),
//! created in code. Every change applies to the overlay immediately; the
//! settings file is written when the window closes, like the Python app.
//!
//! Appearance: background opacity, text size, tint from the album cover.
//! Behaviour: keep above other windows, start automatically at sign-in.

use std::cell::RefCell;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Settings;

pub enum SettingsEvent {
    Opacity(i32),
    TextSize(i32),
    Tint(bool),
    AlwaysOnTop(bool),
    StartWithWindows(bool),
    Closed,
}

const CLASS: PCWSTR = w!("LyricsUpFrontSettings");

const ID_OPACITY: i32 = 101;
const ID_TEXT: i32 = 102;
const ID_TINT: i32 = 103;
const ID_TOP: i32 = 104;
const ID_STARTUP: i32 = 105;
const ID_OPACITY_VALUE: i32 = 201;
const ID_TEXT_VALUE: i32 = 202;
const ID_HINT: i32 = 301;
const ID_NOTE: i32 = 302;
const ID_DONE: i32 = 1; // IDOK, so Enter closes the window
const ID_CANCEL: i32 = 2; // IDCANCEL, sent for Esc

// Control styles and messages (winuser.h / commctrl.h).
const BS_DEFPUSHBUTTON: u32 = 0x1;
const BS_AUTOCHECKBOX: u32 = 0x3;
const BS_GROUPBOX: u32 = 0x7;
const SS_RIGHT: u32 = 0x2;
const TBS_NOTICKS: u32 = 0x10;
const TBM_GETPOS: u32 = WM_USER;
const TBM_SETPOS: u32 = WM_USER + 5;
const TBM_SETRANGEMIN: u32 = WM_USER + 7;
const TBM_SETRANGEMAX: u32 = WM_USER + 8;
const TBM_SETPAGESIZE: u32 = WM_USER + 21;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const DM_GETDEFID: u32 = WM_USER;
const DC_HASDEFID: usize = 0x534B;

/// Client area in DIPs; controls are laid out on this grid and scaled.
const CLIENT_W: i32 = 460;
const CLIENT_H: i32 = 356;

struct Control {
    hwnd: HWND,
    rect: (i32, i32, i32, i32), // x, y, w, h in DIPs
}

struct Ui {
    hwnd: HWND,
    font: HFONT,
    controls: Vec<Control>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// The open Settings window, for keyboard navigation in the message loop.
pub fn dialog() -> Option<HWND> {
    UI.with(|u| u.borrow().as_ref().map(|ui| ui.hwnd))
}

/// Open the window, or bring it forward if it is already open.
/// `near` is the overlay's rectangle, used to pick the monitor.
pub fn open(s: &Settings, near: RECT) {
    if let Some(hwnd) = dialog() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(None).unwrap_or_default();
        // Icon 1 is the embedded app icon (see build.rs).
        let icon = LoadIconW(instance, PCWSTR(1 as _)).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: icon,
            hIconSm: icon,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class); // fails harmlessly if already registered

        // Size for the overlay's monitor; WM_DPICHANGED corrects it if needed.
        let monitor = MonitorFromRect(&near, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(monitor, windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        // Topmost, like the Python dialog: the overlay itself is topmost.
        let ex = WS_EX_TOPMOST;
        let (w, h) = window_size(dx, style, ex);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let work = info.rcWork;
        let x = (work.left + work.right - w) / 2;
        let y = (work.top + work.bottom - h) / 2;
        let Ok(hwnd) = CreateWindowExW(ex, CLASS, w!("Lyrics UpFront \u{2014} Settings"), style, x, y, w, h, None, None, instance, None) else {
            return;
        };

        let mut controls = Vec::new();
        let mut add = |class: PCWSTR, text: &str, style: u32, id: i32, rect: (i32, i32, i32, i32)| -> HWND {
            let c = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                &HSTRING::from(text),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                0,
                0,
                0,
                0,
                hwnd,
                HMENU(id as isize as _),
                instance,
                None,
            )
            .unwrap_or_default();
            controls.push(Control { hwnd: c, rect });
            c
        };
        let tab = WS_TABSTOP.0;
        add(w!("BUTTON"), "Appearance", BS_GROUPBOX, 0, (12, 10, 436, 172));
        add(w!("STATIC"), "Background opacity", 0, 0, (28, 40, 140, 20));
        let opacity = add(w!("msctls_trackbar32"), "", TBS_NOTICKS | tab, ID_OPACITY, (172, 35, 218, 30));
        let opacity_value = add(w!("STATIC"), "", SS_RIGHT, ID_OPACITY_VALUE, (394, 40, 42, 20));
        add(w!("STATIC"), "Text size", 0, 0, (28, 76, 140, 20));
        let text = add(w!("msctls_trackbar32"), "", TBS_NOTICKS | tab, ID_TEXT, (172, 71, 218, 30));
        let text_value = add(w!("STATIC"), "", SS_RIGHT, ID_TEXT_VALUE, (394, 76, 42, 20));
        let tint = add(w!("BUTTON"), "Tint the card to match the album cover", BS_AUTOCHECKBOX | tab, ID_TINT, (28, 108, 400, 22));
        add(
            w!("STATIC"),
            "Only the background is transparent; the lyrics stay solid. Album tinting changes the card\u{2019}s colour, never how dark it is.",
            0,
            ID_HINT,
            (28, 136, 404, 38),
        );
        add(w!("BUTTON"), "Behaviour", BS_GROUPBOX, 0, (12, 192, 436, 112));
        let top = add(w!("BUTTON"), "Keep above other windows", BS_AUTOCHECKBOX | tab, ID_TOP, (28, 218, 400, 22));
        let startup = add(w!("BUTTON"), "Start automatically when I sign in", BS_AUTOCHECKBOX | tab, ID_STARTUP, (28, 244, 400, 22));
        add(w!("STATIC"), "Closing the overlay hides it to the tray. Quit from the tray menu.", 0, ID_NOTE, (28, 272, 404, 22));
        add(w!("BUTTON"), "Done", BS_DEFPUSHBUTTON | tab, ID_DONE, (360, 316, 88, 28));

        slider(opacity, 10, 100, s.opacity_percent());
        slider(text, 60, 200, (s.text_scale * 100.0).round() as i32);
        set_text(opacity_value, &format!("{}%", s.opacity_percent()));
        set_text(text_value, &format!("{}%", (s.text_scale * 100.0).round() as i32));
        check(tint, s.theme_from_album);
        check(top, s.always_on_top);
        check(startup, s.start_with_windows);

        UI.with(|u| *u.borrow_mut() = Some(Ui { hwnd, font: HFONT::default(), controls }));
        layout(hwnd, GetDpiForWindow(hwnd).max(96));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn window_size(dpi: u32, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE) -> (i32, i32) {
    let scale = dpi as f32 / 96.0;
    let mut r = RECT { left: 0, top: 0, right: (CLIENT_W as f32 * scale) as i32, bottom: (CLIENT_H as f32 * scale) as i32 };
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// Place every control for `dpi` and give them the system UI font at that DPI.
unsafe fn layout(hwnd: HWND, dpi: u32) {
    let scale = dpi as f32 / 96.0;
    let mut metrics = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    let _ = SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        metrics.cbSize,
        Some(&mut metrics as *mut _ as *mut _),
        0,
        dpi,
    );
    let font = CreateFontIndirectW(&metrics.lfMessageFont);
    UI.with(|u| {
        if let Some(ui) = u.borrow_mut().as_mut() {
            if ui.hwnd != hwnd {
                return;
            }
            for c in &ui.controls {
                let (x, y, w, h) = c.rect;
                let s = |v: i32| (v as f32 * scale).round() as i32;
                let _ = MoveWindow(c.hwnd, s(x), s(y), s(w), s(h), true);
                SendMessageW(c.hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }
            if !ui.font.is_invalid() {
                let _ = DeleteObject(ui.font);
            }
            ui.font = font;
        }
    });
}

unsafe fn slider(hwnd: HWND, min: i32, max: i32, value: i32) {
    SendMessageW(hwnd, TBM_SETRANGEMIN, WPARAM(0), LPARAM(min as isize));
    SendMessageW(hwnd, TBM_SETRANGEMAX, WPARAM(0), LPARAM(max as isize));
    SendMessageW(hwnd, TBM_SETPAGESIZE, WPARAM(0), LPARAM(10));
    SendMessageW(hwnd, TBM_SETPOS, WPARAM(1), LPARAM(value.clamp(min, max) as isize));
}

unsafe fn check(hwnd: HWND, on: bool) {
    SendMessageW(hwnd, BM_SETCHECK, WPARAM(on as usize), LPARAM(0));
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let _ = SetWindowTextW(hwnd, &HSTRING::from(text));
}

unsafe fn item(parent: HWND, id: i32) -> HWND {
    GetDlgItem(parent, id).unwrap_or_default()
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HSCROLL => {
            let control = HWND(lp.0 as _);
            let id = GetDlgCtrlID(control);
            let value = SendMessageW(control, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as i32;
            match id {
                ID_OPACITY => {
                    set_text(item(hwnd, ID_OPACITY_VALUE), &format!("{value}%"));
                    crate::app::on_settings(SettingsEvent::Opacity(value));
                }
                ID_TEXT => {
                    set_text(item(hwnd, ID_TEXT_VALUE), &format!("{value}%"));
                    crate::app::on_settings(SettingsEvent::TextSize(value));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            let checked = |id| SendMessageW(item(hwnd, id), BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1;
            match id {
                ID_TINT => crate::app::on_settings(SettingsEvent::Tint(checked(ID_TINT))),
                ID_TOP => crate::app::on_settings(SettingsEvent::AlwaysOnTop(checked(ID_TOP))),
                ID_STARTUP => {
                    crate::app::on_settings(SettingsEvent::StartWithWindows(checked(ID_STARTUP)));
                    // Reflect what the registry now says, in case it failed.
                    check(item(hwnd, ID_STARTUP), crate::startup::is_enabled());
                }
                ID_DONE | ID_CANCEL => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        DM_GETDEFID => LRESULT(((DC_HASDEFID << 16) | ID_DONE as usize) as isize),
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wp.0 as _);
            let id = GetDlgCtrlID(HWND(lp.0 as _));
            SetBkMode(hdc, TRANSPARENT);
            if id == ID_HINT || id == ID_NOTE {
                SetTextColor(hdc, COLORREF(0x0073_7373)); // grey hint text
            }
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        }
        WM_DPICHANGED => {
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            layout(hwnd, ((wp.0 >> 16) & 0xFFFF) as u32);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().take() {
                    let _ = DeleteObject(ui.font);
                }
            });
            crate::app::on_settings(SettingsEvent::Closed);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
