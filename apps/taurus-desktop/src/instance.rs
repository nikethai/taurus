//! Single-instance detection and link forwarding.
//!
//! The first launch listens on a local socket (the primary). A later launch
//! (or an OS "open with" on a magnet or `.torrent`) connects to it, writes its
//! links as one JSON line (`["url1","url2"]\n`) and exits; the primary turns
//! each such line into one batch on [`Listener::into_stream`]. An empty batch
//! means "show the window".
//!
//! Platforms: on Linux the namespaced name is an abstract socket (no file, the
//! kernel drops it with the process); on macOS it is a socket file in a temp
//! dir, so a crash leaves a stale file that [`claim`] detects (connect is
//! refused) and replaces; on Windows it is a named pipe that vanishes with the
//! process.

use std::{io, path::Path, time::Duration};

use futures_util::{Stream, StreamExt, stream};
use interprocess::local_socket::{
    GenericNamespaced, ListenerOptions, Name, ToNsName,
    tokio::{Listener as RawListener, Stream as RawStream},
    traits::tokio::{Listener as _, Stream as _},
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    time::timeout,
};

/// Longest line a client may send (a batch of links); longer is dropped.
const MAX_LINE: u64 = 1 << 20;
/// How long a connection may take to deliver its line.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a secondary waits for the primary to confirm (by closing).
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Batches buffered between the connection tasks and the stream consumer.
const QUEUE: usize = 64;

/// Outcome of [`claim`].
pub enum Instance {
    /// No other instance runs: this process owns the socket.
    Primary(Listener),
    /// Another instance runs and has received our links; exit.
    Secondary,
}

/// The primary's socket; turn it into a stream of forwarded link batches.
pub struct Listener {
    inner: RawListener,
}

impl Listener {
    /// Starts accepting connections; each valid line becomes one batch.
    ///
    /// Spawns onto the current tokio runtime. A malformed or silent client
    /// only affects its own connection. Dropping the stream stops listening.
    pub fn into_stream(self) -> impl Stream<Item = Vec<String>> + Unpin {
        let (tx, mut rx) = mpsc::channel::<Vec<String>>(QUEUE);
        tokio::spawn(accept_loop(self.inner, tx));
        stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
    }

    /// Simulates a crash: the name is left behind on drop (a stale file on
    /// Unix filesystems; a no-op elsewhere).
    #[cfg(test)]
    fn abandon(mut self) {
        self.inner.do_not_reclaim_name_on_drop();
    }
}

async fn accept_loop(listener: RawListener, tx: mpsc::Sender<Vec<String>>) {
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            () = tx.closed() => return,
        };
        match accepted {
            Ok(conn) => {
                tokio::spawn(handle_connection(conn, tx.clone()));
            }
            // Transient (e.g. out of descriptors): back off instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Reads one newline-terminated JSON array of strings and forwards it.
/// Anything else (silence, a cut-off line, junk) is dropped.
async fn handle_connection(conn: RawStream, tx: mpsc::Sender<Vec<String>>) {
    let mut reader = BufReader::new(conn).take(MAX_LINE);
    let mut line = String::new();
    let read = timeout(READ_TIMEOUT, reader.read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) || !line.ends_with('\n') {
        return;
    }
    if let Ok(links) = serde_json::from_str::<Vec<String>>(line.trim_end()) {
        let _ = tx.send(links).await;
    }
    // Dropping the connection closes it, which is the secondary's confirmation.
}

/// Hyades socket name for one data folder: two data folders, two instances.
pub fn socket_name(data_dir: &Path) -> String {
    let digest = Sha256::digest(data_dir.to_string_lossy().as_bytes());
    format!("dev.nikethai.taurus.{}", hex::encode(&digest[..4]))
}

/// Becomes the primary instance, or hands `links` to the running one.
///
/// A socket left behind by a crash does not count as a running instance.
pub async fn claim(name: &str, links: &[String]) -> io::Result<Instance> {
    claim_at(name.to_ns_name::<GenericNamespaced>()?, links).await
}

async fn claim_at(name: Name<'_>, links: &[String]) -> io::Result<Instance> {
    let mut retried = false;
    loop {
        match listen(name.clone(), false) {
            Ok(inner) => return Ok(Instance::Primary(Listener { inner })),
            // Unix reports a taken name as AddrInUse, a Windows pipe as access denied.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                ) =>
            {
                match send(name.clone(), links).await {
                    Ok(()) => return Ok(Instance::Secondary),
                    // Nobody answers: a crashed instance left the name behind.
                    Err(c)
                        if matches!(
                            c.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        return listen(name, true)
                            .map(|inner| Instance::Primary(Listener { inner }));
                    }
                    // The primary went away while we were sending: it is
                    // quitting, so claim again (once) instead of failing.
                    Err(c)
                        if !retried
                            && matches!(
                                c.kind(),
                                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                            ) =>
                    {
                        retried = true;
                    }
                    Err(c) => return Err(c),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn listen(name: Name<'_>, overwrite: bool) -> io::Result<RawListener> {
    ListenerOptions::new()
        .name(name)
        .try_overwrite(overwrite)
        .create_tokio()
}

/// Connects, writes `links` as one JSON line, then waits (bounded) for the
/// primary to close its end so the message is known to have been taken.
async fn send(name: Name<'_>, links: &[String]) -> io::Result<()> {
    let mut conn = RawStream::connect(name).await?;
    let mut line = serde_json::to_vec(links).map_err(io::Error::other)?;
    line.push(b'\n');
    conn.write_all(&line).await?;
    conn.flush().await?;
    // Best effort: a slow or hung primary must not hang the second launch.
    let _ = timeout(ACK_TIMEOUT, conn.read(&mut [0u8; 1])).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;

    fn unique(test: &str) -> String {
        format!("hyades-test-{test}-{}", std::process::id())
    }

    fn urls(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    async fn next(s: &mut Pin<Box<dyn Stream<Item = Vec<String>> + Send>>) -> Option<Vec<String>> {
        timeout(Duration::from_secs(5), s.next())
            .await
            .expect("batch within 5s")
    }

    async fn primary(name: &str) -> Listener {
        match claim(name, &[]).await.unwrap() {
            Instance::Primary(l) => l,
            Instance::Secondary => panic!("expected primary"),
        }
    }

    fn boxed(l: Listener) -> Pin<Box<dyn Stream<Item = Vec<String>> + Send>> {
        Box::pin(l.into_stream())
    }

    async fn raw(name: &str) -> RawStream {
        RawStream::connect(name.to_ns_name::<GenericNamespaced>().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn first_claim_is_primary() {
        let name = unique("first");
        assert!(matches!(
            claim(&name, &[]).await.unwrap(),
            Instance::Primary(_)
        ));
    }

    #[tokio::test]
    async fn second_instance_forwards_links() {
        let name = unique("forward");
        let mut stream = boxed(primary(&name).await);
        let links = urls(&["https://x.org/a.iso"]);
        let second = claim(&name, &links).await.unwrap();
        assert!(matches!(second, Instance::Secondary));
        assert_eq!(next(&mut stream).await.unwrap(), links);
        // Nothing else was queued.
        assert!(
            timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn empty_batch_means_show_window() {
        let name = unique("empty");
        let mut stream = boxed(primary(&name).await);
        assert!(matches!(
            claim(&name, &[]).await.unwrap(),
            Instance::Secondary
        ));
        assert_eq!(next(&mut stream).await.unwrap(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn stale_socket_becomes_primary() {
        let name = unique("stale");
        primary(&name).await.abandon();
        assert!(matches!(
            claim(&name, &[]).await.unwrap(),
            Instance::Primary(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_file_is_replaced() {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        let fs_name = || path.as_path().to_fs_name::<GenericFilePath>().unwrap();
        listen(fs_name(), false)
            .map(|inner| Listener { inner })
            .unwrap()
            .abandon();
        assert!(path.exists(), "crash left the socket file behind");
        let Instance::Primary(l) = claim_at(fs_name(), &[]).await.unwrap() else {
            panic!("a stale file must not block the primary");
        };
        let mut stream = boxed(l);
        let links = urls(&["magnet:?xt=urn:btih:abc"]);
        assert!(matches!(
            claim_at(fs_name(), &links).await.unwrap(),
            Instance::Secondary
        ));
        assert_eq!(next(&mut stream).await.unwrap(), links);
    }

    #[tokio::test]
    async fn malformed_line_is_skipped() {
        let name = unique("malformed");
        let mut stream = boxed(primary(&name).await);
        for junk in [
            &b"not json\n"[..],
            b"{\"a\":1}\n",
            b"[1,2]\n",
            b"\n",
            b"\xff\xfe\n",
        ] {
            let mut c = raw(&name).await;
            c.write_all(junk).await.unwrap();
            c.flush().await.unwrap();
        }
        let links = urls(&["https://x.org/ok"]);
        assert!(matches!(
            claim(&name, &links).await.unwrap(),
            Instance::Secondary
        ));
        assert_eq!(next(&mut stream).await.unwrap(), links);
    }

    #[tokio::test]
    async fn silent_and_cut_off_clients_do_not_block_others() {
        let name = unique("silent");
        let mut stream = boxed(primary(&name).await);
        let _silent = raw(&name).await; // connects, sends nothing, stays open
        let mut cut = raw(&name).await; // disconnects mid-line
        cut.write_all(b"[\"https://x.org/cut").await.unwrap();
        cut.flush().await.unwrap();
        drop(cut);
        let mut cut_valid = raw(&name).await; // complete JSON but no newline
        cut_valid
            .write_all(b"[\"https://x.org/nonl\"]")
            .await
            .unwrap();
        cut_valid.flush().await.unwrap();
        drop(cut_valid);
        let links = urls(&["https://x.org/ok"]);
        assert!(matches!(
            claim(&name, &links).await.unwrap(),
            Instance::Secondary
        ));
        assert_eq!(next(&mut stream).await.unwrap(), links);
    }

    #[tokio::test]
    async fn stream_keeps_yielding_from_many_secondaries() {
        let name = unique("many");
        let mut stream = boxed(primary(&name).await);
        for i in 0..20 {
            let links = urls(&[&format!("https://x.org/{i}"), &format!("magnet:?n={i}")]);
            assert!(matches!(
                claim(&name, &links).await.unwrap(),
                Instance::Secondary
            ));
            assert_eq!(next(&mut stream).await.unwrap(), links);
        }
    }

    #[test]
    fn socket_name_is_stable_and_per_data_dir() {
        let a = socket_name(Path::new("/data/a"));
        assert_eq!(a, socket_name(Path::new("/data/a")));
        assert_ne!(a, socket_name(Path::new("/data/b")));
        let suffix = a.strip_prefix("dev.nikethai.taurus.").unwrap();
        assert_eq!(suffix.len(), 8);
        assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
