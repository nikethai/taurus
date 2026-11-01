//! Fake yt-dlp / ffmpeg / ffprobe / deno releases for offline tests.
//!
//! # The fake tool (`src/bin/fake-tool.rs`)
//!
//! One std-only binary that acts as the tool its file name starts with
//! (`yt-dlp`, `ffmpeg`, `ffprobe`, `deno`; case-insensitive, `.exe` ignored).
//! `build.rs` compiles it into OUT_DIR and [`fake_tool_bytes`] embeds it, so
//! any crate's tests can use it. Every invocation:
//!
//! 1. appends one line to the file named by `FAKE_TOOL_ARGS_LOG` (if set):
//!    `{"tool":"yt-dlp","pid":1234,"argv":["--flat-playlist","-J","https://…"]}`
//!    (`argv` excludes the program name; see [`read_args_log`]);
//! 2. if broken, prints to stderr and exits 3 whatever the arguments; if
//!    hanging, writes its pid to [`hang_pid_file`] and sleeps until killed;
//! 3. `--version` or `-version` prints the version and exits 0: yt-dlp
//!    `<v>`; deno `deno <v> (stable, release, <arch>-<os>)` plus two lines;
//!    ffmpeg/ffprobe `<tool> version <v> Copyright (c) 2000-2026 the FFmpeg developers`
//!    plus one line;
//! 4. otherwise: yt-dlp replays a scenario (below); `ffmpeg [-y] -i <in> -c copy <out>`
//!    copies `<in>` to `<out>` (exit 1 if `<in>` is unreadable, or `<out>`
//!    exists without `-y`); every other call exits 0 silently.
//!
//! Version and broken flag come from a trailer appended to the binary
//! ([`fake_tool_release`]): `<binary><payload><u32 LE payload length><"HYADES-FAKETOOL1">`
//! with payload `version=<v>\nbroken=<0|1>\nhang=<0|1>\n`. Without a trailer the fake
//! reads `FAKE_TOOL_VERSION` (default `0.0.0`) and `FAKE_TOOL_BROKEN=1`.
//!
//! # yt-dlp scenarios
//!
//! `FAKE_YTDLP_SCENARIO` names a JSON file; `{version}` in that path is
//! replaced by the binary's version (so a newer release can behave
//! differently). The file is one scenario object, or an array of them where
//! the first entry whose `if_arg` is absent or equals one of the arguments
//! is used. Keys (unknown keys exit 2 with a message):
//!
//! ```json
//! { "if_arg": "-J",                       // array entries only
//!   "stdout": ["line", "..."],            // printed first, one line each, flushed
//!   "stderr": ["ERROR: ..."],             // printed after stdout
//!   "delay_ms_per_line": 50,              // sleep after each printed line
//!   "wait_for_interrupt": false,          // after the lines, wait (max 120 s) for SIGINT/CTRL_BREAK
//!   "write_files": [["rel/or/abs.mp4", 1024]], // written after the lines; relative to cwd; parents created
//!   "exit": 0,
//!   "on_interrupt": { "stdout": [], "stderr": [], "write_files": [], "exit": 1 } }
//! ```
//!
//! With `on_interrupt`, SIGINT (Unix) or CTRL_C/CTRL_BREAK (Windows) stops the
//! replay at the next line or within 10 ms of a delay; the fake then prints
//! `on_interrupt`'s lines, writes only its files and exits with its code.
//! Without `on_interrupt` the signal keeps its default action.
//! Written files hold a repeating byte pattern of the given size.

mod package;
mod scenario;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Response, StatusCode, Uri, header};
use bytes::Bytes;

use package::{
    DENO_TRIPLES, FFMPEG_BUILDS, MACOS_ARCHS, MACOS_ZIPS, YT_DLP_ASSETS, deno_asset,
    ffmpeg_build_asset, published_sha256,
};
pub use scenario::{process_args_log, set_ytdlp_scenario, unique_version};

#[cfg(windows)]
static FAKE_TOOL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fake-tool.exe"));
#[cfg(not(windows))]
static FAKE_TOOL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fake-tool"));

/// The native fake-tool binary, without a trailer.
pub fn fake_tool_bytes() -> Vec<u8> {
    FAKE_TOOL.to_vec()
}

/// The fake-tool binary with a release trailer (`version` must be one line).
pub fn fake_tool_release(version: &str, broken: bool) -> Vec<u8> {
    let mut binary = fake_tool_bytes();
    package::append_trailer(&mut binary, version, broken, false);
    binary
}

/// Writes [`fake_tool_release`] as `dir/<tool>` (plus `.exe` on Windows),
/// executable, and returns its path. `tool`: "yt-dlp", "ffmpeg", "ffprobe", "deno".
pub fn write_fake_tool(dir: &Path, tool: &str, version: &str, broken: bool) -> PathBuf {
    let path = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    write_executable(&path, &fake_tool_release(version, broken));
    path
}

/// Always a new file: macOS kills a process whose signed binary was
/// rewritten in place after it ran.
fn write_executable(path: &Path, bytes: &[u8]) {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, bytes).expect("write fake tool");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make fake tool executable");
    }
}

/// Runs `cmd` to completion like [`Command::output`], retrying for up to
/// about a second while exec fails with ETXTBSY (errno 26, Linux): a
/// binary written a moment ago can still be open for writing in a child
/// that another thread forked meanwhile.
pub fn run_retrying(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    let mut attempt = 0;
    loop {
        match cmd.output() {
            Err(e) if e.raw_os_error() == Some(26) && cfg!(unix) && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

/// One line of the `FAKE_TOOL_ARGS_LOG` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgsLogEntry {
    pub tool: String,
    pub pid: u32,
    /// Arguments without the program name.
    pub argv: Vec<String>,
}

/// Every invocation logged at `path`, oldest first; empty if the file is missing.
pub fn read_args_log(path: &Path) -> Vec<ArgsLogEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).expect("args log line is JSON");
            ArgsLogEntry {
                tool: v["tool"].as_str().expect("tool").to_owned(),
                pid: v["pid"].as_u64().expect("pid") as u32,
                argv: v["argv"]
                    .as_array()
                    .expect("argv")
                    .iter()
                    .map(|a| a.as_str().expect("argv item").to_owned())
                    .collect(),
            }
        })
        .collect()
}

/// What the host publishes for one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRelease {
    pub version: String,
    /// The served binaries exit 3 on every call.
    pub broken: bool,
    /// The published checksums are well-formed but wrong.
    pub corrupt_checksum: bool,
    /// The served binaries never finish (unless broken, which wins): each
    /// call writes its pid to [`hang_pid_file`]`(version)`, then sleeps
    /// until killed.
    pub hang: bool,
}

impl ToolRelease {
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            broken: false,
            corrupt_checksum: false,
            hang: false,
        }
    }
}

struct HostState {
    yt_dlp: ToolRelease,
    ffmpeg: ToolRelease,
    deno: ToolRelease,
    /// Built bodies by path; cleared when a release changes.
    cache: HashMap<String, Bytes>,
    /// Paths of every request received, without the leading `/`.
    requests: Vec<String>,
}

type Shared = Arc<Mutex<HostState>>;

fn lock(shared: &Shared) -> MutexGuard<'_, HostState> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running server thread; dropping its runtime closes every socket.
struct Server {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: JoinHandle<()>,
}

/// Serves fake tool releases from `127.0.0.1` under [`FakeToolHost::base_url`]:
///
/// - `yt-dlp/<asset>` (raw binaries: `yt-dlp.exe`, `yt-dlp_arm64.exe`, `yt-dlp_macos`,
///   `yt-dlp_linux`, `yt-dlp_linux_aarch64`, `yt-dlp_musllinux`, `yt-dlp_musllinux_aarch64`)
///   and `yt-dlp/SHA2-256SUMS` (`<hex>  <asset>` lines);
/// - `deno/deno-<triple>.zip` (`deno` / `deno.exe` at the root) and
///   `deno/deno-<triple>.zip.sha256sum`: `<hex>  <asset>\n`, or for Windows
///   triples PowerShell `Format-List` text (CRLF, `Hash      : <HEX>`), as Deno publishes;
/// - `ffmpeg-builds/ffmpeg-master-latest-{linux64,linuxarm64}-gpl.tar.xz`,
///   `ffmpeg-builds/ffmpeg-master-latest-{win64,winarm64}-gpl.zip` (members
///   `ffmpeg-master-latest-<plat>-gpl/bin/ffmpeg[.exe]`, `…/bin/ffprobe[.exe]`,
///   `…/LICENSE.txt`) and `ffmpeg-builds/checksums.sha256`;
/// - `ffmpeg-macos/{arm64,amd64}/release/{ffmpeg,ffprobe}.zip`: 307 to
///   `/ffmpeg-macos-files/<version>/<arch>/<name>.zip` (root-relative, like
///   martin-riedl), which serves a zip holding the program alone, with
///   `<that path>.sha256` = `<hex>  <name>.zip\n` beside it;
/// - `api/repos/yt-dlp/yt-dlp/releases/latest` → `{"tag_name":"<version>"}`,
///   `api/repos/denoland/deno/releases/latest` → `{"tag_name":"v<version>"}`.
///
/// Every binary is the native fake tool with the tool's release trailer;
/// anything else is 404. Responses carry `connection: close`, so clients
/// never reuse a connection across [`FakeToolHost::go_offline`].
pub struct FakeToolHost {
    addr: SocketAddr,
    state: Shared,
    server: Mutex<Option<Server>>,
}

impl FakeToolHost {
    pub async fn start(
        yt_dlp: ToolRelease,
        ffmpeg: ToolRelease,
        deno: ToolRelease,
    ) -> FakeToolHost {
        let state: Shared = Arc::new(Mutex::new(HostState {
            yt_dlp,
            ffmpeg,
            deno,
            cache: HashMap::new(),
            requests: Vec::new(),
        }));
        let (addr, server) = spawn_server(SocketAddr::from(([127, 0, 0, 1], 0)), state.clone());
        FakeToolHost {
            addr,
            state,
            server: Mutex::new(Some(server)),
        }
    }

    /// `http://127.0.0.1:<port>/`, with the trailing slash; stable across
    /// `go_offline` / `go_online`.
    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// Publishes `release` for `tool` ("yt-dlp", "ffmpeg" or "deno") from now on.
    pub fn set_release(&self, tool: &str, release: ToolRelease) {
        let mut state = lock(&self.state);
        match tool {
            "yt-dlp" => state.yt_dlp = release,
            "ffmpeg" => state.ffmpeg = release,
            "deno" => state.deno = release,
            other => panic!("set_release: unknown tool {other:?}"),
        }
        state.cache.clear();
    }

    /// Requests received whose path (relative to `base_url`, no leading `/`)
    /// starts with `path_prefix`; `""` counts all. Includes 404s and redirects.
    pub fn requests(&self, path_prefix: &str) -> usize {
        lock(&self.state)
            .requests
            .iter()
            .filter(|p| p.starts_with(path_prefix))
            .count()
    }

    /// Stops the server and closes its port and every open connection before
    /// returning: later requests fail to connect. No-op when already offline.
    pub fn go_offline(&self) {
        let server = self.server.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(server) = server {
            let _ = server.stop.send(());
            server.thread.join().expect("fake tool host thread");
        }
    }

    /// Serves again on the same port. No-op when online.
    pub fn go_online(&self) {
        let mut server = self.server.lock().unwrap_or_else(|e| e.into_inner());
        if server.is_none() {
            *server = Some(spawn_server(self.addr, self.state.clone()).1);
        }
    }
}

impl Drop for FakeToolHost {
    fn drop(&mut self) {
        self.go_offline();
    }
}

/// Runs the host on its own thread and runtime, so going offline can wait
/// for the listener and connections to close without the caller's runtime.
fn spawn_server(addr: SocketAddr, state: Shared) -> (SocketAddr, Server) {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake tool host runtime");
        runtime.block_on(async move {
            let listener = bind(addr).await;
            addr_tx
                .send(listener.local_addr().expect("fake tool host address"))
                .expect("report address");
            let app = Router::new().fallback(handle).with_state(state);
            tokio::select! {
                _ = axum::serve(listener, app) => {}
                _ = stop_rx => {}
            }
        });
        // Dropping the runtime here drops the listener and every connection.
    });
    let addr = addr_rx.recv().expect("fake tool host failed to bind");
    (addr, Server { stop, thread })
}

/// Binds `addr`, retrying briefly while a just-closed listener lets go of it.
async fn bind(addr: SocketAddr) -> tokio::net::TcpListener {
    let mut tries = 0;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return listener,
            Err(_) if tries < 100 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("fake tool host cannot bind {addr}: {e}"),
        }
    }
}

enum Reply {
    Body(&'static str, Bytes),
    Redirect(String),
    NotFound,
}

async fn handle(State(state): State<Shared>, uri: Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/').to_owned();
    let reply = {
        let mut state = lock(&state);
        state.requests.push(path.clone());
        reply(&mut state, &path)
    };
    let builder = Response::builder().header(header::CONNECTION, "close");
    match reply {
        Reply::Body(content_type, body) => builder
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body)),
        Reply::Redirect(location) => builder
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, location)
            .body(Body::empty()),
        Reply::NotFound => builder.status(StatusCode::NOT_FOUND).body(Body::empty()),
    }
    .expect("fake tool host response")
}

const BINARY: &str = "application/octet-stream";
const ZIP: &str = "application/zip";
const TEXT: &str = "text/plain; charset=utf-8";

fn reply(state: &mut HostState, path: &str) -> Reply {
    if let Some(rest) = path.strip_prefix("ffmpeg-macos/") {
        let mut parts = rest.split('/');
        return match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(arch), Some("release"), Some(zip), None)
                if MACOS_ARCHS.contains(&arch) && MACOS_ZIPS.contains(&zip) =>
            {
                let version = &state.ffmpeg.version;
                Reply::Redirect(format!("/ffmpeg-macos-files/{version}/{arch}/{zip}"))
            }
            _ => Reply::NotFound,
        };
    }
    match path {
        "api/repos/yt-dlp/yt-dlp/releases/latest" => {
            Reply::Body("application/json", tag_json(&state.yt_dlp.version))
        }
        "api/repos/denoland/deno/releases/latest" => Reply::Body(
            "application/json",
            tag_json(&format!("v{}", state.deno.version)),
        ),
        _ => match body(state, path) {
            Some(bytes) => {
                let content_type = if path.ends_with(".zip") {
                    ZIP
                } else if path.ends_with("SUMS")
                    || path.ends_with(".sha256")
                    || path.ends_with(".sha256sum")
                {
                    TEXT
                } else {
                    BINARY
                };
                Reply::Body(content_type, bytes)
            }
            None => Reply::NotFound,
        },
    }
}

fn tag_json(tag: &str) -> Bytes {
    Bytes::from(serde_json::json!({ "tag_name": tag }).to_string())
}

/// The body served at `path`, built once per release and cached.
fn body(state: &mut HostState, path: &str) -> Option<Bytes> {
    if let Some(bytes) = state.cache.get(path) {
        return Some(bytes.clone());
    }
    let built = Bytes::from(build(state, path)?);
    state.cache.insert(path.to_owned(), built.clone());
    Some(built)
}

fn build(state: &mut HostState, path: &str) -> Option<Vec<u8>> {
    let (dir, name) = path.split_once('/')?;
    match dir {
        "yt-dlp" => {
            let release = state.yt_dlp.clone();
            if name == "SHA2-256SUMS" {
                let binary = binary(&release);
                let hex = published_sha256(&binary, release.corrupt_checksum);
                let entries: Vec<(String, String)> = YT_DLP_ASSETS
                    .iter()
                    .map(|asset| (hex.clone(), (*asset).to_owned()))
                    .collect();
                return Some(package::sums_file(&entries).into_bytes());
            }
            YT_DLP_ASSETS.contains(&name).then(|| binary(&release))
        }
        "deno" => {
            let release = state.deno.clone();
            if let Some(asset) = name.strip_suffix(".sha256sum") {
                let zip = body(state, &format!("deno/{asset}"))?;
                let hex = published_sha256(&zip, release.corrupt_checksum);
                let text = if asset.contains("windows") {
                    package::powershell_sidecar(&hex, asset)
                } else {
                    package::sums_file(&[(hex, asset.to_owned())])
                };
                return Some(text.into_bytes());
            }
            let triple = DENO_TRIPLES.iter().find(|t| deno_asset(t) == name)?;
            Some(package::deno_zip(triple, &binary(&release)))
        }
        "ffmpeg-builds" => {
            let release = state.ffmpeg.clone();
            if name == "checksums.sha256" {
                let mut entries = Vec::new();
                for plat in FFMPEG_BUILDS {
                    let asset = ffmpeg_build_asset(plat);
                    let archive = body(state, &format!("ffmpeg-builds/{asset}"))?;
                    entries.push((published_sha256(&archive, release.corrupt_checksum), asset));
                }
                return Some(package::sums_file(&entries).into_bytes());
            }
            let plat = FFMPEG_BUILDS
                .iter()
                .find(|p| ffmpeg_build_asset(p) == name)?;
            Some(package::ffmpeg_build(plat, &binary(&release)))
        }
        "ffmpeg-macos-files" => {
            let release = state.ffmpeg.clone();
            let (version, rest) = name.split_once('/')?;
            let (arch, file) = rest.split_once('/')?;
            if version != release.version || !MACOS_ARCHS.contains(&arch) {
                return None;
            }
            if let Some(zip_name) = file.strip_suffix(".sha256") {
                let zip = body(state, path.strip_suffix(".sha256")?)?;
                let hex = published_sha256(&zip, release.corrupt_checksum);
                return Some(package::sums_file(&[(hex, zip_name.to_owned())]).into_bytes());
            }
            MACOS_ZIPS
                .contains(&file)
                .then(|| package::macos_zip(file, &binary(&release)))
        }
        _ => None,
    }
}

fn binary(release: &ToolRelease) -> Vec<u8> {
    let mut binary = fake_tool_bytes();
    package::append_trailer(&mut binary, &release.version, release.broken, release.hang);
    binary
}

/// Where a hanging release (`ToolRelease::hang`) of `version` writes the
/// pid of its latest call: `<temp dir>/hyades-fake-tool-hang-<version>.pid`.
/// Use a version unique to the test.
pub fn hang_pid_file(version: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hyades-fake-tool-hang-{version}.pid"))
}
