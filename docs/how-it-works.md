# How Lyrics UpFront works

Lyrics UpFront is the native rebuild of
[Spotify Lyrics Overlay](https://github.com/ijuice-j/spotify-lyrics-overlay), a Python, Qt and Chromium app, without any
of the three. It reads the same media
info and fetches from the same lyrics source. On first run it copies the old
app's settings and lyrics cache from `~/.lyric-overlay` into its own folder,
`~/.lyrics-upfront`, and moves the old start-with-Windows entry to itself.

It has one design: **Focus, whole line** (Spotify style). The card is the
Focus card, with the lines around the sung one blurred and faded by distance,
but the sung line lights up whole when it starts instead of being swept word
by word. Line timestamps are what the lyrics service actually provides, so
this design is never early or late; the word sweep was an estimate.

Everything heavy comes from Windows itself: Direct2D and DirectWrite for
drawing and text, DirectComposition for the transparent window, Windows
Imaging Component for album art, WinHTTP for HTTPS, and the media-session API
for what's playing. The crates used are small pure-Rust ones (`serde`,
`serde_json`, `regex-lite`, `sha1_smol`) plus Microsoft's `windows` bindings.

## Measured against the current app

Same machine (i7-9750H, 12 threads), same measurement script, same synthetic
song for the controlled rows. CPU is % of one core.

| Scenario | Python + Qt + Chromium (Focus, word sweep) | Rust (GPU) | Rust (`LYRICS_UPFRONT_WARP=1`) |
|---|---|---|---|
| Playing, CPU | 67–70% | **0.5–0.8%** | 1.2–2.4% |
| Playing, GPU | ~5% | **0.4%** | |
| Playing, private memory | ~290 MB | **71 MB** | 28 MB |
| Paused, CPU | 15% | 0.1% | 0.04% |
| Frames drawn while playing | 60–144 a second, always | ~44 a second, only around line changes | |
| Processes | 3 | 1 | 1 |
| Start to visible window | ~1.2 s | ~0.3 s | |
| Download | 144–218 MB | **0.8 MB exe** | |

Playing CPU is a range because single runs at this level vary with whatever
else the machine is doing; the GPU and frame counts are steadier signals.
Memory was flat across 2.5-minute runs in both modes. About 40 MB of the GPU
build's private memory is the graphics driver's own allocation for the
Direct3D device; `LYRICS_UPFRONT_WARP=1` draws with Windows' software
rasterizer instead, which removes that at a small CPU cost.

The earlier word-sweep build of this prototype measured 0.35–1.3% CPU and
~5% GPU; whole-line draws far fewer frames, so the clearest saving is GPU.

Absolute CPU on this laptop also drifts with the machine's state: later the
same night, the whole-line build measured 8–9% with most of the extra in the
graphics driver's thread. Comparisons should therefore be made side by side
in one session; measured that way, the title hairline (8.7%) costs the same
as the bottom-edge bar it replaced (9.2%).

## Why it is cheaper

- **Cached layers.** The tinted card, header and the blurred neighbour lines
  change only on a line, track, theme or size change, so they are drawn once
  into bitmaps. The sung line and the one before it are each pre-rendered
  sharp and blurred, so the 720ms roll is a crossfade of bitmaps rather than
  a live blur. A frame is a handful of bitmap copies plus the clock and bar.
- **Frames only when something moves.** The app draws at vsync only during
  the roll and the colour crossfade. Otherwise it sleeps until the next
  visible change: the next line, the clock ticking over, or the progress bar
  moving a pixel. Deadlines are measured from the last frame drawn, so a
  wake-up that lands a few ms late (the system timer is coarse) draws at once
  instead of skipping a change. Paused, idle or hidden, the UI thread does no
  periodic work at all.
- **Changes only.** The media thread wakes the UI only for a new track,
  play/pause, artwork, or a seek; the UI extrapolates the playhead itself.
  The tray icon is loaded once per state and re-sent only on play/pause.
- **No readback.** DirectComposition composites the swap chain directly, so
  frames are never copied back from the GPU to blend the transparent window.

## What is in it

- Focus card, whole-line highlighting: album-tinted plate (radial gradient,
  grain, vignette, hairline), header with cover, title, artist and clock,
  five lyric slots with the neighbours blurred (2.4px/1.2px) and faded,
  top/bottom fade, the sung line lit whole, a 720ms roll on line change
  (the stack rises, the arriving line pulls into focus, the departing one
  softens), 900ms colour crossfade, close button on hover,
  idle/loading/no-lyrics/offline/unsynced states.
- Progress: a 2px hairline in the album colour under the title row, across
  the content width, with the clock showing elapsed and total time
  ("0:45 / 3:26"). It redraws once per physical pixel of travel, about 2.6
  times a second on a typical song.
- Media info with Spotify preferred, snapshot-age correction, art retries.
- LRCLIB lookup with the three strategies in parallel and preference order,
  memory + disk cache in the same format as the Python app,
  transient failures never cached and retried every 15s. Word timing is
  still derived and stored (it matches `app/lyrics.py` exactly; see the
  tests) so the cache format stays the same as the Python app's.
- Colour theme from the cover (port of `app/palette.py`).
- Drag anywhere, resize from any edge (Windows' own move/size loops),
  per-monitor DPI, position saved to the config, tray icon (the app's
  name as its tooltip) with show/hide and quit, close hides to tray, single instance.

- Settings window from the tray menu (standard Windows controls): background
  opacity, text size, tint from the album cover, keep above other windows,
  start automatically at sign-in. Changes apply live and are saved to the
  `~/.lyrics-upfront/config.json` when the window closes.
- Start with Windows through the per-user Run value `LyricsUpFront`, the same
  value the installer writes.
- A one-time "still running" notice the first time the card is closed to
  the tray; the card's 560ms fade-in at start-up; the close button's hover fade.
- The app icon (`assets/logo.png`, sized into `assets/app.ico` and a greyed
  `assets/app-paused.ico` for the paused tray by `assets/make_icons.py`),
  version info (Windows shows "Lyrics UpFront" in Task Manager
  and Startup apps) and an application manifest (Common Controls v6,
  per-monitor DPI) embedded by `build.rs`, which writes the resource object itself, so
  no resource compiler is needed. `--write-icon PATH` writes the same icon as
  a `.ico` for the installer.

## Not included

- The optional Spotify connection (sign-in and queue preloading). Without it,
  the first play of a track waits for its lyrics to download, as before.
- The word sweep and the Tape design, dropped by decision.

## Build and run

```bat
rustup default stable
cargo build --release
target\release\lyrics-upfront.exe
```

Either the MSVC or the GNU toolchain works; nothing needs a C compiler.

`--simulate play|paused|idle|static|hidden` runs a built-in synthetic song
(the one the Python profiling harness used) instead of real media info, for
like-for-like measurement. `--log` writes a diagnostics log to
`%USERPROFILE%\.lyrics-upfront\overlay.log` (or set `LYRICS_UPFRONT_LOG=<file>`):
media changes, every lyrics request with timing and error codes, retries,
settings changes, and frames per second every 5 seconds.

Test hooks: `LYRICS_UPFRONT_LRCLIB_HOST=<host>` sends lyric lookups elsewhere
(an unreachable address exercises the offline path) and
`LYRICS_UPFRONT_CACHE_DIR=<dir>` uses a separate lyrics cache.

Lyric lookups allow 5 s to reach LRCLIB and 20 s for its answer (it can take
4-8 s for some tracks). A failed lookup keeps the loading dots and retries
after 3 s; after a second failure the card says it cannot reach the lyrics
and retries every 15 s.

`--simulate demo[:COVER]` plays a short demo song with original lyrics, and
`--simulate demo:TRACK.json` plays any song from a file (`title`, `artist`,
`album`, `lrc`, `cover`, `start_ms`, `duration_ms`), for screenshots, and `LYRICS_UPFRONT_SNAPSHOT=<dir>` (with `--simulate`) saves
every drawn frame there as raw BGRA at twice the display scale; the README
images are made that way, never from the screen.

`cargo test` runs the lyric parsing/timing, palette and icon tests.

## Fonts and icon

Text is set in [Mona Sans](https://github.com/github/mona-sans), embedded in
the exe (`assets/fonts`, SIL Open Font License; see `assets/fonts/OFL.txt`).
The icon is `assets/logo.png`; `python assets/make_icons.py` regenerates the
`.ico` files from it.
