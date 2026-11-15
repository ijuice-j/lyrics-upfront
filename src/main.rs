//! Lyrics UpFront: synced lyrics overlay for Spotify (Focus, whole-line
//! highlighting).
//!
//! Usage:
//!   lyrics-upfront.exe                      normal run (Windows media info)
//!   lyrics-upfront.exe --log                also write a diagnostics log to
//!                                           %USERPROFILE%\.lyrics-upfront\overlay.log
//!   lyrics-upfront.exe --simulate SCENARIO  measurement mode: play | paused |
//!                                           idle | static | hidden
//!   lyrics-upfront.exe --write-icon PATH    write the app icon as a .ico
//!                                           (used by the installer build)

#![windows_subsystem = "windows"]

/// Write a line to the diagnostics log, if logging is on (see logging.rs).
/// Arguments are only formatted when it is.
macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::logging::enabled() {
            $crate::logging::write(&format!($($arg)*));
        }
    };
}

mod app;
mod art;
mod config;
mod focus;
mod gfx;
mod http;
mod icon_art;
mod logging;
mod lyrics;
mod palette;
mod settings;
mod sim;
mod source;
mod startup;
mod text;
mod tray;

use windows::core::{HSTRING, w};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR};

/// Function form of `log!`, for call sites that already have a String.
pub fn log(msg: impl AsRef<str>) {
    if logging::enabled() {
        logging::write(msg.as_ref());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.iter().position(|a| a == "--write-icon").and_then(|i| args.get(i + 1)) {
        let ok = std::fs::write(path, icon_art::ICON).is_ok();
        std::process::exit(if ok { 0 } else { 1 });
    }
    let simulate = args.iter().position(|a| a == "--simulate").and_then(|i| args.get(i + 1).cloned());
    // Before logging, which would create the folder this checks for.
    let first_run = config::first_run();
    let imported = if first_run { config::import_legacy() } else { 0 };
    logging::init(args.iter().any(|a| a == "--log"));
    log!(
        "start: version {} pid {} exe {:?} args {:?}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::current_exe().ok(),
        &args[1..]
    );
    if first_run {
        log!("first run: imported {imported} file(s) from Spotify Lyrics Overlay's folder");
        if simulate.is_none() && startup::adopt_legacy() {
            log!("first run: moved the old app's start-with-Windows entry to this app");
        }
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if simulate.is_none() && !app::claim_single_instance() {
        log!("another copy is already running: asked it to show itself, exiting");
        return;
    }
    if let Err(e) = app::run(simulate) {
        log!("could not start: {e}");
        unsafe {
            MessageBoxW(None, &HSTRING::from(format!("Lyrics UpFront could not start:\n{e}")), w!("Lyrics UpFront"), MB_ICONERROR);
        }
    }
    log!("exit");
}
