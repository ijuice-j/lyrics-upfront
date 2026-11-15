//! Simulator for measurement: the same synthetic song the Python profiling
//! harness used (70 lines, one 16s break, red cover), fed through the same
//! Forwarder as the real SMTC source, every 120ms like the Python poll.
//!
//!   --simulate play | paused | idle | static | hidden
//!   --simulate demo[:COVER]   a short demo song with original lyrics, for
//!                             screenshots; COVER is an optional image file

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

pub fn spawn<W: Wake>(scenario: String, tx: Sender<Event>, waker: W) {
    std::thread::spawn(move || {
        // "demo" or "demo:<cover image>"
        let demo_cover = scenario.strip_prefix("demo").map(|rest| rest.strip_prefix(':').and_then(|f| std::fs::read(f).ok()));
        let demo = demo_cover.is_some();
        let (lrc, duration_ms) = if demo { demo_lrc() } else { synthetic_lrc() };
        // Without a file, a one-byte thumbnail stands for the simulator's solid red cover.
        let cover = demo_cover.flatten().unwrap_or_else(|| vec![0]);
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
                    let pos = if playing { started.elapsed().as_millis() as i64 + 5000 } else { 42_000 };
                    let (title, artist, album) =
                        if demo { ("Paper Satellites", "Mira Holloway", "Demo") } else { ("Synthetic Song", "Profiler", "Bench") };
                    let p = Playback {
                        title: title.into(),
                        artist: artist.into(),
                        album: album.into(),
                        duration_ms,
                        playing,
                        position_ms: pos % duration_ms,
                        captured: Instant::now(),
                        // Art arrives on the second poll, as with real SMTC.
                        thumbnail: if polls == 2 { Some(cover.clone()) } else { None },
                    };
                    let key = p.key();
                    fwd.push(Some(p));
                    if !sent_lyrics {
                        sent_lyrics = true;
                        let result = LyricsResult {
                            lines: build_lines(&parse_lrc(&lrc)),
                            synced: true,
                            source: if demo { "demo" } else { "synthetic" }.into(),
                            ..Default::default()
                        };
                        fwd.send(Event::Lyrics { key, result });
                    }
                }
            }
        }
    });
}
