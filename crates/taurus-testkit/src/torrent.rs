//! Local BitTorrent peers for tests: a seeder and a leecher, each its own
//! librqbit session on 127.0.0.1 with DHT, trackers, LSD and UPnP off, plus
//! hand-bencoded `.torrent` bytes for crafting odd torrents.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode, ListenerOptions,
    ManagedTorrent, Session, SessionOptions,
};
use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::time::Instant;

const PIECE_LEN: u32 = 16_384;
/// librqbit uploads 16 KiB blocks; its rate limiter's burst is one second of
/// the limit, so a lower limit fails every block and uploading stops.
const MIN_UPLOAD_BPS: u32 = 16_384;
const POLL: Duration = Duration::from_millis(50);

/// A private session seeding files written to a temp dir.
pub struct TorrentSeeder {
    pub addr: SocketAddr,
    pub magnet: String,
    pub torrent_bytes: Vec<u8>,
    /// 40 lowercase hex characters.
    pub info_hash: String,
    peer: Peer,
}

impl TorrentSeeder {
    /// Writes `files` (relative path, content) under a temp dir named `name`
    /// and seeds them from a private session.
    pub async fn start(name: &str, files: &[(&str, Vec<u8>)], upload_bps: Option<u32>) -> Self {
        assert!(
            upload_bps.is_none_or(|bps| bps >= MIN_UPLOAD_BPS),
            "librqbit cannot upload below {MIN_UPLOAD_BPS} bytes/s"
        );
        let dir = tempfile::tempdir().expect("create seeder temp dir");
        let root = dir.path().join(name);
        for (path, data) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).expect("create seed dir");
            std::fs::write(&path, data).expect("write seed file");
        }
        let session = new_session(&dir).await;
        session
            .ratelimits
            .set_upload_bps(upload_bps.and_then(NonZeroU32::new));
        let (created, handle) = session
            .create_and_serve_torrent(
                &root,
                CreateTorrentOptions {
                    name: Some(name),
                    piece_length: Some(PIECE_LEN),
                    trackers: vec![],
                },
            )
            .await
            .expect("create and serve torrent");
        let peer = Peer {
            session,
            handle,
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !peer.is_seeding() {
            assert!(Instant::now() < deadline, "seeder did not start seeding");
            tokio::time::sleep(POLL).await;
        }

        let info_hash = created.info_hash().as_string();
        TorrentSeeder {
            addr: peer.session.listen_addr().expect("seeder listens"),
            magnet: format!("magnet:?xt=urn:btih:{info_hash}&dn={name}"),
            torrent_bytes: created.as_bytes().expect("serialize torrent").to_vec(),
            info_hash,
            peer,
        }
    }

    /// Bytes uploaded since the seeder started.
    pub fn uploaded_bytes(&self) -> u64 {
        self.peer.handle.stats().uploaded_bytes
    }
}

/// A private session that downloads one torrent from one peer.
pub struct TorrentLeecher {
    peer: Peer,
}

impl TorrentLeecher {
    /// Downloads into a temp dir (multi-file torrents land in `<dir>/<name>/`).
    pub async fn start(torrent_bytes: &[u8], peer: SocketAddr) -> Self {
        let dir = tempfile::tempdir().expect("create leecher temp dir");
        let session = new_session(&dir).await;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes.to_vec()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![peer]),
                    ..Default::default()
                }),
            )
            .await
            .expect("add torrent")
            .into_handle()
            .expect("torrent handle");
        TorrentLeecher {
            peer: Peer {
                session,
                handle,
                _dir: dir,
            },
        }
    }

    /// True once every piece is downloaded and verified.
    pub async fn wait_complete(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.peer.handle.stats().finished {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// One session with one torrent and the temp dir it works in.
struct Peer {
    session: Arc<Session>,
    handle: Arc<ManagedTorrent>,
    _dir: TempDir,
}

impl Peer {
    fn is_seeding(&self) -> bool {
        self.handle.stats().finished && self.handle.live().is_some()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Background tasks hold the session alive; cancel them so the
        // listener closes and the temp dir can go.
        self.session.cancellation_token().cancel();
    }
}

async fn new_session(dir: &TempDir) -> Arc<Session> {
    Session::new_with_opts(dir.path().to_owned(), test_session_options(dir.path()))
        .await
        .expect("start librqbit session")
}

/// Bencodes a v1 torrent; paths are taken verbatim (so tests can craft unsafe ones).
pub fn make_torrent_bytes(name: &str, files: &[(&str, &[u8])], piece_len: u32) -> Vec<u8> {
    let all: Vec<u8> = files
        .iter()
        .flat_map(|(_, data)| data.iter().copied())
        .collect();
    let pieces: Vec<u8> = all
        .chunks(piece_len as usize)
        .flat_map(|piece| Sha1::digest(piece).to_vec())
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(b"d4:infod5:filesl");
    for (path, data) in files {
        out.extend_from_slice(format!("d6:lengthi{}e4:pathl", data.len()).as_bytes());
        for part in path.split('/') {
            bencode_str(&mut out, part.as_bytes());
        }
        out.extend_from_slice(b"ee");
    }
    out.extend_from_slice(b"e4:name");
    bencode_str(&mut out, name.as_bytes());
    out.extend_from_slice(format!("12:piece lengthi{piece_len}e6:pieces").as_bytes());
    bencode_str(&mut out, &pieces);
    out.extend_from_slice(b"ee");
    out
}

fn bencode_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(format!("{}:", s.len()).as_bytes());
    out.extend_from_slice(s);
}

/// Options for a session that only talks to peers it is given: no DHT,
/// trackers, LSD, UPnP or persistence, TCP on `127.0.0.1:0`.
pub fn test_session_options(_dir: &Path) -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: None,
        disable_trackers: true,
        disable_local_service_discovery: true,
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
            enable_upnp_port_forwarding: false,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSpec, content};

    fn spec(size: usize, seed: u64) -> Vec<u8> {
        content(&FileSpec {
            seed,
            ..FileSpec::new(size)
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seeder_serves_leecher() {
        let a = spec(300 * 1024, 1);
        let b = spec(100 * 1024, 2);
        let seeder = TorrentSeeder::start(
            "pack",
            &[("a.bin", a.clone()), ("dir/b.bin", b.clone())],
            None,
        )
        .await;
        assert_eq!(seeder.info_hash.len(), 40);
        assert_eq!(
            seeder.magnet,
            format!("magnet:?xt=urn:btih:{}&dn=pack", seeder.info_hash)
        );

        let leecher = TorrentLeecher::start(&seeder.torrent_bytes, seeder.addr).await;
        assert!(leecher.wait_complete(Duration::from_secs(20)).await);

        let out = leecher.peer._dir.path().join("pack");
        assert_eq!(std::fs::read(out.join("a.bin")).unwrap(), a);
        assert_eq!(std::fs::read(out.join("dir").join("b.bin")).unwrap(), b);
        assert!(seeder.uploaded_bytes() >= 400 * 1024);
    }

    #[test]
    fn make_torrent_bytes_roundtrips() {
        let a = vec![7u8; 40_000];
        let b = vec![9u8; 5];
        let bytes = make_torrent_bytes("set", &[("a.bin", &a), ("sub/b.bin", &b)], 16_384);
        let meta = librqbit::torrent_from_bytes(&bytes).unwrap();
        let info = meta.info.data.validate().unwrap();
        assert_eq!(info.name().as_deref(), Some("set"));
        let files: Vec<(Vec<String>, u64)> = info
            .iter_file_details()
            .map(|f| (f.filename.to_vec(), f.len))
            .collect();
        assert_eq!(
            files,
            [
                (vec!["a.bin".to_owned()], 40_000),
                (vec!["sub".to_owned(), "b.bin".to_owned()], 5)
            ]
        );
        // 40_005 bytes in 16 KiB pieces: three SHA-1 hashes.
        assert_eq!(info.info().pieces.as_ref().len(), 3 * 20);
        assert_eq!(info.info().piece_length, 16_384);
    }

    #[test]
    fn crafted_unsafe_path_is_kept_verbatim() {
        let bytes = make_torrent_bytes("x", &[("../evil.txt", b"boo")], 16_384);
        let needle = b"4:pathl2:..8:evil.txte";
        assert!(bytes.windows(needle.len()).any(|w| w == needle));
        // librqbit parses it but refuses it on validation.
        let meta = librqbit::torrent_from_bytes(&bytes).unwrap();
        assert!(meta.info.data.validate().is_err());
    }
}
