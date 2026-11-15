//! Window, input, events and the main loop.
//!
//! The main loop blocks in MsgWaitForMultipleObjectsEx until a message arrives
//! or the next scheduled visual change is due; while something is moving it
//! waits on the swap chain's vsync object instead. When paused, idle or hidden
//! the process does no periodic work at all on this thread.

use std::cell::RefCell;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, Settings};
use crate::focus::{self, Focus, Model, Status, Wake as FrameWake};
use crate::gfx::Gpu;
use crate::lyrics::Provider;
use crate::palette::NEUTRAL;
use crate::source::{self, Event, Wake};
use crate::text::Text;
use crate::tray::{self, Tray};

const CLASS: PCWSTR = w!("LyricsUpFront");
const WM_EVENT: u32 = WM_APP + 1;
pub const WM_SHOW: u32 = WM_APP + 3;
const TIMER_ART: usize = 1;
const TIMER_RETRY: usize = 2;
const TIMER_DRAG: usize = 3;
const TIMER_SIM_HIDE: usize = 4;
const EDGE_DIP: f32 = 8.0;
/// From winuser.h; not exported by the modules this file imports.
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Copy)]
struct HwndWaker(isize);

impl Wake for HwndWaker {
    fn wake(&self) {
        unsafe {
            let _ = PostMessageW(HWND(self.0 as _), WM_EVENT, WPARAM(0), LPARAM(0));
        }
    }
}

enum Press {
    Move,
    Size(usize),
    Close,
}

pub struct App {
    hwnd: HWND,
    gpu: Option<Gpu>,
    text: Text,
    focus: Focus,
    model: Model,
    settings: Settings,
    rx: Receiver<Event>,
    tx: Sender<Event>,
    provider: Arc<Provider>,
    simulate: Option<String>,
    tray: Tray,
    visible: bool,
    dirty: bool,
    dpi: u32,
    current_key: String,
    theme_key: String,
    track: Option<(String, String, String, i64)>,
    cover_pixels: Option<crate::art::Cover>,
    /// The current cover's colour theme, kept even while tinting is off so
    /// turning it back on recolours the card at once.
    album_theme: Option<crate::palette::Theme>,
    /// Failed lyric lookups for the current track, to pace the retries.
    lyrics_failures: u32,
    pressed_close: bool,
    tracking_mouse: bool,
    pub frames: u64,
    /// Opt-in frame statistics (LYRICS_UPFRONT_LOG): window start, frames, draw time.
    stats: Option<(Instant, u32, Duration)>,
    /// Test hook (LYRICS_UPFRONT_SNAPSHOT): where to save drawn frames, and since when.
    snapshot: Option<(std::path::PathBuf, Instant)>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

fn hiword(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}

pub fn claim_single_instance() -> bool {
    unsafe {
        let _ = CreateMutexW(None, false, w!("Local\\LyricsUpFront"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(hwnd) = FindWindowW(CLASS, PCWSTR::null()) {
                let _ = PostMessageW(hwnd, WM_SHOW, WPARAM(0), LPARAM(0));
            }
            return false;
        }
    }
    true
}

pub fn run(simulate: Option<String>) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);
        TASKBAR_CREATED.with(|c| c.set(RegisterWindowMessageW(w!("TaskbarCreated"))));

        let settings = if simulate.is_some() { Settings::default() } else { config::load() };
        // Test hook for README images: with --simulate, LYRICS_UPFRONT_SNAPSHOT=<dir>
        // saves every frame drawn there as raw premultiplied BGRA, at twice the
        // display's scale. Nothing is read from the screen.
        let snapshot = simulate.as_ref().and(std::env::var_os("LYRICS_UPFRONT_SNAPSHOT")).map(std::path::PathBuf::from);
        let boost = if snapshot.is_some() { 2 } else { 1 };

        // Place on the monitor that holds the saved position, else the primary.
        let probe = POINT { x: settings.x.unwrap_or(0), y: settings.y.unwrap_or(0) };
        let monitor = MonitorFromPoint(probe, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = (dx * boost) as f32 / 96.0;
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let work = info.rcWork;
        let (w, h) = ((settings.w as f32 * scale) as i32, (settings.h as f32 * scale) as i32);
        let (mut x, mut y) = match (settings.x, settings.y) {
            (Some(x), Some(y)) => ((x as f32 * scale) as i32, (y as f32 * scale) as i32),
            _ => ((work.left + work.right) / 2 - w / 2, work.bottom - h - (72.0 * scale) as i32),
        };
        x = x.clamp(work.left, (work.right - w).max(work.left));
        y = y.clamp(work.top, (work.bottom - h).max(work.top));

        let mut ex = WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW;
        if settings.always_on_top {
            ex |= WS_EX_TOPMOST;
        }
        let hwnd = CreateWindowExW(ex, CLASS, w!("Lyrics UpFront"), WS_POPUP, x, y, w, h, None, None, instance, None)?;
        let dpi = GetDpiForWindow(hwnd).max(96) * boost;
        log!(
            "app: window at ({x},{y}) {w}x{h} px, dpi {dpi}; settings: opacity {}%, text {}%, tint {}, on top {}, start with Windows {}, simulate {:?}",
            settings.opacity_percent(),
            (settings.text_scale * 100.0).round(),
            settings.theme_from_album,
            settings.always_on_top,
            settings.start_with_windows,
            simulate
        );

        let (tx, rx) = channel();
        let waker = HwndWaker(hwnd.0 as isize);
        match &simulate {
            Some(s) => crate::sim::spawn(s.clone(), tx.clone(), waker),
            None => source::spawn_smtc(tx.clone(), waker),
        }

        let mut model = Model::new();
        model.plate_core = settings.plate_core;
        model.plate_edge = settings.plate_edge;
        model.text_scale = settings.text_scale;

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let gpu = Gpu::new(hwnd, (rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32, dpi as f32)?;
        let app = App {
            hwnd,
            gpu: Some(gpu),
            text: Text::new()?,
            focus: Focus::new(),
            model,
            settings,
            rx,
            tx,
            // Test hook: LYRICS_UPFRONT_CACHE_DIR uses a different lyrics cache.
            provider: Provider::new(
                std::env::var_os("LYRICS_UPFRONT_CACHE_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| config::config_dir().join("cache")),
            ),
            simulate: simulate.clone(),
            tray: Tray::new(hwnd),
            visible: true,
            dirty: true,
            dpi,
            current_key: String::new(),
            theme_key: String::new(),
            track: None,
            cover_pixels: None,
            album_theme: None,
            lyrics_failures: 0,
            pressed_close: false,
            tracking_mouse: false,
            frames: 0,
            stats: std::env::var_os("LYRICS_UPFRONT_LOG").map(|_| (Instant::now(), 0, Duration::ZERO)),
            snapshot: snapshot.map(|dir| (dir, Instant::now())),
        };
        APP.with(|c| *c.borrow_mut() = Some(app));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if simulate.as_deref() == Some("hidden") {
            SetTimer(hwnd, TIMER_SIM_HIDE, 3000, None);
        }

        main_loop();
    }
    // Release the app (graphics device, composition target, tray icon,
    // HTTP session) now, while the process is running normally. Left to the
    // thread-local destructor, it would be dropped during process unload,
    // under the loader lock, where graphics-driver cleanup can deadlock and
    // leave a windowless process holding the single-instance lock.
    let app = APP.with(|c| c.borrow_mut().take());
    drop(app);
    log!("app: released");
    Ok(())
}

fn main_loop() {
    let mut msg = MSG::default();
    loop {
        let (timeout, frame_now) = APP.with(|c| c.borrow().as_ref().map(App::schedule).unwrap_or((INFINITE, false)));
        unsafe {
            if !frame_now {
                MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                // Tab, Enter and Esc in the Settings window.
                if let Some(dialog) = crate::settings::dialog() {
                    if IsDialogMessageW(dialog, &msg).as_bool() {
                        continue;
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        APP.with(|c| {
            if let Ok(mut g) = c.try_borrow_mut() {
                if let Some(app) = g.as_mut() {
                    app.tick();
                }
            }
        });
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Messages whose handling enters a modal loop must not hold the app borrow.
    match msg {
        WM_LBUTTONDOWN => {
            let press = with_app(|a| a.press(lp));
            match press {
                Some(Press::Move) => {
                    let _ = ReleaseCapture();
                    SendMessageW(hwnd, WM_SYSCOMMAND, WPARAM(0xF012), LPARAM(0)); // SC_MOVE | HTCAPTION
                }
                Some(Press::Size(code)) => {
                    let _ = ReleaseCapture();
                    SendMessageW(hwnd, WM_SYSCOMMAND, WPARAM(0xF000 + code), LPARAM(0)); // SC_SIZE + edge
                }
                Some(Press::Close) | None => {}
            }
            return LRESULT(0);
        }
        // Ending the loop must never depend on borrowing the app: WM_DESTROY
        // usually arrives re-entrantly, from inside a handler that holds it.
        WM_DESTROY => {
            log!("app: window destroyed, stopping");
            PostQuitMessage(0);
            return LRESULT(0);
        }
        // Opening Settings creates windows, so it runs without the app borrowed.
        WM_COMMAND if loword(wp.0) as usize == tray::ID_SETTINGS => {
            open_settings(hwnd);
            return LRESULT(0);
        }
        tray::WM_TRAY => {
            let event = loword(lp.0 as usize);
            if event == WM_LBUTTONUP {
                with_app(|a| a.toggle());
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                tray::show_menu(hwnd, IsWindowVisible(hwnd).as_bool());
            }
            return LRESULT(0);
        }
        _ => {}
    }
    if msg != 0 && msg == TASKBAR_CREATED.with(|c| c.get()) {
        with_app(|a| a.tray.add());
        return LRESULT(0);
    }
    let handled = APP.with(|c| match c.try_borrow_mut() {
        Ok(mut g) => g.as_mut().and_then(|a| a.handle(msg, wp, lp)),
        Err(_) => None,
    });
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn open_settings(hwnd: HWND) {
    let Some(settings) = with_app(|a| a.settings.clone()) else { return };
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    crate::settings::open(&settings, rect);
}

/// A change made in the Settings window; applied to the overlay at once.
pub fn on_settings(event: crate::settings::SettingsEvent) {
    with_app(|a| a.on_settings(event));
}

impl App {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    fn client_dips(&self) -> (f32, f32) {
        self.gpu.as_ref().map(|g| g.size_dips()).unwrap_or((600.0, 290.0))
    }

    /// (wait timeout ms, render at the next vsync without waiting for messages)
    fn schedule(&self) -> (u32, bool) {
        if !self.visible || self.gpu.is_none() {
            return (INFINITE, false);
        }
        if self.dirty {
            return (0, true);
        }
        let now = Instant::now();
        let bar_px = focus::progress_width(self.client_dips().0) * self.scale();
        match self.focus.next_wake(&self.model, now, bar_px) {
            FrameWake::Continuous => (0, true),
            FrameWake::Never => (INFINITE, false),
            FrameWake::At(t) => {
                let left = t.saturating_duration_since(now);
                if left <= Duration::from_millis(4) {
                    (0, true)
                } else {
                    // Wake slightly early: the system timer is ~15.6ms coarse.
                    ((left.as_millis() as u32).saturating_sub(6).max(1), false)
                }
            }
        }
    }

    fn tick(&mut self) {
        let (_, frame) = self.schedule();
        if frame {
            let t0 = Instant::now();
            if let Some(gpu) = &self.gpu {
                unsafe {
                    WaitForSingleObjectEx(gpu.waitable, 100, false);
                }
            }
            let t1 = Instant::now();
            self.render();
            let (wait, draw) = (t1 - t0, t1.elapsed());
            if wait > Duration::from_millis(40) || draw > Duration::from_millis(20) {
                crate::log(format!("slow frame: waited {}ms, drew {}ms", wait.as_millis(), draw.as_millis()));
            }
            if let Some((start, frames, total)) = &mut self.stats {
                *frames += 1;
                *total += draw;
                if start.elapsed() >= Duration::from_secs(5) {
                    let secs = start.elapsed().as_secs_f64();
                    crate::log(format!(
                        "frames: {:.1}/s, average draw {:.2} ms",
                        *frames as f64 / secs,
                        total.as_secs_f64() * 1000.0 / (*frames).max(1) as f64
                    ));
                    *start = Instant::now();
                    *frames = 0;
                    *total = Duration::ZERO;
                }
            }
        }
    }

    fn render(&mut self) {
        if !self.visible {
            return;
        }
        let Some(gpu) = &self.gpu else { return };
        let now = Instant::now();
        let drawn = self.focus.render(gpu, &self.text, &self.model, now);
        if let (Ok(()), Some((dir, start))) = (&drawn, &self.snapshot) {
            if let Ok((w, h, px)) = gpu.read_frame() {
                let _ = std::fs::write(dir.join(format!("{:06}_{w}x{h}.bgra", start.elapsed().as_millis())), px);
            }
        }
        match drawn.and_then(|_| gpu.present()) {
            Ok(()) => {
                self.frames += 1;
                self.dirty = false;
            }
            Err(e) => {
                crate::log(format!("render failed after {} frames: {:#010x} {}", self.frames, e.code().0, e.message()));
                // Only a lost device needs a new one; anything else just
                // rebuilds the cached layers on the next frame.
                const D2DERR_RECREATE_TARGET: i32 = 0x8899000Cu32 as i32;
                const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A0005u32 as i32;
                const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A0007u32 as i32;
                if matches!(e.code().0, D2DERR_RECREATE_TARGET | DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET) {
                    self.recreate_device();
                } else {
                    self.focus.invalidate_all();
                }
            }
        }
    }

    fn recreate_device(&mut self) {
        self.focus.reset_device();
        self.gpu = None;
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        self.gpu = Gpu::new(self.hwnd, (rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32, self.dpi as f32).ok();
        if let (Some(gpu), Some(cover)) = (&self.gpu, &self.cover_pixels) {
            self.focus.cover = focus::cover_bitmap(gpu, cover).ok();
        }
        self.dirty = true;
    }

    fn set_visible(&mut self, visible: bool) {
        log!("app: overlay {}", if visible { "shown" } else { "hidden" });
        self.visible = visible;
        unsafe {
            let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        if visible {
            self.dirty = true;
        } else {
            self.save_geometry();
        }
    }

    fn toggle(&mut self) {
        self.set_visible(!self.visible);
    }

    /// Hide from the close button. The first time, say where it went:
    /// Windows tucks new tray icons into the overflow, so otherwise the app
    /// looks like it quit.
    fn hide_to_tray(&mut self) {
        self.set_visible(false);
        if !self.settings.tray_hint_shown {
            log!("app: first hide, showing the one-time tray notice");
            self.tray.notify(
                "Still running",
                "Lyrics UpFront is in the notification area. Click the tray icon to bring it back.",
            );
            self.settings.tray_hint_shown = true;
            self.save_settings();
        }
    }

    fn save_settings(&self) {
        if self.simulate.is_none() {
            config::save_settings(&self.settings);
        }
    }

    /// The colour theme the card should show for the current cover.
    fn wanted_theme(&self) -> crate::palette::Theme {
        match (self.settings.theme_from_album, self.album_theme) {
            (true, Some(t)) => t,
            _ => NEUTRAL,
        }
    }

    fn on_settings(&mut self, event: crate::settings::SettingsEvent) {
        use crate::settings::SettingsEvent as E;
        log!(
            "settings: {}",
            match &event {
                E::Opacity(v) => format!("background opacity {v}%"),
                E::TextSize(v) => format!("text size {v}%"),
                E::Tint(on) => format!("tint from album cover {on}"),
                E::AlwaysOnTop(on) => format!("keep above other windows {on}"),
                E::StartWithWindows(on) => format!("start automatically {on}"),
                E::Closed => "window closed, saving".to_string(),
            }
        );
        match event {
            E::Opacity(percent) => {
                self.settings.set_opacity_percent(percent);
                self.model.plate_core = self.settings.plate_core;
                self.model.plate_edge = self.settings.plate_edge;
                self.focus.invalidate_plate();
            }
            E::TextSize(percent) => {
                self.settings.text_scale = (percent.clamp(60, 200) as f32) / 100.0;
                self.model.text_scale = self.settings.text_scale;
                self.focus.invalidate_all(); // line layers are sized by the text
            }
            E::Tint(on) => {
                self.settings.theme_from_album = on;
                let theme = self.wanted_theme();
                self.model.set_theme(theme, Instant::now());
                self.focus.invalidate_plate();
            }
            E::AlwaysOnTop(on) => {
                self.settings.always_on_top = on;
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        if on { HWND_TOPMOST } else { HWND_NOTOPMOST },
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            E::StartWithWindows(on) => {
                // The simulator never touches the registry.
                if self.simulate.is_none() {
                    crate::startup::set_enabled(on);
                    self.settings.start_with_windows = crate::startup::is_enabled();
                } else {
                    self.settings.start_with_windows = on;
                }
            }
            E::Closed => self.save_settings(),
        }
        self.dirty = true;
    }

    fn save_geometry(&self) {
        if self.simulate.is_some() {
            return;
        }
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        let s = self.scale();
        let f = |v: i32| (v as f32 / s).round() as i32;
        config::save_geometry(f(r.left), f(r.top), f(r.right - r.left), f(r.bottom - r.top));
    }

    fn cursor_client(&self) -> (f32, f32) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        (p.x as f32, p.y as f32)
    }

    /// Edge under the cursor as a WMSZ_* code (1..8), or 0.
    fn edge(&self) -> usize {
        let (x, y) = self.cursor_client();
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let e = EDGE_DIP * self.scale();
        let (l, r, t, b) = (x <= e, x >= rc.right as f32 - e, y <= e, y >= rc.bottom as f32 - e);
        match (t, b, l, r) {
            (true, _, true, _) => 4,  // WMSZ_TOPLEFT
            (true, _, _, true) => 5,  // WMSZ_TOPRIGHT
            (_, true, true, _) => 7,  // WMSZ_BOTTOMLEFT
            (_, true, _, true) => 8,  // WMSZ_BOTTOMRIGHT
            (true, ..) => 3,          // WMSZ_TOP
            (_, true, ..) => 6,       // WMSZ_BOTTOM
            (_, _, true, _) => 1,     // WMSZ_LEFT
            (_, _, _, true) => 2,     // WMSZ_RIGHT
            _ => 0,
        }
    }

    fn over_close(&self) -> bool {
        let (x, y) = self.cursor_client();
        let s = self.scale();
        let r = Focus::close_rect(self.client_dips().0);
        let (x, y) = (x / s, y / s);
        x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
    }

    fn press(&mut self, _lp: LPARAM) -> Press {
        if self.over_close() {
            self.pressed_close = true;
            return Press::Close;
        }
        match self.edge() {
            0 => Press::Move,
            code => Press::Size(code),
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_EVENT => {
                    while let Ok(ev) = self.rx.try_recv() {
                        self.on_event(ev);
                    }
                    Some(LRESULT(0))
                }
                WM_SHOW => {
                    self.set_visible(true);
                    Some(LRESULT(0))
                }
                WM_LBUTTONUP => {
                    if self.pressed_close && self.over_close() {
                        self.hide_to_tray();
                    }
                    self.pressed_close = false;
                    Some(LRESULT(0))
                }
                WM_SETCURSOR if loword(lp.0 as usize) == HTCLIENT as u32 => {
                    let cursor = match self.edge() {
                        1 | 2 => IDC_SIZEWE,
                        3 | 6 => IDC_SIZENS,
                        4 | 8 => IDC_SIZENWSE,
                        5 | 7 => IDC_SIZENESW,
                        _ => IDC_ARROW,
                    };
                    SetCursor(LoadCursorW(None, cursor).unwrap_or_default());
                    Some(LRESULT(1))
                }
                WM_MOUSEMOVE => {
                    if !self.tracking_mouse {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: self.hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        self.tracking_mouse = true;
                    }
                    let close = self.over_close();
                    if self.focus.set_hover(true, close, Instant::now()) {
                        self.dirty = true;
                    }
                    Some(LRESULT(0))
                }
                WM_MOUSELEAVE => {
                    self.tracking_mouse = false;
                    self.focus.set_hover(false, false, Instant::now());
                    self.dirty = true;
                    Some(LRESULT(0))
                }
                WM_SIZE => {
                    let (w, h) = (loword(lp.0 as usize), hiword(lp.0 as usize));
                    self.apply_size(w, h);
                    Some(LRESULT(0))
                }
                WM_DPICHANGED => {
                    self.dpi = hiword(wp.0).max(96);
                    log!("app: display scaling changed, dpi {}", self.dpi);
                    let r = &*(lp.0 as *const RECT);
                    // SetWindowPos sends WM_SIZE re-entrantly while the app is
                    // borrowed, so that message never reaches the handler
                    // above. Apply the new size explicitly once it returns.
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                    let mut rc = RECT::default();
                    let _ = GetClientRect(self.hwnd, &mut rc);
                    self.apply_size((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32);
                    Some(LRESULT(0))
                }
                WM_GETMINMAXINFO => {
                    let info = &mut *(lp.0 as *mut MINMAXINFO);
                    let s = self.scale();
                    info.ptMinTrackSize = POINT { x: (config::MIN_W as f32 * s) as i32, y: (config::MIN_H as f32 * s) as i32 };
                    info.ptMaxTrackSize = POINT { x: (config::MAX_W as f32 * s) as i32, y: (config::MAX_H as f32 * s) as i32 };
                    Some(LRESULT(0))
                }
                WM_ENTERSIZEMOVE => {
                    // Windows runs its own loop while moving/sizing; keep animating.
                    SetTimer(self.hwnd, TIMER_DRAG, 16, None);
                    Some(LRESULT(0))
                }
                WM_EXITSIZEMOVE => {
                    let _ = KillTimer(self.hwnd, TIMER_DRAG);
                    self.save_geometry();
                    Some(LRESULT(0))
                }
                WM_TIMER => {
                    match wp.0 {
                        TIMER_DRAG => {
                            if self.schedule().1 {
                                self.render();
                            }
                        }
                        TIMER_ART => {
                            let _ = KillTimer(self.hwnd, TIMER_ART);
                            if self.theme_key.is_empty() {
                                log!("art: none arrived within 3 s, using the neutral theme");
                                self.album_theme = None;
                                self.model.set_theme(NEUTRAL, Instant::now());
                                self.focus.cover = None;
                                self.focus.invalidate_plate();
                                self.dirty = true;
                            }
                        }
                        TIMER_RETRY => {
                            let _ = KillTimer(self.hwnd, TIMER_RETRY);
                            log!("lyrics: retrying after a failed lookup");
                            self.request_lyrics();
                        }
                        TIMER_SIM_HIDE => {
                            let _ = KillTimer(self.hwnd, TIMER_SIM_HIDE);
                            self.set_visible(false);
                        }
                        _ => {}
                    }
                    Some(LRESULT(0))
                }
                WM_COMMAND => {
                    match loword(wp.0) as usize {
                        tray::ID_TOGGLE => self.toggle(),
                        tray::ID_QUIT => {
                            log!("app: quit from the tray menu");
                            self.save_geometry();
                            self.save_settings();
                            self.tray.remove();
                            // Ask the loop to stop first: DestroyWindow sends
                            // WM_DESTROY re-entrantly, while the app is still
                            // borrowed here.
                            PostQuitMessage(0);
                            let _ = DestroyWindow(self.hwnd);
                        }
                        _ => {}
                    }
                    Some(LRESULT(0))
                }
                WM_CLOSE => {
                    self.hide_to_tray();
                    Some(LRESULT(0))
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }
    }

    /// Resize the drawing surface to a new client size (physical pixels).
    fn apply_size(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let dpi = self.dpi as f32;
        if let Some(gpu) = &mut self.gpu {
            if gpu.resize(w, h, dpi).is_err() {
                self.recreate_device();
            }
        }
        self.focus.invalidate_all();
        self.dirty = true;
        self.render(); // keep the content in step while resizing
    }

    fn request_lyrics(&self) {
        if self.simulate.is_some() {
            return; // the simulator delivers lyrics itself
        }
        let Some((artist, title, album, duration)) = self.track.clone() else { return };
        let key = self.current_key.clone();
        let provider = Arc::clone(&self.provider);
        let tx = self.tx.clone();
        let waker = HwndWaker(self.hwnd.0 as isize);
        std::thread::spawn(move || {
            let result = provider.fetch(&artist, &title, &album, duration);
            let _ = tx.send(Event::Lyrics { key, result });
            waker.wake();
        });
    }

    fn on_event(&mut self, ev: Event) {
        let now = Instant::now();
        match ev {
            Event::Playback(None) => {
                if self.model.status != Status::Idle || !self.current_key.is_empty() {
                    log!("app: nothing playing -> idle");
                    self.model.status = Status::Idle;
                    self.model.title.clear();
                    self.model.artist.clear();
                    self.model.lines.clear();
                    self.model.duration_ms = 0;
                    self.model.playing = false;
                    self.focus.cover = None;
                    self.cover_pixels = None;
                    self.focus.invalidate_plate();
                    self.focus.invalidate_stack();
                    self.dirty = true;
                }
                self.current_key.clear();
                self.theme_key.clear();
                self.track = None;
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_RETRY);
                }
                self.tray.set_playing(false);
            }
            Event::Playback(Some(p)) => {
                self.tray.set_playing(p.playing);
                let key = p.key();
                if key != self.current_key {
                    log!("app: new track {:?} by {:?}: showing loading, requesting lyrics", p.title, p.artist);
                    self.current_key = key.clone();
                    self.lyrics_failures = 0;
                    self.model.title = p.title.clone();
                    self.model.artist = p.artist.clone();
                    self.model.duration_ms = p.duration_ms;
                    self.model.lines.clear();
                    self.model.status = Status::Loading;
                    self.focus.invalidate_plate();
                    self.focus.invalidate_stack();
                    self.track = Some((p.artist.clone(), p.title.clone(), p.album.clone(), p.duration_ms));
                    self.theme_key.clear();
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_RETRY);
                        SetTimer(self.hwnd, TIMER_ART, 3000, None);
                    }
                    self.request_lyrics();
                } else if p.duration_ms != self.model.duration_ms {
                    self.model.duration_ms = p.duration_ms;
                }
                if let Some(bytes) = p.thumbnail.clone() {
                    if self.theme_key != key {
                        self.theme_key = key.clone();
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_ART);
                        }
                        // Always derive the album theme, so tinting can be
                        // switched on later without decoding the cover again.
                        let (tx, waker, sim) = (self.tx.clone(), HwndWaker(self.hwnd.0 as isize), self.simulate.is_some());
                        std::thread::spawn(move || {
                            // The simulator's one-byte thumbnail stands for a solid red cover.
                            let decoded =
                                if sim && bytes.len() <= 1 { Some(crate::art::solid([190, 40, 60], true)) } else { crate::art::decode(&bytes, true) };
                            let _ = tx.send(Event::Art { key, decoded });
                            waker.wake();
                        });
                    }
                }
                self.model.anchor_ms = p.position_now();
                self.model.anchor_at = now;
                self.model.playing = p.playing;
                self.dirty = true;
            }
            Event::Lyrics { key, result } => {
                if key != self.current_key {
                    log!("app: ignoring lyrics for a track that is no longer playing ({key:?})");
                    return;
                }
                // A failed lookup is retried: the first time quickly and
                // quietly (the loading dots stay up, since LRCLIB is often
                // just slow), after that every 15 s with the offline message.
                let mut retry_ms = 0;
                if result.source == "error" {
                    self.lyrics_failures += 1;
                    retry_ms = if self.lyrics_failures == 1 { 3_000 } else { 15_000 };
                    unsafe {
                        SetTimer(self.hwnd, TIMER_RETRY, retry_ms, None);
                    }
                }
                self.model.status = if result.lines.is_empty() {
                    match (result.source.as_str(), self.lyrics_failures) {
                        ("error", 1) => Status::Loading,
                        ("error", _) => Status::Offline,
                        _ => Status::NoLyrics,
                    }
                } else if !result.synced {
                    Status::Unsynced
                } else {
                    Status::Ready
                };
                log!(
                    "app: lyrics arrived ({} lines, source {}) -> showing {:?}{}",
                    result.lines.len(),
                    result.source,
                    self.model.status,
                    if retry_ms > 0 { format!(", failure {} for this track, retry in {} s", self.lyrics_failures, retry_ms / 1000) } else { String::new() }
                );
                self.model.lines = result.lines;
                self.focus.invalidate_stack();
                self.dirty = true;
            }
            Event::Art { key, decoded } => {
                if key != self.current_key {
                    return;
                }
                match &decoded {
                    Some(d) => log!(
                        "art: decoded; theme plate {:?} accent {:?}; tint {}",
                        d.theme.plate.map(|c| (c * 255.0).round() as u8),
                        d.theme.accent.map(|c| (c * 255.0).round() as u8),
                        if self.settings.theme_from_album { "on" } else { "off" }
                    ),
                    None => log!("art: could not be decoded, using the neutral theme"),
                }
                match decoded {
                    Some(d) => {
                        self.album_theme = Some(d.theme);
                        let theme = self.wanted_theme();
                        self.model.set_theme(theme, now);
                        if let Some(gpu) = &self.gpu {
                            self.focus.cover = focus::cover_bitmap(gpu, &d.cover).ok();
                        }
                        self.cover_pixels = Some(d.cover);
                    }
                    None => {
                        self.album_theme = None;
                        self.model.set_theme(NEUTRAL, now);
                        self.focus.cover = None;
                        self.cover_pixels = None;
                    }
                }
                self.focus.invalidate_plate();
                self.dirty = true;
            }
        }
    }
}

