# M4: Desktop App, Simple Mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the first usable desktop alpha of Hyades: a Slint app in Simple mode (welcome, downloads list, add a link, settings) with tray icon, notifications, copied-link offers, drag and drop, open at login, single instance with link forwarding, and when-done handling, on top of the existing engine (HTTP + torrents).

**Architecture:** New crate `apps/taurus-desktop`. The Slint event loop owns the main thread; a tokio multi-thread runtime runs the `Engine` in the background. A `Bridge` subscribes to engine events, coalesces them, and pushes view models into Slint with `slint::invoke_from_event_loop`; Slint callbacks call back into the engine through the runtime handle. All user-facing text lives in `.slint` files behind `@tr()` (Rust passes codes and numbers, Slint words them), so translation later touches only `.slint` files. Pure Rust mapping (job → row, formatting, link detection, event coalescing) is unit-tested; screens get smoke tests with Slint's testing backend.

**Tech Stack:** `slint` 1.18 (+ `slint-build` 1.18; `i-slint-backend-testing` 1.18 for tests), `tray-icon` 0.26 (Linux: `default-features = false, features = ["ksni"]` — D-Bus StatusNotifier, no GTK; Windows/macOS: default features), `notify-rust` 4.18, `arboard` 3.7, `auto-launch` 0.6, `interprocess` 2.4 (feature `tokio`), `opener` 0.9 (feature `reveal`), `rfd` 0.17 (folder picker). All checked on crates.io on 2026-11-01.

**Spec:** `docs/superpowers/specs/2026-10-27-taurus-v2-design.md` — sections 4.1–4.4, 6.8, 9 row M4. **Design reference:** the "Taurus Redesign" canvas, row "Simple mode — the default" (`SimpleWelcome`, `SimpleMain`, `SimpleAdd`, `SimpleSettings`); colors and icon paths are copied from those artboards.

## Global Constraints

- All earlier Global Constraints hold (CI on 3 OSes with fmt + clippy `-D warnings`, `rust-version = "1.89"`, no internet in tests).
- Product name shown to users: **Hyades** (window title, tray tooltip, notifications, about line). Crate and binary keep the codename (`taurus-desktop`) until M7.
- Simple mode only. Advanced screens are M6; M4 does not show a "Show advanced options" switch (no dead controls).
- Every user-facing string is in a `.slint` file inside `@tr(...)`; Rust never builds sentences for display. Numbers and sizes are formatted in Rust (`format.rs`) and passed as strings.
- Window: default 420 × 640, minimum 360 × 480, resizable; Simple content is a single centered column, max width 640 px.
- Colors: the canvas tokens — light ground `#FBFBFC`, dark `#1C1C1F`; accents Aldebaran `#E8590C`/strong `#C2410C` (default), Cobalt, Jade, Grape, Rose, Sky; file-type colors (disc blue `#2563C9`, media violet `#6B3FD4`, torrent teal `#0B7160`, archive amber `#92580A`, music pink `#C0266D`, image green `#3B6D11`, live red `#C02B1E`, file slate `#4A5372`) with their light backgrounds and dark-mode variants exactly as in the canvas CSS. Theme follows the OS (`Palette.color-scheme`) unless overridden.
- UI updates: at most 10 model refreshes per second, whatever the engine sends.
- Closing the window hides it to the tray when a tray icon exists; "Quit" (tray menu or app menu) shuts the engine down cleanly. Without a tray (e.g. a Linux desktop with no StatusNotifier host), closing quits.
- Single instance: a second launch forwards its command-line links to the running app and exits 0.

## Review Focus

1. Closing the window while downloads run must not stop them, and quitting must leave every job resumable (engine `shutdown`, not a kill). Test: Task 6 `quit_shuts_down_engine_cleanly` and Task 4 `close_policy` unit test.
2. A second launch (or an OS "open with" on a magnet/`.torrent`) must hand the link to the running app instead of failing on the locked data folder. Test: Task 5 `second_instance_forwards_links`.
3. The copied-link offer must not nag: the same link twice, a link Hyades copied itself, text that is not a single link, or huge clipboard text must not trigger it. Test: Task 3 `clipboard_candidates`.
4. A burst of engine events (hundreds of progress updates) must not freeze the UI: updates are coalesced to ≤ 10/s and the newest values win. Test: Task 3 `coalescer_keeps_latest_and_limits_rate`.
5. A failed download must show a plain-language reason and "Try again" must actually retry. Test: Task 7 `failed_row_explains_and_retries`.

---

## File Structure

```
apps/taurus-desktop/
  Cargo.toml
  build.rs                      # slint_build::compile_with_config("ui/app.slint", style "fluent")
  ui/
    app.slint                   # AppWindow: screen routing, models, callbacks
    theme.slint                 # global Theme (tokens, accents, kind colors, light/dark)
    icons.slint                 # Path-based icons (SVG path data from the canvas)
    messages.slint              # global Messages: error/status wording via @tr
    welcome.slint  downloads.slint  row.slint  add.slint  settings.slint  banner.slint  countdown.slint
  src/
    main.rs                     # single instance, runtime, engine, window, tray, run
    app.rs                      # wiring: callbacks -> engine, close/quit policy
    bridge.rs                   # engine events -> coalesced view models
    rows.rs                     # JobView -> RowVm (pure)
    format.rs                   # sizes, ETA buckets, ratio (pure)
    links.rs                    # clipboard/drag text -> link candidate (pure)
    instance.rs                 # single instance + link forwarding (interprocess)
    tray.rs  notify.rs  clipboard.rs  login.rs  icon.rs
  tests/ui.rs                   # testing-backend smoke test (one #[test])
crates/taurus-core/src/settings.rs   # + UiSettings
crates/taurus-core/src/engine/...    # + inspect(), find_by_url(), JobView.retry_at
```

---

### Task 1: Engine additions the UI needs

**Files:**
- Modify: `crates/taurus-core/src/{settings.rs,engine/mod.rs,engine/jobs.rs,engine/schedule.rs,page.rs}`

**Interfaces:**
- Produces:

```rust
// settings.rs — Settings gains `pub ui: UiSettings` (serde default)
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub mode: UiMode,                 // Simple
    pub theme: ThemeChoice,           // System
    pub accent: String,               // "Aldebaran"
    pub progress_by_type: bool,       // true
    pub offer_copied_links: bool,     // true
    pub open_at_login: bool,          // false
    pub sort_by_type: bool,           // true (Videos/Music/Documents subfolders chosen by the UI when adding)
    pub language: Option<String>,     // None = OS language
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)] pub enum UiMode { Simple, Advanced }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)] pub enum ThemeChoice { System, Light, Dark }
// engine/mod.rs
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum LinkInfo {
    File { url: String, name: String, size: Option<u64>, content_type: Option<String> },
    Torrent(TorrentInfo),
}
impl Engine {
    pub async fn inspect(&self, input: &str) -> Result<LinkInfo, CoreError>; // fixups + resolve for HTTP; torrent_files for torrents; no job created
    pub fn find_by_url(&self, input: &str) -> Option<JobId>;               // matches source_url or url after fixups, or info hash for torrents
}
// JobView gains: pub retry_at: Option<i64>   // unix seconds of the next Waiting retry
```

Also (deferred from M2): narrow the quota-page rule to the Google Drive sentence or the phrase `quota exceeded` within the first 16 KiB, so ordinary pages mentioning "quota" are not misread.

- [ ] **Step 1: Write failing tests:** `ui_settings_defaults` and `old_settings_json_without_ui_loads` (settings JSON from M3 → `ui == UiSettings::default()`); `inspect_http_file` (TestServer 2 MiB with disposition → `LinkInfo::File{name, size: Some(..)}`, `snapshot()` empty); `inspect_torrent` (seeder magnet → `LinkInfo::Torrent`); `inspect_web_page_fails_site_not_supported`; `find_by_url_matches_fixed_up_link` (add a Dropbox `dl=0` link, `find_by_url` with the same input → Some(id)); `find_by_url_matches_info_hash`; `waiting_job_has_retry_at` (fast timing, 503s → view in Waiting has `retry_at` within `waiting_retry_first` of now); `quota_word_alone_is_not_quota_page`.
- [ ] **Step 2: Run** `cargo test -p taurus-core` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): inspect links, find duplicates, retry time and UI settings`.

---

### Task 2: Desktop crate scaffold, theme and CI

**Files:**
- Create: `apps/taurus-desktop/{Cargo.toml,build.rs,ui/app.slint,ui/theme.slint,ui/icons.slint,src/main.rs}`, `apps/taurus-desktop/tests/ui.rs`
- Modify: root `Cargo.toml` (`members = ["crates/*", "apps/*"]`, workspace deps for the Tech Stack crates), `.github/workflows/ci.yml` (Ubuntu: `sudo apt-get install -y libfontconfig1-dev libxkbcommon-dev` before clippy/test; add more only if the build proves it needs them)

**Interfaces:**
- Produces: `slint::include_modules!()` exposing `AppWindow`; `ui/theme.slint` exports `global Theme { in-out property <bool> dark; in-out property <string> accent; out property <brush> bg, panel, line, text, muted, chip, track, ok, err, accent-base, accent-strong, accent-text, accent-soft, sel; pure function kind-bg(kind: string) -> brush; pure function kind-fg(kind: string) -> brush; }`; `ui/icons.slint` exports `component Icon inherits Path { in property <string> name; }` covering: down, plus, pause, play, stop, retry, folder, trash, settings, close, back, disc, media, magnet, archive, music, file, image, live, link, check, moon.
- `build.rs`: `slint_build::compile_with_config("ui/app.slint", slint_build::CompilerConfiguration::new().with_style("fluent".into()))`.

- [ ] **Step 1: Write the failing smoke test** in `tests/ui.rs` (the only `#[test]` in the file, because the testing backend initializes once per process):

```rust
#[test]
fn ui_smoke() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = taurus_desktop::AppWindow::new().unwrap();
    assert_eq!(ui.get_window_title(), "Hyades");
    // later tasks append their checks to this same function
}
```

(`taurus-desktop` exposes a small `lib.rs` re-exporting the Slint types and pure modules so tests can reach them; `main.rs` stays thin.)
- [ ] **Step 2: Run** `cargo test -p taurus-desktop` → FAIL. **Step 3: Implement** the scaffold (`AppWindow` with `in property <string> window-title: "Hyades"`, empty screens), `Theme` and `Icon`. **Step 4: Run** → PASS; `cargo clippy --workspace --all-targets -- -D warnings` clean on CI for all OSes.
- [ ] **Step 5: Commit** `feat(desktop): Slint app scaffold with theme and icons`.

---

### Task 3: Pure view-model logic

**Files:**
- Create: `apps/taurus-desktop/src/{format.rs,rows.rs,links.rs,bridge.rs}` (bridge: only the `Coalescer` in this task)

**Interfaces:**
- Produces:

```rust
// format.rs
pub fn bytes(n: u64) -> String;              // 1000-based: "0 B", "999 B", "1.0 KB", "3.8 GB", "61 MB" (1 decimal under 10)
pub enum Eta { Unknown, UnderMinute, Minutes(u32), Hours(u32) }
pub fn eta(remaining: u64, speed_bps: u64) -> Eta;  // speed 0 -> Unknown; < 60 s -> UnderMinute; < 90 min -> Minutes(ceil); else Hours(round)
pub fn ratio(r: f64) -> String;              // "0.42", "1.0", "12"
// rows.rs — mirrors the Slint `RowVm` struct field by field
pub fn kind_for(file_name: &str, job_kind: JobKind) -> &'static str; // "disc" iso/img/dmg, "media" video exts, "music" audio exts, "archive", "image", "magnet" torrents, else "file"
pub fn row_of(view: &JobView, now_unix: i64) -> RowVm;
pub fn split_tabs(views: &[JobView]) -> (Vec<JobView> /*Downloading tab*/, Vec<JobView> /*Finished tab*/); // Finished = Completed; everything else (incl. Seeding, Failed, Paused) in Downloading; newest first within Finished, queue order within Downloading
pub fn summary(views: &[JobView]) -> SummaryVm; // counts + overall ETA for the footer
// links.rs
pub fn candidate(text: &str) -> Option<String>; // trimmed single http(s)/magnet link, ≤ 4096 bytes, no whitespace inside; else None
pub struct ClipboardFilter { /* last seen text, last link we copied */ }
impl ClipboardFilter { pub fn new() -> Self; pub fn we_copied(&mut self, text: &str); pub fn offer(&mut self, text: &str) -> Option<String>; } // Some only for a new candidate not copied by us
// bridge.rs
pub struct Coalescer { /* latest JobView per id + structural-change flag, last flush Instant */ }
impl Coalescer {
    pub fn new(min_interval: std::time::Duration) -> Self;   // 100 ms
    pub fn push(&mut self, event: taurus_core::Event);
    pub fn take_if_due(&mut self, now: std::time::Instant) -> Option<Flush>; // Flush { views: Vec<JobView>, settings: Option<Settings>, when_done: Option<WhenDoneUi> }
}
```

`RowVm` (Slint struct, also used by Task 4):

```slint
export struct RowVm {
    id: int, name: string, kind: string,
    state: string,          // "queued" "downloading" "paused" "waiting" "seeding" "failed" "done" "recording"
    progress: float,        // 0..1, -1 = unknown size
    done-text: string, total-text: string,
    eta-kind: string,       // "unknown" "under-minute" "minutes" "hours"
    eta-value: int, retry-seconds: int,
    error-kind: string,     // ErrorKind::as_str() or ""
    ratio-text: string, when-text: string, peers: int,
}
```

- [ ] **Step 1: Write failing unit tests:** `bytes_cases`; `eta_buckets` (0 speed → Unknown; 30 s → UnderMinute; 121 s → Minutes(3); 3 h → Hours(3)); `kind_for_cases` (`a.iso` disc, `x.MKV` media, `y.flac` music, `z.tar.gz` archive, `p.png` image, torrent job → magnet, `README` file); `row_states` (one `JobView` per `JobState` incl. Waiting with `retry_at = now+65` → `retry-seconds == 65`, Failed with `LinkExpired` → `error-kind == "link_expired"`, unknown total → `progress == -1`); `tabs_and_order`; `clipboard_candidates` (accepts `https://x.org/a.iso`, ` magnet:?xt=urn:btih:abc… ` trimmed; rejects `hello`, two links, a 5000-byte URL, `ftp://x`; `ClipboardFilter`: same link twice → second None; after `we_copied(link)` → None); `coalescer_keeps_latest_and_limits_rate` (500 `Progress` events for one job within 50 ms → one `Flush` with the last values; `take_if_due` returns None again until 100 ms passed).
- [ ] **Step 2: Run** `cargo test -p taurus-desktop --lib` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): view models, formatting, link detection, event coalescing`.

---

### Task 4: Simple-mode screens

**Files:**
- Create: `apps/taurus-desktop/ui/{messages.slint,welcome.slint,downloads.slint,row.slint,add.slint,settings.slint,banner.slint,countdown.slint}`; Modify: `ui/app.slint`, `src/app.rs` (`close_policy`)

**Interfaces:**
- Produces (`AppWindow` API used by Tasks 5–7):

```slint
export enum Screen { welcome, downloads, add, settings }
export struct AddVm { loading: bool, error-kind: string, kind: string, name: string, size-text: string, folder: string, is-torrent: bool, files-text: string, duplicate: bool }
export struct SettingsVm { folder: string, sort-by-type: bool, speed: string /* gentle|balanced|fastest */, when-done: string /* nothing|notify|sleep|shutdown */, offer-copied-links: bool, open-at-login: bool, theme: string, accent: string }
export struct SummaryVm { downloading: int, finished: int, eta-kind: string, eta-value: int, all-paused: bool }
export struct CountdownVm { visible: bool, action: string, seconds: int }
export struct BannerVm { visible: bool, host: string, kind: string }
// AppWindow properties
in-out property <Screen> screen; in property <[RowVm]> downloading-rows; in property <[RowVm]> finished-rows;
in-out property <int> tab;  // 0 Downloading, 1 Finished
in property <AddVm> add; in property <SettingsVm> settings; in property <SummaryVm> summary;
in property <CountdownVm> countdown; in property <BannerVm> banner; in property <bool> has-tray;
// callbacks
callback link-entered(string); callback add-confirmed(); callback add-cancelled(); callback choose-folder();
callback pause(int); callback resume(int); callback retry(int); callback stop-recording(int);
callback open-file(int); callback reveal(int); callback remove(int);
callback pause-all(); callback resume-all();
callback banner-download(); callback banner-dismiss();
callback setting-changed(string /*key*/, string /*value*/);
callback cancel-countdown(); callback link-dropped(string);
```

Screen content follows the canvas artboards exactly (layout, sizes, radii, copy). Copy that differs from the canvas: the welcome tips mention drag-and-drop only as "Or drag a link onto this window"; no "Show advanced options" row in Settings. Messages (`messages.slint`, `global Messages`) — Simple wording for each error kind, all in `@tr`:

| error-kind | Text |
|---|---|
| link_expired | This link stopped working. It may have expired. |
| needs_sign_in | This site wants you to sign in first. |
| blocked_by_bot_check | This site wants to check you're human. Open it in your browser and start the download there. |
| captive_portal | Your network wants you to sign in (hotel or café Wi-Fi?). Open your browser, then try again. |
| rate_limited / connection_lost / timeout / no_peers | (Waiting rows) Waiting for a connection. Trying again in {n}. |
| quota_exceeded | Too many people downloaded this file recently. Try again later. |
| disk_full | Your disk is full. Free up some space, then try again. |
| folder_unavailable | The save folder isn't available. Is the drive plugged in? |
| no_permission | Hyades can't save to this folder. Pick another one in Settings. |
| file_too_large_for_disk | This file is too big for that drive. Pick another folder. |
| checksum_mismatch | The downloaded file didn't match its checksum, so it was removed. Try again. |
| site_not_supported | This link is a web page, not a file. |
| unsafe_torrent | This torrent has unsafe file names, so it wasn't saved. |
| already_added | This download is already in your list. |
| file_changed_on_server | The file changed on the server, so the download started over. |
| (other) | Something went wrong. Try again. |

Status lines (`row.slint`): downloading `@tr("{0} of {1} · About {2} min left", done, total, n)` / `@tr("{0} of {1} · Less than a minute left", ..)` / `@tr("{0} of {1}", ..)` when ETA unknown; paused `@tr("Paused · {0} of {1}", ..)`; queued `@tr("Waiting to start")`; seeding `@tr("Sharing · ratio {0}", ratio)`; done `@tr("{0} · {1}", total, when)`. Row buttons: pause/resume (round icon button), "Try again" text button for failed, "Open" for finished, stop for recording. Every icon-only button has `accessible-label`.

`close_policy(has_tray: bool) -> ClosePolicy { HideToTray, Quit }` in `app.rs` (pure).

- [ ] **Step 1: Extend the smoke test** in `tests/ui.rs` (same single function): set `screen = welcome` → an element with accessible label "Paste a link" exists; set `downloading-rows` to three rows (downloading, waiting with `retry-seconds = 65`, failed `link_expired`) and `screen = downloads` → text "About 3 min left" is visible for the first row (via `find_by_accessible_label` on the row's status text, which carries `accessible-label: self.text`), "This link stopped working. It may have expired." is visible for the failed row, and clicking the row's "Try again" (`invoke_accessible_default_action`) calls `retry(id)` once (recorded through a callback handler set in the test). Add a unit test `close_policy` in `app.rs`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** the screens. **Step 4: Run** → PASS; manual check: `cargo run -p taurus-desktop` opens the welcome screen in light and dark OS themes.
- [ ] **Step 5: Commit** `feat(desktop): Simple mode screens`.

---

### Task 5: Single instance and link forwarding

**Files:**
- Create: `apps/taurus-desktop/src/instance.rs`

**Interfaces:**
- Produces:

```rust
pub enum Instance { Primary(Listener), Secondary }
pub struct Listener { /* interprocess tokio local socket listener */ }
impl Listener { pub fn into_stream(self) -> impl futures_util::Stream<Item = Vec<String>>; } // each message = one batch of links
pub async fn claim(name: &str, links: &[String]) -> std::io::Result<Instance>;
// Secondary: connects, sends `links` as one line of JSON (`["url1","url2"]\n`), returns Secondary. Primary: listens.
pub fn socket_name(data_dir: &std::path::Path) -> String; // "dev.nikethai.taurus.<8 hex of sha256(data_dir)>", namespaced (GenericNamespaced)
```

Decisions: a stale socket (crash) must not block the primary: if connecting fails with "connection refused"/not found, become primary (on Unix remove the stale path first). An empty batch from a secondary means "show the window". `main.rs` calls `claim` before opening the engine; a `Secondary` exits with code 0.

- [ ] **Step 1: Write failing tests** (`#[tokio::test]` in `instance.rs`, unique names per test): `first_claim_is_primary`; `second_instance_forwards_links` (primary then `claim(name, ["https://x.org/a.iso"])` → `Secondary`, primary stream yields `vec!["https://x.org/a.iso"]`); `empty_batch_means_show_window`; `stale_socket_becomes_primary` (drop a primary without cleanup, claim again → Primary).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS on all OSes.
- [ ] **Step 5: Commit** `feat(desktop): single instance with link forwarding`.

---

### Task 6: Wiring the app (bridge, engine, tray, notifications, clipboard, login)

**Files:**
- Modify: `src/{main.rs,app.rs,bridge.rs}`; Create: `src/{tray.rs,notify.rs,clipboard.rs,login.rs,icon.rs}`

**Interfaces:**
- Consumes: Tasks 1, 3–5; `Engine` API.
- Produces:

```rust
// app.rs
pub struct App { /* runtime handle, Engine, Weak<AppWindow>, ClipboardFilter, pending add */ }
impl App {
    pub fn start(window: &AppWindow, engine: taurus_core::Engine, rt: tokio::runtime::Handle, has_tray: bool) -> std::rc::Rc<App>; // installs all callbacks and the bridge
    pub fn quit(&self);   // engine.shutdown() then slint::quit_event_loop()
}
// icon.rs
pub fn app_icon_rgba(size: u32) -> Vec<u8>;   // rounded square in accent strong #C2410C with a white down arrow; used for tray and window icon
// tray.rs
pub fn build(on: TrayActions) -> Option<tray_icon::TrayIcon>; // None when the platform has no tray host; menu: "Open Hyades", "Pause all"/"Resume all", "Quit"
// notify.rs
pub fn finished(name: &str); pub fn failed(name: &str, message: &str); pub fn all_done();
// clipboard.rs
pub fn watch(interval: std::time::Duration, on_text: impl Fn(String) + Send + 'static) -> std::thread::JoinHandle<()>; // 1 s polling with arboard
// login.rs
pub fn set_open_at_login(enabled: bool) -> Result<(), String>; // auto-launch with app name "Hyades" and the current exe path
```

Decisions:
- `main.rs`: `claim` → (Secondary: exit 0) → build tokio runtime (multi-thread) → `Engine::open(EngineConfig::new(data_dir))` on the runtime (`block_on`) → `AppWindow::new()` → tray (created on the main thread after the window exists; Linux uses the KSNI backend which runs its own thread; tray events are read with a 100 ms `slint::Timer` from `TrayIconEvent::receiver()` / `MenuEvent::receiver()`) → `App::start` → forwarded links stream → `link-entered` flow → `ui.run()`.
- Data dir: `directories::ProjectDirs::from("dev", "nikethai", "taurus")` — the same as the CLI, so both see the same jobs (the engine's lock still prevents running both at once; the CLI's `list` read-only fallback keeps working).
- Startup screen: `welcome` when the engine has no jobs, else `downloads`.
- Add flow: `link-entered(text)` → `find_by_url` (duplicate → `add.duplicate = true`, the Add screen says "already in your list" and offers to show it) → `inspect` (loading spinner) → Add screen; `add-confirmed` → `Engine::add` with `dest_dir` = settings folder (default OS Downloads), plus the type subfolder `Videos`/`Music`/`Documents` when `sort_by_type` (by `kind_for`; created if missing) → screen `downloads`.
- Bridge: a tokio task forwards `engine.subscribe()` into a `Coalescer`; a 100 ms `slint::Timer` on the UI thread drains `take_if_due` and sets models (`VecModel` replaced only when ids change; otherwise rows updated in place).
- Notifications: `JobUpdated` to Completed → `notify::finished`; to Failed → `notify::failed` (message text resolved by the same table as `messages.slint`, kept in one Rust `fn simple_message(kind) -> &'static str` that mirrors it — this single exception to "no sentences in Rust" exists because OS notifications are outside Slint; a unit test asserts every `ErrorKind` has a message). `WhenDoneFired{Notify}` → `notify::all_done`; `{Quit}` → `App::quit`; countdown events drive `countdown`.
- Clipboard: watcher → `ClipboardFilter::offer` → `banner` (only when `offer_copied_links`); "Copy link" actions call `we_copied`.
- Drag and drop: a window-wide `DropArea` accepts text/URL drops and calls `link-dropped(text)` → same as `link-entered` (works where Slint supports drops from other apps).
- Close: `window().on_close_requested` returns `HideWindow` per `close_policy(has_tray)`; Quit path calls `App::quit`.
- Settings: `setting-changed(key, value)` updates `Settings` (engine `set_settings`) and side effects (`login::set_open_at_login`, folder via `rfd::FileDialog::new().pick_folder()` on the UI thread).

- [ ] **Step 1: Write failing tests:**
  - Unit: `app_icon_has_right_size` (`len == size*size*4`, corner pixel transparent, center pixel opaque); `every_error_kind_has_a_simple_message`; `sort_folder_for_kind` pins `fn sort_folder(kind: &str) -> Option<&'static str>`: media → `Videos`, live → `Videos`, music → `Music`, image → `Pictures`, file → `Documents`; disc, archive and magnet → `None` (saved in the folder itself).
  - Integration in `tests/ui.rs` (append): build the real `App` with an `Engine` in a temp dir and a `TestServer`; `invoke link-entered(url)` → wait (bounded loop calling `slint::platform::update_timers_and_animations()` and sleeping 20 ms) until `add.name == "x.bin"` and `add.size-text == "2.1 MB"`; `invoke add-confirmed` → wait until `finished-rows` has one row; `quit_shuts_down_engine_cleanly`: start a throttled download, invoke `App::quit` → the engine reports the job Queued/Paused on reopen (not Downloading), and the `.tapa` progress is in the store.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --workspace` → PASS. Manual check on your machine: tray icon appears with menu; closing hides to tray; a second launch with a link brings the window to front with the Add screen; copying a link shows the banner; finishing a download shows a notification.
- [ ] **Step 5: Commit** `feat(desktop): engine bridge, tray, notifications, clipboard, login item`.

---

### Task 7: Behaviour polish and end-to-end UI checks

**Files:**
- Modify: files from Tasks 4 and 6; Test: `tests/ui.rs`

- [ ] **Step 1: Append to the UI test:**
  - `failed_row_explains_and_retries`: TestServer path with `fail_after = FailAfter { after_requests: 1, status: 404, headers: vec![] }` (the probe succeeds, the download gets 404) → row shows the `link_expired` text; replace the spec to succeed; invoke "Try again" → the row reaches Finished.
  - `waiting_row_counts_down`: fast timing, 503s → row text contains "Trying again in".
  - `torrent_row_shows_sharing`: seeder + `TorrentTestNet`, `stop_ratio = None` → the row shows "Sharing · ratio".
  - `countdown_cancel`: `when_done = sleep`, `PowerMode::DryRun`, fast timing; finish a job → `countdown.visible`; invoke `cancel-countdown` → hidden and `WhenDoneCancelled` seen.
  - `duplicate_link_is_flagged`: add a URL, then `link-entered` with the same URL → `add.duplicate == true` and no second job.
- [ ] **Step 2: Run** → fix until PASS on CI for all three OSes.
- [ ] **Step 3: README** — "Desktop app (alpha)" section: how to run, where data lives, Linux tray requirement (a StatusNotifier host; GNOME needs the AppIndicator extension).
- [ ] **Step 4: Commit** `feat(desktop): Simple mode alpha behaviour and UI tests (M4)`.

---

## Done criteria (spec M4 — "first usable alpha")

- `cargo run -p taurus-desktop` gives a working Simple-mode app for HTTP and torrent downloads on Windows, macOS and Linux.
- CI green on all three OSes, including the UI smoke test.
- All five Review Focus tests pass.

Deferred: Advanced mode and the wide layout (M6), video sites (M5), installers/signing/auto-update and translation extraction with a pseudo-locale (M7), browser extension (M8), Windows legacy-console colours in the CLI (later list).
