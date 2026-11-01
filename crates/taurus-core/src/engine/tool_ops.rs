//! The engine's face of the tool manager: installs and updates run in the
//! caller's task and announce each status change as `Event::ToolStatus`.

use crate::errors::CoreError;
use crate::tools::{ToolKind, ToolStatus};

use super::{Engine, Event};

impl Engine {
    /// yt-dlp, ffmpeg and Deno, in `ToolKind::ALL` order.
    pub fn tools_status(&self) -> Vec<ToolStatus> {
        self.tools.status()
    }

    /// Installs every tool that is not installed yet (all at once); an
    /// installed one is left as it is. Each status change is sent as
    /// `Event::ToolStatus`. Every tool is tried; the first error is
    /// returned (see `ToolManager::ensure`).
    pub async fn install_tools(&self) -> Result<(), CoreError> {
        self.check_running()?;
        let _hold = self.hold_when_done();
        let emit = self.tool_emitter();
        let [yt_dlp, ffmpeg, deno] = ToolKind::ALL;
        let (a, b, c) = tokio::join!(
            self.tools.ensure(yt_dlp, &emit),
            self.tools.ensure(ffmpeg, &emit),
            self.tools.ensure(deno, &emit),
        );
        first_error([a.map(drop), b.map(drop), c.map(drop)])
    }

    /// Updates every installed tool whose publisher has a newer version.
    /// Unless `force`, yt-dlp is checked at most once per 24 h and ffmpeg
    /// and Deno once per 7 days (the times survive restarts). A failed
    /// update keeps the active version. Every tool is tried; the first
    /// error is returned (see `ToolManager::update`).
    pub async fn update_tools(&self, force: bool) -> Result<(), CoreError> {
        self.check_running()?;
        let _hold = self.hold_when_done();
        let emit = self.tool_emitter();
        let [yt_dlp, ffmpeg, deno] = ToolKind::ALL;
        let (a, b, c) = tokio::join!(
            self.tools.update(yt_dlp, force, &emit),
            self.tools.update(ffmpeg, force, &emit),
            self.tools.update(deno, force, &emit),
        );
        first_error([a.map(drop), b.map(drop), c.map(drop)])
    }

    /// Makes the tool's other kept version active (see
    /// `ToolManager::rollback`) and sends its new status.
    pub async fn rollback_tool(&self, kind: ToolKind) -> Result<(), CoreError> {
        self.check_running()?;
        self.tools.rollback(kind)?;
        if let Some(status) = self.tools_status().into_iter().find(|s| s.kind == kind) {
            let _ = self.events.send(Event::ToolStatus(status));
        }
        Ok(())
    }

    fn tool_emitter(&self) -> impl Fn(ToolStatus) + Send + Sync + use<> {
        let events = self.events.clone();
        move |status| {
            // No subscriber is fine.
            let _ = events.send(Event::ToolStatus(status));
        }
    }
}

fn first_error(results: [Result<(), CoreError>; 3]) -> Result<(), CoreError> {
    results.into_iter().collect()
}
