//! One yt-dlp download run: its arguments, reading what it prints, stopping
//! it gracefully, and keeping what a stopped live recording left behind.
//!
//! The engine's media job (`engine::media`) decides when to run, retry or
//! give up; this module only knows yt-dlp.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::tools::tree::TreeChild;

use super::{ConvertPreset, MediaOptions, extra_args_for};

/// Output name of a single video, inside the job's folder.
pub(crate) const SINGLE_TEMPLATE: &str = "%(title).200B [%(id)s].%(ext)s";
/// Output name of a playlist entry: a folder per playlist.
pub(crate) const PLAYLIST_TEMPLATE: &str =
    "%(playlist_title).100B/%(playlist_index)03d - %(title).150B [%(id)s].%(ext)s";

const PROGRESS: &str = "TAURUS-PROGRESS ";
const POSTPROCESS: &str = "TAURUS-PP ";
const FILE: &str = "TAURUS-FILE ";
const ENTRY: &str = "TAURUS-ENTRY ";

/// The most of stderr kept: its end, where the errors are.
const MAX_STDERR: usize = 256 * 1024;
/// How long the output is still read after yt-dlp exits (something it
/// started may hold the pipe open).
const DRAIN: Duration = Duration::from_secs(2);

/// What a media job is doing, for its view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum MediaPhase {
    /// yt-dlp, ffmpeg or Deno is being installed.
    GettingTools,
    /// A live or upcoming stream has not sent anything yet.
    WaitingForStream,
    Downloading,
    /// ffmpeg joins the video and audio.
    Merging,
    /// ffmpeg converts or extracts the audio.
    Converting,
}

/// yt-dlp's arguments after the global flags, ending with the link.
pub(crate) fn download_args(options: &MediaOptions, dest_dir: &Path, url: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-f".into(), options.selector.clone().into()];
    let audio = options.audio_only || options.convert == Some(ConvertPreset::Mp3);
    if audio {
        let extra = match options.convert {
            Some(ConvertPreset::Mp3) => ConvertPreset::Mp3.args(),
            _ if extra_args_for(&options.quality).is_empty() => &["-x"][..],
            _ => extra_args_for(&options.quality),
        };
        args.extend(extra.iter().map(OsString::from));
    } else {
        args.extend(["--merge-output-format", "mp4"].map(OsString::from));
        if options.convert == Some(ConvertPreset::SmallerMp4) {
            args.extend(ConvertPreset::SmallerMp4.args().iter().map(OsString::from));
        }
    }
    args.extend(["--embed-thumbnail", "--embed-chapters", "--embed-metadata"].map(OsString::from));
    if !options.playlist {
        args.push("--no-playlist".into());
    }
    if options.live_from_start {
        args.extend(["--live-from-start", "--wait-for-video", "30-600"].map(OsString::from));
    }
    let template = if options.playlist {
        PLAYLIST_TEMPLATE
    } else {
        SINGLE_TEMPLATE
    };
    args.push("-o".into());
    args.push(dest_dir.join(template).into());
    // `--print` implies `--quiet`, which hides progress unless asked for.
    args.push("--progress".into());
    for (flag, value) in [
        (
            "--progress-template",
            "download:TAURUS-PROGRESS %(progress)j",
        ),
        (
            "--progress-template",
            "postprocess:TAURUS-PP %(progress.postprocessor)s %(progress.status)s",
        ),
        ("--print", "after_move:TAURUS-FILE %(filepath)j"),
        (
            "--print",
            "before_dl:TAURUS-ENTRY %(playlist_index|0)s %(n_entries|1)s %(title)j",
        ),
    ] {
        args.push(flag.into());
        args.push(value.into());
    }
    // So that a link can never be taken for an option.
    args.push("--".into());
    args.push(url.into());
    args
}

/// What a run has shown so far, from its output lines.
#[derive(Clone, Debug)]
pub(crate) struct Tracker {
    dest_dir: PathBuf,
    pub phase: MediaPhase,
    pub entries_done: u32,
    pub entries_total: u32,
    /// The finished files, in the order yt-dlp moved them into place.
    pub files: Vec<PathBuf>,
    /// The partial files the download wrote to, newest last.
    pub parts: Vec<PathBuf>,
    /// Whether any progress line came (a live stream has begun).
    pub progressed: bool,
    /// Bytes of the entries before the current one.
    before_entry: u64,
    /// Bytes of the current entry's finished streams (video, then audio).
    entry_finished: u64,
    /// The stream downloading now: its file, bytes and size.
    stream: Option<String>,
    stream_done: u64,
    stream_total: Option<u64>,
    speed: u64,
}

/// What one line changed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Change {
    /// `(downloaded, total, speed)` to report.
    pub progress: Option<(u64, Option<u64>, u64)>,
    /// Phase, entries or files changed.
    pub view: bool,
}

impl Tracker {
    pub(crate) fn new(dest_dir: &Path, phase: MediaPhase) -> Tracker {
        Tracker {
            dest_dir: dest_dir.to_path_buf(),
            phase,
            entries_done: 0,
            entries_total: 0,
            files: Vec::new(),
            parts: Vec::new(),
            progressed: false,
            before_entry: 0,
            entry_finished: 0,
            stream: None,
            stream_done: 0,
            stream_total: None,
            speed: 0,
        }
    }

    /// Reads one stdout line of yt-dlp; lines that are not ours change
    /// nothing.
    pub(crate) fn apply(&mut self, line: &str) -> Change {
        let line = line.trim_end();
        if let Some(json) = line.strip_prefix(PROGRESS) {
            return serde_json::from_str::<Value>(json).map_or_else(
                |_| Change::default(),
                |progress| self.on_progress(&progress),
            );
        }
        if let Some(rest) = line.strip_prefix(POSTPROCESS) {
            let mut words = rest.split_whitespace();
            let (pp, status) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let phase = match pp {
                "Merger" => MediaPhase::Merging,
                "FFmpegExtractAudio" | "FFmpegVideoConvertor" | "FFmpegVideoRemuxer" => {
                    MediaPhase::Converting
                }
                _ => return Change::default(),
            };
            if status == "finished" {
                return Change::default();
            }
            return self.set_phase(phase);
        }
        if let Some(json) = line.strip_prefix(FILE) {
            let Ok(path) = serde_json::from_str::<String>(json) else {
                return Change::default();
            };
            let path = self.dest_dir.join(path);
            if !self.files.contains(&path) {
                self.files.push(path);
            }
            let done = u32::try_from(self.files.len()).unwrap_or(u32::MAX);
            self.entries_done = self.entries_done.max(done.min(self.entries_total.max(1)));
            return Change {
                progress: None,
                view: true,
            };
        }
        if let Some(rest) = line.strip_prefix(ENTRY) {
            let mut words = rest.split_whitespace();
            let number = |w: Option<&str>| w.and_then(|w| w.parse::<u32>().ok());
            let (Some(index), Some(count)) = (number(words.next()), number(words.next())) else {
                return Change::default();
            };
            self.before_entry += self.entry_finished + self.stream_done;
            self.entry_finished = 0;
            self.stream = None;
            self.stream_done = 0;
            self.stream_total = None;
            self.entries_total = count.max(1);
            self.entries_done = self.entries_done.max(index.saturating_sub(1));
            return Change {
                progress: None,
                view: true,
            };
        }
        Change::default()
    }

    /// The run ended well: every entry is done.
    pub(crate) fn finish(&mut self) {
        self.entries_total = self.entries_total.max(1);
        self.entries_done = self.entries_total;
    }

    fn on_progress(&mut self, progress: &Value) -> Change {
        let number = |key: &str| progress.get(key).and_then(Value::as_f64);
        let text = |key: &str| progress.get(key).and_then(Value::as_str);
        let done = number("downloaded_bytes").map_or(0, whole);
        let total = number("total_bytes")
            .or_else(|| number("total_bytes_estimate"))
            .map(whole);
        let file = text("filename").map(str::to_owned);
        if let Some(part) = text("tmpfilename") {
            let part = self.dest_dir.join(part);
            if self.parts.last() != Some(&part) {
                self.parts.retain(|p| *p != part);
                self.parts.push(part);
            }
        }
        // Another stream of the same entry (the audio after the video).
        if file.is_some() && self.stream.is_some() && file != self.stream {
            self.entry_finished += self.stream_done;
            self.stream_done = 0;
        }
        self.stream = file;
        self.speed = number("speed").map_or(0, whole);
        if text("status") == Some("finished") {
            self.entry_finished += total.unwrap_or(done).max(done);
            self.stream = None;
            self.stream_done = 0;
            self.stream_total = None;
            self.speed = 0;
        } else {
            self.stream_done = done;
            self.stream_total = total;
        }
        self.progressed = true;
        let view = self.phase != MediaPhase::Downloading;
        self.phase = MediaPhase::Downloading;
        Change {
            progress: Some(self.totals()),
            view,
        }
    }

    fn set_phase(&mut self, phase: MediaPhase) -> Change {
        let view = self.phase != phase;
        self.phase = phase;
        Change {
            progress: None,
            view,
        }
    }

    /// `(downloaded, total, speed)` of the whole job: for a playlist the
    /// entries done plus the current one's share, scaled to bytes when its
    /// size is known (else the bytes so far, with no total).
    fn totals(&self) -> (u64, Option<u64>, u64) {
        let entry_done = self.entry_finished + self.stream_done;
        let entry_total = self
            .stream_total
            .map(|t| self.entry_finished + t.max(self.stream_done));
        if self.entries_total <= 1 {
            return (entry_done, entry_total, self.speed);
        }
        match entry_total {
            Some(t) if t > 0 => {
                let share = entry_done as f64 / t as f64;
                let total = t.saturating_mul(u64::from(self.entries_total));
                let fraction =
                    (f64::from(self.entries_done) + share) / f64::from(self.entries_total);
                (whole(fraction * total as f64), Some(total), self.speed)
            }
            _ => (self.before_entry + entry_done, None, self.speed),
        }
    }
}

fn whole(n: f64) -> u64 {
    if n.is_finite() && n > 0.0 {
        n as u64
    } else {
        0
    }
}

/// Why a run was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Paused (or the engine shuts down): the next run continues.
    Pause,
    /// A live recording ends here, keeping what was recorded.
    Finish,
}

/// How a run ended.
#[derive(Debug)]
pub(crate) struct Ended {
    /// `None` if it was killed.
    pub status: Option<ExitStatus>,
    pub stopped: Option<Stop>,
    /// The end of what it printed to stderr.
    pub stderr: String,
}

impl Ended {
    pub(crate) fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }
}

/// Starts `command` (yt-dlp with all its arguments) as a process tree and
/// hands each stdout line to `on_line` until it exits. When `pause` or
/// `finish` is cancelled, the tree gets the graceful interrupt (Ctrl+C;
/// Ctrl+Break on Windows), and is killed if it has not exited `grace`
/// later, or at once if the interrupt cannot be sent.
pub(crate) async fn run_process(
    mut command: Command,
    pause: &CancellationToken,
    finish: &CancellationToken,
    grace: Duration,
    mut on_line: impl FnMut(&str),
) -> Result<Ended, CoreError> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut run = TreeChild::spawn(&mut command)
        .await
        .map_err(|e| CoreError {
            kind: ErrorKind::ToolFailed,
            detail: format!("could not run yt-dlp: {e}"),
        })?;
    let stdout = run.child.stdout.take().expect("stdout is piped");
    let stderr = run.child.stderr.take().expect("stderr is piped");
    let stderr = tokio::spawn(read_tail(stderr, MAX_STDERR));
    let mut lines = BufReader::new(stdout).lines();
    let mut out_open = true;
    let mut stopped = None;
    let mut deadline: Option<Instant> = None;
    let status = loop {
        tokio::select! {
            // Stopping first, so a flood of output cannot hold it up.
            biased;
            _ = pause.cancelled(), if stopped.is_none() => {
                stopped = Some(Stop::Pause);
                deadline = Some(interrupt(&run, grace));
            }
            _ = finish.cancelled(), if stopped.is_none() => {
                stopped = Some(Stop::Finish);
                deadline = Some(interrupt(&run, grace));
            }
            () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                if deadline.is_some() =>
            {
                tracing::info!(pid = run.pid(), "yt-dlp did not stop in time; killing it");
                if let Err(e) = run.kill().await {
                    tracing::warn!("could not kill yt-dlp: {e}");
                }
                break None;
            }
            line = lines.next_line(), if out_open => match line {
                Ok(Some(line)) => on_line(&line),
                _ => out_open = false,
            },
            status = run.child.wait() => break Some(status),
        }
    };
    let status = status.transpose().map_err(|e| CoreError {
        kind: ErrorKind::ToolFailed,
        detail: format!("could not wait for yt-dlp: {e}"),
    })?;
    if out_open {
        let _ = tokio::time::timeout(DRAIN, async {
            while let Ok(Some(line)) = lines.next_line().await {
                on_line(&line);
            }
        })
        .await;
    }
    let stderr = match tokio::time::timeout(DRAIN, stderr).await {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        _ => String::new(),
    };
    Ok(Ended {
        status,
        stopped,
        stderr,
    })
}

/// Sends the graceful interrupt; returns when to kill if the tree is still
/// there: after `grace`, or now if the interrupt could not be sent.
fn interrupt(run: &TreeChild, grace: Duration) -> Instant {
    match run.interrupt() {
        Ok(()) => Instant::now() + grace,
        Err(e) => {
            tracing::info!("could not interrupt yt-dlp, killing it: {e}");
            Instant::now()
        }
    }
}

/// Reads to the end, keeping the last `cap` bytes or so.
async fn read_tail(mut reader: impl AsyncRead + Unpin, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * cap {
                    kept.drain(..kept.len() - cap);
                }
            }
        }
    }
}

/// The partial file a stopped recording left: the newest one the run
/// reported that still exists, else the newest `.part` or `.ts` file in
/// `dir` changed since `since` (yt-dlp names it after the stream).
pub(crate) fn recorded_part(
    dir: &Path,
    reported: &[PathBuf],
    since: SystemTime,
) -> Option<PathBuf> {
    if let Some(part) = reported.iter().rev().find(|p| p.is_file()) {
        return Some(part.clone());
    }
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.ends_with(".part") || name.ends_with(".ts")
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?;
            (meta.is_file() && modified >= since).then(|| (modified, e.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Where a partial recording goes once remuxed: its name without `.part`
/// and without its own extension, as `.mp4`.
pub(crate) fn remuxed_name(part: &Path) -> PathBuf {
    let name = part
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let name = name.strip_suffix(".part").unwrap_or(&name);
    let stem = Path::new(name)
        .file_stem()
        .map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned());
    part.with_file_name(format!("{stem}.mp4"))
}

/// Copies the streams of a partial recording into a playable MP4 with
/// ffmpeg (`ffmpeg -y -i <part> -c copy <final>.mp4`) and deletes the part
/// only if that worked. Returns the MP4.
pub(crate) async fn remux_part(ffmpeg: &Path, part: &Path) -> Result<PathBuf, CoreError> {
    let out = remuxed_name(part);
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-i")
        .arg(part)
        .args(["-c", "copy"])
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut run = TreeChild::spawn(&mut command)
        .await
        .map_err(|e| tool_failed(format!("could not run ffmpeg: {e}")))?;
    let mut stderr = run.child.stderr.take().expect("stderr is piped");
    let (tail, status) = tokio::join!(read_tail(&mut stderr, 16 * 1024), run.child.wait());
    let status = status.map_err(|e| tool_failed(format!("could not wait for ffmpeg: {e}")))?;
    if !status.success() || !out.is_file() {
        let tail = String::from_utf8_lossy(&tail);
        let last = tail.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(tool_failed(format!(
            "ffmpeg could not finish the recording ({status}): {last}"
        )));
    }
    if let Err(e) = std::fs::remove_file(part) {
        tracing::warn!("could not remove {}: {e}", part.display());
    }
    Ok(out)
}

fn tool_failed(detail: String) -> CoreError {
    CoreError {
        kind: ErrorKind::ToolFailed,
        detail,
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
