# M6b: Advanced Mode Screens — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Advanced mode to the desktop app — the compact Advanced window, the wide layout when the window is stretched, row details, the Advanced add screens (video options, file options, torrent file picker), grab links, several links, the when-done and speed menu, and full settings — on top of the M6a engine features, switchable from Simple mode's settings.

**Architecture:** Same app, same engine, same bridge. `UiSettings.mode` selects which screen set the window shows; Simple screens stay untouched. New screens are new `.slint` files; new Rust "flows" (one per screen family) translate callbacks into `Engine` calls, following the existing `add_flow.rs` / `settings_flow.rs` / `media_add.rs` pattern. Pure Rust (filters, batch parsing, layout choice, detail mapping) is unit-tested; screens get testing-backend checks appended to the single UI test.

**Tech Stack:** as M4–M6a; no new crates.

**Spec:** `docs/superpowers/specs/2026-10-27-taurus-v2-design.md` — 4.1 (Advanced mode), 4.2 (wide layout ≥ stretched), 4.3, 6.4, 6.6, 6.7, 9 row M6. **Design reference:** the "Taurus Redesign" canvas: row "Advanced mode — compact window" (`CompactMain`, `CompactAdd`, `CompactSettings`, `CompactTorrent`, `CompactGrab`, `CompactBatch`, `CompactWhenDone`) and row "Wide layout" (`Main`, `AddDownload`, `Settings`). Layout, copy, colors and icons follow those artboards; where this plan says otherwise, the plan wins.

## Global Constraints

- All earlier Global Constraints hold (all user text in `.slint` behind `@tr`, ≤ 10 UI refreshes/s, CI on 3 OSes, offline tests).
- Simple stays the default. Turning Advanced on or off never changes jobs or engine settings; Simple shows a setting it cannot express (custom speed, per-torrent overrides) read-only (e.g. "Custom · 3 MB/s").
- Wide layout: Advanced mode only, when the window is at least 900 px wide; below that the compact Advanced layout. Switching keeps the selected job.
- Lists that can be long (downloads, grab results, torrent files) use `ListView` (virtualized).
- Password fields use `LineEdit { input-type: password; }`; the Slint property holding a typed password is cleared as soon as it is handed to the engine.
- No browser-extension settings (that is M8); no language picker (M7).
- Labels from the canvas that this plan changes: torrent picker has no "Download in order" switch (librqbit always downloads in order); the Settings "Browser" section is replaced by "General" (copied links, open at login).

## Review Focus

1. Switching Simple ↔ Advanced must not change any job or engine setting, and Simple must show Advanced-only values honestly. Test: Task 2 `mode_switch_changes_nothing`.
2. The Remove dialog must default to keeping files and must use `RemoveFiles` correctly (finished files are only deleted when the user ticks "Also delete the files"). Test: Task 3 `remove_dialog_defaults_to_keep`.
3. A typed password must not linger in the UI model or reach logs after it is submitted. Test: Task 3 `password_cleared_after_submit`.
4. Grab results with 500 links must stay responsive and fill in sizes progressively. Test: Task 5 `grab_500_links_streams_sizes`.
5. Crossing the 900 px threshold must keep the selected job and its open detail. Test: Task 2 `wide_switch_keeps_selection`.

---

## File Structure

```
crates/taurus-core/src/engine/detail.rs          # job_detail(), probe_sizes_stream()
crates/taurus-core/src/convert/...               # guards (M6a leftovers)
crates/taurus-core/src/secrets.rs                # forget_saved_passwords support
apps/taurus-desktop/ui/
  adv/main.slint  adv/row.slint  adv/detail.slint  adv/sidebar.slint  adv/wide.slint
  adv/add.slint  adv/torrent.slint  adv/grab.slint  adv/batch.slint  adv/whendone.slint
  adv/settings.slint  adv/dialogs.slint          # remove confirm, password, choose files
apps/taurus-desktop/src/
  mode.rs        # mode + layout choice (pure)
  filters.rs     # library/source filters and counts (pure)
  detail.rs      # JobDetail -> DetailVm (pure)
  batch.rs       # several-links parsing (pure)
  adv_flow.rs    # Advanced callbacks -> Engine (rows, detail, dialogs, when-done menu)
  adv_add.rs     # Advanced add, torrent picker
  grab_flow.rs   # grab screen
  adv_settings.rs
```

---

### Task 1: Engine additions for Advanced screens

**Files:**
- Create: `crates/taurus-core/src/engine/detail.rs`; Modify: `engine/{mod.rs,types.rs}`, `convert/mod.rs`, `secrets.rs`, `store` (settings key `secret_accounts`)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct JobDetail {
    pub id: JobId,
    pub segments: Vec<SegmentView>,            // HTTP: start, end, done; empty otherwise
    pub connections: u32,                      // HTTP workers running now
    pub files: Vec<TorrentFileProgress>,       // torrents: index, path, size, done, selected
    pub media_files: Vec<std::path::PathBuf>,  // media: finished files so far
    pub mirrors: Vec<String>, pub referer: Option<String>, pub checksum: Option<String>,
    pub has_password: bool,                    // never the password
    pub overrides: Option<TorrentOverrides>,
}
pub struct SegmentView { pub start: u64, pub end: u64, pub done: u64 }
pub struct TorrentFileProgress { pub index: usize, pub path: String, pub size: u64, pub done: u64, pub selected: bool }
impl Engine {
    pub fn job_detail(&self, id: JobId) -> Option<JobDetail>;
    pub fn probe_sizes_stream(&self, urls: Vec<String>) -> tokio::sync::mpsc::Receiver<(usize, Option<u64>)>; // same rules as probe_sizes; results as they arrive
    pub async fn forget_saved_passwords(&self) -> Result<u32 /*removed*/, CoreError>; // every account Taurus ever wrote (tracked in settings `secret_accounts`), plus clears the list
}
// JobView gains: pub progress_unit: ProgressUnit   // Bytes (default) | Millis (convert jobs: downloaded/total are milliseconds of media time)
```

M6a leftovers fixed here: `convert` refuses a source that is not a Completed job's file, that ends in `.tapa`/`.part`, or that is the output of a convert job that has not completed (`Err` kind `Unknown`, detail "that file isn't finished yet"); `grab` no longer probes sizes itself (the UI streams them).

- [ ] **Step 1: Write failing tests:** `job_detail_http_segments` (throttled 20 MiB job → ≥ 2 segments, `done` grows); `job_detail_torrent_files` (seeder → two files with `selected` flags); `job_detail_never_has_password_text` (debug-format of a detail for a job with credentials contains no password); `probe_sizes_stream_yields_all` (5 URLs incl. one 404 → 5 results, any order, `None` for the 404); `forget_saved_passwords_removes_all` (MemoryStore with two accounts → 2, store empty); `convert_refuses_unfinished_sources` (a running convert's output path → Err; a `.tapa` path → Err); `convert_progress_unit_is_millis`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): job details, streamed size probes, password cleanup, convert guards`.

---

### Task 2: Mode switch and the Advanced shell (compact + wide)

**Files:**
- Create: `apps/taurus-desktop/ui/adv/{main.slint,row.slint,sidebar.slint,wide.slint}`, `apps/taurus-desktop/src/{mode.rs,filters.rs}`; Modify: `ui/app.slint`, `ui/types.slint`, `ui/settings.slint` (Simple: "Show advanced options" switch), `src/{bridge.rs,rows.rs,settings_flow.rs}`

**Interfaces:**
- Produces:

```rust
// mode.rs
pub fn layout_for(mode: UiMode, width_px: f32) -> Layout;   // Simple -> Simple; Advanced & width >= 900 -> Wide; else Compact
pub enum Layout { Simple, Compact, Wide }
// filters.rs
pub enum Filter { All, Downloading, Queued, Paused, Completed, Failed, Http, Torrents, Media }
pub fn matches(f: Filter, v: &JobView) -> bool;   // Downloading includes Downloading/Recording/Seeding; Queued includes Queued/Waiting
pub fn counts(views: &[JobView]) -> Vec<(Filter, u32)>;
pub enum Tab { Active, Waiting, Done }            // compact tabs: Active = Downloading/Recording/Seeding; Waiting = Queued/Waiting/Paused/Failed; Done = Completed
pub fn tab_of(v: &JobView) -> Tab;
```

```slint
// AppWindow gains:
in property <string> layout;            // "simple" | "compact" | "wide"
in property <[AdvRowVm]> adv-rows;      // rows for the current tab/filter, plus source chip, speed text, selected flag
in-out property <int> selected-id: -1;
in-out property <string> filter: "all"; in property <[FilterCountVm]> filter-counts;
in property <string> speed-down-text; in property <string> speed-up-text;
callback select(int); callback filter-changed(string); callback tab-changed(int); callback search-changed(string);
callback open-batch(); callback open-whendone(); callback open-grab(string);
```

Decisions: `AdvRowVm` = `RowVm` + `source: string` ("HTTP" / "Torrent" / "yt-dlp" / "Convert"), `speed-text`, `selected`. The compact layout follows `CompactMain`; the wide layout follows `Main` (sidebar with Library and Sources counts, search, list, detail pane from Task 3). The bridge recomputes `layout` on window resize (`changed width`) and keeps `selected-id`. Simple Settings gains the "Show advanced options" row (writes `ui.mode`).

- [ ] **Step 1: Write failing tests:** unit `layout_threshold` (899 → Compact, 900 → Wide, Simple at 2000 → Simple), `filter_and_tab_rules` (one view per state), `counts_cover_all_filters`; UI test (append): switch `ui.mode` to Advanced → `layout == "compact"`, tabs show counts; `mode_switch_changes_nothing` (snapshot of jobs and `Engine::settings()` except `ui.mode` identical before/after two switches; Simple shows "Custom" for a custom speed); `wide_switch_keeps_selection` (select a row, set window width 1200 → `layout == "wide"`, `selected-id` unchanged and the detail pane shows that job).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): Advanced mode shell with compact and wide layouts`.

---

### Task 3: Row details, actions and dialogs

**Files:**
- Create: `apps/taurus-desktop/ui/adv/{detail.slint,dialogs.slint}`, `apps/taurus-desktop/src/{detail.rs,adv_flow.rs}`

**Interfaces:**
- Produces:

```rust
// detail.rs (pure)
pub fn detail_vm(view: &JobView, detail: &JobDetail, now: i64) -> DetailVm; // segment cells (96, as in the canvas), torrent file rows, media steps, stats pairs
pub fn segment_cells(segments: &[SegmentView], total: u64, cells: usize) -> Vec<f32>; // 0..1 fill per cell
```

```slint
export struct DetailVm { visible: bool, name: string, kind: string, source: string, big: string, size-line: string,
    cells: [float], has-cells: bool, files: [FileRowVm], steps: [StepVm], stats: [StatVm],
    folder: string, url: string, can-convert: bool, can-retry-cookies: bool, needs-password: bool, can-choose-files: bool,
    ratio-override: string, seed-time-override: string }
callback copy-link(int); callback convert(int, string /*mp3|smaller-mp4*/);
callback ask-remove(int); callback confirm-remove(int, bool /*delete files*/);
callback retry-with-browser(int, string /*browser*/);
callback submit-password(int, string, string);   // id, user, password
callback choose-files(int); callback files-chosen(int, [int]);
callback set-override(int, string /*ratio|seed-time*/, string /*value or "default"*/);
```

Decisions:
- Detail refresh: while a detail is visible, `job_detail` is polled every 500 ms (not through the coalescer).
- Remove dialog: "Remove <name> from the list?" with "Also delete the files" unticked by default → `remove_with(id, Keep)`; ticked → `remove_with(id, All)`; Simple mode keeps its current behaviour.
- Password dialog appears for HTTP jobs failed with `NeedsSignIn` (and from "Enter password…" in the detail); on submit → `set_credentials`, then both fields are set to "".
- "Use my browser's sign-in" (media jobs failed with `NeedsSignIn` or `BlockedByBotCheck`): browser menu (Chrome, Firefox, Safari on macOS, Edge, Brave) → `retry_with_cookies`.
- Convert menu on finished media/audio/video files → `convert`.
- "Copy link" → clipboard + `ClipboardFilter::we_copied`.
- Torrent detail: per-file progress list, "Choose files…" opens the picker (Task 4) prefilled → `set_torrent_files`; ratio / seed-time override selects → `set_torrent_overrides`.

- [ ] **Step 1: Write failing tests:** unit `segment_cells_fill` (two half-done segments over 96 cells → expected fills), `detail_vm_per_kind`; UI test (append): select an HTTP row → detail shows cells; `remove_dialog_defaults_to_keep` (confirm without ticking → file still on disk, job gone); `password_cleared_after_submit` (protected TestServer file fails → dialog visible → submit → the password `LineEdit` text is "" and the job completes); convert action on a finished file (fake ffmpeg) → a Convert row appears with time-based progress text.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): Advanced row details, remove/password dialogs, convert and sign-in retry`.

---

### Task 4: Advanced add screens and the torrent picker

**Files:**
- Create: `apps/taurus-desktop/ui/adv/{add.slint,torrent.slint}`, `apps/taurus-desktop/src/adv_add.rs`

**Interfaces:**
- Produces:

```slint
export struct AdvAddVm {
    kind: string /* "file" | "media" | "torrent" */, loading: bool, error-kind: string, duplicate: bool,
    // file
    name: string, size-text: string, connections: int, mirrors: [string], checksum: string, referer: string, needs-user: bool,
    // media
    qualities: [QualityVm], quality-id: string, audio-only: bool, container: string, subtitle-langs: [LangVm], auto-subs: bool, embed-subs: bool, cookies-browser: string,
    title: string, uploader: string, duration-text: string, thumbnail: image, live: string, playlist-count: int,
    // torrent
    files: [TorrentFileVm], selected-count: int, selected-size: string, ratio: string,
    folder: string, free-text: string }
callback adv-field-changed(string /*field*/, string /*value*/);
callback adv-toggle-file(int); callback adv-toggle-all(bool); callback adv-toggle-group(string /*folder path*/);
callback adv-add-confirmed(bool /*start now*/);
```

Decisions: the screens follow `CompactAdd` (media) and `CompactTorrent` (torrent); file links show name (editable), folder, connections (1/4/8/16), mirrors (one per line), checksum, referer, and user name + password (password passed once, then cleared). The torrent picker groups files by top folder with tri-state group checkboxes (all / some / none shown as checked / dash / empty), shows selected count and size, re-checks free space, and the "Keep sharing" select sets the per-torrent ratio override. Quality list = `all_qualities` + audio options; subtitle languages from `MediaInfo`; cookies browser select. "Start immediately" unchecked → added and left Queued at the end (`start_now` not called; with autostart the queue starts it in order).

- [ ] **Step 1: Write failing tests:** UI test (append): media link (fake tools) → quality list contains every height from the fixture; pick 720p + MKV + subtitles `en` embedded + cookies Firefox → fake yt-dlp args contain `-f bv*[height<=720]…`, `--merge-output-format mkv`, `--sub-langs en`, `--embed-subs`, `--cookies-from-browser firefox`; torrent (seeder with `a/1.bin`, `a/2.bin`, `b.bin`) → toggling group `a` off selects only `b.bin`; group shows dash after re-ticking one file; confirm → `only_files == [2]`; file link with mirrors + checksum → `JobDetail.mirrors`/`checksum` match.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): Advanced add for files, videos and torrents`.

---

### Task 5: Grab links and several links

**Files:**
- Create: `apps/taurus-desktop/ui/adv/{grab.slint,batch.slint}`, `apps/taurus-desktop/src/{grab_flow.rs,batch.rs}`

**Interfaces:**
- Produces:

```rust
// batch.rs (pure)
pub enum LineKind { Http, Torrent, Media, Page, Invalid }
pub struct BatchLine { pub n: usize, pub text: String, pub kind: LineKind, pub mirror_of: Option<usize> }
pub fn parse_batch(text: &str, mirrors: bool) -> Vec<BatchLine>; // blank and '#' lines skipped; magnet / .torrent -> Torrent; fast-path video hosts -> Media; URL ending "/" or ".html" -> Page; non-URL -> Invalid; with `mirrors`, an Http line whose last path segment equals an earlier Http line's (case-insensitive) is a mirror of it
pub fn to_requests(lines: &[BatchLine], dest: &std::path::Path) -> Vec<AddRequest>; // mirrors folded into the first line's `mirrors`
```

Decisions: Grab screen follows `CompactGrab`: page URL, type chips (All/Video/Docs/Archives/Images by extension, colors by kind), extension filter, "Subfolders" switch (re-runs `grab`), "Select all shown", size column filled from `probe_sizes_stream`, footer "N selected · size" and "Download N" (adds one HTTP job per link). Several-links screen follows `CompactBatch`: textarea, the parsed list with kind chips, "Treat copies as mirrors" switch, "Find files →" on Page lines opens Grab, "Add N downloads". Media lines are added with Simple defaults.

- [ ] **Step 1: Write failing tests:** unit `batch_kinds`, `batch_mirrors_fold`, `batch_skips_comments`; UI test (append): grab a TestServer listing → chips counts match; `grab_500_links_streams_sizes` (page with 500 links → model has 500 rows immediately with empty sizes; sizes appear over subsequent timer ticks; the UI loop keeps ticking (each `update_timers_and_animations` call returns within 50 ms)); "Download 3" adds 3 jobs; batch text with 2 copies → 1 job with 1 mirror.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): grab links and add several links`.

---

### Task 6: When-done and speed menu

**Files:**
- Create: `apps/taurus-desktop/ui/adv/whendone.slint`; Modify: `src/adv_flow.rs`

**Interfaces:**
- Produces: popover per `CompactWhenDone` — radio list (Do nothing, Just notify me, Put the computer to sleep, Shut down, Quit Hyades) with the 60-second note for sleep/shutdown; speed segmented Gentle/Balanced/Fastest/Custom (Custom shows a numeric field in MB/s, 0.1–10000); footer button label follows the choice ("When done", "Notify", "Then sleep", "Then shut down", "Then quit"); the footer's download speed text follows the actual total speed.

- [ ] **Step 1: Write failing tests:** UI test (append): choose "Then sleep" → `Engine::settings().when_done == Sleep` and footer label updates; Custom 2.5 → `SpeedLimit::Custom { bytes_per_sec: 2_500_000 }`; invalid custom "abc" → field shows the error state and the setting is unchanged.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(desktop): when-done and speed menu`.

---

### Task 7: Advanced settings

**Files:**
- Create: `apps/taurus-desktop/ui/adv/settings.slint`, `apps/taurus-desktop/src/adv_settings.rs`

**Interfaces:**
- Produces: settings pages (compact: one scrolling page with sections; wide: section list on the left per `Settings`):
  - **Downloads:** save folder, sort by type, active downloads at once (1–10), default connections per file (1/4/8/16), speed (presets + custom), verify checksums automatically, keep the computer awake.
  - **Torrents:** stop sharing at ratio (0 / 0.5 / 1 / 2 / never), stop after time (off / 1 h / 24 h / 7 d), upload limit, torrent download limit, active torrents, peers per torrent, listening port (random or number), UPnP, DHT — with "applies the next time Hyades starts" under port/UPnP/DHT.
  - **Network:** proxy System / None / Manual (URL field; a rejected URL shows the engine's error text under the field and keeps the old setting).
  - **Engines:** yt-dlp, ffmpeg, Deno — version, state, Install / Check now / Roll back (`install_tools`, `update_tools(true)`, `rollback_tool`); a tool row in `Installing` shows a progress bar.
  - **Appearance:** theme (System/Light/Dark), accent (six swatches), color progress bars by file type.
  - **Privacy:** "Forget saved passwords" (→ `forget_saved_passwords`, shows "Removed N").
  - **General:** offer to download copied links, open at login, "Show advanced options" (the same switch as Simple).

- [ ] **Step 1: Write failing tests:** UI test (append): each section reachable; set ratio "never" → `stop_ratio == None`; bad proxy "localhost:1" → error text visible, `network.proxy` unchanged; Roll back on yt-dlp with two installed fake versions → previous version active; accent "Jade" → `Theme.accent == "Jade"`; "Forget saved passwords" → "Removed 1" after one protected download.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; `cargo test --workspace`, clippy, fmt clean on CI.
- [ ] **Step 5: Commit** `feat(desktop): Advanced settings`.

---

### Task 8: Polish and manual check

- [ ] **Step 1:** Keyboard: Tab order through rows/actions/dialogs; Enter confirms dialogs; Escape closes popovers and dialogs. Add these checks to the UI test where the testing backend supports key events.
- [ ] **Step 2:** README — "Advanced mode" section with a screenshot list of what each screen does.
- [ ] **Step 3: Manual check on your machine:** switch modes back and forth during a running download; stretch the window across 900 px; add a torrent picking a subset of files then add one more file later; grab a real "Index of /" page; a password-protected download; a video with subtitles and MKV; convert a video to MP3; the when-done menu with "Notify".
- [ ] **Step 4: Commit** `feat(desktop): Advanced mode polish (M6b)`.

---

## Done criteria (spec M6 — "Every screen on the canvas exists in Slint")

- Every artboard in the canvas's Advanced compact and wide rows has a working Slint screen (with the changes listed in Global Constraints).
- CI green on all three OSes; all five Review Focus tests pass; the manual check above is done.

Next: M7 — packaging (installers, signing, auto-update) and i18n setup.
