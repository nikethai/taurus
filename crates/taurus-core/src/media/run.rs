//! One yt-dlp download run: its arguments, reading what it prints, and
//! stopping it gracefully. What a stopped live recording left behind is
//! kept by `remux`.
//!
//! The engine's media job (`engine::media`) decides when to run, retry or
//! give up; this module only knows yt-dlp.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::tools::tree::TreeChild;

use super::tracker::OURS;
pub(crate) use super::tracker::Tracker;
#[cfg(test)]
use super::tracker::{Change, ENTRY, FILE, POSTPROCESS, PROGRESS, local_path};
use super::{Container, ConvertPreset, MediaOptions, cookie_and_subtitle_args, extra_args_for};

/// Output name of a single video, inside the job's folder.
pub(crate) const SINGLE_TEMPLATE: &str = "%(title).200B [%(id)s].%(ext)s";
/// Output name of a playlist entry: a folder per playlist.
pub(crate) const PLAYLIST_TEMPLATE: &str =
    "%(playlist_title).100B/%(playlist_index)03d - %(title).150B [%(id)s].%(ext)s";

/// The most of one output line kept; the rest of a longer line is dropped.
const MAX_LINE: usize = 64 * 1024;
/// The most of stderr kept: its end, where the errors are.
const MAX_STDERR: usize = 256 * 1024;
/// How long the output is still read after yt-dlp exits (something it
/// started may hold the pipe open).
const DRAIN: Duration = Duration::from_secs(2);
/// How often a run's caller hears `Output::Tick`.
const TICK: Duration = Duration::from_secs(1);

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
/// `limit_rate` (bytes per second) caps the download speed. A live or
/// upcoming stream waits for its start; with `from_start` it is recorded
/// from its beginning (`--live-from-start`, which only some sites allow).
pub(crate) fn download_args(
    options: &MediaOptions,
    dest_dir: &Path,
    url: &str,
    limit_rate: Option<u64>,
    from_start: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-f".into(), options.selector.clone().into()];
    let audio = options.audio_only || options.convert == Some(ConvertPreset::Mp3);
    args.extend(cookie_and_subtitle_args(
        options.cookies_from_browser.as_deref(),
        options.subtitles.as_ref(),
        audio,
        options.container,
    ));
    if audio {
        let extra = match options.convert {
            Some(ConvertPreset::Mp3) => ConvertPreset::Mp3.args(),
            _ if extra_args_for(&options.quality).is_empty() => &["-x"][..],
            _ => extra_args_for(&options.quality),
        };
        args.extend(extra.iter().map(OsString::from));
    } else {
        let container = options.container.unwrap_or(Container::Mp4);
        args.extend(["--merge-output-format", container.format()].map(OsString::from));
        if options.convert == Some(ConvertPreset::SmallerMp4) {
            args.extend(ConvertPreset::SmallerMp4.args().iter().map(OsString::from));
        }
    }
    args.extend(["--embed-thumbnail", "--embed-chapters", "--embed-metadata"].map(OsString::from));
    if !options.playlist {
        args.push("--no-playlist".into());
    }
    if options.live_from_start {
        if from_start {
            args.push("--live-from-start".into());
        }
        args.extend(["--wait-for-video", "30-600"].map(OsString::from));
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
            // `filename`: where the download goes (its `.part` first).
            "before_dl:TAURUS-ENTRY %(playlist_index|0)s %(n_entries|1)s %(id)j %(title)j \
             %(filename)j",
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

/// What a run hands its caller while it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Output<'a> {
    /// A line yt-dlp printed (on stdout, or one of ours on stderr).
    Line(&'a str),
    /// Another second has passed (yt-dlp may print nothing for long, as
    /// while it records a live stream through ffmpeg).
    Tick,
}

/// Starts `command` (yt-dlp with all its arguments) as a process tree and
/// hands each line of ours to `on_output` until it exits: from stdout, and
/// from stderr too, where yt-dlp prints the postprocessing template under
/// the `--quiet` that `--print` implies (the rest of stderr is kept as the
/// error tail); and an `Output::Tick` every second. When `pause` or
/// `finish` is cancelled, the tree gets the graceful interrupt (Ctrl+C;
/// Ctrl+Break on Windows), and is killed if it has not exited `grace`
/// later, or at once if the interrupt cannot be sent. However it ends,
/// nothing it started is left running.
pub(crate) async fn run_process(
    mut command: Command,
    pause: &CancellationToken,
    finish: &CancellationToken,
    grace: Duration,
    mut on_output: impl FnMut(Output<'_>),
) -> Result<Ended, CoreError> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let program = Path::new(command.as_std().get_program())
        .file_stem()
        .map_or_else(|| "the tool".into(), |n| n.to_string_lossy().into_owned());
    let mut run = TreeChild::spawn(&mut command)
        .await
        .map_err(|e| CoreError {
            kind: ErrorKind::ToolFailed,
            detail: format!("could not run {program}: {e}"),
        })?;
    let mut out = Stream::new(run.child.stdout.take().expect("stdout is piped"));
    let mut err = Stream::new(run.child.stderr.take().expect("stderr is piped"));
    let mut tail = Vec::new();
    let mut stopped = None;
    let mut deadline: Option<Instant> = None;
    let mut ticks = tokio::time::interval_at(Instant::now() + TICK, TICK);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                    on_output(Output::Line(&text));
                }
            }
            line = err.next(), if err.open => {
                if let Some(text) = line {
                    from_stderr(&text, &mut on_output, &mut tail);
                }
            }
            // Also kills what it leaves running (see `TreeChild::wait`).
            status = run.wait() => break Some(status),
            _ = ticks.tick() => on_output(Output::Tick),
        }
    };
    let status = status.transpose().map_err(|e| CoreError {
        kind: ErrorKind::ToolFailed,
        detail: format!("could not wait for {program}: {e}"),
    })?;
    // What is still in the pipes.
    let _ = tokio::time::timeout(DRAIN, async {
        while out.open || err.open {
            tokio::select! {
                line = out.next(), if out.open => {
                    if let Some(text) = line {
                        on_output(Output::Line(&text));
                    }
                }
                line = err.next(), if err.open => {
                    if let Some(text) = line {
                        from_stderr(&text, &mut on_output, &mut tail);
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

/// A stderr line: ours goes to `on_output`, the rest to the error tail, of
/// which the last `MAX_STDERR` bytes or so are kept.
fn from_stderr(line: &str, on_output: &mut impl FnMut(Output<'_>), tail: &mut Vec<u8>) {
    if line.starts_with(OURS) {
        on_output(Output::Line(line));
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
