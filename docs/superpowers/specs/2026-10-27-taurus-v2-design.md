# Taurus v2 — Design Spec

Date: 2026-10-27 · Branch: `v2` · Status: draft for review

## 1. Summary

Taurus is an open-source, cross-platform desktop download manager in the spirit of Deepin Download Manager and Free Download Manager. v2 is a rewrite of the 2022 Tauri + React prototype into a pure-Rust engine with a native Slint UI. It handles direct HTTP(S) downloads with parallel segments, torrents and magnet links, and video/audio from sites supported by yt-dlp, including live recording.

**Goal:** both a daily-driver app other people use and a Rust project worth showing — built engine-first, polished after.

**Success looks like:**
- One installer per OS (Windows, macOS, Linux; x64 and arm64 where practical).
- Direct and torrent downloads that resume byte-perfect after a crash or reboot.
- Video sites working after a one-time, automatic tool download.
- A first-time user can paste a link and get a file without reading anything ("Simple mode").

**Constraints:**
- Desktop only. No Android/iOS.
- English first, with i18n from day one.
- Open source, GPL-3.0 (compatible with Slint's GPL-3.0-only option and with GPL dependencies).
- Solo developer working with an AI assistant.
- The `Yada` Flutter project is retired; all work happens in the `taurus` repo.

## 2. Key decisions

| Area | Decision | Why |
|---|---|---|
| Engine | Pure-Rust library crate `taurus-core`, no UI code | Single binary, full control, testable headless, UI swappable |
| Direct downloads | Own segmented HTTP engine on `reqwest`/tokio | Replaces aria2; no C++ sidecar to build and sign per platform |
| Torrents | `librqbit` embedded | Pure Rust; magnets, DHT, file selection, streaming |
| Video sites | yt-dlp + ffmpeg + Deno as external tools, downloaded on first use | No Rust equivalent exists; tools must self-update |
| UI | Slint (native rendering, no webview) | Avoids WebKitGTK problems on Linux; small binary, low RAM; stable 1.x API |
| Persistence | SQLite (`rusqlite`, bundled), WAL mode | Crash-safe, single file |
| Partial file extension | `.tapa` | Short; `.tp` collides with MPEG transport-stream files |
| HTTP identity | One stable, honest User-Agent; browser's exact UA for extension handoffs | Random UA rotation (2022 code) triggers bot blocks |

Rejected: Tauri (webview performance and compatibility on Linux), iced (pre-1.0 API churn, weaker accessibility), aria2 sidecar (per-platform C++ builds, less control), embedding Python/Scrapy (runtime weight).

## 3. Scope

**v1.0**
- Engines: segmented HTTP(S) with mirrors and resume; torrents (file picker, download in order, seeding rules); yt-dlp media (formats, playlists, live recording, convert presets).
- Adding: paste, drag and drop, clipboard watcher, add several links at once, grab links from a page (one level + folder listings), small link fixes for Google Drive / Dropbox / GitHub.
- Control: queue with max active downloads, speed presets and limits, when-done actions (notify, sleep, shut down, quit), notifications, resume after restart, keep computer awake while downloading.
- App: Simple mode (default) and Advanced mode, compact window with a wide layout in Advanced, light/dark themes, per-file-type colors, six accent colors, tray icon, open at login, single instance, system proxy support, OS "downloaded from the internet" marks.
- Packaging and auto-update for all three OSes.

**1.x (right after 1.0):** browser extension (Chromium + Firefox) — link handoff with cookies, Referer and exact User-Agent. This is the fix for Cloudflare-guarded and login-only downloads, so it is the first post-1.0 priority.

**Later:** Vietnamese translation, scheduler, remote control, portable Windows build, ytarchive engine, FTP, deeper page crawling, hold large downloads on metered connections.

**Out of scope:** file-hoster plugins, captcha solving, solving or bypassing bot checks, partial ZIP downloads, community comments.

## 4. User experience

Design reference: the "Taurus Redesign" canvas (Simple, Advanced compact, Advanced wide rows).

### 4.1 Modes
- **Simple mode** (default for new installs): one column, plain language, no engine terms. Tabs "Downloading" / "Finished". Adding a video offers only Video / Audio only, save folder and quality (default "Best available"). Settings: save folder + sort by type, speed (Gentle / Balanced / Fastest), when done, offer to download copied links, open at login, show advanced options.
- **Advanced mode:** everything — source tags (HTTP / Torrent / yt-dlp), segment map, per-download details, Active / Waiting / Done tabs, torrent file picker, grab links, several links, when-done and speed menu, full settings (engines, connections, proxy, torrents).
- Both modes use the same engine. `ui.mode` setting switches screens and wording only.

### 4.2 Layout
- Compact window, default 420 x 640.
- Simple mode stretched: content stays a centered column, max width ~640 px.
- Advanced mode stretched: wide layout with sidebar, list and detail pane.

### 4.3 Visual system
- Neutral light/dark grounds, Inter-style UI font with tabular numbers.
- File-type colors: ISO/disk blue, video violet, torrent teal, archive amber, music pink, image green, live red, other slate. Progress bars use the type color by default (option: accent color).
- Accents: Aldebaran (orange, default), Cobalt, Jade, Grape, Rose, Sky.
- Text contrast meets WCAG AA in both themes.

### 4.4 Error language
Every engine error category maps to two strings: a plain sentence for Simple mode ("This link stopped working. It may have expired.") and a detailed one for Advanced ("403 Forbidden · retried 10x").

## 5. Architecture

Cargo workspace:

```
taurus/
  crates/
    taurus-core/      # engine library (no UI)
    taurus-cli/       # headless front end: taurus add|list|pause|resume|remove|...
  apps/
    taurus-desktop/   # Slint app, tray, notifications, clipboard, i18n (@tr)
  docs/superpowers/specs/
```

### 5.1 Engine API (shape, not final signatures)

```rust
pub struct Engine { /* cheap to clone handle */ }

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<Engine>;   // opens DB, restores jobs
    pub async fn probe(&self, input: &str) -> Result<Probe>;     // what is this link? (file, torrent, media, page)
    pub async fn add(&self, req: AddRequest) -> Result<JobId>;
    pub async fn pause(&self, id: JobId) -> Result<()>;
    pub async fn resume(&self, id: JobId) -> Result<()>;
    pub async fn remove(&self, id: JobId, delete_files: bool) -> Result<()>;
    pub async fn set_limits(&self, limits: Limits) -> Result<()>;
    pub fn snapshot(&self) -> Vec<JobView>;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event>;
    pub async fn shutdown(&self) -> Result<()>;                  // flush state, stop workers
}
```

- UI and CLI only talk to `Engine`. They never touch network or files directly.
- `Event` covers job added/changed/removed, throttled progress (max ~4 per second per job), queue idle, when-done countdown, tool install progress.
- Async runtime: tokio.

### 5.2 Modules in `taurus-core`
- `http` — probe, segment planner, workers, mirrors, finalize.
- `torrent` — librqbit session adapter and seeding policy.
- `media` — yt-dlp runner, JSON progress parser, live recording, ffmpeg convert.
- `tools` — download, verify, update and roll back yt-dlp / ffmpeg / Deno.
- `grab` — page fetch and link extraction (`scraper`), folder listings.
- `fixups` — link rewrites for Google Drive, Dropbox, GitHub.
- `queue` — scheduling, speed limiter (shared token bucket), when-done actions.
- `store` — SQLite schema, migrations, settings.
- `errors` — `ErrorKind` categories and retry policy.
- `system` — keep-awake, sleep/shutdown commands, file safety marks, system proxy.

Code carried over from 2022: retry strategy, error-to-category mapping idea, disk-space and permission checks (rewritten on reqwest error methods instead of string matching). The React/Tauri frontend is removed (kept in git history).

## 6. Engine behavior

### 6.1 Direct downloads (`http`)
1. **Probe** with `GET` + `Range: bytes=0-0` (not HEAD; some servers drop HEAD). Learn size, range support (206 vs 200), file name, ETag / Last-Modified, Content-Type. Request uncompressed bodies (`Accept-Encoding: identity`).
2. **File name** priority: Content-Disposition (`filename*` RFC 5987 first) > final URL path segment (percent-decoded) > generated from Content-Type. Sanitize: strip path components and `..`, replace Windows-illegal characters, avoid reserved names (CON, NUL, ...), trim trailing dots/spaces, cap at 255 bytes, keep extension.
3. **Plan:** if ranges are supported and size is known, split into up to N segments (default 16, min 1 MiB each). Files under 1 MiB use one segment.
4. **Write** into `<name>.tapa`, each worker writing at its own offset. Grow the file as data arrives (no full pre-allocation on exFAT/network drives); free space is checked before starting.
5. **Work stealing:** a finished worker splits the largest remaining segment in half.
6. **Mirrors:** segments spread across all URLs; a mirror with repeated errors is dropped.
7. **No range support:** single stream; pause restarts from zero (UI warns first).
8. **Adaptive connections:** extra connections answered with 429/503/403 reduce the connection count; a per-host "one connection only" flag is remembered.
9. **Finalize:** verify size (and checksum if given or auto-found, e.g. `SHA256SUMS`), atomic rename `.tapa` -> final name (retry on Windows file locks), add " (1)" instead of overwriting, set OS safety marks (Windows Zone.Identifier, macOS quarantine).

### 6.2 Crash safety (`store`)
- SQLite in the app data folder: `jobs`, `segments`, `settings`, `host_flags`.
- Segment progress is committed every ~1 s and on pause, only after file data is flushed, so recorded progress never exceeds data on disk.
- On start: reconcile jobs with `.tapa` files; validate by ETag / Last-Modified + size. Changed or 416 -> restart from zero. No validator -> trust size, verify at end.
- Corrupt database -> moved aside, new database created, `.tapa` files left in place.

### 6.3 Errors and retries (`errors`)
`ErrorKind`: ConnectionLost, Timeout, LinkExpired (401/403/404/410 after success), NeedsSignIn, BlockedByBotCheck (Cloudflare `cf-mitigated: challenge` or challenge HTML), CaptivePortal (HTML where binary expected), RateLimited, QuotaExceeded, DiskFull, FolderUnavailable, NoPermission, FileTooLargeForDisk (FAT32 4 GB), FileChangedOnServer, ToolMissing, ToolFailed, SiteNotSupported, Unknown.

- Transient (ConnectionLost, Timeout, RateLimited): exponential backoff with jitter; after the retry budget the job shows "Waiting for connection" and keeps retrying slowly.
- LinkExpired: re-resolve from the original link once; if that fails, mark failed with "get a fresh link".
- BlockedByBotCheck / NeedsSignIn: never attempt to bypass; tell the user to start the download in the browser (extension handoff).
- Everything else: fail with the mapped message.

### 6.4 Torrents (`torrent`)
- One librqbit session inside the engine.
- Magnet / `.torrent` -> fetch metadata -> file list to UI (Advanced: picker; Simple: all files).
- Options: download in order, seeding rules.
- Torrent settings (Advanced): stop sharing at ratio (default 1.0; 0.5 / 2.0 / never stop / don't share), stop after time, upload limit, torrent-only download limit, max active torrents, max peers per torrent, listening port, UPnP on/off, DHT on/off, default "download in order".
- Settings librqbit lacks are implemented on top (e.g. ratio stop = watch uploaded bytes, then pause).

### 6.5 Media (`media`, `tools`)
- First video link installs tools for the OS/arch from official or standard build sources into the app data folder; each file is checksum-verified.
- yt-dlp update check at most daily; previous binary kept for rollback.
- Flow: `yt-dlp -J` for formats -> choose (Simple: best) -> run with JSON progress lines -> parse.
- Live: `--live-from-start`, `--wait-for-video` for scheduled streams. Stop sends a graceful interrupt so the file is finalized (special handling on Windows).
- Playlists over 50 items ask for confirmation and are added as one group.
- ffmpeg merge failure keeps separate video/audio files and reports the error.
- Convert presets: MP3, smaller MP4.
- Error mapping from exit code + stderr patterns: private, region-locked, age-gated, members-only, "confirm you're not a bot" (-> BlockedByBotCheck).

### 6.6 Grab links (`grab`)
- Fetch page, extract `a[href]`, media `src`, images; resolve relative URLs; dedupe; classify by extension.
- Folder listings ("Index of /"): optional one level into subfolders.
- Sizes probed lazily, a few at a time.
- Pasting a page URL in the normal add box routes here.

### 6.7 Queue and system (`queue`, `system`)
- Max active downloads default 3; FIFO; "Start now" jumps the queue.
- Shared speed limiter; changes apply live. Presets are relative to the connection's recently measured peak speed, so they work on slow and fast lines alike: Gentle (~25%), Balanced (~60%), Fastest (no limit). Advanced adds Custom (absolute MB/s). Both modes use the same preset names.
- When done: runs when no job is active (paused/failed don't block). 60 s cancellable countdown for sleep/shutdown; new downloads cancel it.
- Keep computer awake while jobs run (default on).
- System proxy on Windows/macOS, env vars on Linux; manual proxy in Advanced.

### 6.8 Desktop shell (`taurus-desktop`)
- Single instance; a second launch or OS "open with" forwards the link to the running instance.
- Tray icon (`tray-icon`), notifications, open at login.
- Clipboard watcher (~1 s): links only, ask once per link, ignore links Taurus copied itself.
- i18n via Slint `@tr()`; a pseudo-locale with long strings for layout testing.

## 7. Edge cases (each needs a test)

**HTTP:** server ignores Range (200 instead of 206); missing or compressed Content-Length; 0-byte and tiny files; signed CDN URLs expiring mid-download; per-IP connection limits; 416 or changed ETag on resume; no ETag/Last-Modified; captive portal HTML; 401; hotlink protection (Referer); server closes early but reports success (short segment re-downloaded); all filename cases in 6.1; same URL added twice; two jobs targeting the same name; disk full mid-download; save folder disappears; FAT32 4 GB limit; sleep / Wi-Fi switch / VPN change; monotonic clock for ETA; quit during finalize; Windows file lock on rename; second app instance.

**Hosts:** Google Drive virus-scan confirm page and quota error; Dropbox `?dl=0`; GitHub `blob/`; file hosts that expire links and then show a Cloudflare challenge (AkiraBox-style) -> LinkExpired then BlockedByBotCheck with browser handoff advice.

**Torrents:** magnet with no peers or metadata never arriving; unsafe file paths in torrents; all files deselected; 10k+ files (virtualized list); blocked listening port; free space counted for selected files only.

**Media:** large playlists; private / region / age / members-only; tool install while offline; broken yt-dlp update (rollback); live stream ends during recording; scheduled stream not started; ffmpeg merge failure; graceful stop on Windows.

**App:** download added during shutdown countdown; when-done with paused/failed jobs; live speed-limit changes; corrupt database; repeated clipboard links; long translations.

## 8. Testing
- **Unit:** segment planning and work stealing, range math, filename extraction and sanitizing, link fixups, error mapping and messages, queue rules, yt-dlp progress parsing.
- **Fake HTTP server** in tests (local, deterministic): range / no range, slow, mid-stream drop, 403 after N requests, 429 on extra connections, ETag change, captive-portal HTML, Cloudflare-style challenge response. Every download test checks the final checksum.
- **Crash test:** kill the engine mid-download, reopen from the database, verify the final file hash.
- **Torrents:** two librqbit sessions on localhost sharing a small test torrent; interrupt and resume; ratio stop.
- **Media:** fake `yt-dlp` script replaying recorded JSON and stderr; real-site tests marked `#[ignore]`.
- **CLI as end-to-end harness** for every engine feature.
- **UI:** event-to-row formatting is plain Rust with unit tests; a few Slint testing-backend smoke tests.
- **CI:** GitHub Actions on Windows, macOS, Linux for every push.

## 9. Milestones

| # | Content | Done when |
|---|---|---|
| M0 | Workspace on `v2`, CI, remove React/Tauri code | CI green on 3 OSes |
| M1 | `http`, `store`, CLI add/list/pause/resume | Segmented download resumes byte-perfect after kill |
| M2 | Queue, limits, error categories, when-done (CLI dry-run), link fixups | HTTP edge-case tests pass |
| M3 | Torrents + torrent settings | Local two-session test and ratio stop pass |
| M4 | Slint desktop, Simple mode (welcome, list, add, settings), tray, notifications, clipboard, keep-awake | First usable alpha |
| M5 | Media: tools manager, yt-dlp, live, convert | Video/audio/live work from Simple mode |
| M6 | Advanced mode: compact + wide, torrent picker, grab links, several links, when-done menu, full settings | All canvas screens exist in Slint |
| M7 | Packaging (Windows installer, macOS DMG, Linux AppImage/deb/rpm), auto-update, i18n setup | v1.0 release |
| M8 | Browser extension (Chromium + Firefox) | 1.x release |

## 10. Risks and open questions
- **librqbit maturity:** current releases include betas; pin a version and wrap it behind our adapter so it can be swapped.
- **Slint custom widgets:** chips, segment map and popovers are hand-built; spike them early in M4.
- **macOS signing/notarization** needs an Apple Developer account (cost); unsigned builds work but warn users.
- **Tool download sources** for ffmpeg/Deno per OS/arch need a final list and checksums at M5.
- **Cloudflare / bot checks** remain unsolvable inside the app by design; the extension is the answer.
