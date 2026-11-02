//! Making the session agree with the store, which stays the source of
//! truth: done on every session start.

use std::collections::HashMap;
use std::sync::Arc;

use librqbit::Session;
use librqbit::api::TorrentIdOrHash;

use super::{
    TorrentHandle, TorrentHub, delete_in, info_from_bytes, is_single_name, lib_error, may_run,
    parse_hash, pause_in, torrents,
};
use crate::errors::{CoreError, ErrorKind};
use crate::store::{JobKind, JobRecord};
use crate::torrent::files::stored_output_dir;

impl TorrentHub {
    /// Makes the session agree with the store. Every start does this; it is
    /// idempotent, so calling it again is harmless.
    /// - a session torrent with no job, or whose job is Completed or Failed,
    ///   is removed (its files stay);
    /// - one whose job may still run is paused, so only the queue starts it;
    /// - a job that may still run, has metadata and is missing from the
    ///   session is added back, paused.
    ///
    /// A torrent that cannot be fixed is logged and skipped; its job meets
    /// the error when it runs.
    pub async fn reconcile(&self) -> Result<(), CoreError> {
        let session = self.session().await?;
        self.reconcile_with(&session).await
    }

    pub(super) async fn reconcile_with(&self, session: &Arc<Session>) -> Result<(), CoreError> {
        let jobs = self.store.list_jobs()?;
        let live: HashMap<&str, &JobRecord> = jobs
            .iter()
            .filter(|j| j.kind == JobKind::Torrent && may_run(j.state))
            .filter_map(|j| Some((j.info_hash.as_deref()?, j)))
            .collect();

        for handle in torrents(session) {
            let hash = handle.info_hash().as_string();
            let owned = live.contains_key(hash.as_str());
            if let Err(e) = self.reconcile_present(session, &handle, &hash, owned).await {
                tracing::warn!(info_hash = %hash, "could not reconcile a torrent: {e}");
            }
        }
        for (hash, job) in live {
            let missing =
                parse_hash(hash).is_ok_and(|id| session.get(TorrentIdOrHash::Hash(id)).is_none());
            if job.torrent_meta.is_some()
                && missing
                && let Err(e) = self.restore(session, job).await
            {
                tracing::warn!(job = job.id.0, "could not restore a torrent: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_present(
        &self,
        session: &Arc<Session>,
        handle: &TorrentHandle,
        hash: &str,
        owned: bool,
    ) -> Result<(), CoreError> {
        if !owned {
            tracing::info!(info_hash = %hash, "dropping a torrent no running job owns");
            return delete_in(session, hash, false).await;
        }
        if self.test.as_ref().is_none_or(|t| t.restore_in_place) {
            return pause_in(session, hash).await;
        }
        // Torrents librqbit restores lose their initial peers; add them
        // again with the test peers.
        let meta = handle
            .with_metadata(|m| m.torrent_bytes.to_vec())
            .map_err(lib_error)?;
        let output = handle.output_folder().to_path_buf();
        let only_files = handle.only_files();
        delete_in(session, hash, false).await?;
        self.add_bytes_in(session, &meta, &output, only_files, true)
            .await?;
        Ok(())
    }

    async fn restore(&self, session: &Arc<Session>, job: &JobRecord) -> Result<(), CoreError> {
        // With overwrite on, a bad name could aim at a neighbour's files.
        if !is_single_name(&job.file_name) {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("stored name {:?} is not a plain name", job.file_name),
            });
        }
        let meta = job.torrent_meta.as_deref().unwrap_or_default();
        let info = info_from_bytes(meta)?;
        let output = stored_output_dir(&job.dest_dir, &job.file_name, &info);
        tracing::info!(job = job.id.0, output = %output.display(), "restoring a torrent");
        self.add_bytes_in(session, meta, &output, job.only_files.clone(), true)
            .await?;
        Ok(())
    }
}
