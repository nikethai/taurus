//! Reading ffprobe and ffmpeg: the length of the file to convert, and the
//! progress lines ffmpeg prints.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::tools::tree::TreeChild;

/// How long ffprobe may take; a file it cannot read quickly is converted
/// without a known length.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The media time in milliseconds of an `out_time_ms=` line of ffmpeg's
/// `-progress` output (whose `_ms` is in fact microseconds; `out_time_us=`
/// says the same); `None` for any other line or `N/A`.
pub(crate) fn progress_ms(line: &str) -> Option<u64> {
    let value = line
        .strip_prefix("out_time_ms=")
        .or_else(|| line.strip_prefix("out_time_us="))?;
    let micros: i64 = value.trim().parse().ok()?;
    Some(u64::try_from(micros).unwrap_or(0) / 1000)
}

/// The length of `input` from `ffprobe -v error -show_entries
/// format=duration -of csv=p=0 <input>` (a `ffprobe` beside `ffmpeg`);
/// `None` if it cannot say. The process is stopped, with what it started,
/// when this future is dropped.
pub(crate) async fn probe_duration(ffprobe: &Path, input: &Path) -> Option<Duration> {
    let mut command = Command::new(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut run = TreeChild::spawn(&mut command).await.ok()?;
    let stdout = run.child.stdout.take()?;
    let mut text = String::new();
    let read = async {
        // A duration is a few bytes.
        stdout.take(4096).read_to_string(&mut text).await?;
        run.wait().await
    };
    let status = tokio::time::timeout(PROBE_TIMEOUT, read).await.ok()?.ok()?;
    if !status.success() {
        return None;
    }
    let seconds: f64 = text.lines().next()?.trim().parse().ok()?;
    (seconds.is_finite() && seconds > 0.0 && seconds < 1e9)
        .then(|| Duration::from_secs_f64(seconds))
}
