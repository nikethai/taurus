# M2: Queue, Limits, Real-World Errors — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the M1 engine into one that behaves well on the real internet: a download queue, speed limits, mirrors, adaptive connection counts, checksum verification, OS safety marks, detection of web pages / bot checks / quota pages, link fixups, a "waiting for connection" state, when-done actions and keep-awake — all driven from the CLI.

**Architecture:** Everything lands in `taurus-core` behind the existing `Engine` actor. New pure modules (`fixups`, `page`, `checksum`, `limit`, `system::*`) are unit-tested in isolation; the actor gains a scheduler (queue + waiting retries + when-done countdown) and passes a shared `Limiter` and `Timing` into `run_job`. The CLI becomes a queue runner: it enqueues, runs until the queue is idle, and exposes settings.

**Tech Stack:** as M1, plus `fs4` 1.1 (`available_space`), `keepawake` 0.6, `xattr` 1.6 (macOS/Linux only), `scraper` 0.29 (HTML forms), `libc` (Linux/macOS `statfs`) and `windows-sys` (Windows volume info). `sha2` and `hex` move from dev- to normal dependencies of `taurus-core`.

**Spec:** `docs/superpowers/specs/2026-10-27-taurus-v2-design.md` — sections 6.1 (mirrors, adaptive connections, finalize), 6.3, 6.7, 7 "HTTP" + "Hosts" + "App", 9 row M2. Also the "deliberately deferred" list at the end of `docs/superpowers/plans/2026-10-27-m0-m1-core-http.md`.

## Global Constraints

- Everything from the M0+M1 plan's Global Constraints still holds (`.tapa`, one stable User-Agent, probe with `GET bytes=0-0`, durability, no internet in tests, `rust-version = "1.89"`, CI on 3 OSes with fmt + clippy `-D warnings`).
- Never try to solve or bypass a bot check. Detect it, fail with `BlockedByBotCheck`, stop.
- Defaults: `max_active = 3`, speed `Fastest`, when-done `Nothing`, `auto_checksum = true`, `keep_awake = true`, when-done countdown 60 s, waiting retry starts at 60 s doubling to 10 min.
- Speed presets are relative to the measured peak: Gentle = 25 %, Balanced = 60 % of `measured_peak_bps`; with no measurement yet, Gentle = 1 MiB/s, Balanced = 5 MiB/s. Fastest = unlimited. Custom = absolute bytes/s.
- Free-space margin: available must be ≥ remaining bytes + 16 MiB.
- FAT32 file size limit: a file larger than 4_294_967_295 bytes is refused with `FileTooLargeForDisk`.
- HTML inspected by the engine is capped at 64 KiB; a `SHA256SUMS` file at 1 MiB.
- Safety marks are best-effort: failure is logged with `tracing::warn!`, never fails the download.
- When-done actions that touch the OS (sleep, shutdown) run only when `PowerMode::Execute`; tests and `--dry-run-power` use `PowerMode::DryRun`.
- User-facing message strings for error kinds are NOT in core; they arrive with the UI in M4.

## Review Focus

1. A page served with HTTP 200 and `text/html` in the middle of a download (expired hotel Wi-Fi, a host's error page) must never be written into the `.tapa` → job fails `CaptivePortal` and the partial bytes before it are intact. Test: Task 3 `html_mid_download_is_not_written`.
2. A host that allows two connections and answers the third with 429 or 403 must not fail the job — it finishes with fewer connections and the next download to that host starts within the limit. Test: Task 6 `connection_limit_is_learned` (+ `_with_403` variant).
3. When every mirror but one is broken, the download completes from the healthy one; when all are broken, it fails with the primary's error, not a mirror's. Test: Task 7 `all_mirrors_broken_reports_primary_error`.
4. A queue that holds paused and failed jobs is still "idle" for when-done; a new download added during the countdown cancels it. Test: Task 10 `paused_jobs_do_not_block_when_done`, `adding_during_countdown_cancels`.
5. A signed download URL that expires after a few requests (CDN-style redirect) still completes, because every segment request re-follows the original link. Test: Task 1 server feature + Task 7 `expiring_redirect_target_still_completes`.

---

## File Structure

```
crates/taurus-testkit/src/lib.rs          # + pages, redirect cycles, connection limits, fail_after, header capture
crates/taurus-core/src/
  errors.rs                               # + ChecksumMismatch, backoff_delay_with()
  timing.rs                               # Timing (all durations/attempt counts in one place)
  settings.rs                             # Settings, SpeedLimit, WhenDone (+ serde, defaults)
  store.rs                                # + schema v2 migration, host_flags, settings JSON, job extras
  page.rs                                 # classify HTML responses: bot check, quota, confirm form, captive portal
  fixups.rs                               # Dropbox / GitHub / Google Drive URL rewrites
  checksum.rs                             # parse sums, SHA256SUMS lookup, streaming hash
  limit.rs                                # shared token-bucket Limiter + peak tracker
  system/mod.rs
  system/space.rs                         # free space, filesystem kind, FAT32 rule
  system/marks.rs                         # Zone.Identifier / quarantine / xdg origin
  system/power.rs                         # sleep/shutdown commands, PowerMode
  system/awake.rs                         # keep-awake guard
  http/probe.rs                           # + resolve(): page classification, one confirm-form hop
  http/download.rs                        # + limiter, mirrors, adaptive cap, html/bot guard, finalize extras
  engine.rs                               # + queue, waiting retries, settings, when-done, keep-awake
crates/taurus-core/tests/{download.rs,engine.rs,queue.rs}
crates/taurus-cli/src/{main.rs,render.rs}, tests/cli.rs
```

---

### Task 1: Testkit extensions

**Files:**
- Modify: `crates/taurus-testkit/src/lib.rs`

**Interfaces:**
- Produces (additions only; existing API unchanged):

```rust
pub struct FailAfter { pub after_requests: usize, pub status: u16, pub headers: Vec<(String, String)> }
// FileSpec gains (FileSpec::new defaults in brackets):
pub max_concurrent: Option<usize>,      // [None] requests beyond this many in flight get `over_limit_status`
pub over_limit_status: u16,             // [429]
pub fail_after: Option<FailAfter>,      // [None] every request after N answers status+headers, body "<html><body>blocked</body></html>"
pub extra_headers: Vec<(String, String)>, // [empty] added to every successful response
impl TestServer {
    pub fn serve_page(&self, path: &str, status: u16, headers: &[(&str, &str)], html: &str) -> String; // text/html; charset=utf-8
    pub fn redirect_cycle(&self, path: &str, targets: &[&str]) -> String; // 302 to targets[n % len] on the n-th request
    pub fn peak_concurrency(&self, path: &str) -> usize;   // max simultaneous in-flight bodies seen
    pub fn last_request_headers(&self, path: &str) -> Vec<(String, String)>; // lower-cased names
    pub fn over_limit_rejections(&self, path: &str) -> usize;
}
```

- [ ] **Step 1: Write failing tests** in the testkit:
  - `max_concurrent_rejects_extra`: `max_concurrent=Some(1)`, a throttled first request in flight → a second request gets 429; `over_limit_rejections == 1`; `peak_concurrency == 1`.
  - `fail_after_switches_status_and_headers`: `fail_after = {after_requests: 2, status: 403, headers: [("cf-mitigated","challenge")]}` → requests 1–2 succeed, request 3 is 403 with that header.
  - `serve_page_returns_html`: status/headers/body as given, content type `text/html; charset=utf-8`.
  - `redirect_cycle_rotates`: three GETs (no redirect following) → `location` headers `targets[0]`, `[1]`, `[0]` for two targets.
  - `records_request_headers`: send `Referer: https://a.example/` → `last_request_headers` contains `("referer","https://a.example/")`.
- [ ] **Step 2: Run** `cargo test -p taurus-testkit` → FAIL. **Step 3: Implement** (an in-flight counter decremented when the body stream drops). **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `test(testkit): pages, redirects, connection limits, header capture`.

---

### Task 2: Timing, settings, error additions, store v2

**Files:**
- Create: `crates/taurus-core/src/timing.rs`, `crates/taurus-core/src/settings.rs`
- Modify: `errors.rs`, `store.rs`, `lib.rs` (re-export `Settings, SpeedLimit, WhenDone, Timing`)

**Interfaces:**
- Produces:

```rust
// timing.rs
#[derive(Clone, Debug)]
pub struct Timing {
    pub backoff_base: Duration,        // 500 ms
    pub backoff_cap: Duration,         // 30 s
    pub max_segment_attempts: u32,     // 8
    pub waiting_retry_first: Duration, // 60 s
    pub waiting_retry_cap: Duration,   // 10 min
    pub when_done_countdown: Duration, // 60 s
}
impl Default for Timing { /* values above */ }
impl Timing { pub fn fast_for_tests() -> Timing; } // 10 ms, 50 ms, 2, 100 ms, 400 ms, 300 ms

// errors.rs
ErrorKind::ChecksumMismatch            // as_str "checksum_mismatch", not transient
pub fn backoff_delay_with(attempt: u32, base: Duration, cap: Duration) -> Duration; // backoff_delay = with(500ms, 30s)

// settings.rs
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SpeedLimit { Fastest, Balanced, Gentle, Custom { bytes_per_sec: u64 } }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WhenDone { Nothing, Notify, Sleep, Shutdown, Quit }
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings { pub max_active: u32, pub speed: SpeedLimit, pub when_done: WhenDone, pub auto_checksum: bool, pub keep_awake: bool }
impl Default for Settings { /* 3, Fastest, Nothing, true, true */ }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerMode { Execute, DryRun }   // used by EngineConfig (Task 9) and system::power (Task 10)
impl SpeedLimit { pub fn bytes_per_sec(&self, measured_peak_bps: Option<u64>) -> Option<u64>; } // None = unlimited

// store.rs (schema v2; v1 databases migrate in place)
JobState::Waiting                       // stored "waiting"
// JobRecord + NewJob gain:
pub mirrors: Vec<String>, pub checksum: Option<String>, pub referer: Option<String>, pub queue_pos: i64,
impl Store {
    pub fn load_settings(&self) -> Result<Settings, CoreError>;          // missing -> Default
    pub fn save_settings(&self, s: &Settings) -> Result<(), CoreError>;
    pub fn measured_peak(&self) -> Result<Option<(u64, i64 /*unix secs*/)>, CoreError>;
    pub fn save_measured_peak(&self, bps: u64, at: i64) -> Result<(), CoreError>;
    pub fn host_cap(&self, host: &str) -> Result<Option<u32>, CoreError>;
    pub fn set_host_cap(&self, host: &str, cap: u32) -> Result<(), CoreError>;
}
```

Schema v2 migration: `ALTER TABLE jobs ADD COLUMN mirrors TEXT NOT NULL DEFAULT '[]'` (JSON array), `checksum TEXT`, `referer TEXT`, `queue_pos INTEGER NOT NULL DEFAULT 0` (backfill `= id`); `CREATE TABLE host_flags(host TEXT PRIMARY KEY, max_connections INTEGER NOT NULL, updated_at INTEGER NOT NULL)`; settings stored under keys `settings` (JSON) and `measured_peak` (`"<bps>,<unix secs>"`). `PRAGMA user_version = 2`.

- [ ] **Step 1: Write failing tests:**
  - `speed_presets`: with peak `Some(40_000_000)`: Gentle → `Some(10_000_000)`, Balanced → `Some(24_000_000)`; with `None`: Gentle → `Some(1_048_576)`, Balanced → `Some(5_242_880)`; Fastest → `None`; Custom 123 → `Some(123)`.
  - `settings_default_and_partial_json`: `serde_json::from_str::<Settings>(r#"{"max_active":5}"#)` → max_active 5, others default.
  - `checksum_mismatch_kind`: roundtrip `as_str`/`from_str`, not transient.
  - `backoff_with_respects_base_and_cap`: base 10 ms → attempt 0 in 8..=12 ms; cap 50 ms → attempt 10 ≤ 50 ms.
  - Store: `v1_database_migrates_to_v2` (create a v1 DB with the M1 schema SQL and one job, open → job has `mirrors == []`, `queue_pos == id`, `user_version == 2`); `settings_roundtrip`; `host_cap_roundtrip`; `waiting_state_roundtrip`; `job_extras_roundtrip` (mirrors, checksum, referer).
- [ ] **Step 2: Run** `cargo test -p taurus-core` → FAIL. **Step 3: Implement** (keep the M1 schema SQL as a test fixture for the migration test). **Step 4: Run** → PASS (all existing tests too).
- [ ] **Step 5: Commit** `feat(core): timing, settings, schema v2`.

---

### Task 3: Web pages, bot checks, quota pages, confirm forms

**Files:**
- Create: `crates/taurus-core/src/page.rs`
- Modify: `http/probe.rs`, `http/download.rs` (`check_response`)

**Interfaces:**
- Produces:

```rust
// page.rs
#[derive(Clone, Debug, PartialEq)]
pub enum PageVerdict {
    BotCheck,            // -> ErrorKind::BlockedByBotCheck
    QuotaExceeded,       // -> ErrorKind::QuotaExceeded
    ConfirmForm(url::Url),
    CaptivePortal,       // -> ErrorKind::CaptivePortal
    WebPage,             // -> ErrorKind::SiteNotSupported, detail "this link is a web page"
}
pub fn is_html(content_type: Option<&str>) -> bool;           // text/html or application/xhtml+xml
pub fn is_bot_check(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> bool;
pub fn classify(requested: &url::Url, status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> PageVerdict;
pub const MAX_HTML_BYTES: usize = 65_536;

// probe.rs
pub async fn resolve(client: &reqwest::Client, url: &str, referer: Option<&str>) -> Result<Probe, CoreError>;
// probe() + page handling; follows at most ONE ConfirmForm; every other verdict becomes the mapped CoreError.
```

Rules (in this order):
1. Bot check: header `cf-mitigated: challenge`, or (`server` header contains `cloudflare` and body contains `Just a moment` or `challenge-platform`). Checked for every status, before the generic status error.
2. Quota: body (case-insensitive) contains `too many users have viewed or downloaded this file recently` or both `quota` and `exceeded`.
3. Confirm form: exactly one `<form>` whose method is GET (or absent) and that has a hidden input named `confirm`; URL = form `action` resolved against the page URL, query = every `<input>` name/value in the form (`scraper`).
4. Captive portal: the requested URL's last path segment ends with a binary extension (`iso img zip 7z rar gz xz bz2 zst tar exe msi dmg pkg deb rpm appimage apk mp4 mkv mov avi webm mp3 flac m4a pdf bin`).
5. Otherwise `WebPage`.

In `download.rs::check_response`: a bot-check response → `BlockedByBotCheck` (not transient); a 200/206 whose `Content-Type` is HTML while the job's probe type was not HTML → `CaptivePortal` (not transient) — checked before any byte is written.

- [ ] **Step 1: Write failing tests** (unit tests in `page.rs` with literal HTML; integration in `tests/download.rs` with `serve_page`):
  - `cloudflare_header_is_bot_check`, `cloudflare_body_is_bot_check` (403, `server: cloudflare`, "Just a moment..."), `plain_403_is_not_bot_check`.
  - `quota_page_detected` with the Google Drive sentence.
  - `confirm_form_builds_url`: `<form action="/download" method="get"><input type="hidden" name="id" value="X"><input type="hidden" name="confirm" value="t"><input type="hidden" name="uuid" value="u"></form>` at `https://h.example/view` → `https://h.example/download?id=X&confirm=t&uuid=u`.
  - `binary_url_html_is_captive_portal` (`/files/a.iso`) vs `page_url_html_is_web_page` (`/files/`).
  - Integration: `resolve_follows_one_confirm_form` (page at `/drive` with a form to `/file?confirm=t`, `/file` serves 2 MiB → `Probe.size == Some(2 MiB)`); `resolve_bot_check_fails` (`serve_page` 403 + `cf-mitigated: challenge` → `BlockedByBotCheck`); `resolve_quota_fails`; `resolve_web_page_fails` (`SiteNotSupported`).
  - `html_mid_download_is_not_written`: 8 MiB file with `fail_after = {after_requests: 3, status: 200, headers: [("content-type","text/html")]}` → `Failed(CaptivePortal)`; every byte in the `.tapa` up to the recorded durable progress equals `content(&spec)` at the same offset.
  - `bot_check_mid_download_fails`: `fail_after` 403 + `cf-mitigated: challenge` → `Failed(BlockedByBotCheck)`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** The engine switches from `probe` to `resolve` in `add` and `resume`. **Step 4: Run** `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit** `feat(core): detect web pages, bot checks and quota pages; follow confirm forms`.

---

### Task 4: Link fixups

**Files:**
- Create: `crates/taurus-core/src/fixups.rs`; Modify: `engine.rs` (`add` applies `fixups::rewrite` to `req.url` before `resolve`; the stored `url` is the rewritten one)

**Interfaces:**
- Produces: `pub fn rewrite(url: &url::Url) -> Option<url::Url>` (None = leave unchanged).

Rules:
- Dropbox (`www.dropbox.com`, `dropbox.com`): set query `dl=1` (replace `dl=0` or add it), keep other params.
- GitHub: `github.com/<owner>/<repo>/blob/<ref>/<path...>` → `https://raw.githubusercontent.com/<owner>/<repo>/<ref>/<path...>`.
- Google Drive: `drive.google.com/file/d/<ID>/<anything>` or `drive.google.com/open?id=<ID>` or `drive.google.com/uc?id=<ID>` → `https://drive.usercontent.google.com/download?id=<ID>&export=download`. (The virus-scan page that may follow is handled by Task 3's confirm form.)

- [ ] **Step 1: Write failing unit tests** for each rule plus `unrelated_url_unchanged` (`https://example.org/a.zip` → None) and `dropbox_keeps_other_params` (`?rlkey=abc&dl=0` → `?rlkey=abc&dl=1`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): link fixups for Dropbox, GitHub and Google Drive`.

---

### Task 5: Shared speed limiter and peak measurement

**Files:**
- Create: `crates/taurus-core/src/limit.rs`
- Modify: `http/download.rs` (`JobContext` gains `limiter: Limiter`, `timing: Timing`; each worker calls `limiter.acquire(n).await` before writing a chunk), `engine.rs` (owns one `Limiter`, applies `Settings::speed`, feeds `PeakTracker`)

**Interfaces:**
- Produces:

```rust
#[derive(Clone)]
pub struct Limiter { /* Arc: AtomicU64 rate (0 = unlimited), Mutex<bucket> */ }
impl Limiter {
    pub fn new(bytes_per_sec: Option<u64>) -> Limiter;
    pub fn set_rate(&self, bytes_per_sec: Option<u64>);     // takes effect for the next acquire
    pub async fn acquire(&self, bytes: u64);                  // waits until tokens allow; bucket size = 1 s of rate (min 64 KiB)
}
pub struct PeakTracker { /* rolling 5 s window of aggregate bytes */ }
impl PeakTracker {
    pub fn new() -> PeakTracker;
    pub fn record(&mut self, now: std::time::Instant, total_downloaded_all_jobs: u64) -> Option<u64>; // Some(new 5-s average) when it beats the previous max
}
```

Engine rule: only while `Settings::speed == Fastest`, a new peak from `PeakTracker` is saved with `store.save_measured_peak`; a stored peak older than 7 days is replaced by the next measurement even if lower. `set_settings` recomputes the rate immediately.

- [ ] **Step 1: Write failing tests:**
  - `limiter_unlimited_does_not_wait` and `limiter_enforces_rate` (`#[tokio::test(start_paused = true)]`: rate 1_000_000, acquire 3_000_000 in 64 KiB pieces → virtual elapsed between 1.9 s and 3.1 s).
  - `set_rate_applies_live` (paused time: switch from 100_000 to unlimited mid-way → remaining acquires return immediately).
  - `peak_tracker_reports_new_max_only`.
  - Integration (`tests/engine.rs`): `custom_limit_slows_download` — 6 MiB unthrottled server, `Custom{bytes_per_sec: 2 MiB}` → wall time ≥ 2.5 s, hash matches; `limit_change_applies_to_running_job` — start at 1 MiB/s on 8 MiB, after 1 s set Fastest → completes within 4 s.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): shared speed limiter with peak-relative presets`.

---

### Task 6: Adaptive connection count

**Files:**
- Modify: `http/download.rs`, `engine.rs`

**Interfaces:**
- Consumes: `Store::host_cap`, `Store::set_host_cap` (Task 2).
- Decisions:
  - A job starts with `min(job.connections, host_cap(final_url host).unwrap_or(u32::MAX))` workers.
  - During a round, if a worker's response is 429, 503 or 403 **while at least one other worker of the same job has an open 206 response**, it is a connection limit, not an error: the round's worker cap becomes `max(1, active_workers - 1)`, the segment goes back to the pending pool (its durable progress kept), `set_host_cap(host, new_cap)` is stored, and no attempt is counted.
  - A 429/503/403 with no other active worker keeps the M1 behavior (retry transient / fail non-transient).

- [ ] **Step 1: Write failing tests** (`tests/download.rs`):
  - `connection_limit_is_learned`: 20 MiB, `max_concurrent=Some(2)` → Completed, hash matches, `host_cap == Some(2)` (or 1), then a second download of a second path on the same server → `over_limit_rejections` for that path == 0 and `peak_concurrency <= 2`.
  - `connection_limit_is_learned_with_403`: same with `over_limit_status = 403`.
  - `single_connection_429_is_still_rate_limited`: `connections=1`, `fail_first=vec![429; 50]` with `Timing::fast_for_tests()` → `Failed(RateLimited)` (no cap change).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): learn per-host connection limits`.

---

### Task 7: Mirrors and Referer

**Files:**
- Modify: `engine.rs` (`AddRequest` gains `mirrors: Vec<String>`, `checksum: Option<String>`, `referer: Option<String>`; derive `Default`), `http/download.rs`, `http/probe.rs` (send `Referer` when given)

**Interfaces:**
- Decisions:
  - At `add`, each mirror is resolved; a mirror whose size differs from the primary, or whose ETag differs when both have one, is dropped and noted with `tracing::info!`. Surviving mirrors are stored in `JobRecord::mirrors`.
  - URL list = `[url] + mirrors`. Segment `i`'s worker uses `urls[i % len]`. A URL that returns a non-transient error, or fails `max_segment_attempts` times, is marked bad for this run; its segment continues on the next good URL from its durable position.
  - The job fails only when every URL is bad; the error reported is the primary URL's last error (or the last error seen if the primary never failed).
  - Every request of the job (probe and segments) sends `Referer` when `referer` is set.

- [ ] **Step 1: Write failing tests:**
  - `mirrors_share_the_work`: two `TestServer`s with the same spec (seed 1), 16 MiB, 4 connections → Completed, hash matches, `bytes_served` > 0 on both.
  - `broken_mirror_is_skipped`: mirror path with `fail_first = vec![404; 100]` → Completed from the primary.
  - `all_mirrors_broken_reports_primary_error`: primary `fail_after` 410 after the probe, mirror 404 → `Failed(LinkExpired)` with the primary's detail ("410").
  - `mismatched_mirror_dropped_at_add`: mirror with a different size → job's `mirrors` is empty.
  - `expiring_redirect_target_still_completes`: `redirect_cycle("/dl", ["/s1", "/s2"])`, `/s1` and `/s2` both 8 MiB seed 1 with `fail_after = {after_requests: 3, status: 403}` → Completed, hash matches.
  - `referer_is_sent`: `last_request_headers` contains the referer on the segment path.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): mirrors and Referer for downloads`.

---

### Task 8: Pre-flight checks and finalize extras

**Files:**
- Create: `crates/taurus-core/src/checksum.rs`, `crates/taurus-core/src/system/{mod.rs,space.rs,marks.rs}`
- Modify: `engine.rs` (pre-flight in `add` and `resume`), `http/download.rs` (finalize)

**Interfaces:**
- Produces:

```rust
// checksum.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sha256Sum(pub String);                                // 64 lowercase hex
pub fn parse_sum(s: &str) -> Option<Sha256Sum>;                   // "sha256:<hex>" or bare 64 hex, any case
pub fn find_in_sums_file(text: &str, file_name: &str) -> Option<Sha256Sum>; // lines "<hex>  name" or "<hex> *name"
pub fn sums_url(final_url: &url::Url) -> Option<url::Url>;        // same directory + "SHA256SUMS"
pub async fn hash_file(path: &std::path::Path) -> std::io::Result<Sha256Sum>; // spawn_blocking, 1 MiB buffer
// system/space.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind { Fat32, Other }
pub fn fs_kind(dir: &std::path::Path) -> FsKind;     // Linux statfs f_type 0x4d44; macOS f_fstypename "msdos"; Windows GetVolumeInformationW name "FAT32"/"FAT"
pub fn check_fits(dir: &std::path::Path, remaining: u64, total: Option<u64>) -> Result<(), CoreError>;
pub fn check_fits_with(available: u64, fs: FsKind, remaining: u64, total: Option<u64>) -> Result<(), CoreError>; // pure rule
// system/marks.rs
pub fn mark_downloaded(path: &std::path::Path, url: &str, referer: Option<&str>);
```

Decisions:
- Pre-flight (`add` before the job row; `resume` before restarting): `check_fits(dest_dir, total - downloaded, total)` when `total` is known. Errors: `DiskFull` (available < remaining + 16 MiB), `FileTooLargeForDisk` (FAT32 and total > 4_294_967_295).
- Finalize order: length check → checksum (explicit `checksum`, else when `auto_checksum` fetch `sums_url` with the job's client: 404/any error/over 1 MiB → skip; look up the **URL's** last path segment, not the deduped local name) → on mismatch delete the `.tapa`, clear segments, `Failed(ChecksumMismatch)` with detail `expected <hex>, got <hex>` → rename → `mark_downloaded(final, job.url, referer)`.
- Marks: Windows writes `<file>:Zone.Identifier` = `"[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=<referer>\r\nHostUrl=<url>\r\n"` (omit ReferrerUrl when None); macOS sets xattr `com.apple.quarantine` = `format!("0081;{:08x};Hyades;", unix_secs)`; Linux sets `user.xdg.origin.url` (and `user.xdg.referrer.url`).

- [ ] **Step 1: Write failing tests:**
  - Unit: `parse_sum_forms`, `find_in_sums_file_both_formats` (text and `*binary` markers, missing name → None), `sums_url_same_dir` (`https://h/a/b/x.iso?t=1` → `https://h/a/b/SHA256SUMS`), `fits_rules` (`check_fits_with`: available 100 MiB remaining 90 MiB → DiskFull; FAT32 with total 5 GiB → FileTooLargeForDisk; Other with 5 GiB and enough space → Ok), `tempdir_is_not_fat32`.
  - Integration: `explicit_checksum_ok`, `explicit_checksum_mismatch_fails_and_removes_partial` (no final file, no `.tapa`), `auto_sums_file_verifies` (serve `/rel/SHA256SUMS` as a page with the right line and `/rel/x.iso`), `auto_sums_missing_is_skipped`, `auto_sums_mismatch_fails`.
  - Marks, per platform with `#[cfg]`: Windows reads `<file>:Zone.Identifier` and asserts `ZoneId=3` and `HostUrl=`; macOS reads the xattr and asserts it starts with `0081;` and contains `;Hyades;`; Linux reads `user.xdg.origin.url` (skip the assertion when setting returns `ENOTSUP`/`EOPNOTSUPP`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS (on CI for all three OSes).
- [ ] **Step 5: Commit** `feat(core): free-space and FAT32 checks, checksums, OS safety marks`.

---

### Task 9: Queue and "waiting for connection"

**Files:**
- Modify: `engine.rs`; Test: `crates/taurus-core/tests/queue.rs`

**Interfaces:**
- Produces (Engine additions):

```rust
pub struct EngineConfig { pub data_dir: PathBuf, pub timing: Timing, pub power: PowerMode } // EngineConfig::new(data_dir) = Default timing, PowerMode::Execute
impl Engine {
    pub async fn start_now(&self, id: JobId) -> Result<(), CoreError>;  // starts a Queued/Waiting job even above max_active
    pub async fn set_settings(&self, s: Settings) -> Result<(), CoreError>;
    pub fn settings(&self) -> Settings;
}
// Event gains:
Event::QueueIdle, Event::SettingsChanged(Settings)
```

Decisions:
- `add` creates the job `Queued` (queue_pos = id) and calls the scheduler. `resume` of a Paused/Failed job sets it `Queued`. The scheduler starts Queued jobs in `queue_pos` order while `Downloading` count < `max_active`. `start_now` jobs count toward the total but bypass the limit.
- A job ending `Failed` with a transient kind (`is_transient()`) becomes `Waiting` with the error kept, and is re-queued after `waiting_retry_first * 2^n` (cap `waiting_retry_cap`; `n` resets when the job makes progress). `pause` cancels the timer.
- `QueueIdle` fires once each time the count of `Downloading` + `Queued` + `Waiting` jobs drops to zero. Paused, Failed and Completed jobs do not block it.
- On `Engine::open`, jobs left `Downloading` become `Paused` (as in M1); `Queued` and `Waiting` stay and the scheduler starts them.

- [ ] **Step 1: Write failing tests** (`tests/queue.rs`, `Timing::fast_for_tests()`):
  - `max_active_limits_concurrency`: max_active 2, four 4 MiB jobs throttled at 2 MiB/s → sampling `snapshot()` every 50 ms never shows > 2 `Downloading`; all Completed; one `QueueIdle`.
  - `start_now_bypasses_limit`: max_active 1, two jobs → `start_now(second)` → two `Downloading` at once.
  - `queue_order_is_fifo`: max_active 1, jobs A, B, C → `JobUpdated(Downloading)` events in order A, B, C.
  - `transient_failure_waits_then_recovers`: `fail_first = vec![503; 6]` → a `JobUpdated` with `Waiting` and error kind `RateLimited`, then Completed.
  - `pause_cancels_waiting_retry`: a job in `Waiting`, `pause` → stays Paused after `waiting_retry_cap` elapses.
  - `raising_max_active_starts_more`: max_active 1 with 3 queued → `set_settings(max_active 3)` → three Downloading.
  - `settings_persist_across_reopen`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit** `feat(core): download queue with waiting retries`.

---

### Task 10: When-done actions and keep-awake

**Files:**
- Create: `crates/taurus-core/src/system/{power.rs,awake.rs}`
- Modify: `engine.rs`

**Interfaces:**
- Produces:

```rust
// system/power.rs (PowerMode comes from settings.rs, Task 2)
pub fn command_for(action: WhenDone) -> Option<(String, Vec<String>)>; // only Sleep/Shutdown return Some
// macOS:   Sleep -> pmset sleepnow;  Shutdown -> osascript -e 'tell application "System Events" to shut down'
// Windows: Sleep -> rundll32.exe powrprof.dll,SetSuspendState 0,1,0;  Shutdown -> shutdown /s /t 0
// Linux:   Sleep -> systemctl suspend;  Shutdown -> systemctl poweroff
// system/awake.rs
pub struct AwakeGuard { /* Option<keepawake::KeepAwake> */ }
impl AwakeGuard { pub fn new() -> Self; pub fn set(&mut self, want: bool); pub fn is_requested(&self) -> bool; }
// keepawake::Builder::default().idle(true).reason("Downloading files").app_name("Hyades").app_reverse_domain("dev.nikethai.hyades").create(); errors -> tracing::warn!, keep is_requested = true
// engine.rs
impl Engine {
    pub async fn cancel_when_done(&self) -> Result<(), CoreError>;
    pub fn keeping_awake(&self) -> bool;
}
// Event gains:
Event::WhenDoneCountdown { action: WhenDone, seconds_left: u64 }, // emitted at start and every second
Event::WhenDoneCancelled,
Event::WhenDoneFired { action: WhenDone, command: Option<String> }, // command line that ran (or would run in DryRun)
```

Decisions:
- On `QueueIdle` with `when_done` = Sleep or Shutdown → countdown of `timing.when_done_countdown`; then run `command_for` (Execute) or not (DryRun) and emit `WhenDoneFired`. Notify and Quit fire `WhenDoneFired` immediately (front ends act on them); Nothing does nothing.
- `add`, `resume`, `start_now`, `cancel_when_done` and a settings change of `when_done` cancel a running countdown (`WhenDoneCancelled`).
- Keep-awake: `AwakeGuard::set(settings.keep_awake && any job Downloading)` after every state change.

- [ ] **Step 1: Write failing tests:**
  - Unit: `commands_per_platform` (`#[cfg(target_os = ...)]` per OS: exact program and args; Notify/Quit/Nothing → None).
  - Engine (DryRun, fast timing): `sleep_fires_after_countdown` (events in order `QueueIdle`, `WhenDoneCountdown`, `WhenDoneFired{Sleep, Some(cmd)}`), `notify_fires_immediately`, `adding_during_countdown_cancels`, `cancel_when_done_stops_countdown`, `paused_jobs_do_not_block_when_done` (one job paused, one completes → fires), `keep_awake_follows_downloads` (`keeping_awake()` true while a throttled job runs, false after; false throughout when `keep_awake = false`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): when-done actions and keep-awake`.

---

### Task 11: CLI as a queue runner

**Files:**
- Modify: `crates/taurus-cli/src/{main.rs,render.rs}`, `crates/taurus-cli/tests/cli.rs`, `README.md`

**Interfaces:**
- Produces:

```
taurus [--data-dir <PATH>] [--json] [--dry-run-power] <COMMAND>
  add <URL>... [--file <PATH>] [--dir <PATH>] [--connections <N>] [--name <NAME>] [--mirror <URL>]... [--checksum <SUM>] [--referer <URL>]
  resume <ID>...
  run                         # run queued/waiting jobs until idle
  start-now <ID>
  list
  remove <ID> [--delete-files]
  config get [<KEY>]
  config set <KEY> <VALUE>    # max_active <N> | speed fastest|balanced|gentle|<N>KB/s|<N>MB/s | when_done nothing|notify|sleep|shutdown|quit | auto_checksum on|off | keep_awake on|off
```

Decisions:
- `add`, `resume` and `run` enqueue, then stay in the foreground until `QueueIdle` (and, if a when-done countdown starts, until it fires or is cancelled). Ctrl-C pauses every running job and exits 2. Exit 0 when every job touched by this run Completed, 1 if any Failed.
- `--file` reads one URL per line, ignoring blank lines and lines starting with `#`. `--name`, `--mirror` and `--checksum` are only valid with exactly one URL (clap error otherwise).
- `WhenDoneFired{Notify}` prints `All downloads finished.`; `Quit` exits immediately with the normal exit code.
- `config` reads/writes through `Engine::settings` / `set_settings`; `config get` prints `key = value` lines (or one JSON object with `--json`).
- Human progress shows one line per running job; `Waiting` shows `waiting for connection (retry in 1m)`.

- [ ] **Step 1: Write failing tests** (`tests/cli.rs`):
  - `add_several_urls_and_file`: two URLs on the command line plus a `--file` with two more (and a comment line) → exit 0, four files with matching hashes.
  - `config_roundtrip`: `config set max_active 2`, `config set speed 5MB/s` → `config get --json` shows `max_active: 2`, `speed: {"Custom":{"bytes_per_sec":5000000}}`.
  - `invalid_config_value_exits_1` (`speed fast` → exit 1, stderr names the allowed values).
  - `mirror_with_two_urls_is_rejected` (clap usage error, exit 2 from clap — assert non-zero and message).
  - `when_done_dry_run_prints_command`: `config set when_done sleep`, then `--dry-run-power add URL` with fast timing env `TAURUS_FAST_TIMING=1` (test-only switch read by the CLI to use `Timing::fast_for_tests()`) → stdout contains the platform's sleep command.
  - Existing M1 CLI tests still pass (update for the queue runner where needed).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --workspace`, clippy, fmt → clean.
- [ ] **Step 5: README** — document the new commands and config keys.
- [ ] **Step 6: Commit** `feat(cli): queue runner, batch add, config and when-done (M2)`.

---

## Done criteria (spec M2: "HTTP edge-case tests pass")

- CI green on Linux, macOS, Windows, including the platform-specific marks and power-command tests.
- Every test named in this plan passes; the five Review Focus tests are present.
- Spec section 7 "HTTP" and "Hosts" cases are all covered by a test in M1 or M2, except: 401 credential prompt and manual Referer UI (M6 Advanced UI), Google Drive end-to-end against the real site (manual check only).

Deferred to later plans: user-facing message strings per `ErrorKind` (M4, with i18n), asking before adding a URL that is already in the list (M4 UI), manual proxy setting (M6 Advanced settings; the system proxy already works via reqwest's default `system-proxy` feature), cookie / exact-User-Agent handoff from the browser (M8), metered-connection hold (later list).
