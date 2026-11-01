//! Torrents: pure logic (input classification, path safety, output folders
//! and seeding policy), in `hub`, the librqbit session, and in `job`, the
//! runner that drives one torrent job.

pub mod files;
pub mod hub;
pub mod job;
mod meta;
mod options;
mod persist;
pub mod policy;
pub mod source;

pub(crate) use meta::{MAX_TORRENT_BYTES, read_torrent_file, too_large};

/// One file of a torrent.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TorrentFile {
    pub index: usize,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    pub size: u64,
}

/// What a torrent's metadata says about it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TorrentInfo {
    /// 40 lowercase hex characters.
    pub info_hash: String,
    pub name: String,
    pub files: Vec<TorrentFile>,
    pub total: u64,
}
