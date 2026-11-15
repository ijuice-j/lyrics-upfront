//! Playback source: Windows media info (SMTC) on a background thread.
//!
//! The thread polls the local media API (microseconds per call) but only wakes
//! the UI when something the UI cares about changes: a new track, play/pause,
//! artwork arriving, or the position jumping (a seek). Between those, the UI
//! extrapolates the playhead itself, so a playing song costs the UI thread
//! nothing from this side -- the Python app sent the position 8 times a second.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::lyrics::LyricsResult;

pub const POLL: Duration = Duration::from_millis(150);
const ART_RETRY: Duration = Duration::from_secs(1);
const MAX_THUMBNAIL: u64 = 8 * 1024 * 1024;
/// A position this far from where the UI's own clock says it should be is a seek.
const SEEK_THRESHOLD_MS: i64 = 400;

#[derive(Clone, Debug)]
pub struct Playback {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub playing: bool,
    pub position_ms: i64,
    pub captured: Instant,
    pub thumbnail: Option<Vec<u8>>,
}

impl Playback {
    pub fn key(&self) -> String {
        format!("{}\u{241f}{}", self.artist, self.title)
    }

    pub fn position_now(&self) -> i64 {
        if !self.playing {
            return self.position_ms;
        }
        let pos = self.position_ms + self.captured.elapsed().as_millis() as i64;
        if self.duration_ms > 0 { pos.min(self.duration_ms) } else { pos }
    }
}

pub enum Event {
    Playback(Option<Playback>),
    Lyrics { key: String, result: LyricsResult },
    Art { key: String, decoded: Option<crate::art::Decoded> },
}

/// Something that can wake the UI thread after an event is queued.
pub trait Wake: Send + 'static {
    fn wake(&self);
}

/// Turns a stream of snapshots into the few events the UI needs.
pub struct Forwarder<W: Wake> {
    tx: Sender<Event>,
    waker: W,
    last: Option<Playback>,
    had_session: Option<bool>,
}

impl<W: Wake> Forwarder<W> {
    pub fn new(tx: Sender<Event>, waker: W) -> Self {
        Forwarder { tx, waker, last: None, had_session: None }
    }

    /// Queue any other event and wake the UI.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        self.waker.wake();
    }

    pub fn push(&mut self, snapshot: Option<Playback>) {
        let reason: Option<String> = match (&self.last, &snapshot) {
            (None, None) => (self.had_session != Some(false)).then(|| "nothing playing".into()),
            (Some(_), None) => Some("playback stopped / session gone".into()),
            (None, Some(_)) => Some("playback found".into()),
            (Some(prev), Some(now)) => {
                let drift = now.position_now() - prev.position_now();
                if now.key() != prev.key() {
                    Some("new track".into())
                } else if now.playing != prev.playing {
                    Some(if now.playing { "resumed".into() } else { "paused".into() })
                } else if now.thumbnail.is_some() {
                    Some("album art arrived".into())
                } else if (now.duration_ms - prev.duration_ms).abs() > 1000 {
                    Some(format!("duration changed {} -> {} ms", prev.duration_ms, now.duration_ms))
                } else if drift.abs() > SEEK_THRESHOLD_MS {
                    Some(format!("position jumped {drift:+} ms (seek)"))
                } else {
                    None
                }
            }
        };
        let changed = reason.is_some();
        if let Some(reason) = &reason {
            match &snapshot {
                Some(p) => log!(
                    "media: {reason}: {:?} by {:?} (album {:?}), {} at {} / {} ms{}",
                    p.title,
                    p.artist,
                    p.album,
                    if p.playing { "playing" } else { "paused" },
                    p.position_now(),
                    p.duration_ms,
                    p.thumbnail.as_ref().map(|t| format!(", art {} bytes", t.len())).unwrap_or_default()
                ),
                None => log!("media: {reason}"),
            }
        }
        self.had_session = Some(snapshot.is_some());
        if changed {
            let _ = self.tx.send(Event::Playback(snapshot.clone()));
            self.waker.wake();
            // Remember the snapshot that the UI's clock is now anchored to.
            self.last = snapshot.map(|mut p| {
                p.thumbnail = None;
                p
            });
        }
    }
}

fn now_filetime() -> i64 {
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

fn find_session(manager: &Manager) -> Option<Session> {
    let sessions = manager.GetSessions().ok()?;
    let mut first = None;
    for s in &sessions {
        let id = s.SourceAppUserModelId().map(|h| h.to_string_lossy().to_lowercase()).unwrap_or_default();
        if id.contains("spotify") {
            return Some(s);
        }
        if first.is_none() {
            first = Some(s);
        }
    }
    manager.GetCurrentSession().ok().or(first)
}

fn read_thumbnail(props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties) -> Option<Vec<u8>> {
    let reference = props.Thumbnail().ok()?;
    let stream = reference.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_THUMBNAIL {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    reader.LoadAsync(size as u32).ok()?.get().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some(buf)
}

fn read_session(session: &Session, want_art: bool) -> windows::core::Result<Playback> {
    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let timeline = session.GetTimelineProperties()?;
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;
    let mut position_ms = timeline.Position()?.Duration / 10_000;
    let duration_ms = timeline.EndTime()?.Duration / 10_000;
    if playing {
        // SMTC publishes a snapshot plus when it was taken; correct for its age.
        if let Ok(updated) = timeline.LastUpdatedTime() {
            let age_ms = (now_filetime() - updated.UniversalTime) / 10_000;
            if (0..=5000).contains(&age_ms) {
                position_ms += age_ms;
            }
        }
    }
    Ok(Playback {
        title: props.Title().map(|s| s.to_string_lossy()).unwrap_or_default(),
        artist: props.Artist().map(|s| s.to_string_lossy()).unwrap_or_default(),
        album: props.AlbumTitle().map(|s| s.to_string_lossy()).unwrap_or_default(),
        duration_ms,
        playing,
        position_ms,
        captured: Instant::now(),
        thumbnail: if want_art { read_thumbnail(&props) } else { None },
    })
}

pub fn spawn_smtc<W: Wake>(tx: Sender<Event>, waker: W) {
    std::thread::Builder::new()
        .name("smtc".into())
        .spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            let mut fwd = Forwarder::new(tx, waker);
            let mut manager: Option<Manager> = None;
            let (mut seen_key, mut art_key) = (String::new(), String::new());
            let mut last_art_try = Instant::now() - ART_RETRY;
            let mut last_app = String::new();
            let mut last_error = String::new();
            loop {
                if manager.is_none() {
                    manager = match Manager::RequestAsync().and_then(|op| op.get()) {
                        Ok(m) => {
                            log!("media: connected to Windows media sessions");
                            Some(m)
                        }
                        Err(e) => {
                            let msg = e.message();
                            if msg != last_error {
                                log!("media: cannot open Windows media sessions: {msg}");
                                last_error = msg;
                            }
                            None
                        }
                    };
                }
                let snapshot = manager.as_ref().and_then(find_session).and_then(|s| {
                    let app = s.SourceAppUserModelId().map(|h| h.to_string_lossy()).unwrap_or_default();
                    if app != last_app {
                        log!("media: using session from {app:?}");
                        last_app = app;
                    }
                    let want_art = art_key != seen_key && last_art_try.elapsed() >= ART_RETRY;
                    if want_art {
                        last_art_try = Instant::now();
                    }
                    match read_session(&s, want_art) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            let msg = e.message();
                            if msg != last_error {
                                log!("media: reading the session failed: {msg}");
                                last_error = msg;
                            }
                            None
                        }
                    }
                });
                let snapshot = snapshot.filter(|p| !p.title.is_empty());
                if let Some(p) = &snapshot {
                    if p.key() != seen_key {
                        // New track: forget art; ask for this one's next poll.
                        seen_key = p.key();
                        art_key.clear();
                        last_art_try = Instant::now() - ART_RETRY;
                    } else if p.thumbnail.is_some() {
                        art_key = p.key();
                    }
                }
                if manager.is_some() && snapshot.is_none() {
                    // Possibly a dropped manager (Spotify restarted); re-acquire.
                    manager = None;
                }
                fwd.push(snapshot);
                std::thread::sleep(POLL);
            }
        })
        .expect("spawn smtc thread");
}
