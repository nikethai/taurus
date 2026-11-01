//! Looking at a link without adding it: what a file or torrent is called and
//! how big it is (for an "add" dialog), and whether a job already has it.

use std::io::Read;

use crate::errors::{CoreError, ErrorKind};
use crate::http::resolve;
use crate::torrent::MAX_TORRENT_BYTES;
use crate::torrent::TorrentInfo;
use crate::torrent::hub::info_from_bytes;
use crate::torrent::source::{Source, classify};

use super::jobs::fixed_url;
use super::{Engine, JobId, read};

/// What a link names, as `Engine::inspect` found it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum LinkInfo {
    File {
        /// The link that serves the file (after link fixups and any confirm
        /// form).
        url: String,
        /// The name the server suggests, sanitized.
        name: String,
        size: Option<u64>,
        content_type: Option<String>,
    },
    Torrent(TorrentInfo),
}

impl Engine {
    /// Reads what `input` names without adding a job or storing anything: a
    /// web link is resolved as `add` would (link fixups, a confirm page
    /// followed; any other web page or bot check fails as it does there),
    /// a magnet link, `.torrent` URL or local `.torrent` file is listed as
    /// `torrent_files` does (a magnet link's metadata comes from its peers,
    /// which starts the torrent session).
    pub async fn inspect(&self, input: &str) -> Result<LinkInfo, CoreError> {
        self.check_running()?;
        match classify(input) {
            Ok(Source::Http(_)) => {}
            Ok(_) => return self.torrent_files(input).await.map(LinkInfo::Torrent),
            // A bad magnet link; anything else is for `resolve` to report.
            Err(e) if e.kind == ErrorKind::SiteNotSupported => return Err(e),
            Err(_) => {}
        }
        let probe = resolve(&self.client, &fixed_url(input.trim()), None).await?;
        Ok(LinkInfo::File {
            url: probe.requested_url.to_string(),
            name: probe.file_name,
            size: probe.size,
            content_type: probe.content_type,
        })
    }

    /// The job that already has `input`, if any (the lowest id when several
    /// do): one whose link as added, or whose link requested for the file,
    /// is the same after link fixups; for a magnet link or a local
    /// `.torrent` file, one with the same torrent, whichever way it was
    /// added. A `.torrent` URL matches only by its link. Reads the jobs the
    /// engine holds; the network is not asked.
    pub fn find_by_url(&self, input: &str) -> Option<JobId> {
        let input = input.trim();
        // The file is read before taking the lock, so the actor never waits
        // on disk I/O.
        let hash = match classify(input) {
            Ok(Source::Magnet { info_hash, .. }) => Some(info_hash),
            Ok(Source::TorrentFile(path)) => torrent_file_hash(&path),
            _ => None,
        };
        let fixed = fixed_url(input);
        let views = read(&self.views);
        views
            .values()
            .filter(|v| {
                (hash.is_some() && v.info_hash == hash)
                    || fixed_url(&v.source_url) == fixed
                    || fixed_url(&v.url) == fixed
            })
            .map(|v| v.id)
            .min()
    }
}

/// The info hash of a local `.torrent` file; `None` if it cannot be read.
fn torrent_file_hash(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TORRENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_TORRENT_BYTES {
        return None;
    }
    info_from_bytes(&bytes).ok().map(|info| info.info_hash)
}
