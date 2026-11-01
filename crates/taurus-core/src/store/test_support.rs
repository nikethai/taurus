//! Fixtures shared by the store unit tests.

use std::path::PathBuf;

use crate::plan::Segment;

use super::{JobKind, NewJob, Store};

pub(super) fn open_temp() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("taurus.db")).unwrap();
    (dir, store)
}

pub(super) fn new_job(name: &str, dest: &str) -> NewJob {
    NewJob {
        url: format!("http://example.test/{name}"),
        source_url: format!("http://example.test/share/{name}"),
        dest_dir: PathBuf::from(dest),
        file_name: name.to_string(),
        connections: 8,
        mirrors: Vec::new(),
        checksum: None,
        referer: None,
        queue_pos: 0,
        kind: JobKind::Http,
        info_hash: None,
        torrent_meta: None,
        only_files: None,
    }
}

pub(super) fn torrent_job(name: &str, info_hash: &str) -> NewJob {
    NewJob {
        kind: JobKind::Torrent,
        info_hash: Some(info_hash.into()),
        torrent_meta: Some(vec![0, 1, 2, 255]),
        only_files: Some(vec![0, 2]),
        ..new_job(name, "/d")
    }
}

pub(super) fn seg(start: u64, end: u64, done: u64) -> Segment {
    Segment { start, end, done }
}
