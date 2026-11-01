//! Running yt-dlp to completion and keeping what it printed.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::errors::{CoreError, ErrorKind};
use crate::tools::tree::TreeChild;

/// The most of each stream that is kept; the rest is read and dropped.
const MAX_STDOUT: usize = 64 * 1024 * 1024;
const MAX_STDERR: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Captured {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Starts `command` (stdout and stderr are piped here) and waits for it to
/// finish. After `timeout` the process and everything it started are
/// killed (see `tools::tree`) and the error is `Timeout`; if it cannot
/// start, `ToolFailed`.
pub(super) async fn run_captured(
    mut command: tokio::process::Command,
    timeout: Duration,
) -> Result<Captured, CoreError> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut run = TreeChild::spawn(&mut command)
        .await
        .map_err(|e| CoreError {
            kind: ErrorKind::ToolFailed,
            detail: format!("could not run yt-dlp: {e}"),
        })?;
    let mut stdout = run.child.stdout.take().expect("stdout is piped");
    let mut stderr = run.child.stderr.take().expect("stderr is piped");
    let finished = tokio::time::timeout(timeout, async {
        let (out, err, status) = tokio::join!(
            read_capped(&mut stdout, MAX_STDOUT),
            read_capped(&mut stderr, MAX_STDERR),
            run.child.wait()
        );
        (out, err, status)
    })
    .await;
    match finished {
        Err(_) => {
            if let Err(e) = run.kill().await {
                tracing::warn!("could not stop yt-dlp: {e}");
            }
            Err(CoreError {
                kind: ErrorKind::Timeout,
                detail: format!("yt-dlp did not finish within {} s", timeout.as_secs()),
            })
        }
        Ok((out, err, status)) => {
            let status = status.map_err(|e| CoreError {
                kind: ErrorKind::ToolFailed,
                detail: format!("could not wait for yt-dlp: {e}"),
            })?;
            Ok(Captured {
                success: status.success(),
                stdout: out,
                stderr: String::from_utf8_lossy(&err).into_owned(),
            })
        }
    }
}

/// Reads to the end, keeping the first `cap` bytes; a read error ends it.
async fn read_capped(reader: &mut (impl AsyncRead + Unpin), cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    // On the heap: a future holding it inline would be 16 KiB larger.
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len()).min(n);
                kept.extend_from_slice(&buf[..room]);
            }
        }
    }
}
