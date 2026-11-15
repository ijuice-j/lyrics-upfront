//! Lyrics: LRCLIB lookup, LRC parsing, derived word timing and the disk cache.
//!
//! A line-for-line port of the earlier version's app/lyrics.py, including its fixes (instrumental
//! flag kept through the cache, transient failures never cached, misses expire,
//! one-digit LRC fractions, artist-normalised cache keys). The cache keeps the
//! Python app's file names and JSON layout, so the lyrics it had already
//! downloaded are imported on first run (see config::import_legacy).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{Client, HttpError};

const LRCLIB_HOST: &str = "lrclib.net";
const USER_AGENT: &str = concat!("lyrics-upfront/", env!("CARGO_PKG_VERSION"), " (https://github.com/ijuice-j/lyrics-upfront)");

const WORD_FLAT_WEIGHT: f64 = 2.2;
const DEFAULT_MS_PER_CHAR: f64 = 155.0;
const GAP_RATIO: f64 = 2.1;
const GAP_SWEEP_SCALE: f64 = 1.25;
const MIN_MS_PER_CHAR: f64 = 60.0;
const MAX_MS_PER_CHAR: f64 = 420.0;
const FILL_RATIO: f64 = 0.96;
const SWEEP_TAIL_MS: i64 = 40;
const LAST_LINE_MS: i64 = 4000;
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DURATION_BUCKET_MS: i64 = 5000;
const MEMORY_ENTRIES: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub words: Vec<Word>,
    #[serde(default)]
    pub instrumental: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LyricsResult {
    pub lines: Vec<Line>,
    pub synced: bool,
    /// "lrclib:get", "lrclib:search", "cache", "none", or "error" when LRCLIB
    /// could not be reached (shown, never cached, retried by the caller).
    pub source: String,
    pub track: String,
    pub artist: String,
}

impl LyricsResult {
    fn empty(source: &str) -> Self {
        LyricsResult { source: source.into(), ..Default::default() }
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    lines: Vec<Line>,
    #[serde(default)]
    synced: bool,
    #[serde(default = "cache_source")]
    source: String,
    #[serde(default)]
    track: String,
    #[serde(default)]
    artist: String,
}

fn cache_source() -> String {
    "cache".into()
}

/// LRCLIB could not be reached or answered 429/5xx: says nothing about the track.
#[derive(Debug)]
struct Transient;

// ---------------------------------------------------------------- parsing --

fn timestamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[(\d+):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap())
}

/// LRC text to (timestamp_ms, text), sorted by time; handles repeated stamps.
pub fn parse_lrc(raw: &str) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for raw_line in raw.lines() {
        let stamps: Vec<_> = timestamp_re().captures_iter(raw_line).collect();
        let Some(last) = stamps.last() else { continue };
        let text = raw_line[last.get(0).unwrap().end()..].trim().to_string();
        for c in &stamps {
            let minutes: i64 = c[1].parse().unwrap_or(0);
            let seconds: i64 = c[2].parse().unwrap_or(0);
            let mut ms = minutes * 60_000 + seconds * 1000;
            if let Some(frac) = c.get(3) {
                // Tenths, centiseconds or milliseconds: pad right to 3 digits.
                let padded = format!("{:0<3}", frac.as_str());
                ms += padded.parse::<i64>().unwrap_or(0);
            }
            out.push((ms, text.clone()));
        }
    }
    out.sort_by_key(|p| p.0); // stable, like Python's sort
    out
}

fn is_instrumental(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| "♪♫♬♩ -~. ".contains(c))
}

fn split_words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Unicode punctuation (P*) or separator (Z*), approximated without tables.
/// ASCII is exact; symbols such as $ + < = > ^ ` | ~ are S*, so they count.
fn is_punct_or_sep(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    if c.is_ascii() {
        return "!\"#%&'()*,-./:;?@[\\]_{}".contains(c);
    }
    matches!(c as u32,
        0x00A1 | 0x00A7 | 0x00AB | 0x00B6 | 0x00B7 | 0x00BB | 0x00BF
        | 0x2010..=0x2027 | 0x2030..=0x2043 | 0x2045..=0x2051 | 0x2053..=0x205E
        | 0x3001..=0x3003 | 0x3008..=0x3011 | 0x3014..=0x301F | 0x30FB
        | 0xFE10..=0xFE19 | 0xFE30..=0xFE4F | 0xFF01..=0xFF03 | 0xFF05..=0xFF0A
        | 0xFF0C..=0xFF0F | 0xFF1A | 0xFF1B | 0xFF1F | 0xFF20 | 0xFF3B..=0xFF3D
        | 0xFF3F | 0xFF5B | 0xFF5D | 0xFF5F..=0xFF65)
}

fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

fn visible_length(word: &str) -> usize {
    word.chars().filter(|&c| !is_combining(c) && !is_punct_or_sep(c)).count().max(1)
}

fn median_rate(pairs: &[(i64, String)]) -> f64 {
    let mut rates = Vec::new();
    for (i, (start, text)) in pairs.iter().enumerate() {
        if i + 1 >= pairs.len() || text.is_empty() || is_instrumental(text) {
            continue;
        }
        let window = pairs[i + 1].0 - start;
        let chars: usize = split_words(text).iter().map(|w| visible_length(w)).sum();
        if window > 0 && chars > 0 {
            rates.push(window as f64 / chars as f64);
        }
    }
    if rates.len() < 6 {
        return DEFAULT_MS_PER_CHAR;
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    rates[rates.len() / 2].clamp(MIN_MS_PER_CHAR, MAX_MS_PER_CHAR)
}

fn time_words(text: &str, start: i64, end: i64, ms_per_char: f64) -> Vec<Word> {
    let words = split_words(text);
    if words.is_empty() {
        return vec![];
    }
    let available = (end - start - SWEEP_TAIL_MS).max(0);
    if available <= 0 {
        return words.iter().map(|w| Word { text: w.to_string(), start_ms: start, end_ms: start }).collect();
    }
    let total_chars: usize = words.iter().map(|w| visible_length(w)).sum();
    let expected = total_chars as f64 * ms_per_char;
    let available = available as f64;
    let sweep = if available <= expected * GAP_RATIO {
        available * FILL_RATIO
    } else {
        (available * FILL_RATIO).min(expected * GAP_SWEEP_SCALE)
    };
    let sweep = sweep as i64; // int() truncation, as in Python

    let weights: Vec<f64> = words.iter().map(|w| visible_length(w) as f64 + WORD_FLAT_WEIGHT).collect();
    let total_weight: f64 = weights.iter().sum();
    let mut cursor = start as f64;
    let mut out: Vec<Word> = words
        .iter()
        .zip(&weights)
        .map(|(w, weight)| {
            let span = sweep as f64 * (weight / total_weight);
            let word = Word { text: w.to_string(), start_ms: cursor as i64, end_ms: (cursor + span) as i64 };
            cursor += span;
            word
        })
        .collect();
    if let Some(last) = out.last_mut() {
        last.end_ms = start + sweep;
    }
    out
}

pub fn build_lines(pairs: &[(i64, String)]) -> Vec<Line> {
    let rate = median_rate(pairs);
    let mut lines = Vec::new();
    for (i, (start, text)) in pairs.iter().enumerate() {
        let end = if i + 1 < pairs.len() { pairs[i + 1].0 } else { start + LAST_LINE_MS };
        if text.is_empty() {
            continue;
        }
        if is_instrumental(text) {
            lines.push(Line { start_ms: *start, end_ms: end, text: String::new(), words: vec![], instrumental: true });
            continue;
        }
        lines.push(Line {
            start_ms: *start,
            end_ms: end,
            text: text.clone(),
            words: time_words(text, *start, end, rate),
            instrumental: false,
        });
    }
    lines
}

// --------------------------------------------------- title normalisation --

fn noise_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\s*[\(\[][^)\]]*(remaster|remastered|feat\.?|featuring|with|version|edit|mono|stereo|deluxe|bonus|live|radio|explicit)[^)\]]*[\)\]]",
        )
        .unwrap()
    })
}

fn dash_noise_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\s+-\s+.*(remaster|remastered|version|edit|mono|stereo|live|radio|mix)\b.*$").unwrap()
    })
}

pub fn clean_title(title: &str) -> String {
    let cleaned = noise_re().replace_all(title, "");
    let cleaned = dash_noise_re().replace_all(&cleaned, "");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() { title.trim().to_string() } else { cleaned.to_string() }
}

pub fn primary_artist(artist: &str) -> String {
    let mut a = artist.to_string();
    for sep in [",", ";", " & ", " feat. ", " featuring ", " x ", " X "] {
        if a.contains(sep) {
            a = a.split(sep).next().unwrap_or("").to_string();
        }
    }
    a.trim().to_string()
}

// ----------------------------------------------------------------- provider --

pub struct Provider {
    cache_dir: PathBuf,
    client: Client,
    memory: Mutex<VecDeque<(String, LyricsResult)>>,
}

impl Provider {
    pub fn new(cache_dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&cache_dir);
        // LRCLIB is sometimes slow to answer (4-6 s seen for some tracks, and
        // over 8 s now and then), so allow 20 s for the reply. Reaching the
        // server stays short, so an unreachable server still fails in about
        // 8 s (measured; the system's proxy detection adds to the 5 s).
        let timeouts = crate::http::Timeouts { resolve_ms: 5_000, connect_ms: 5_000, send_ms: 5_000, receive_ms: 20_000 };
        Arc::new(Provider { cache_dir, client: Client::new(USER_AGENT, timeouts), memory: Mutex::new(VecDeque::new()) })
    }

    /// Cache keys: [key to write, neighbouring-bucket key to also read].
    pub fn cache_keys(artist: &str, title: &str, duration_ms: i64) -> [String; 2] {
        let base = format!("{}|{}|", primary_artist(artist).to_lowercase(), title.to_lowercase());
        let d = duration_ms.max(0);
        let (bucket, offset) = (d / DURATION_BUCKET_MS, d % DURATION_BUCKET_MS);
        let neighbour = if offset >= DURATION_BUCKET_MS / 2 { bucket + 1 } else { bucket - 1 };
        [format!("{base}{bucket}"), format!("{base}{neighbour}")]
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        let digest = sha1_smol::Sha1::from(key.as_bytes()).digest().to_string();
        self.cache_dir.join(format!("{}.json", &digest[..20]))
    }

    fn read_cache(path: &Path) -> Option<LyricsResult> {
        let text = std::fs::read_to_string(path).ok()?;
        let file: CacheFile = serde_json::from_str(&text).ok()?;
        if file.lines.is_empty() {
            let age = std::fs::metadata(path).ok()?.modified().ok()?;
            if SystemTime::now().duration_since(age).unwrap_or_default() > NEGATIVE_CACHE_TTL {
                return None; // stale miss: look again
            }
        }
        Some(LyricsResult { lines: file.lines, synced: file.synced, source: file.source, track: file.track, artist: file.artist })
    }

    fn write_cache(path: &Path, r: &LyricsResult) {
        let file = CacheFile {
            lines: r.lines.clone(),
            synced: r.synced,
            source: r.source.clone(),
            track: r.track.clone(),
            artist: r.artist.clone(),
        };
        if let Ok(json) = serde_json::to_string(&file) {
            let _ = std::fs::write(path, json);
        }
    }

    fn remember(&self, key: String, r: LyricsResult) {
        let mut m = self.memory.lock().unwrap();
        m.retain(|(k, _)| *k != key);
        m.push_back((key, r));
        while m.len() > MEMORY_ENTRIES {
            m.pop_front();
        }
    }

    pub fn fetch(self: &Arc<Self>, artist: &str, title: &str, album: &str, duration_ms: i64) -> LyricsResult {
        let keys = Self::cache_keys(artist, title, duration_ms);
        let started = std::time::Instant::now();
        log!("lyrics: fetch {artist:?} / {title:?} / album {album:?} / {duration_ms} ms");
        {
            let m = self.memory.lock().unwrap();
            for key in &keys {
                if let Some((_, r)) = m.iter().find(|(k, _)| k == key) {
                    log!("lyrics: memory cache hit ({})", summary(r));
                    return r.clone();
                }
            }
        }
        for key in &keys {
            let path = self.cache_path(key);
            if let Some(r) = Self::read_cache(&path) {
                log!("lyrics: disk cache hit {:?} ({})", path.file_name().unwrap_or_default(), summary(&r));
                self.remember(keys[0].clone(), r.clone());
                return r;
            }
        }
        log!("lyrics: not cached, asking LRCLIB");
        let (result, definitive) = self.fetch_remote(artist, title, album, duration_ms);
        log!(
            "lyrics: done in {} ms: {} ({})",
            started.elapsed().as_millis(),
            summary(&result),
            if definitive { "cached" } else { "not cached: a lookup failed, will retry" }
        );
        if definitive {
            Self::write_cache(&self.cache_path(&keys[0]), &result);
            self.remember(keys[0].clone(), result.clone());
        }
        result
    }

    /// All strategies at once, picked by preference (exact, cleaned exact, search).
    fn fetch_remote(self: &Arc<Self>, artist: &str, title: &str, album: &str, duration_ms: i64) -> (LyricsResult, bool) {
        let duration_s = if duration_ms > 0 { (duration_ms as f64 / 1000.0).round() as i64 } else { 0 };
        let clean = clean_title(title);
        let primary = primary_artist(artist);

        #[derive(Clone)]
        enum Strategy {
            Get(String, String),
            Search(String, String),
        }
        let mut strategies = vec![Strategy::Get(artist.into(), title.into())];
        if (clean.as_str(), primary.as_str()) != (title, artist) {
            strategies.push(Strategy::Get(primary.clone(), clean.clone()));
        }
        strategies.push(Strategy::Search(primary, clean));

        let label = |s: &Strategy| match s {
            Strategy::Get(a, t) => format!("exact get ({a:?} / {t:?})"),
            Strategy::Search(a, t) => format!("search ({a:?} / {t:?})"),
        };
        let labels: Vec<String> = strategies.iter().map(label).collect();
        for (i, l) in labels.iter().enumerate() {
            log!("lyrics: strategy {} = {l}", i + 1);
        }

        let (tx, rx) = mpsc::channel();
        for (i, s) in strategies.iter().cloned().enumerate() {
            let tx = tx.clone();
            let me = Arc::clone(self);
            let album = album.to_string();
            std::thread::Builder::new()
                .name(format!("lyrics-{}", i + 1))
                .spawn(move || {
                    let started = std::time::Instant::now();
                    let r = match s {
                        Strategy::Get(a, t) => me.api_get(&a, &t, &album, duration_s),
                        Strategy::Search(a, t) => me.api_search(&a, &t, duration_s).map(Some),
                    };
                    let _ = tx.send((i, r, started.elapsed().as_millis()));
                })
                .expect("spawn lyrics strategy");
        }
        drop(tx);

        let n = strategies.len();
        let mut results: Vec<Option<LyricsResult>> = vec![None; n];
        let mut pending: Vec<bool> = vec![true; n];
        let mut transient = false;
        for (i, r, ms) in rx {
            pending[i] = false;
            match r {
                Ok(v) => {
                    log!(
                        "lyrics: strategy {} answered in {ms} ms: {}",
                        i + 1,
                        v.as_ref().map(summary).unwrap_or_else(|| "no match".into())
                    );
                    results[i] = v;
                }
                Err(Transient) => {
                    log!("lyrics: strategy {} FAILED in {ms} ms (network or server error)", i + 1);
                    transient = true;
                }
            }
            // Return as soon as no better-ranked strategy could still win.
            for j in 0..n {
                if pending[j] {
                    break;
                }
                if let Some(r) = &results[j] {
                    if r.synced && !r.lines.is_empty() {
                        log!("lyrics: using strategy {} ({})", j + 1, labels[j]);
                        return (r.clone(), true);
                    }
                }
            }
        }
        for r in results.iter().flatten() {
            if !r.lines.is_empty() {
                return (r.clone(), !transient);
            }
        }
        if transient {
            (LyricsResult::empty("error"), false)
        } else {
            (LyricsResult::empty("none"), true)
        }
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<crate::http::Response, Transient> {
        // Test hook: LYRICS_UPFRONT_LRCLIB_HOST points lookups elsewhere (e.g.
        // an unreachable address, to exercise the offline and retry path).
        let host = std::env::var("LYRICS_UPFRONT_LRCLIB_HOST").unwrap_or_else(|_| LRCLIB_HOST.to_string());
        match self.client.get(&host, path, query) {
            Err(HttpError::Transport(_)) => Err(Transient), // logged by http.rs
            Ok(r) if r.status == 429 || r.status >= 500 => {
                log!("lyrics: LRCLIB answered HTTP {} (rate limited or server error)", r.status);
                Err(Transient)
            }
            Ok(r) => Ok(r),
        }
    }

    fn api_get(&self, artist: &str, title: &str, album: &str, duration_s: i64) -> Result<Option<LyricsResult>, Transient> {
        let mut q = vec![("artist_name", artist.to_string()), ("track_name", title.to_string())];
        if !album.is_empty() {
            q.push(("album_name", album.to_string()));
        }
        if duration_s > 0 {
            q.push(("duration", duration_s.to_string()));
        }
        let r = self.get("/api/get", &q)?;
        if r.status != 200 {
            return Ok(None);
        }
        Ok(serde_json::from_slice::<Value>(&r.body).ok().map(|v| to_result(&v, "lrclib:get")))
    }

    fn api_search(&self, artist: &str, title: &str, duration_s: i64) -> Result<LyricsResult, Transient> {
        let r = self.get("/api/search", &[("track_name", title.to_string()), ("artist_name", artist.to_string())])?;
        let candidates: Vec<Value> = if r.status == 200 {
            serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
        } else {
            vec![]
        };
        let has = |c: &Value, k: &str| c.get(k).and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty());
        let mut synced: Vec<&Value> = candidates.iter().filter(|c| has(c, "syncedLyrics")).collect();
        if synced.is_empty() {
            return Ok(candidates
                .iter()
                .find(|c| has(c, "plainLyrics"))
                .map(|c| to_result(c, "lrclib:search"))
                .unwrap_or_else(|| LyricsResult::empty("none")));
        }
        if duration_s > 0 {
            let dur = |c: &Value| c.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            synced.sort_by(|a, b| {
                let da = (dur(a) - duration_s as f64).abs();
                let db = (dur(b) - duration_s as f64).abs();
                da.partial_cmp(&db).unwrap()
            });
        }
        Ok(to_result(synced[0], "lrclib:search"))
    }
}

/// One-line description of a result, for the log.
fn summary(r: &LyricsResult) -> String {
    if r.lines.is_empty() {
        return format!("no lyrics, source {}", r.source);
    }
    format!(
        "{} lines, {}, source {}, matched {:?} / {:?}",
        r.lines.len(),
        if r.synced { "synced" } else { "unsynced" },
        r.source,
        r.artist,
        r.track
    )
}

fn to_result(payload: &Value, source: &str) -> LyricsResult {
    let s = |k: &str| payload.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (track, artist) = (s("trackName"), s("artistName"));
    let synced = s("syncedLyrics");
    if !synced.is_empty() {
        let lines = build_lines(&parse_lrc(&synced));
        if !lines.is_empty() {
            return LyricsResult { lines, synced: true, source: source.into(), track, artist };
        }
    }
    let plain = s("plainLyrics");
    if !plain.is_empty() {
        let lines = plain
            .lines()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| Line { start_ms: 0, end_ms: 0, text: t.into(), words: vec![], instrumental: false })
            .collect();
        return LyricsResult { lines, synced: false, source: source.into(), track, artist };
    }
    LyricsResult { source: "none".into(), track, artist, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions() {
        for (s, want) in [("[00:12.5]", 12500), ("[00:12.50]", 12500), ("[00:12.500]", 12500),
                          ("[00:12.05]", 12050), ("[01:02.123]", 62123), ("[00:12]", 12000)] {
            assert_eq!(parse_lrc(&format!("{s}x"))[0].0, want, "{s}");
        }
    }

    #[test]
    fn instrumental_survives_cache() {
        let lines = build_lines(&parse_lrc("[00:01.00]hello there friend\n[00:04.00]♪\n[00:20.00]back again now"));
        assert_eq!(lines.iter().map(|l| l.instrumental).collect::<Vec<_>>(), [false, true, false]);
        let dir = std::env::temp_dir().join(format!("lo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.json");
        Provider::write_cache(&p, &LyricsResult { lines: lines.clone(), synced: true, source: "t".into(), ..Default::default() });
        assert_eq!(Provider::read_cache(&p).unwrap().lines, lines);
    }

    #[test]
    fn matches_python_timing() {
        // Expected values produced by app/lyrics.py (_build_lines) for the same input.
        let lrc = "[00:01.00]hello there friend\n[00:04.00]♪\n[00:20.00]back again now, isn\u{2019}t it?\n\
                   [00:22.50]a\n[00:23.00]x y z\n[00:30.00]one two three four five six\n\
                   [00:33.00]seven eight\n[00:35.00]nine ten eleven\n[00:38.00]twelve";
        let expected: &[(i64, i64, bool, &[(i64, i64)])] = &[
            (1000, 4000, false, &[(1000, 1905), (1905, 2810), (2810, 3841)]),
            (4000, 20000, true, &[]),
            (20000, 22500, false, &[(20000, 20504), (20504, 21090), (21090, 21514), (21514, 22019), (22019, 22361)]),
            (22500, 23000, false, &[(22500, 22750)]),
            (23000, 30000, false, &[(23000, 23250), (23250, 23500), (23500, 23750)]),
            (30000, 33000, false, &[(30000, 30419), (30419, 30839), (30839, 31420), (31420, 31920), (31920, 32421), (32421, 32841)]),
            (33000, 35000, false, &[(33000, 33940), (33940, 34881)]),
            (35000, 38000, false, &[(35000, 35898), (35898, 36652), (36652, 37841)]),
            (38000, 42000, false, &[(38000, 39500)]),
        ];
        let lines = build_lines(&parse_lrc(lrc));
        assert_eq!(lines.len(), expected.len());
        for (line, (s, e, inst, words)) in lines.iter().zip(expected) {
            assert_eq!((line.start_ms, line.end_ms, line.instrumental), (*s, *e, *inst));
            let got: Vec<(i64, i64)> = line.words.iter().map(|w| (w.start_ms, w.end_ms)).collect();
            assert_eq!(got, words.to_vec(), "line at {s}");
        }
    }

    #[test]
    fn instrumental_markers_by_code_point() {
        // Written as escapes so a source-encoding accident cannot hide a regression.
        for marker in ["\u{266a}", "\u{266b}", "\u{266c}", "\u{2669}", "\u{266a} \u{266a}"] {
            assert!(is_instrumental(marker), "{marker:?}");
        }
        assert!(!is_instrumental("la la"));
    }

    #[test]
    fn keys_and_titles() {
        assert_eq!(primary_artist("Artist, Guest"), "Artist");
        assert_eq!(clean_title("Get Lucky (feat. Pharrell Williams) - Radio Edit"), "Get Lucky");
        let a = Provider::cache_keys("Artist", "Song", 204_999);
        let b = Provider::cache_keys("Artist, Guest", "Song", 205_001);
        assert!(a.iter().any(|k| b.contains(k)));
    }
}
