//! Protected downloads in the engine: where a job's password is kept (the
//! secret store, else memory until the engine stops), reading it back for a
//! request, and giving a job new credentials.

use std::sync::Arc;

use crate::errors::{CoreError, ErrorKind};
use crate::http::Auth;
use crate::secrets::{Credentials, MemoryStore, SecretStore};
use crate::store::{JobId, JobKind, JobRecord, JobState, Store};

use super::actor::{Actor, Command};
use super::jobs::fixed_url;
use super::{AddRequest, Engine, no_such_job};

/// The engine's passwords: `EngineConfig::secrets`, with memory standing
/// in for it while it fails (no keychain). Cheap to clone.
#[derive(Clone)]
pub(super) struct Secrets {
    store: Arc<dyn SecretStore>,
    /// Passwords the store would not take, kept until the engine stops.
    session: Arc<MemoryStore>,
}

impl Secrets {
    pub(super) fn new(store: Arc<dyn SecretStore>) -> Secrets {
        Secrets {
            store,
            session: Arc::new(MemoryStore::new()),
        }
    }

    /// Keeps the job's password, replacing any earlier one. Blocks for the
    /// keychain.
    pub(super) fn keep(&self, id: JobId, password: &str) {
        let account = account(id);
        match self.store.set(&account, password) {
            Ok(()) => {
                let _ = self.session.delete(&account);
            }
            Err(e) => {
                tracing::warn!(
                    job = id.0,
                    "keeping the password in memory until the engine stops: {e}"
                );
                let _ = self.session.set(&account, password);
            }
        }
    }

    /// The job's password, if one is kept. Blocks for the keychain.
    fn password(&self, id: JobId) -> Option<String> {
        let account = account(id);
        if let Ok(Some(password)) = self.session.get(&account) {
            return Some(password);
        }
        self.store.get(&account).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not read the password: {e}");
            None
        })
    }

    /// Deletes the job's password. Blocks for the keychain.
    pub(super) fn forget(&self, id: JobId) {
        let account = account(id);
        let _ = self.session.delete(&account);
        if let Err(e) = self.store.delete(&account) {
            tracing::warn!(job = id.0, "could not delete the password: {e}");
        }
    }

    /// What the job signs in with (for its link's origin only): `None`
    /// without a user name, or without a password (one kept in memory by
    /// an earlier session is gone: the job then asks for it again). The
    /// keychain is read off the async threads.
    pub(super) async fn auth_for(&self, job: &JobRecord) -> Option<Auth> {
        let username = job.auth_user.clone()?;
        let (secrets, id) = (self.clone(), job.id);
        let password = tokio::task::spawn_blocking(move || secrets.password(id))
            .await
            .ok()
            .flatten();
        let Some(password) = password else {
            tracing::info!(job = id.0, "no password is kept for the job's user name");
            return None;
        };
        tracing::debug!(
            job = id.0,
            "signing in with the job's user name and password"
        );
        Auth::new(
            Credentials { username, password },
            &fixed_url(&job.source_url),
        )
    }

    /// `auth_for` the stored job, if it is still stored.
    pub(super) async fn auth_for_stored(&self, store: &Store, id: JobId) -> Option<Auth> {
        match store.get_job(id) {
            Ok(Some(job)) => self.auth_for(&job).await,
            _ => None,
        }
    }
}

/// The secret store's account of the job's password.
fn account(id: JobId) -> String {
    format!("job:{}", id.0)
}

/// Fails unless the request has no credentials: only web downloads take
/// them (not torrents, nor what yt-dlp downloads).
pub(super) fn web_only(req: &AddRequest) -> Result<(), CoreError> {
    match req.credentials {
        Some(_) => Err(not_for_this_job()),
        None => Ok(()),
    }
}

fn not_for_this_job() -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: "a user name and password apply only to web downloads".to_string(),
    }
}

impl Engine {
    /// Gives a web download the user name and password it signs in with
    /// (replacing any it had; the password goes to `EngineConfig::secrets`),
    /// then resumes it if it failed with `NeedsSignIn`; that resume's error
    /// (e.g. a wrong password) is returned. Any other job uses them the next
    /// time it runs. Torrents and media jobs are refused.
    pub async fn set_credentials(&self, id: JobId, creds: Credentials) -> Result<(), CoreError> {
        self.check_running()?;
        let job = self
            .call(|reply| Command::SetCredentials { id, creds, reply })
            .await??;
        let needs_sign_in = matches!(job.error, Some((ErrorKind::NeedsSignIn, _)));
        if job.state == JobState::Failed && needs_sign_in {
            self.resume(id).await?;
        }
        Ok(())
    }
}

impl Actor {
    /// Records the job's user name and keeps its password; returns the job.
    pub(super) fn set_credentials(
        &mut self,
        id: JobId,
        creds: Credentials,
    ) -> Result<JobRecord, CoreError> {
        let mut job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        if job.kind != JobKind::Http {
            return Err(not_for_this_job());
        }
        self.store_credentials(&mut job, Some(creds))?;
        Ok(job)
    }

    /// Records the user name of `creds` with the job and keeps its
    /// password; nothing without `creds`.
    pub(super) fn store_credentials(
        &mut self,
        job: &mut JobRecord,
        creds: Option<Credentials>,
    ) -> Result<(), CoreError> {
        let Some(Credentials { username, password }) = creds else {
            return Ok(());
        };
        self.store.set_auth_user(job.id, Some(&username))?;
        self.secrets.keep(job.id, &password);
        job.auth_user = Some(username);
        Ok(())
    }

    /// Deletes the password of a job being removed, if it has one.
    pub(super) fn forget_credentials(&self, job: Option<&JobRecord>) {
        if let Some(job) = job.filter(|job| job.auth_user.is_some()) {
            self.secrets.forget(job.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keychain that is not there.
    struct NoKeychain;

    impl SecretStore for NoKeychain {
        fn set(&self, _: &str, _: &str) -> Result<(), CoreError> {
            Err(not_for_this_job())
        }
        fn get(&self, _: &str) -> Result<Option<String>, CoreError> {
            Err(not_for_this_job())
        }
        fn delete(&self, _: &str) -> Result<(), CoreError> {
            Err(not_for_this_job())
        }
    }

    #[test]
    fn without_a_keychain_passwords_live_in_memory() {
        let secrets = Secrets::new(Arc::new(NoKeychain));
        assert_eq!(secrets.password(JobId(1)), None);
        secrets.keep(JobId(1), "pw");
        assert_eq!(secrets.password(JobId(1)).as_deref(), Some("pw"));
        assert_eq!(secrets.password(JobId(2)), None);
        secrets.forget(JobId(1));
        assert_eq!(secrets.password(JobId(1)), None);
    }

    #[test]
    fn a_working_store_holds_the_password() {
        let store = Arc::new(MemoryStore::new());
        let secrets = Secrets::new(store.clone());
        secrets.keep(JobId(7), "pw");
        assert_eq!(store.get("job:7").unwrap().as_deref(), Some("pw"));
        assert_eq!(secrets.session.get("job:7").unwrap(), None);
        secrets.keep(JobId(7), "new");
        assert_eq!(secrets.password(JobId(7)).as_deref(), Some("new"));
        secrets.forget(JobId(7));
        assert_eq!(store.get("job:7").unwrap(), None);
    }
}
