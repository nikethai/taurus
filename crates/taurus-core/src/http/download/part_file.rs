//! The partial file, written through blocking calls that report their
//! own result.

use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;

/// One worker's handle on the partial file. Every operation runs on the
/// blocking pool and returns its own result, so a chunk is counted as
/// written only if its write succeeded. (`tokio::fs::File` acknowledges a
/// write before it happens and reports a failure on a later call, after
/// which `flush` and `sync_data` succeed.)
pub(super) struct PartFile(Arc<std::fs::File>);

impl PartFile {
    pub(super) async fn open(path: &Path) -> io::Result<PartFile> {
        let path = path.to_path_buf();
        let file = blocking(move || std::fs::OpenOptions::new().write(true).open(path)).await?;
        Ok(PartFile(Arc::new(file)))
    }

    pub(super) async fn write_at(&self, offset: u64, data: Bytes) -> io::Result<()> {
        let file = self.0.clone();
        blocking(move || {
            let mut file = &*file;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&data)
        })
        .await
    }

    pub(super) async fn sync_data(&self) -> io::Result<()> {
        let file = self.0.clone();
        blocking(move || file.sync_data()).await
    }

    pub(super) async fn set_len(&self, len: u64) -> io::Result<()> {
        let file = self.0.clone();
        blocking(move || file.set_len(len)).await
    }
}

async fn blocking<T, F>(op: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(op)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_write_fails_that_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.tapa");
        std::fs::write(&path, b"").unwrap();
        // Every write through a read-only handle fails, and must fail at once
        // (tokio's `File` would report it only on a later call).
        let file = PartFile(Arc::new(std::fs::File::open(&path).unwrap()));
        assert!(file.write_at(0, Bytes::from_static(b"abc")).await.is_err());
        assert!(file.write_at(3, Bytes::from_static(b"def")).await.is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn writes_land_at_their_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.tapa");
        std::fs::write(&path, b"").unwrap();
        let file = PartFile::open(&path).await.unwrap();
        file.write_at(6, Bytes::from_static(b"world"))
            .await
            .unwrap();
        file.write_at(0, Bytes::from_static(b"hello "))
            .await
            .unwrap();
        file.sync_data().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        file.set_len(5).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }
}
