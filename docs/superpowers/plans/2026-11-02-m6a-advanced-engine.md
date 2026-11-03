# M6a: Advanced Engine Features — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the engine and CLI everything Advanced mode needs: grab links from a page, a manual proxy, passwords for protected downloads, browser cookies / subtitles / container choice for videos, converting finished files, changing a torrent's file selection and per-torrent sharing rules — plus the two M5 leftovers (stale tools banner, video speed limit not following changes). The Advanced screens themselves are M6b.

**Architecture:** Everything lands behind the existing `Engine` API. New pure modules (`grab/parse.rs`, `network.rs` proxy parsing, `convert/args.rs`) are unit-tested; network-facing pieces are tested offline with `taurus-testkit` (TestServer pages, a proxy-mode TestServer, the fake tools from M5). Secrets go through a `SecretStore` trait backed by the OS keychain (`keyring`) with an in-memory fallback, and never touch SQLite or logs.

**Tech Stack:** as M5, plus `keyring` 4.2 (default `v1` feature: `Entry::new(service, user)`, `set_password`, `get_password`, `delete_credential`), reqwest's `socks` feature, `rpassword` 7.5 (CLI password prompt). Checked on crates.io on 2026-11-02.

**Spec:** `docs/superpowers/specs/2026-10-27-taurus-v2-design.md` — 6.1 (credentials, Referer), 6.4 (torrent settings), 6.5 (cookies, convert), 6.6 (grab links), 6.7 (manual proxy), 7 (401, hotlink, folder listings), 9 row M6. Deferred lists at the ends of the M3, M4 and M5 plans.

## Global Constraints

- All earlier Global Constraints hold (offline tests, CI on 3 OSes, fmt + clippy `-D warnings`, `rust-version = "1.89"`, test hooks only in debug builds).
- Passwords are stored only in the OS keychain (service `dev.nikethai.taurus`, account `job:<id>`); when no keychain is available they live in memory for the session and the job asks again after a restart. Never in SQLite, settings JSON, events, `JobView`, or logs.
- Proxy setting: `System` (default; reqwest's `system-proxy`), `None`, or `Manual(url)` with scheme `http`, `https` or `socks5`/`socks5h`. It applies to HTTP jobs, probes, grab, tool downloads and yt-dlp (`--proxy`). Torrents use it only when it is `socks5`/`socks5h` (librqbit `ConnectionOptions::proxy_url`), and only from the next session start (CLI prints the restart note). Changing it rebuilds the shared HTTP client for new requests.
- Grab: page ≤ 5 MiB of HTML; links only from `a[href]`, `img[src]`, `video[src]`, `audio[src]`, `source[src]`; subfolders only one level deep, only same scheme+host, and only paths under the page's own directory; at most 500 links returned; size probes at most 4 at a time, 10 s each.
- Convert never overwrites: output `<stem> (mp3).mp3` or `<stem> (smaller).mp4` next to the source via `unique_path`; the source is never modified; a failed conversion deletes only its own partial output.
- Per-torrent sharing overrides fall back to the global torrent settings field by field.

## Review Focus

1. A password must never end up in the database file, settings, events or logs. Test: Task 2 `password_never_persisted_or_logged` (scan the DB file bytes and captured tracing output for the password).
2. Grab with subfolders must stay inside the page's folder on the same host — a listing linking to `../`, another host, or `/` must not be crawled. Test: Task 1 `subfolders_stay_inside`.
3. A manual proxy must apply to every network path, and a malformed proxy URL must be rejected when set, not at the first download. Test: Task 3 `proxy_applies_everywhere`, `bad_proxy_rejected`.
4. Converting must never touch the original or another file, even when the target name exists or ffmpeg fails halfway. Test: Task 5 `convert_never_overwrites`, `failed_convert_keeps_source`.
5. Changing a torrent's file selection mid-download must not delete files already downloaded for still-selected entries, and must re-check free space for newly added files. Test: Task 6 `reselect_keeps_done_files_and_checks_space`.

---

## File Structure

```
crates/taurus-testkit/src/lib.rs         # + proxy mode (absolute-form requests) and basic-auth paths
crates/taurus-core/src/
  grab/mod.rs  grab/parse.rs             # page -> links (pure parse + fetch), folder listings
  network.rs                              # ProxySetting parsing, client building
  secrets.rs                              # SecretStore trait, KeyringStore, MemoryStore
  convert/mod.rs  convert/args.rs         # ffmpeg conversion jobs
  media/{mod.rs,run.rs,inspect.rs}        # cookies, subtitles, container, full quality list
  torrent/job.rs  engine/torrent.rs       # reselect files, per-torrent overrides
  engine/{types.rs,mod.rs,...}            # new Engine methods
  store/migrate.rs                        # schema v7
crates/taurus-cli/src/                    # grab, convert, torrent-select, job set, new add flags, proxy config
```

---

### Task 1: Grab links from a page

**Files:**
- Create: `crates/taurus-core/src/grab/{mod.rs,parse.rs}`; Modify: `engine/{mod.rs,types.rs}`

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GrabbedLink { pub url: String, pub name: String, pub folder: String /* "/" or "/talks/" relative to the page dir */, pub extension: Option<String> }
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GrabResult { pub page_url: String, pub links: Vec<GrabbedLink>, pub subfolders_scanned: u32, pub truncated: bool }
pub struct GrabOptions { pub subfolders: bool }
// parse.rs (pure)
pub fn extract_links(page_url: &url::Url, html: &str) -> Vec<url::Url>;      // resolved, fragment removed, deduped, order kept
pub fn is_folder_listing(html: &str) -> bool;                                // <title> or <h1> starting "Index of", or Apache/nginx autoindex markers
pub fn is_inside(page_dir: &url::Url, candidate: &url::Url) -> bool;         // same scheme+host(+port), path starts with page dir path, no "..", not the dir itself
pub fn looks_like_file(u: &url::Url) -> bool;                                // last segment has an extension and is not .html/.htm/.php/.asp/.aspx/.jsp
impl Engine {
    pub async fn grab(&self, page_url: &str, opts: GrabOptions) -> Result<GrabResult, CoreError>;
    pub async fn probe_sizes(&self, urls: Vec<String>) -> Vec<Option<u64>>;   // same order; None on error/timeout
}
```

Decisions: `grab` fetches with the engine client (proxy, referer none), rejects non-HTML with `SiteNotSupported` detail "not a web page"; when `subfolders` and the page is a folder listing, links ending in `/` that pass `is_inside` are fetched (one level) and their files added with `folder` = that subfolder; only `looks_like_file` links are returned; truncation at 500 sets `truncated`.

- [ ] **Step 1: Write failing tests:** parse unit tests (`relative_links_resolved`, `fragments_and_duplicates_removed`, `media_and_image_sources_included`, `index_of_detected`, `inside_rules` — same dir yes, `../x` no, other host no, `/` no, `sub/` yes, `https` vs `http` no, `looks_like_file_cases`); engine tests with TestServer (`grab_page_lists_files` — a page with 3 files + 2 html links → 3 links; `subfolders_stay_inside` — listing with `sub/`, `../up/`, `/`, `http://other.host/` links → only `sub/` scanned, `subfolders_scanned == 1`; `grab_rejects_non_html`; `truncates_at_500`; `probe_sizes_in_order` with one 404).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): grab links from a page and folder listings`.

---

### Task 2: Passwords for protected downloads

**Files:**
- Create: `crates/taurus-core/src/secrets.rs`; Modify: `engine/{types.rs,mod.rs,jobs.rs}`, `http/{probe.rs,download/worker.rs}`, `crates/taurus-testkit/src/lib.rs` (`FileSpec::basic_auth: Option<(String, String)>` → 401 with `WWW-Authenticate: Basic realm="test"` unless matched)

**Interfaces:**
- Produces:

```rust
pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, secret: &str) -> Result<(), CoreError>;
    fn get(&self, account: &str) -> Result<Option<String>, CoreError>;
    fn delete(&self, account: &str) -> Result<(), CoreError>;
}
pub struct KeyringStore;   // keyring::Entry::new("dev.nikethai.taurus", account); NoEntry -> Ok(None); platform errors -> Err (caller falls back)
pub struct MemoryStore;    // HashMap behind a Mutex
#[derive(Clone)]
pub struct Credentials { pub username: String, pub password: String } // Debug prints "Credentials { username: .., password: *** }"
// AddRequest gains: pub credentials: Option<Credentials>
// EngineConfig gains: pub secrets: std::sync::Arc<dyn SecretStore>   // default: KeyringStore with MemoryStore fallback
impl Engine { pub async fn set_credentials(&self, id: JobId, creds: Credentials) -> Result<(), CoreError>; } // stores, then resumes the job if it failed with NeedsSignIn
// JobRecord gains (schema v7): auth_user TEXT (username only; password in the SecretStore)
```

Decisions: requests send HTTP Basic auth only when a job has credentials; a 401 without credentials fails `NeedsSignIn` (detail includes the realm); a 401 with credentials fails `NeedsSignIn` with detail "wrong user name or password". Removing a job deletes its secret. Logged URLs and errors keep the existing credential stripping.

- [ ] **Step 1: Write failing tests:** `memory_store_roundtrip`; `debug_hides_password`; `protected_file_without_credentials_needs_sign_in` (realm in detail); `with_credentials_downloads` (hash matches); `set_credentials_resumes_failed_job`; `wrong_password_reports_clearly`; `password_never_persisted_or_logged` (after a download with password `s3cr3t-Pa55`, the bytes of `taurus.db`, `taurus.db-wal` and captured tracing output contain no `s3cr3t-Pa55`); `remove_deletes_secret`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS (CI uses `MemoryStore` via `EngineConfig`).
- [ ] **Step 5: Commit** `feat(core): passwords for protected downloads, kept in the OS keychain`.

---

### Task 3: Manual proxy

**Files:**
- Create: `crates/taurus-core/src/network.rs`; Modify: `settings.rs` (`Settings.network: NetworkSettings { proxy: ProxySetting }`), `http/client.rs`, `engine/settings.rs`, `tools/install.rs`, `media/run.rs` + `media/inspect.rs`, `torrent/hub.rs`, workspace `Cargo.toml` (reqwest `socks`), `crates/taurus-testkit/src/lib.rs` (proxy mode)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProxySetting { System, None, Manual { url: String } }   // default System
pub fn parse_proxy(url: &str) -> Result<url::Url, CoreError>;     // schemes http, https, socks5, socks5h; host required; else ErrorKind::Unknown with detail "unsupported proxy …" — surfaced by set_settings
pub fn build_client(proxy: &ProxySetting) -> reqwest::Client;     // replaces http::client::build_client(); same defaults otherwise
pub fn ytdlp_proxy_args(proxy: &ProxySetting) -> Vec<String>;     // Manual -> ["--proxy", url]; None -> ["--proxy", ""]; System -> []
pub fn torrent_proxy(proxy: &ProxySetting) -> Option<String>;     // Some only for socks5/socks5h
// testkit: TestServer::start_proxy() -> TestServer that answers absolute-form requests ("GET http://any.host/path") by serving `path`, and records them: proxied_requests() -> Vec<String>
```

- [ ] **Step 1: Write failing tests:** `parse_proxy_cases` (http/https/socks5h accepted; `ftp://`, `localhost:8080` without scheme, empty host rejected); `bad_proxy_rejected` (`set_settings` with `Manual{"nope"}` → Err, setting unchanged); `proxy_applies_everywhere` (proxy TestServer; job URL `http://unresolvable.invalid/x.bin` downloads via the proxy; a grab of `http://unresolvable.invalid/page/` works; a tool install with sources on `unresolvable.invalid` works; the fake yt-dlp args log contains `--proxy <url>`); `system_and_none_args` for `ytdlp_proxy_args`; `torrent_proxy_only_socks`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): manual proxy for downloads, tools and videos`.

---

### Task 4: Video options — cookies, subtitles, container, full quality list

**Files:**
- Modify: `crates/taurus-core/src/media/{mod.rs,inspect.rs,run.rs}`, `engine/{types.rs,media.rs}`

**Interfaces:**
- Produces:

```rust
// MediaChoice gains:
pub cookies_from_browser: Option<String>,   // "chrome", "firefox", "safari", "edge", "brave", "chromium", "opera", "vivaldi", optionally ":<profile>"
pub subtitles: Option<SubtitleChoice>,
pub container: Option<Container>,           // video only; default Mp4
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SubtitleChoice { pub languages: Vec<String> /* ["en","vi"] or ["all"] */, pub auto_generated: bool, pub embed: bool }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Container { Mp4, Mkv, Webm }
// MediaInfo gains: pub subtitle_languages: Vec<String>, pub auto_caption_languages: Vec<String>, pub all_qualities: Vec<QualityOption>   // one per distinct height, highest first, ids "<h>p"
pub fn validate_browser(spec: &str) -> Result<(), CoreError>;   // known names only; else Unknown "unknown browser"
impl Engine { pub async fn retry_with_cookies(&self, id: JobId, browser: String) -> Result<(), CoreError>; } // stores the choice in MediaOptions and requeues
```

Args: cookies → `--cookies-from-browser <spec>`; subtitles → `--write-subs`, `--write-auto-subs` (when `auto_generated`), `--sub-langs <a,b|all>`, `--embed-subs` (when `embed`, only for Mp4/Mkv/Webm); container → `--merge-output-format <mp4|mkv|webm>`. Stored in `MediaOptions` (JSON column; old rows deserialize with defaults).

- [ ] **Step 1: Write failing tests:** `parse_subtitle_languages` (fixture with `subtitles` and `automatic_captions` keys); `all_qualities_one_per_height`; `args_for_cookies_subs_container` (fake tool args log: exact flags in order); `validate_browser_cases`; `retry_with_cookies_requeues` (first run `ERROR: Sign in to confirm your age` → NeedsSignIn; `retry_with_cookies(id, "firefox")` → rerun args contain `--cookies-from-browser firefox`, Completed); `old_media_options_json_loads`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): browser cookies, subtitles and container for videos`.

---

### Task 5: Convert finished files

**Files:**
- Create: `crates/taurus-core/src/convert/{mod.rs,args.rs}`; Modify: `store` (`JobKind::Convert`, schema v7 column `convert_source TEXT`), `engine/{types.rs,mod.rs,schedule.rs}`, `crates/taurus-testkit/src/bin/fake-tool.rs` (ffmpeg: with `-progress pipe:1`, print `out_time_ms=` lines for 3 steps then copy the input to the output; `FAKE_FFMPEG_FAIL=1` writes half the output then exits 1)

**Interfaces:**
- Produces:

```rust
pub fn ffmpeg_args(preset: ConvertPreset, input: &std::path::Path, output: &std::path::Path) -> Vec<String>;
// Mp3:        -hide_banner -nostdin -n -i IN -vn -c:a libmp3lame -q:a 2 -progress pipe:1 OUT   (-n: never overwrite; -y is never used)
// SmallerMp4: -hide_banner -nostdin -n -i IN -c:v libx264 -crf 28 -preset veryfast -c:a aac -b:a 128k -movflags +faststart -progress pipe:1 OUT
pub fn output_path(source: &std::path::Path, preset: ConvertPreset, reserved: &std::collections::HashSet<String>) -> std::path::PathBuf; // "<stem> (mp3).mp3", "<stem> (smaller).mp4", via unique_path
impl Engine {
    pub async fn convert(&self, source: ConvertSource, preset: ConvertPreset) -> Result<JobId, CoreError>;
}
pub enum ConvertSource { Job(JobId) /* its finished file; for playlists: every finished file, one job each */, Path(std::path::PathBuf) }
```

Decisions: duration from `ffprobe -v error -show_entries format=duration -of csv=p=0 IN` for progress (unknown → progress by `out_time_ms` only, total None); the job runs in the normal queue (counts toward `max_active`), name `<output file name>`, state flow like HTTP; ffmpeg is ensured first; cancel → kill ffmpeg, delete partial output; done → `mark_downloaded` not applied (local file), Completed with the output path.

- [ ] **Step 1: Write failing tests:** `args_never_overwrite` (contains `-n`, not `-y`); `output_names` (`song.webm` → `song (mp3).mp3`; existing name → ` (1)`); `convert_job_completes` (fake ffmpeg; output exists, source unchanged byte-for-byte, progress events seen); `convert_never_overwrites` (pre-existing `song (mp3).mp3` untouched; new file `song (mp3) (1).mp3`); `failed_convert_keeps_source` (`FAKE_FFMPEG_FAIL=1` → Failed `ToolFailed`, partial output removed, source intact); `convert_playlist_job_makes_one_job_per_file`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): convert finished downloads with ffmpeg`.

---

### Task 6: Torrent file reselection and per-torrent sharing rules

**Files:**
- Modify: `crates/taurus-core/src/{torrent/job.rs,torrent/hub.rs,torrent/policy.rs,engine/torrent.rs,engine/types.rs,store}` (schema v7 column `torrent_overrides TEXT` JSON)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TorrentOverrides { pub stop_ratio: Option<Option<f64>>, pub stop_after_secs: Option<Option<u64>> } // outer None = use global
impl Engine {
    pub async fn set_torrent_files(&self, id: JobId, only_files: Vec<usize>) -> Result<(), CoreError>; // librqbit Session::update_only_files; empty -> Err Unknown "select at least one file"
    pub async fn set_torrent_overrides(&self, id: JobId, o: TorrentOverrides) -> Result<(), CoreError>;
}
// TorrentView gains: pub overrides: TorrentOverrides
pub fn effective_policy(global: &TorrentSettings, o: &TorrentOverrides) -> SeedPolicy; // pure
```

Decisions: reselection recomputes `total` (selected size), runs the space check for the bytes still missing (DiskFull → the change is refused, selection unchanged), never deletes files of entries that stay selected; deselected entries' finished data stays on disk (librqbit behaviour) and is noted in the README. A Completed torrent with new files selected goes back to Queued.

- [ ] **Step 1: Write failing tests:** `effective_policy_falls_back_per_field`; `reselect_keeps_done_files_and_checks_space` (two-file seeder; download only file 0; add file 1 → file 0 untouched (same mtime and hash), job completes with both; with test space source too small for file 1 → Err DiskFull and selection unchanged); `empty_selection_rejected`; `override_ratio_stops_earlier` (global ratio None, override Some(0.0) → Completed at finish).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(core): change torrent file selection and per-torrent sharing rules`.

---

### Task 7: M5 leftovers

**Files:**
- Modify: `crates/taurus-core/src/{tools/mod.rs,engine/tool_ops.rs,engine/media.rs,engine/settings.rs}`, `apps/taurus-desktop/src/bridge.rs`

**Decisions:**
- RX-2: a cancelled or abandoned install sets the tool back to `Missing` (or its previous `Ready` version) and emits `Event::ToolStatus`, so the banner clears.
- RX-3: a speed-limit change that moves a running media job's share by more than 20 % restarts that job's yt-dlp process gracefully with the new `--limit-rate` (yt-dlp resumes its `.part`); live recordings are never restarted (they keep their starting limit).

- [ ] **Step 1: Write failing tests:** `cancelled_install_clears_installing_state` (cancel mid-download with a throttled fake host → status not `Installing`, event emitted); `media_restarts_on_big_limit_change` (fake yt-dlp args log shows a second run with the new `--limit-rate`; final file intact); `recording_not_restarted_on_limit_change`; desktop `banner_clears_after_cancel` (bridge unit test).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `fix(core): tools banner clears after a cancelled install; video speed follows limit changes`.

---

### Task 8: CLI

**Files:**
- Modify: `crates/taurus-cli/src/{main.rs,render.rs}`, `crates/taurus-cli/tests/cli.rs`, `README.md`

**Interfaces:**
- Produces:

```
taurus grab <PAGE_URL> [--subfolders] [--ext pdf,zip] [--add] [--dir <PATH>]   # lists (index, size, folder, name); --add adds every listed file
taurus add <URL> [--user <NAME>]                     # prompts for the password with rpassword; env TAURUS_PASSWORD in debug builds only (tests)
taurus add <URL> [--cookies-from <BROWSER[:PROFILE]>] [--subs <en,vi|all>] [--auto-subs] [--embed-subs] [--container mp4|mkv|webm]
taurus login <ID> --user <NAME>                      # set credentials for an existing job (prompt), then it resumes
taurus convert <ID|PATH> mp3|smaller-mp4
taurus torrent-select <ID> <LIST>                    # "0,2,5-7"
taurus job set <ID> ratio <N|never|default>          # also: seed_time <30m|2h|off|default>
taurus config set network.proxy system|none|<URL>
```

- [ ] **Step 1: Write failing tests:** `grab_lists_and_adds` (TestServer page; `--ext pdf` filters; `--add` downloads); `add_with_user_prompts` (debug env password; protected file downloads); `convert_command`; `torrent_select_command`; `job_set_ratio`; `proxy_config_roundtrip_and_rejects_bad`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --workspace`, clippy, fmt → clean.
- [ ] **Step 5: README** — the new commands and the password/keychain note.
- [ ] **Step 6: Commit** `feat(cli): grab, passwords, video options, convert, torrent selection, proxy (M6a)`.

---

## Done criteria

- CI green on all three OSes.
- All five Review Focus tests pass.
- Every engine capability the M6b Advanced screens need exists and is reachable from the CLI.

Next plan: M6b — Advanced mode screens (compact + wide layout, torrent picker, grab links, several links, when-done menu, full settings) on top of these APIs.
