//! Test-only helpers shared by the unit tests.

use std::path::PathBuf;
use taurus_core::{JobId, JobKind, JobState, JobView};

pub fn view(id: i64, name: &str, state: JobState) -> JobView {
    JobView {
        id: JobId(id),
        url: format!("https://example.org/{name}"),
        source_url: format!("https://example.org/{name}"),
        file_name: name.to_string(),
        dest_dir: PathBuf::from("/tmp"),
        state,
        total: Some(1000),
        downloaded: 0,
        speed_bps: 0,
        resumable: true,
        error: None,
        kind: JobKind::Http,
        torrent: None,
        retry_at: None,
        finished_at: None,
        info_hash: None,
    }
}
