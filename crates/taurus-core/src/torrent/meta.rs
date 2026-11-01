//! Reading a torrent's metainfo: into a [`TorrentInfo`], its info hash,
//! and what librqbit adds for a source.

use librqbit::dht::Id20;
use librqbit::{AddTorrent, ValidatedTorrentMetaV1Info};

use super::hub::{lib_error, parse_hash};
use super::source::Source;
use super::{TorrentFile, TorrentInfo};
use crate::errors::{CoreError, ErrorKind, classify_io};

/// Parses and validates metainfo bytes.
pub fn info_from_bytes(bytes: &[u8]) -> Result<TorrentInfo, CoreError> {
    let meta = librqbit::torrent_from_bytes(bytes).map_err(lib_error)?;
    let info = meta.info.data.validate().map_err(lib_error)?;
    Ok(torrent_info(meta.info_hash, &info, None))
}

/// `fallback` names a torrent whose metadata has none (else the info hash).
pub(super) fn torrent_info<B: AsRef<[u8]>>(
    info_hash: Id20,
    info: &ValidatedTorrentMetaV1Info<B>,
    fallback: Option<String>,
) -> TorrentInfo {
    let info_hash = info_hash.as_string();
    let name = info
        .name()
        .map(|n| n.into_owned())
        .filter(|n| !n.is_empty())
        .or(fallback)
        .unwrap_or_else(|| info_hash.clone());
    let files = info
        .iter_file_details()
        .enumerate()
        .map(|(index, f)| TorrentFile {
            index,
            path: f.filename.to_vec().join("/"),
            size: f.len,
        })
        .collect();
    TorrentInfo {
        info_hash,
        name,
        files,
        total: info.lengths().total_length(),
    }
}

/// The info hash of metainfo bytes.
pub(super) fn hash_of(torrent_meta: &[u8]) -> Result<Id20, CoreError> {
    Ok(librqbit::torrent_from_bytes(torrent_meta)
        .map_err(lib_error)?
        .info_hash)
}

/// What librqbit adds for `source`, and the info hash of that torrent.
pub(super) async fn add_torrent(source: &Source) -> Result<(AddTorrent<'static>, Id20), CoreError> {
    Ok(match source {
        Source::Magnet { uri, info_hash, .. } => {
            (AddTorrent::from_url(uri.clone()), parse_hash(info_hash)?)
        }
        Source::TorrentUrl(url) => {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("fetch {url} first: the torrent session takes its bytes"),
            });
        }
        Source::TorrentFile(path) => {
            let bytes = tokio::fs::read(path).await.map_err(|e| CoreError {
                kind: classify_io(&e),
                detail: format!("cannot read {}: {e}", path.display()),
            })?;
            let info_hash = hash_of(&bytes)?;
            (AddTorrent::from_bytes(bytes), info_hash)
        }
        Source::Http(url) => {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("{url} is not a torrent"),
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_from_bytes_lists_files() {
        let bytes = taurus_testkit::make_torrent_bytes(
            "set",
            &[("a.bin", &[1; 40_000]), ("sub/b.bin", &[2; 5])],
            16_384,
        );
        let info = info_from_bytes(&bytes).unwrap();
        assert_eq!(info.name, "set");
        assert_eq!(info.total, 40_005);
        assert_eq!(info.info_hash.len(), 40);
        let files: Vec<_> = info
            .files
            .iter()
            .map(|f| (f.index, &*f.path, f.size))
            .collect();
        assert_eq!(files, [(0, "a.bin", 40_000), (1, "sub/b.bin", 5)]);

        let evil = taurus_testkit::make_torrent_bytes("x", &[("../e", b"boo")], 16_384);
        assert_eq!(
            info_from_bytes(&evil).unwrap_err().kind,
            ErrorKind::UnsafeTorrent
        );
    }
}
