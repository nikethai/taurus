//! Pure torrent logic: input classification, path safety, output folders and
//! seeding policy. Nothing here touches the network or a librqbit session.

pub mod files;
pub mod policy;
pub mod source;

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
