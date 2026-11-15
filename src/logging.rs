//! Opt-in diagnostics log.
//!
//! Enabled by `--log` (writes `%USERPROFILE%\.lyrics-upfront\overlay.log`) or by
//! setting `LYRICS_UPFRONT_LOG` to a file path. The file is started fresh on
//! each launch. Each line: local time to the millisecond, thread, message.
//! When logging is off, `log!` costs one atomic load.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(flag: bool) {
    let path = std::env::var_os("LYRICS_UPFRONT_LOG")
        .map(std::path::PathBuf::from)
        .or_else(|| flag.then(|| crate::config::config_dir().join("overlay.log")));
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(file) = File::create(&path) {
        let _ = FILE.set(Mutex::new(file));
        ENABLED.store(true, Ordering::Relaxed);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn write(msg: &str) {
    let Some(file) = FILE.get() else { return };
    let t = unsafe { GetLocalTime() };
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("worker");
    if let Ok(mut f) = file.lock() {
        let _ = writeln!(f, "{:02}:{:02}:{:02}.{:03} [{}] {}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, name, msg);
        let _ = f.flush();
    }
}
