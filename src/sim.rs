//! Simulator for measurement: the same synthetic song the Python profiling
//! harness used (70 lines, one 16s break, red cover), fed through the same
//! Forwarder as the real SMTC source, every 120ms like the Python poll.
//!
//!   --simulate play | paused | idle | static | hidden
//!   --simulate demo[:COVER]   a short demo song with original lyrics, for
//!                             screenshots; COVER is an optional image file
//!   --simulate demo:TRACK.json  any song, for screenshots: {"title", "artist",
//!                             "album", "lrc": file, "cover": file,
//!                             "start_ms", "duration_ms"} (paths relative to
//!                             the JSON file)

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::lyrics::{build_lines, parse_lrc, LyricsResult};
use crate::source::{Event, Forwarder, Playback, Wake};

pub fn synthetic_lrc() -> (String, i64) {
    let words: Vec<&str> = "never gonna stop the feeling running through the night tonight \
                            we were young and fearless under city lights"
        .split_whitespace()
        .collect();
    let (lines, gap_at) = (70usize, 30usize);
    let mut out = Vec::new();
    let mut t = 8.0f64;
    let stamp = |t: f64| format!("[{:02}:{:05.2}]", (t / 60.0) as i64, t % 60.0);
    for i in 0..lines {
        if i == gap_at {
            out.push(format!("{}\u{266a}", stamp(t)));
            t += 16.0;
        }
        let n = 5 + (i * 7) % 5;
        let text: Vec<&str> = (0..n).map(|k| words[(i * 3 + k) % words.len()]).collect();
        out.push(format!("{}{}", stamp(t), text.join(" ")));
        t += 2.6 + (i % 3) as f64 * 0.4;
    }
    (out.join("\n"), ((t + 6.0) * 1000.0) as i64)
}

/// The demo song: original lyrics (written for this app, free to show in
/// screenshots), one line every 3.4 s from 0:04.
pub fn demo_lrc() -> (String, i64) {
    const LINES: [&str; 16] = [
        "We kept the radio low on the drive home",
        "Headlights folding into the dark",
        "You said the city hums when no one's listening",
        "So we listened, and it sang",
        "Paper satellites over the river",
        "Every window holding a light",
        "If the night forgets the words we gave it",
        "I'll sing them back up front",
        "Turn the corner where the streetlamps flicker",
        "Count the seconds in between",
        "Every chorus is a door left open",
        "Every verse a place to stay",
        "Paper satellites over the river",
        "Spinning slow in silver light",
        "If the morning takes the words we gave it",
        "We'll still know them by heart",
    ];
    let stamp = |t: f64| format!("[{:02}:{:05.2}]", (t / 60.0) as i64, t % 60.0);
    let lrc: Vec<String> = LINES.iter().enumerate().map(|(i, l)| format!("{}{l}", stamp(4.0 + i as f64 * 3.4))).collect();
    (lrc.join("
"), 200_000)
}

/// What the simulator plays.
struct Track {
    title: String,
    artist: String,
    album: String,
    lrc: String,
    duration_ms: i64,
    /// Playhead at launch.
    start_ms: i64,
    /// Cover image bytes; one byte stands for the simulator's solid red cover.
    cover: Vec<u8>,
    source: &'static str,
}

impl Track {
    fn synthetic() -> Self {
        let (lrc, duration_ms) = synthetic_lrc();
        Track {
            title: "Synthetic Song".into(),
            artist: "Profiler".into(),
            album: "Bench".into(),
            lrc,
            duration_ms,
            start_ms: 5000,
            cover: vec![0],
            source: "synthetic",
        }
    }

    /// "demo", "demo:<cover image>" or "demo:<track.json>".
    fn demo(arg: &str) -> Self {
        let file = arg.strip_prefix(':').map(std::path::Path::new);
        if let Some(track) = file.filter(|f| f.extension().map_or(false, |e| e == "json")).and_then(Self::from_json) {
            return track;
        }
        let (lrc, duration_ms) = demo_lrc();
        Track {
            title: "Paper Satellites".into(),
            artist: "Mira Holloway".into(),
            album: "Demo".into(),
            lrc,
            duration_ms,
            start_ms: 5000,
            cover: file.and_then(|f| std::fs::read(f).ok()).unwrap_or_else(|| vec![0]),
            source: "demo",
        }
    }

    fn from_json(path: &std::path::Path) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let dir = path.parent()?;
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let file = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(|f| std::fs::read(dir.join(f)).ok());
        Some(Track {
            title: text("title"),
            artist: text("artist"),
            album: text("album"),
            lrc: String::from_utf8(file("lrc")?).ok()?,
            duration_ms: v.get("duration_ms")?.as_i64()?,
            start_ms: v.get("start_ms").and_then(|x| x.as_i64()).unwrap_or(5000),
            cover: file("cover").unwrap_or_else(|| vec![0]),
            source: "demo",
        })
    }
}

pub fn spawn<W: Wake>(scenario: String, tx: Sender<Event>, waker: W) {
    std::thread::spawn(move || {
        let track = match scenario.strip_prefix("demo") {
            Some(arg) => Track::demo(arg),
            None => Track::synthetic(),
        };
        let started = Instant::now();
        let mut fwd = Forwarder::new(tx, waker);
        let mut polls = 0u32;
        let mut sent_lyrics = false;
        loop {
            std::thread::sleep(Duration::from_millis(120));
            match scenario.as_str() {
                "static" => continue,
                "idle" => fwd.push(None),
                _ => {
                    polls += 1;
                    let playing = scenario != "paused";
                    let pos = if playing { started.elapsed().as_millis() as i64 + track.start_ms } else { 42_000 };
                    let p = Playback {
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        album: track.album.clone(),
                        duration_ms: track.duration_ms,
                        playing,
                        position_ms: pos % track.duration_ms,
                        captured: Instant::now(),
                        // Art arrives on the second poll, as with real SMTC.
                        thumbnail: if polls == 2 { Some(track.cover.clone()) } else { None },
                    };
                    let key = p.key();
                    fwd.push(Some(p));
                    if !sent_lyrics {
                        sent_lyrics = true;
                        let result = LyricsResult {
                            lines: build_lines(&parse_lrc(&track.lrc)),
                            synced: true,
                            source: track.source.into(),
                            ..Default::default()
                        };
                        fwd.send(Event::Lyrics { key, result });
                    }
                }
            }
        }
    });
}
