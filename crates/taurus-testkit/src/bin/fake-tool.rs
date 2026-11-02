//! A fake yt-dlp / ffmpeg / ffprobe / deno for offline tests; it acts as the
//! tool its own file name starts with (`.exe` ignored, case-insensitive).
//!
//! std only: `build.rs` also compiles this file with `$RUSTC --edition 2021`
//! so `taurus_testkit::fake_tool_bytes()` works from any crate's tests.
//! The full contract (trailer, scenario schema, args log) is documented in
//! `taurus_testkit::tools`.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Last 16 bytes of a binary carrying a release trailer.
const MAGIC: &[u8; 16] = b"HYADES-FAKETOOL1";
/// Longest `wait_for_interrupt` wait before finishing normally.
const MAX_WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(10);

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(tool) = tool_name() else {
        eprintln!("fake-tool: unknown tool name");
        return 2;
    };
    log_args(tool, &args);
    let (version, broken, hang) = release();
    if broken {
        eprintln!("fake {tool}: this build is broken");
        return 3;
    }
    if hang {
        // Same path as the testkit's `hang_pid_file`.
        let pid_file = env::temp_dir().join(format!("hyades-fake-tool-hang-{version}.pid"));
        let _ = fs::write(pid_file, std::process::id().to_string());
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    if args.iter().any(|a| a == "--version" || a == "-version") {
        print_version(tool, &version);
        return 0;
    }
    match tool {
        "yt-dlp" => yt_dlp(&args, &version),
        "ffmpeg" => ffmpeg(&args),
        _ => 0,
    }
}

/// The tool named by argv[0] (falling back to the executable's own path).
fn tool_name() -> Option<&'static str> {
    let path = env::args_os()
        .next()
        .map(std::path::PathBuf::from)
        .or_else(|| env::current_exe().ok())?;
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    ["yt-dlp", "ffprobe", "ffmpeg", "deno"]
        .into_iter()
        .find(|t| name.starts_with(t))
}

/// Version, broken and hang flags: the trailer if present, else
/// `FAKE_TOOL_VERSION` (default "0.0.0") and `FAKE_TOOL_BROKEN=1` (never hangs).
fn release() -> (String, bool, bool) {
    if let Some(payload) = env::current_exe().ok().and_then(|p| read_trailer(&p)) {
        let mut version = String::from("0.0.0");
        let (mut broken, mut hang) = (false, false);
        for line in payload.lines() {
            if let Some(v) = line.strip_prefix("version=") {
                version = v.to_owned();
            } else if let Some(b) = line.strip_prefix("broken=") {
                broken = b == "1";
            } else if let Some(h) = line.strip_prefix("hang=") {
                hang = h == "1";
            }
        }
        return (version, broken, hang);
    }
    let version = env::var("FAKE_TOOL_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let broken = env::var("FAKE_TOOL_BROKEN").is_ok_and(|v| v == "1");
    (version, broken, false)
}

/// `<binary><payload><payload len: u32 LE><MAGIC>` -> payload.
fn read_trailer(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let tail = (MAGIC.len() + 4) as i64;
    file.seek(SeekFrom::End(-tail)).ok()?;
    let mut buf = [0u8; 20];
    file.read_exact(&mut buf).ok()?;
    if &buf[4..] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as i64;
    file.seek(SeekFrom::End(-tail - len)).ok()?;
    let mut payload = vec![0u8; len as usize];
    file.read_exact(&mut payload).ok()?;
    String::from_utf8(payload).ok()
}

fn print_version(tool: &str, version: &str) {
    match tool {
        "yt-dlp" => println!("{version}"),
        "deno" => {
            let triple = format!("{}-{}", env::consts::ARCH, env::consts::OS);
            println!("deno {version} (stable, release, {triple})");
            println!("v8 14.0.0");
            println!("typescript 5.9.2");
        }
        _ => {
            println!("{tool} version {version} Copyright (c) 2000-2026 the FFmpeg developers");
            println!("built with fake-tool");
        }
    }
}

/// Appends `{"tool":"…","pid":N,"argv":[…]}` and a newline to `FAKE_TOOL_ARGS_LOG`.
fn log_args(tool: &str, args: &[String]) {
    let Ok(path) = env::var("FAKE_TOOL_ARGS_LOG") else {
        return;
    };
    let argv: Vec<String> = args.iter().map(|a| json_string(a)).collect();
    let line = format!(
        "{{\"tool\":{},\"pid\":{},\"argv\":[{}]}}\n",
        json_string(tool),
        std::process::id(),
        argv.join(",")
    );
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        eprintln!("fake-tool: cannot write args log {path}: {e}");
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------- ffmpeg

/// `ffmpeg [-y] -i <in> [-i <in> ...] [-map N ...] -c copy <out>` writes
/// the inputs, one after the other, to `<out>` (one input: a plain copy;
/// several: what a real remux joining a video and an audio stream would
/// hold, for a test's purposes); anything else exits 0 without doing
/// anything.
fn ffmpeg(args: &[String]) -> i32 {
    let inputs: Vec<&String> = args
        .windows(2)
        .filter(|w| w[0] == "-i")
        .map(|w| &w[1])
        .collect();
    let copy = args
        .windows(2)
        .any(|w| (w[0] == "-c" || w[0] == "-codec") && w[1] == "copy");
    let (false, true, Some(output)) = (inputs.is_empty(), copy, args.last()) else {
        return 0;
    };
    if inputs.contains(&output) {
        return 0;
    }
    if Path::new(output).exists() && !args.iter().any(|a| a == "-y") {
        eprintln!("File '{output}' already exists. Exiting.");
        return 1;
    }
    let mut joined = Vec::new();
    for input in &inputs {
        match fs::read(input) {
            Ok(bytes) => joined.extend_from_slice(&bytes),
            Err(e) => {
                eprintln!("{input}: {e}");
                return 1;
            }
        }
    }
    match fs::write(output, joined) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{output}: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------- yt-dlp

#[derive(Default)]
struct Scenario {
    stdout: Vec<String>,
    stderr: Vec<String>,
    exit: i32,
    write_files: Vec<(String, u64)>,
    /// Renamed `[from, to]` before the first line (before `start_files`).
    rename_files: Vec<(String, String)>,
    /// Written before the first line, whatever happens later.
    start_files: Vec<(String, u64)>,
    delay_ms_per_line: u64,
    wait_for_interrupt: bool,
    on_interrupt: Option<Box<Scenario>>,
}

fn yt_dlp(args: &[String], version: &str) -> i32 {
    let Ok(path) = env::var("FAKE_YTDLP_SCENARIO") else {
        eprintln!("fake yt-dlp: FAKE_YTDLP_SCENARIO is not set");
        return 2;
    };
    let path = path.replace("{version}", version);
    let scenario = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {path}: {e}"))
        .and_then(|text| Parser::parse(&text))
        .and_then(|value| pick(value, args));
    match scenario {
        Ok(s) => replay(&s),
        Err(e) => {
            eprintln!("fake yt-dlp: bad scenario: {e}");
            2
        }
    }
}

/// A single object, or an array whose first entry with no `if_arg` (or an
/// `if_arg` equal to one of the arguments) is used.
fn pick(value: Json, args: &[String]) -> Result<Scenario, String> {
    let entries = match value {
        Json::Arr(items) => items,
        other => vec![other],
    };
    for entry in entries {
        let Json::Obj(fields) = &entry else {
            return Err("scenario entries must be objects".into());
        };
        let wanted = fields.iter().find(|(k, _)| k == "if_arg");
        let matches = match wanted {
            None => true,
            Some((_, Json::Str(arg))) => args.iter().any(|a| a == arg),
            Some(_) => return Err("if_arg must be a string".into()),
        };
        if matches {
            return scenario(entry, true);
        }
    }
    Err("no scenario entry matches the arguments".into())
}

fn scenario(value: Json, top: bool) -> Result<Scenario, String> {
    let Json::Obj(fields) = value else {
        return Err("a scenario must be an object".into());
    };
    let mut s = Scenario::default();
    for (key, value) in fields {
        match key.as_str() {
            "stdout" => s.stdout = strings(value, &key)?,
            "stderr" => s.stderr = strings(value, &key)?,
            "exit" => s.exit = number(&value, &key)? as i32,
            "write_files" => s.write_files = files(value)?,
            "start_files" => s.start_files = files(value)?,
            "rename_files" => s.rename_files = renames(value)?,
            "delay_ms_per_line" => s.delay_ms_per_line = number(&value, &key)? as u64,
            "wait_for_interrupt" if top => s.wait_for_interrupt = matches!(value, Json::Bool(true)),
            "on_interrupt" if top => s.on_interrupt = Some(Box::new(scenario(value, false)?)),
            "if_arg" if top => {}
            other => return Err(format!("unknown key {other:?}")),
        }
    }
    Ok(s)
}

fn strings(value: Json, key: &str) -> Result<Vec<String>, String> {
    let Json::Arr(items) = value else {
        return Err(format!("{key} must be an array of strings"));
    };
    items
        .into_iter()
        .map(|item| match item {
            Json::Str(s) => Ok(s),
            _ => Err(format!("{key} must be an array of strings")),
        })
        .collect()
}

fn number(value: &Json, key: &str) -> Result<f64, String> {
    match value {
        Json::Num(n) => Ok(*n),
        _ => Err(format!("{key} must be a number")),
    }
}

fn files(value: Json) -> Result<Vec<(String, u64)>, String> {
    let bad = || "write_files must be [[path, size], ...]".to_owned();
    let Json::Arr(items) = value else {
        return Err(bad());
    };
    items
        .into_iter()
        .map(|item| match item {
            Json::Arr(pair) => match pair.as_slice() {
                [Json::Str(path), Json::Num(size)] => Ok((path.clone(), *size as u64)),
                _ => Err(bad()),
            },
            _ => Err(bad()),
        })
        .collect()
}

/// `[[from, to], ...]`.
fn renames(value: Json) -> Result<Vec<(String, String)>, String> {
    let bad = || "rename_files must be [[from, to], ...]".to_owned();
    let Json::Arr(items) = value else {
        return Err(bad());
    };
    items
        .into_iter()
        .map(|item| match item {
            Json::Arr(pair) => match pair.as_slice() {
                [Json::Str(from), Json::Str(to)] => Ok((from.clone(), to.clone())),
                _ => Err(bad()),
            },
            _ => Err(bad()),
        })
        .collect()
}

/// Renames the scenario's `rename_files`, then writes its `start_files`;
/// false (after saying why) if one fails.
fn start(s: &Scenario) -> bool {
    for (from, to) in &s.rename_files {
        if let Err(e) = fs::rename(from, to) {
            eprintln!("fake yt-dlp: cannot rename {from} to {to}: {e}");
            return false;
        }
    }
    for (path, size) in &s.start_files {
        if let Err(e) = write_file(path, *size) {
            eprintln!("fake yt-dlp: cannot write {path}: {e}");
            return false;
        }
    }
    true
}

fn replay(s: &Scenario) -> i32 {
    if s.on_interrupt.is_some() {
        watch_interrupt();
    }
    if !start(s) {
        return 2;
    }
    let delay = Duration::from_millis(s.delay_ms_per_line);
    let lines = s
        .stdout
        .iter()
        .map(|l| (false, l))
        .chain(s.stderr.iter().map(|l| (true, l)));
    for (to_stderr, line) in lines {
        if interrupted_now() {
            break;
        }
        emit(to_stderr, line);
        if pause(delay) {
            break;
        }
    }
    if s.wait_for_interrupt {
        pause(MAX_WAIT);
    }
    match &s.on_interrupt {
        Some(on) if interrupted_now() => {
            if !start(on) {
                return 2;
            }
            // Not interruptible: a slow way out.
            let delay = Duration::from_millis(on.delay_ms_per_line);
            for line in &on.stdout {
                emit(false, line);
                thread::sleep(delay);
            }
            for line in &on.stderr {
                emit(true, line);
                thread::sleep(delay);
            }
            finish(on)
        }
        _ => finish(s),
    }
}

/// Writes the scenario's files and returns its exit code.
fn finish(s: &Scenario) -> i32 {
    for (path, size) in &s.write_files {
        if let Err(e) = write_file(path, *size) {
            eprintln!("fake yt-dlp: cannot write {path}: {e}");
            return 2;
        }
    }
    s.exit
}

fn emit(to_stderr: bool, line: &str) {
    if to_stderr {
        let mut err = io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    } else {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// `size` bytes of a repeating pattern; parent directories are created.
fn write_file(path: &str, size: u64) -> io::Result<()> {
    let path = Path::new(path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let chunk: Vec<u8> = (0..65_536u32).map(|i| (i % 251) as u8).collect();
    let mut file = File::create(path)?;
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn interrupted_now() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Sleeps `total`, returning early (true) when interrupted.
fn pause(total: Duration) -> bool {
    let end = Instant::now() + total;
    loop {
        if interrupted_now() {
            return true;
        }
        let now = Instant::now();
        if now >= end {
            return false;
        }
        thread::sleep(POLL.min(end - now));
    }
}

#[cfg(unix)]
fn watch_interrupt() {
    const SIGINT: i32 = 2;
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    extern "C" fn on_signal(_: i32) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        signal(SIGINT, on_signal);
    }
}

#[cfg(windows)]
fn watch_interrupt() {
    type Handler = unsafe extern "system" fn(u32) -> i32;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }
    unsafe extern "system" fn on_ctrl(kind: u32) -> i32 {
        // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1.
        if kind <= 1 {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }
    // SAFETY: plain Win32 calls; the handler only stores to an atomic. The
    // first call re-enables Ctrl+C, which CREATE_NEW_PROCESS_GROUP disables.
    unsafe {
        SetConsoleCtrlHandler(None, 0);
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

#[cfg(not(any(unix, windows)))]
fn watch_interrupt() {}

// ---------------------------------------------------------------- JSON

enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// A minimal JSON reader, enough for scenario files.
struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser {
            text: text.as_bytes(),
            pos: 0,
        };
        let value = p.value()?;
        p.skip_ws();
        if p.pos != p.text.len() {
            return Err(p.error("trailing characters"));
        }
        Ok(value)
    }

    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.pos)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.text.len() && self.text[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.text.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected {:?}", byte as char)))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.text[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("expected a value")),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.expect(b':')?;
            fields.push((key, self.value()?));
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while self.pos < self.text.len()
            && matches!(
                self.text[self.pos],
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
            )
        {
            self.pos += 1;
        }
        std::str::from_utf8(&self.text[start..self.pos])
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Json::Num)
            .ok_or_else(|| self.error("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| self.error("bad \\u escape"))?;
        self.pos += 4;
        Ok(digits)
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.text.get(self.pos) else {
                return Err(self.error("unterminated string"));
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&esc) = self.text.get(self.pos) else {
                        return Err(self.error("unterminated escape"));
                    };
                    self.pos += 1;
                    let c = match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code)
                                && self.text[self.pos..].starts_with(b"\\u")
                            {
                                self.pos += 2;
                                let low = self.hex4()?;
                                code =
                                    0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00));
                            }
                            char::from_u32(code).ok_or_else(|| self.error("bad \\u escape"))?
                        }
                        _ => return Err(self.error("bad escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8"))
    }
}
