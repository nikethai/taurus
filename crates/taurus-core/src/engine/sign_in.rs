//! Protected downloads in the engine: what a job signs in with, and giving
//! a job new credentials. Where the passwords are kept: `keychain`.

use crate::errors::{CoreError, ErrorKind};
use crate::http::Auth;
use crate::secrets::Credentials;
use crate::store::{JobId, JobKind, JobRecord, JobState, Store};

use super::actor::{Actor, Command};
use super::jobs::fixed_url;
use super::keychain::Secrets;
use super::{AddRequest, Engine, no_such_job};

impl Secrets {
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
    /// (replacing any it had; the password goes to `EngineConfig::secrets`,
    /// and is kept before this returns), then resumes it if it failed with
    /// `NeedsSignIn`. If that resume fails to sign in (e.g. a wrong
    /// password), its error is returned and recorded as the job's error;
    /// the job stays Failed. Any other job, a running one included, uses
    /// them from its next run (or resume). Torrents and media jobs are
    /// refused.
    pub async fn set_credentials(&self, id: JobId, creds: Credentials) -> Result<(), CoreError> {
        self.check_running()?;
        let job = self
            .call(|reply| Command::SetCredentials { id, creds, reply })
            .await??;
        self.secrets.flush().await;
        let needs_sign_in = matches!(job.error, Some((ErrorKind::NeedsSignIn, _)));
        if job.state != JobState::Failed || !needs_sign_in {
            return Ok(());
        }
        match self.resume(id).await {
            Err(e) if e.kind == ErrorKind::NeedsSignIn => {
                let error = e.clone();
                self.call(|reply| Command::SignInFailed { id, error, reply })
                    .await?;
                Err(e)
            }
            resumed => resumed,
        }
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

    /// Records a failed sign-in as the error of the job, if it is still
    /// Failed (its state does not change).
    pub(super) fn sign_in_failed(&mut self, id: JobId, error: CoreError) {
        if self.state_of(id) == Some(JobState::Failed) {
            self.record_error(id, JobState::Failed, None, error);
        }
    }

    /// Deletes the password of a job being removed, if it has one.
    pub(super) fn forget_credentials(&self, job: Option<&JobRecord>) {
        if let Some(job) = job.filter(|job| job.auth_user.is_some()) {
            self.secrets.forget(job.id);
        }
    }
}
