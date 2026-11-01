//! Looking at a link without adding it: what a file or torrent is called and
//! how big it is (for an "add" dialog), and whether a job already has it.

use std::io::Read;
use std::time::Duration;

use crate::errors::{CoreError, ErrorKind};
use crate::http::resolve;
use crate::media::{self, MediaInfo};
use crate::tools::ToolKind;
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
    /// A video, audio or playlist link that yt-dlp reads.
    Media(MediaInfo),
}

/// How long yt-dlp may take to read a link before it is killed.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(90);

impl Engine {
    /// Reads what `input` names without adding a job or storing anything: a
    /// web link is resolved as `add` would (link fixups, a confirm page
    /// followed; any other web page or bot check fails as it does there),
    /// a magnet link, `.torrent` URL or local `.torrent` file is listed as
    /// `torrent_files` does (a magnet link's metadata comes from its peers,
    /// which starts the torrent session). A link to a well-known video
    /// site (`media::fast_path_host`) is read by yt-dlp straight away, and
    /// its errors are reported as they are; a web page (the web server
    /// answers with a page, not a file) is tried with yt-dlp too, and stays
    /// `SiteNotSupported` if that fails in any way. The first read installs
    /// yt-dlp and Deno if needed, announcing each step as
    /// `Event::ToolStatus`.
    pub async fn inspect(&self, input: &str) -> Result<LinkInfo, CoreError> {
        self.check_running()?;
        match classify(input) {
            Ok(Source::Http(_)) => {}
            Ok(_) => return self.torrent_files(input).await.map(LinkInfo::Torrent),
            // A bad magnet link; anything else is for `resolve` to report.
            Err(e) if e.kind == ErrorKind::SiteNotSupported => return Err(e),
            Err(_) => {}
        }
        let link = input.trim();
        if media::fast_path_host(link) {
            return self.inspect_media(link).await;
        }
        match resolve(&self.client, &fixed_url(link), None).await {
            Ok(probe) => Ok(LinkInfo::File {
                url: probe.requested_url.to_string(),
                name: probe.file_name,
                size: probe.size,
                content_type: probe.content_type,
            }),
            // A web page: yt-dlp may know it. If it does not, or cannot be
            // asked (tools offline, a timeout, any yt-dlp error), the page
            // stays as unsupported as it was: the person never asked for
            // video, so no video error is shown for it.
            Err(page) if page.kind == ErrorKind::SiteNotSupported => {
                self.inspect_media(link).await.map_err(|e| {
                    tracing::debug!("yt-dlp could not read {link}: {e}");
                    page
                })
            }
            Err(e) => Err(e),
        }
    }

    async fn inspect_media(&self, link: &str) -> Result<LinkInfo, CoreError> {
        // Installing the tools may take minutes: the machine must not sleep.
        let _hold = self.hold_when_done();
        let emit = self.tool_emitter();
        let (yt_dlp, deno) = tokio::join!(
            self.tools.ensure(ToolKind::YtDlp, &emit),
            self.tools.ensure(ToolKind::Deno, &emit),
        );
        yt_dlp?;
        deno?;
        media::inspect(&self.tools, link, MEDIA_TIMEOUT)
            .await
            .map(LinkInfo::Media)
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
