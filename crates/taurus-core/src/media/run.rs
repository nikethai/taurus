//! One yt-dlp download run: its arguments, reading what it prints, and
//! stopping it gracefully. What a stopped live recording left behind is
//! kept by `remux`.
//!
//! The engine's media job (`engine::media`) decides when to run, retry or
//! give up; this module only knows yt-dlp.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
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
/// What every line of ours starts with, on either stream.
const OURS: &str = "TAURUS-";

/// The most of one output line kept; the rest of a longer line is dropped.
const MAX_LINE: usize = 64 * 1024;
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

/// yt-dlp's arguments after the global flags, ending with the link;
/// `limit_rate` (bytes per second) caps the download speed.
pub(crate) fn download_args(
    options: &MediaOptions,
    dest_dir: &Path,
    url: &str,
    limit_rate: Option<u64>,
) -> Vec<OsString> {
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
    if let Some(rate) = limit_rate {
        args.push("--limit-rate".into());
        args.push(rate.max(1).to_string().into());
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
            "before_dl:TAURUS-ENTRY %(playlist_index|0)s %(n_entries|1)s %(id)j %(title)j",
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
    /// The id of the video downloading now (yt-dlp's `%(id)s`, which the
    /// output names carry as `[<id>]`).
    pub video_id: Option<String>,
    /// The job's unfinished files: every `tmpfilename` reported, and the
    /// `filename` it becomes, until reported finished (`TAURUS-FILE`).
    /// Seeded by the runner with what earlier runs recorded; a finished
    /// entry also drops those that no longer exist (yt-dlp removed them).
    pub partials: Vec<PathBuf>,
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
    /// `partials` changed (to be stored).
    pub partials: bool,
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
            video_id: None,
            partials: Vec::new(),
            before_entry: 0,
            entry_finished: 0,
            stream: None,
            stream_done: 0,
            stream_total: None,
            speed: 0,
        }
    }

    /// Reads one line yt-dlp printed (on stdout or stderr); lines that are
    /// not ours change nothing.
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
            // yt-dlp's `pp_key()`: the class name without `FFmpeg` and `PP`
            // (the prefixed spellings are kept, in case that ever changes).
            let phase = match pp.strip_prefix("FFmpeg").unwrap_or(pp) {
                "Merger" => MediaPhase::Merging,
                "ExtractAudio" | "VideoConvertor" | "VideoRemuxer" => MediaPhase::Converting,
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
            let path = local_path(&self.dest_dir, &path);
            let recorded = self.partials.len();
            // Finished, it is the user's; what yt-dlp cleaned up is gone.
            self.partials.retain(|p| *p != path && p.exists());
            if !self.files.contains(&path) {
                self.files.push(path);
            }
            let done = u32::try_from(self.files.len()).unwrap_or(u32::MAX);
            self.entries_done = self.entries_done.max(done.min(self.entries_total.max(1)));
            return Change {
                progress: None,
                view: true,
                partials: self.partials.len() != recorded,
            };
        }
        if let Some(rest) = line.strip_prefix(ENTRY) {
            let mut words = rest.splitn(3, ' ');
            let number = |w: Option<&str>| w.and_then(|w| w.parse::<u32>().ok());
            let (Some(index), Some(count)) = (number(words.next()), number(words.next())) else {
                return Change::default();
            };
            // Then the id and the title, each a JSON string.
            let mut strings = serde_json::Deserializer::from_str(words.next().unwrap_or(""))
                .into_iter::<String>();
            self.video_id = strings
                .next()
                .and_then(Result::ok)
                .filter(|id| !id.is_empty());
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
                partials: false,
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
        let mut partials = false;
        if let Some(part) = text("tmpfilename") {
            let part = local_path(&self.dest_dir, part);
            if self.parts.last() != Some(&part) {
                self.parts.retain(|p| *p != part);
                self.parts.push(part.clone());
            }
            // Only with a `tmpfilename`: this run writes it (a file already
            // downloaded is reported without one, and is not the job's to
            // delete).
            partials |= self.record(part);
            if let Some(file) = &file {
                partials |= self.record(local_path(&self.dest_dir, file));
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
            partials,
        }
    }

    /// Adds `path` to `partials`; true if it was not there.
    fn record(&mut self, path: PathBuf) -> bool {
        if self.partials.contains(&path) {
            return false;
        }
        self.partials.push(path);
        true
    }

    fn set_phase(&mut self, phase: MediaPhase) -> Change {
        let view = self.phase != phase;
        self.phase = phase;
        Change {
            progress: None,
            view,
            partials: false,
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

/// A path yt-dlp printed (absolute, or relative to its working folder,
/// `dest_dir`) in one spelling: no `\\?\` prefix where a plain path does,
/// and the platform's separator throughout (yt-dlp may mix `/` into a
/// Windows path from the output template).
pub(crate) fn local_path(dest_dir: &Path, printed: &str) -> PathBuf {
    dunce::simplified(&dest_dir.join(printed))
        .components()
        .collect()
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
    /// The end of what it printed to stderr, without the lines of ours.
    pub stderr: String,
}

impl Ended {
    pub(crate) fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }
}

/// Starts `command` (yt-dlp with all its arguments) as a process tree and
/// hands each line of ours to `on_line` until it exits: from stdout, and
/// from stderr too, where yt-dlp prints the postprocessing template under
/// the `--quiet` that `--print` implies (the rest of stderr is kept as the
/// error tail). When `pause` or `finish` is cancelled, the tree gets the
/// graceful interrupt (Ctrl+C; Ctrl+Break on Windows), and is killed if it
/// has not exited `grace` later, or at once if the interrupt cannot be sent.
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
    let mut out = Stream::new(run.child.stdout.take().expect("stdout is piped"));
    let mut err = Stream::new(run.child.stderr.take().expect("stderr is piped"));
    let mut tail = Vec::new();
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
            line = out.next(), if out.open => {
                if let Some(text) = line {
                    on_line(&text);
                }
            }
            line = err.next(), if err.open => {
                if let Some(text) = line {
                    from_stderr(&text, &mut on_line, &mut tail);
                }
            }
            status = run.child.wait() => break Some(status),
        }
    };
    let status = status.transpose().map_err(|e| CoreError {
        kind: ErrorKind::ToolFailed,
        detail: format!("could not wait for yt-dlp: {e}"),
    })?;
    // What is still in the pipes.
    let _ = tokio::time::timeout(DRAIN, async {
        while out.open || err.open {
            tokio::select! {
                line = out.next(), if out.open => {
                    if let Some(text) = line {
                        on_line(&text);
                    }
                }
                line = err.next(), if err.open => {
                    if let Some(text) = line {
                        from_stderr(&text, &mut on_line, &mut tail);
                    }
                }
            }
        }
    })
    .await;
    Ok(Ended {
        status,
        stopped,
        stderr: String::from_utf8_lossy(&tail).into_owned(),
    })
}

/// One of yt-dlp's output pipes, read line by line.
struct Stream<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
    open: bool,
}

impl<R: AsyncRead + Unpin> Stream<R> {
    fn new(reader: R) -> Stream<R> {
        Stream {
            reader: BufReader::new(reader),
            line: Vec::new(),
            open: true,
        }
    }

    /// The next line; `None` at the end, or on a read error (nothing more
    /// can come), after which `open` is false. Cancel-safe.
    async fn next(&mut self) -> Option<String> {
        match read_line(&mut self.reader, &mut self.line).await {
            Ok(true) => Some(take_line(&mut self.line)),
            Ok(false) | Err(_) => {
                self.open = false;
                None
            }
        }
    }
}

/// A stderr line: ours goes to `on_line`, the rest to the error tail, of
/// which the last `MAX_STDERR` bytes or so are kept.
fn from_stderr(line: &str, on_line: &mut impl FnMut(&str), tail: &mut Vec<u8>) {
    if line.starts_with(OURS) {
        on_line(line);
        return;
    }
    tail.extend_from_slice(line.as_bytes());
    tail.push(b'\n');
    if tail.len() > 2 * MAX_STDERR {
        tail.drain(..tail.len() - MAX_STDERR);
    }
}

/// Reads the rest of a line into `line` (without the newline), keeping at
/// most `MAX_LINE` bytes of it so a line without end cannot use up memory;
/// false at the end of the output (a last line without newline is still
/// handed out). Cancel-safe: what was read stays in `line`.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        let room = MAX_LINE.saturating_sub(line.len()).min(chunk.len());
        line.extend_from_slice(&chunk[..room]);
        let consumed = newline.map_or(available.len(), |at| at + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// The line `read_line` read, as text (bytes that are not UTF-8 replaced),
/// leaving `line` empty for the next one.
fn take_line(line: &mut Vec<u8>) -> String {
    let text = String::from_utf8_lossy(line)
        .trim_end_matches('\r')
        .to_owned();
    line.clear();
    text
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

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
