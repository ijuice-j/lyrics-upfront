//! Settings in ~/.lyrics-upfront/config.json. The keys are the ones the
//! earlier Python version used, so its file can be imported as is. Saving rewrites only the keys this app owns, so any other
//! keys in the file survive.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    home_dir().join(".lyrics-upfront")
}

/// Where the earlier version kept its settings and lyrics cache.
fn legacy_dir() -> PathBuf {
    home_dir().join(".lyric-overlay")
}

/// Whether this is the first run of Lyrics UpFront (its folder does not exist yet).
pub fn first_run() -> bool {
    !config_dir().exists()
}

/// On first run, copy the earlier version's settings and downloaded lyrics
/// into this app's folder, so position, preferences and cached lyrics carry
/// over. The old folder is left untouched. Returns how many files were copied.
pub fn import_legacy() -> usize {
    let (old, new) = (legacy_dir(), config_dir());
    if !old.is_dir() || new.exists() {
        return 0;
    }
    let mut copied = 0;
    let mut copy = |from: &Path, to: &Path| {
        if std::fs::copy(from, to).is_ok() {
            copied += 1;
        }
    };
    if std::fs::create_dir_all(new.join("cache")).is_err() {
        return 0;
    }
    copy(&old.join("config.json"), &new.join("config.json"));
    if let Ok(entries) = std::fs::read_dir(old.join("cache")) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                copy(&entry.path(), &new.join("cache").join(entry.file_name()));
            }
        }
    }
    copied
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: i32,
    pub h: i32,
    pub plate_core: f32,
    pub plate_edge: f32,
    pub text_scale: f32,
    pub theme_from_album: bool,
    pub always_on_top: bool,
    /// Mirrors the registry Run entry, which is the source of truth.
    pub start_with_windows: bool,
    /// Whether the one-time "still running in the tray" notice was shown.
    pub tray_hint_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            x: None,
            y: None,
            w: 600,
            h: 290,
            plate_core: 0.75,
            plate_edge: 0.64,
            text_scale: 1.0,
            theme_from_album: true,
            always_on_top: true,
            start_with_windows: false,
            tray_hint_shown: false,
        }
    }
}

impl Settings {
    /// Background opacity as the Settings slider shows it (10-100).
    pub fn opacity_percent(&self) -> i32 {
        (self.plate_core * 100.0).round() as i32
    }

    /// Set the background opacity. The edge stays proportionally thinner than
    /// the centre (as in the Python app), which keeps the card's vignette.
    pub fn set_opacity_percent(&mut self, percent: i32) {
        self.plate_core = (percent.clamp(10, 100) as f32) / 100.0;
        self.plate_edge = (self.plate_core * 0.85 * 1000.0).round() / 1000.0;
    }
}

pub const MIN_W: i32 = 340;
pub const MIN_H: i32 = 150;
pub const MAX_W: i32 = 1800;
pub const MAX_H: i32 = 900;

fn path() -> PathBuf {
    config_dir().join("config.json")
}

fn read_map() -> Map<String, Value> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub fn load() -> Settings {
    let m = read_map();
    let d = Settings::default();
    let int = |k: &str| m.get(k).and_then(Value::as_i64).map(|v| v as i32);
    let float = |k: &str, def: f32| m.get(k).and_then(Value::as_f64).map(|v| v as f32).unwrap_or(def);
    let boolean = |k: &str, def: bool| m.get(k).and_then(Value::as_bool).unwrap_or(def);
    Settings {
        x: int("x"),
        y: int("y"),
        w: int("w").unwrap_or(d.w).clamp(MIN_W, MAX_W),
        h: int("h").unwrap_or(d.h).clamp(MIN_H, MAX_H),
        plate_core: float("plate_core", d.plate_core).clamp(0.10, 1.0),
        plate_edge: float("plate_edge", d.plate_edge).clamp(0.05, 1.0),
        text_scale: float("text_scale", d.text_scale).clamp(0.6, 2.0),
        theme_from_album: boolean("theme_from_album", d.theme_from_album),
        always_on_top: boolean("always_on_top", d.always_on_top),
        start_with_windows: crate::startup::is_enabled(),
        tray_hint_shown: boolean("tray_hint_shown", d.tray_hint_shown),
    }
}

/// Save the Focus geometry (logical pixels, like the Qt app), atomically.
pub fn save_geometry(x: i32, y: i32, w: i32, h: i32) {
    let mut m = read_map();
    for (k, v) in [("x", x), ("y", y), ("w", w), ("h", h)] {
        m.insert(k.into(), Value::from(v));
    }
    write_map(m);
}

/// Save the user-facing settings; every other key in the file is kept.
pub fn save_settings(s: &Settings) {
    let mut m = read_map();
    let f = |v: f32| Value::from(((v as f64) * 1000.0).round() / 1000.0);
    m.insert("plate_core".into(), f(s.plate_core));
    m.insert("plate_edge".into(), f(s.plate_edge));
    m.insert("text_scale".into(), f(s.text_scale));
    m.insert("theme_from_album".into(), Value::from(s.theme_from_album));
    m.insert("always_on_top".into(), Value::from(s.always_on_top));
    m.insert("start_with_windows".into(), Value::from(s.start_with_windows));
    m.insert("tray_hint_shown".into(), Value::from(s.tray_hint_shown));
    write_map(m);
}

fn write_map(m: Map<String, Value>) {
    let _ = std::fs::create_dir_all(config_dir());
    let tmp = path().with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(m)) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}
