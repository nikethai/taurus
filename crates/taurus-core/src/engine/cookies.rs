//! Signing a video in with a browser's cookies after it failed for want of
//! it.

use crate::errors::{CoreError, ErrorKind};
use crate::media::validate_browser;
use crate::store::{JobId, JobKind, JobState};

use super::actor::{Actor, Command};
use super::{Engine, no_such_job};

impl Engine {
    /// Retries a stopped video (one that failed, typically with
    /// `NeedsSignIn` or `BlockedByBotCheck`, or is paused) with the cookies
    /// of `browser` (`chrome`, `firefox`, `safari`, `edge`, `brave`,
    /// `chromium`, `opera` or `vivaldi`, optionally `:<profile>`): yt-dlp
    /// reads them from the browser itself, so only its name is stored with
    /// the job's options, and the job is queued again. A browser that is
    /// not known, a job that is not a video, or one that is running,
    /// queued or finished is refused and left as it was.
    pub async fn retry_with_cookies(&self, id: JobId, browser: String) -> Result<(), CoreError> {
        self.check_running()?;
        let browser = browser.trim().to_owned();
        validate_browser(&browser)?;
        self.call(|reply| Command::SetMediaCookies { id, browser, reply })
            .await??;
        self.resume(id).await
    }
}

impl Actor {
    pub(super) fn set_media_cookies(&mut self, id: JobId, browser: &str) -> Result<(), CoreError> {
        let job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        let refuse = |detail: &str| {
            Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: detail.to_owned(),
            })
        };
        let Some(mut options) = job.media.filter(|_| job.kind == JobKind::Media) else {
            return refuse("cookies apply only to videos");
        };
        if self.running.contains_key(&id)
            || !matches!(job.state, JobState::Paused | JobState::Failed)
        {
            return refuse("the video is not stopped");
        }
        options.cookies_from_browser = Some(browser.to_owned());
        self.store.set_media(id, &options)
    }
}
