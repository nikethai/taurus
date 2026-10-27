//! Human-readable text for the terminal: pure functions, no I/O.

use std::fmt::Write;

use taurus_core::{JobState, JobView};

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
        JobState::Queued | JobState::Downloading => format!("{:?}", view.state),
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

    use taurus_core::{ErrorKind, JobId, JobState};

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
