//! Human-readable text for the terminal: pure functions, no I/O.

use std::fmt::Write;
use std::time::Duration;

use taurus_core::{JobId, JobState, JobView, WhenDone};

const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];

/// `62.0%  3.8 GiB / 6.1 GiB  18.4 MB/s`; without a known total,
/// `3.8 GiB  18.4 MB/s`.
pub fn progress_line(downloaded: u64, total: Option<u64>, speed_bps: u64) -> String {
    match total {
        Some(total) => format!(
            "{}  {} / {}  {}",
            percent(downloaded, total),
            bytes(downloaded),
            bytes(total),
            speed(speed_bps)
        ),
        None => format!("{}  {}", bytes(downloaded), speed(speed_bps)),
    }
}

/// Binary units with one decimal; whole bytes below 1 KiB.
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Always decimal megabytes per second.
pub fn speed(bps: u64) -> String {
    format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
}

fn percent(downloaded: u64, total: u64) -> String {
    let ratio = if total == 0 {
        1.0
    } else {
        (downloaded as f64 / total as f64).min(1.0)
    };
    format!("{:.1}%", ratio * 100.0)
}

/// The line printed when a foreground download ends.
pub fn outcome_line(view: &JobView) -> String {
    match view.state {
        JobState::Completed => {
            format!(
                "Completed: {}",
                view.dest_dir.join(&view.file_name).display()
            )
        }
        JobState::Paused => format!("Paused (continue with: taurus resume {})", view.id.0),
        JobState::Failed => match &view.error {
            Some((kind, detail)) => format!("Failed: {kind:?}: {detail}"),
            None => "Failed".to_string(),
        },
        // Left so by Ctrl-C: the next queue run starts it.
        JobState::Queued | JobState::Waiting => {
            format!("{:?} (continue with: taurus run)", view.state)
        }
        JobState::Downloading => format!("{:?}", view.state),
    }
}

/// One running job in the progress block: `#3 x.bin  62.0%  ...`.
pub fn job_line(id: JobId, name: &str, status: &str) -> String {
    format!("#{} {name}  {status}", id.0)
}

/// The status of a job that waits for the network to come back.
pub fn waiting_line(retry_in: Duration) -> String {
    format!("waiting for connection (retry in {})", rough(retry_in))
}

/// Whole seconds (rounded up) below a minute, else whole minutes (rounded).
pub fn rough(d: Duration) -> String {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", (secs + 30) / 60)
    }
}

/// Shown while a Sleep or Shutdown counts down.
pub fn countdown_line(action: WhenDone, seconds_left: u64) -> String {
    format!(
        "{} in {seconds_left}s (Ctrl-C cancels)",
        when_done_name(action)
    )
}

pub fn cancelled_line(action: WhenDone) -> String {
    format!("{} cancelled.", when_done_name(action))
}

/// The line for a Sleep or Shutdown that has come due: its command, which
/// only would have run in a dry run; `None` for the other actions.
pub fn fired_line(action: WhenDone, command: Option<&str>, dry_run: bool) -> Option<String> {
    if !matches!(action, WhenDone::Sleep | WhenDone::Shutdown) {
        return None;
    }
    Some(match (command, dry_run) {
        (Some(command), true) => format!("would run: {command}"),
        (Some(command), false) => format!("running: {command}"),
        (None, _) => format!("{} is not supported here", when_done_name(action)),
    })
}

/// Printed when the queue is done and when-done is Notify.
pub const ALL_FINISHED: &str = "All downloads finished.";

fn when_done_name(action: WhenDone) -> &'static str {
    match action {
        WhenDone::Sleep => "Sleep",
        WhenDone::Shutdown => "Shutdown",
        WhenDone::Nothing => "Nothing",
        WhenDone::Notify => "Notify",
        WhenDone::Quit => "Quit",
    }
}

/// One row per job: id, state, percent, size, name.
pub fn job_table(views: &[JobView]) -> String {
    if views.is_empty() {
        return "No downloads.\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:>4}  {:<11}  {:>7}  {:>10}  NAME",
        "ID", "STATE", "PERCENT", "SIZE"
    );
    for view in views {
        let (percent, size) = match view.total {
            Some(total) => (percent(view.downloaded, total), bytes(total)),
            None => ("-".to_string(), "-".to_string()),
        };
        let _ = writeln!(
            out,
            "{:>4}  {:<11}  {percent:>7}  {size:>10}  {}",
            view.id.0,
            format!("{:?}", view.state),
            view.file_name
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use taurus_core::ErrorKind;

    use super::*;

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn view(state: JobState) -> JobView {
        JobView {
            id: JobId(7),
            url: "http://example.test/x.bin".to_string(),
            file_name: "x.bin".to_string(),
            dest_dir: PathBuf::from("out"),
            state,
            total: Some(2048),
            downloaded: 512,
            speed_bps: 0,
            resumable: true,
            error: None,
        }
    }

    #[test]
    fn progress_line_matches_the_spec_example() {
        let total = (6.1 * GIB) as u64;
        let downloaded = (total as f64 * 0.62) as u64;
        assert_eq!(
            progress_line(downloaded, Some(total), 18_400_000),
            "62.0%  3.8 GiB / 6.1 GiB  18.4 MB/s"
        );
    }

    #[test]
    fn progress_line_without_total_omits_percent() {
        assert_eq!(progress_line(3 * 1024 * 1024, None, 0), "3.0 MiB  0.0 MB/s");
    }

    #[test]
    fn progress_line_of_empty_file_is_complete() {
        assert_eq!(progress_line(0, Some(0), 0), "100.0%  0 B / 0 B  0.0 MB/s");
    }

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes((3.8 * GIB) as u64), "3.8 GiB");
        assert_eq!(bytes(2 * 1024u64.pow(4)), "2.0 TiB");
        assert_eq!(bytes(2048 * 1024u64.pow(4)), "2048.0 TiB");
    }

    #[test]
    fn speed_is_decimal_megabytes() {
        assert_eq!(speed(0), "0.0 MB/s");
        assert_eq!(speed(18_400_000), "18.4 MB/s");
        assert_eq!(speed(960_000), "1.0 MB/s");
        assert_eq!(speed(2_000_000_000), "2000.0 MB/s");
    }

    #[test]
    fn outcome_lines() {
        let done = view(JobState::Completed);
        let path = PathBuf::from("out").join("x.bin");
        assert_eq!(
            outcome_line(&done),
            format!("Completed: {}", path.display())
        );
        assert_eq!(
            outcome_line(&view(JobState::Paused)),
            "Paused (continue with: taurus resume 7)"
        );
        let mut failed = view(JobState::Failed);
        failed.error = Some((ErrorKind::LinkExpired, "HTTP 404".to_string()));
        assert_eq!(outcome_line(&failed), "Failed: LinkExpired: HTTP 404");
        assert_eq!(
            outcome_line(&view(JobState::Queued)),
            "Queued (continue with: taurus run)"
        );
    }

    #[test]
    fn job_and_waiting_lines() {
        assert_eq!(
            job_line(JobId(3), "x.bin", "1.0 KiB  0.0 MB/s"),
            "#3 x.bin  1.0 KiB  0.0 MB/s"
        );
        assert_eq!(
            waiting_line(Duration::from_secs(60)),
            "waiting for connection (retry in 1m)"
        );
    }

    #[test]
    fn rough_durations() {
        let rough_ms = |ms| rough(Duration::from_millis(ms));
        assert_eq!(rough_ms(0), "0s");
        assert_eq!(rough_ms(300), "1s");
        assert_eq!(rough_ms(59_000), "59s");
        assert_eq!(rough_ms(59_001), "1m");
        assert_eq!(rough_ms(89_000), "1m");
        assert_eq!(rough_ms(90_000), "2m");
        assert_eq!(rough_ms(600_000), "10m");
    }

    #[test]
    fn when_done_lines() {
        assert_eq!(
            countdown_line(WhenDone::Sleep, 60),
            "Sleep in 60s (Ctrl-C cancels)"
        );
        assert_eq!(cancelled_line(WhenDone::Shutdown), "Shutdown cancelled.");
        assert_eq!(
            fired_line(WhenDone::Sleep, Some("systemctl suspend"), true).as_deref(),
            Some("would run: systemctl suspend")
        );
        assert_eq!(
            fired_line(WhenDone::Shutdown, Some("shutdown /s /t 0"), false).as_deref(),
            Some("running: shutdown /s /t 0")
        );
        assert_eq!(
            fired_line(WhenDone::Sleep, None, true).as_deref(),
            Some("Sleep is not supported here")
        );
        assert_eq!(fired_line(WhenDone::Notify, None, false), None);
        assert_eq!(fired_line(WhenDone::Quit, None, false), None);
    }

    #[test]
    fn completed_job_with_a_note_is_not_a_failure() {
        let mut done = view(JobState::Completed);
        done.error = Some((ErrorKind::FileChangedOnServer, "ETag changed".to_string()));
        assert!(outcome_line(&done).starts_with("Completed: "));
    }

    #[test]
    fn job_table_lists_id_state_percent_size_name() {
        let mut unknown = view(JobState::Downloading);
        unknown.id = JobId(12);
        unknown.total = None;
        unknown.file_name = "stream.ts".to_string();
        let table = job_table(&[view(JobState::Paused), unknown]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "  ID  STATE        PERCENT        SIZE  NAME");
        assert_eq!(lines[1], "   7  Paused         25.0%     2.0 KiB  x.bin");
        assert_eq!(
            lines[2],
            "  12  Downloading        -           -  stream.ts"
        );
    }

    #[test]
    fn job_table_of_no_jobs_says_so() {
        assert_eq!(job_table(&[]), "No downloads.\n");
    }
}
