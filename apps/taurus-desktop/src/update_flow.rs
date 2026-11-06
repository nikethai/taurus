//! In-app updates: when to check, what the answer means for the window,
//! the background download, and Restart.
//!
//! `UpdateFlow` holds the rules and is pure (no I/O but `apply`), so they
//! are unit-tested; the `impl App` below runs the checks and downloads on
//! the runtime and shows the state through the `UpdateUi` global.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use slint::{ComponentHandle, Timer, TimerMode};
use taurus_core::update::{self, Asset, Release, UpdateConfig, UpdateError};
use url::Url;

use crate::app::{App, UiMsg};
use crate::detail::warn;
use crate::update_apply::{self, ApplyError, InstallKind, Restart};
use crate::{AppWindow, UpdateUi};

/// Where releases are announced, unless `HYADES_UPDATE_MANIFEST` says
/// otherwise.
pub const DEFAULT_MANIFEST: &str =
    "https://github.com/nikethai/taurus/releases/latest/download/latest.json";
/// "Get it" when no release named its page.
const RELEASES_PAGE: &str = "https://github.com/nikethai/taurus/releases/latest";
/// `packaging/update-key.pub`: a comment line, then the key's base64 line.
const KEY_FILE: &str = include_str!("../../../packaging/update-key.pub");
/// The first automatic check, after launch.
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// Later automatic checks.
const DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// The release key compiled in (its base64 line).
pub fn public_key() -> &'static str {
    KEY_FILE.lines().nth(1).unwrap_or_default().trim()
}

/// `HYADES_UPDATE_MANIFEST` (`env`) when it is a URL, else the default.
/// Only the address changes: the manifest must still be signed by the
/// compiled-in key.
pub fn manifest_url(env: Option<&str>) -> Url {
    env.and_then(|s| Url::parse(s.trim()).ok())
        .unwrap_or_else(|| Url::parse(DEFAULT_MANIFEST).expect("default manifest URL"))
}

/// This build's update settings when installed as `kind`.
pub fn config_for(kind: &InstallKind, manifest_env: Option<&str>) -> UpdateConfig {
    UpdateConfig {
        manifest_url: manifest_url(manifest_env),
        public_key: public_key().to_owned(),
        platform: update_apply::platform_for(kind),
        current: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version"),
    }
}

/// What Hyades is doing about updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    /// A check the user asked for is running.
    Checking,
    /// ... and found nothing newer.
    UpToDate,
    /// A newer version to get from the website.
    Available {
        version: String,
        page: Url,
    },
    Downloading {
        version: String,
        percent: i32,
    },
    /// Downloaded and verified: Restart installs it.
    Ready {
        version: String,
        file: PathBuf,
    },
    /// A check the user asked for failed.
    CheckFailed,
    /// The downloaded update could not be put in place.
    InstallFailed {
        page: Url,
    },
}

pub struct UpdateFlow {
    cfg: UpdateConfig,
    kind: InstallKind,
    updates_dir: PathBuf,
    state: UpdateState,
    /// The release last found, for its asset and page.
    release: Option<Release>,
    /// A check or a download is running.
    busy: bool,
    /// A quiet failure was logged (once per run).
    warned: bool,
    /// What Restart does instead of `apply` (tests).
    prepared: Option<Restart>,
    /// The version that could not be installed: for the rest of the run
    /// nothing is downloaded again and the website is offered instead.
    failed_version: Option<String>,
}

impl UpdateFlow {
    pub fn new(cfg: UpdateConfig, kind: InstallKind, updates_dir: PathBuf) -> Self {
        Self {
            cfg,
            kind,
            updates_dir,
            state: UpdateState::Idle,
            release: None,
            busy: false,
            warned: false,
            prepared: None,
            failed_version: None,
        }
    }

    pub fn config(&self) -> &UpdateConfig {
        &self.cfg
    }

    pub fn state(&self) -> &UpdateState {
        &self.state
    }

    pub fn updates_dir(&self) -> &Path {
        &self.updates_dir
    }

    /// Whether a check may start now: none is running, nothing is
    /// downloading and no update is ready (Restart installs that one; a
    /// check would hide it). "Check now" shows `Checking`; an automatic
    /// check shows nothing.
    pub fn begin_check(&mut self, manual: bool) -> bool {
        if self.busy || matches!(self.state, UpdateState::Ready { .. }) {
            return false;
        }
        self.busy = true;
        if manual {
            self.state = UpdateState::Checking;
        }
        true
    }

    /// What a check's answer means; `manual` for "Check now".
    pub fn on_check_result(
        &mut self,
        manual: bool,
        r: Result<Option<Release>, UpdateError>,
    ) -> UpdateState {
        self.busy = false;
        // A ready update stays ready whatever a check says.
        if matches!(self.state, UpdateState::Ready { .. }) {
            return self.state.clone();
        }
        // An automatic check that finds nothing keeps an offer shown.
        let offered = matches!(
            self.state,
            UpdateState::Available { .. } | UpdateState::InstallFailed { .. }
        );
        let next = match r {
            Err(e) if manual => {
                warn(format_args!("update check failed: {e}"));
                UpdateState::CheckFailed
            }
            Err(e) => {
                self.warn_once(&e);
                if offered {
                    return self.state.clone();
                }
                UpdateState::Idle
            }
            Ok(None) if manual => UpdateState::UpToDate,
            Ok(None) if offered => return self.state.clone(),
            Ok(None) => UpdateState::Idle,
            Ok(Some(release)) => {
                let version = release.version.to_string();
                let page = release.page.clone();
                let downloads = release.asset.is_some()
                    && self.kind != InstallKind::NotifyOnly
                    && self.failed_version.is_none();
                let failed_again = self.failed_version.as_deref() == Some(version.as_str());
                self.release = Some(release);
                if failed_again {
                    UpdateState::InstallFailed { page }
                } else if downloads {
                    self.busy = true;
                    UpdateState::Downloading {
                        version,
                        percent: 0,
                    }
                } else {
                    UpdateState::Available { version, page }
                }
            }
        };
        self.state = next.clone();
        next
    }

    /// The asset to download while the state is `Downloading`.
    pub fn download_job(&self) -> Option<Asset> {
        match self.state {
            UpdateState::Downloading { .. } => self.release.as_ref()?.asset.clone(),
            _ => None,
        }
    }

    pub fn on_progress(&mut self, percent: i32) {
        if let UpdateState::Downloading { percent: p, .. } = &mut self.state {
            *p = percent;
        }
    }

    /// What the download's end means (`manual`: the check that found it).
    pub fn on_download_result(
        &mut self,
        manual: bool,
        r: Result<PathBuf, UpdateError>,
    ) -> UpdateState {
        self.busy = false;
        let UpdateState::Downloading { version, .. } = &self.state else {
            return self.state.clone();
        };
        self.state = match r {
            Ok(file) => UpdateState::Ready {
                version: version.clone(),
                file,
            },
            Err(e) if manual => {
                warn(format_args!("update download failed: {e}"));
                UpdateState::CheckFailed
            }
            Err(e) => {
                self.warn_once(&e);
                UpdateState::Idle
            }
        };
        self.state.clone()
    }

    /// Restart: puts the ready update in place and answers what to start
    /// after quitting; `InstallFailed` when it cannot. `None` when no
    /// update is ready.
    pub fn restart(&mut self) -> Option<Result<Restart, ApplyError>> {
        let UpdateState::Ready { version, file } = &self.state else {
            return None;
        };
        let result = match self.prepared.take() {
            Some(restart) => Ok(restart),
            None => update_apply::apply(&self.kind, file),
        };
        if result.is_err() {
            self.failed_version = Some(version.clone());
            self.state = UpdateState::InstallFailed { page: self.page() };
        }
        Some(result)
    }

    /// The new version's web page.
    pub fn page(&self) -> Url {
        match &self.release {
            Some(release) => release.page.clone(),
            None => Url::parse(RELEASES_PAGE).expect("releases page URL"),
        }
    }

    fn warn_once(&mut self, e: &UpdateError) {
        if !std::mem::replace(&mut self.warned, true) {
            warn(format_args!("update check failed: {e}"));
        }
    }
}

/// Deletes every file in `dir` but the newest (left-over downloads, and
/// `.part` files of an interrupted one, which never count as newest).
pub fn clean_updates_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(Option<std::time::SystemTime>, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| (e.metadata().and_then(|m| m.modified()).ok(), e.path()))
        .collect();
    let part = |p: &Path| p.extension().is_some_and(|e| e == "part");
    let newest = files
        .iter()
        .filter(|(_, p)| !part(p))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, p)| p.clone());
    files.retain(|(_, p)| Some(p) != newest.as_ref());
    for (_, path) in files {
        if let Err(e) = std::fs::remove_file(&path) {
            warn(format_args!("cannot remove {}: {e}", path.display()));
        }
    }
}

/// The window's name for `state` (`UpdateUi.state`), the new version and
/// the download's percent.
fn ui_state(state: &UpdateState) -> (&'static str, &str, i32) {
    match state {
        UpdateState::Idle => ("", "", 0),
        UpdateState::Checking => ("checking", "", 0),
        UpdateState::UpToDate => ("up-to-date", "", 0),
        UpdateState::Available { version, .. } => ("available", version, 0),
        UpdateState::Downloading { version, percent } => ("downloading", version, *percent),
        UpdateState::Ready { version, .. } => ("ready", version, 0),
        UpdateState::CheckFailed => ("check-failed", "", 0),
        UpdateState::InstallFailed { .. } => ("install-failed", "", 0),
    }
}

/// The app's side of updates (in `App::updates`).
#[derive(Default)]
pub(crate) struct Updates {
    /// Set by `use_updates` (never in tests, but by the test seam).
    flow: RefCell<Option<UpdateFlow>>,
    /// "Later" or "Not now": no banner until the next launch.
    dismissed: Cell<bool>,
    first: Timer,
    daily: Timer,
}

/// What the runtime reports back about updates.
pub(crate) enum UpdateMsg {
    Checked {
        manual: bool,
        result: Box<Result<Option<Release>, UpdateError>>,
    },
    Progress(i32),
    Downloaded {
        manual: bool,
        result: Result<PathBuf, UpdateError>,
    },
}

impl App {
    /// Checks for updates with `flow`: 30 s after launch, then every
    /// 24 h, while the setting is on. Old downloads are cleared first.
    pub fn use_updates(self: &Rc<Self>, flow: UpdateFlow) {
        clean_updates_dir(flow.updates_dir());
        *self.updates.flow.borrow_mut() = Some(flow);
        let weak = Rc::downgrade(self);
        self.updates
            .first
            .start(TimerMode::SingleShot, FIRST_CHECK, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_for_update(false);
                }
            });
        let weak = Rc::downgrade(self);
        self.updates
            .daily
            .start(TimerMode::Repeated, DAILY, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_for_update(false);
                }
            });
        self.push_update();
    }

    /// Shows `version` as downloaded and ready, with `restart` as what
    /// Restart does (no download, no `apply`): for tests. With `None`,
    /// Restart runs `apply` for a copy that cannot update itself, which
    /// fails.
    #[doc(hidden)]
    pub fn show_update_ready(&self, version: &str, restart: Option<Restart>) {
        {
            let mut flow = self.updates.flow.borrow_mut();
            let flow = flow.get_or_insert_with(|| {
                let kind = InstallKind::NotifyOnly;
                UpdateFlow::new(config_for(&kind, None), kind, PathBuf::new())
            });
            flow.state = UpdateState::Ready {
                version: version.to_owned(),
                file: PathBuf::new(),
            };
            flow.prepared = restart;
        }
        self.updates.dismissed.set(false);
        self.push_update();
    }

    pub(crate) fn connect_updates(self: &Rc<Self>, ui: &AppWindow) {
        let global = ui.global::<UpdateUi>();
        global.set_current(env!("CARGO_PKG_VERSION").into());
        let call = |f: fn(&App)| {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(app) = weak.upgrade() {
                    f(&app);
                }
            }
        };
        global.on_check_now(call(|a| a.check_for_update(true)));
        global.on_restart(call(App::update_restart));
        global.on_get_it(call(App::open_update_page));
        global.on_dismiss(call(|a| {
            a.updates.dismissed.set(true);
            a.push_update();
        }));
    }

    /// Starts a check on the runtime (`manual`: "Check now", which always
    /// runs; an automatic one only while the setting is on).
    fn check_for_update(&self, manual: bool) {
        if !manual && !self.settings.borrow().ui.check_updates {
            return;
        }
        let cfg = {
            let mut flow = self.updates.flow.borrow_mut();
            let Some(flow) = flow.as_mut() else {
                return;
            };
            if !flow.begin_check(manual) {
                return;
            }
            flow.config().clone()
        };
        if manual {
            self.updates.dismissed.set(false);
        }
        let proxy = self.settings.borrow().network.proxy.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let client = taurus_core::network::build_client(&proxy);
            let result = update::check(&client, &cfg).await;
            tx.send(UiMsg::Update(UpdateMsg::Checked {
                manual,
                result: Box::new(result),
            }));
        });
        self.push_update();
    }

    pub(crate) fn update_msg(&self, msg: UpdateMsg) {
        let job = {
            let mut flow = self.updates.flow.borrow_mut();
            let Some(flow) = flow.as_mut() else {
                return;
            };
            match msg {
                UpdateMsg::Checked { manual, result } => {
                    flow.on_check_result(manual, *result);
                    flow.download_job()
                        .map(|asset| (manual, asset, flow.updates_dir().to_path_buf()))
                }
                UpdateMsg::Progress(percent) => {
                    flow.on_progress(percent);
                    None
                }
                UpdateMsg::Downloaded { manual, result } => {
                    flow.on_download_result(manual, result);
                    None
                }
            }
        };
        if let Some((manual, asset, dir)) = job {
            self.download_update(manual, asset, dir);
        }
        self.push_update();
    }

    /// Downloads `asset` into `dir` on the runtime, through the proxy
    /// set in the settings.
    fn download_update(&self, manual: bool, asset: Asset, dir: PathBuf) {
        let proxy = self.settings.borrow().network.proxy.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let client = taurus_core::network::build_client(&proxy);
            let shown = AtomicI32::new(0);
            let progress = |done: u64, total: u64| {
                let percent = (done.saturating_mul(100) / total.max(1)).min(100) as i32;
                if shown.swap(percent, Ordering::Relaxed) != percent {
                    tx.send(UiMsg::Update(UpdateMsg::Progress(percent)));
                }
            };
            let result = update::download(&client, &asset, &dir, progress).await;
            tx.send(UiMsg::Update(UpdateMsg::Downloaded { manual, result }));
        });
    }

    /// Restart (banner or settings): puts the update in place, then quits
    /// so `main` can start it; the website is offered when it cannot.
    fn update_restart(&self) {
        let result = self
            .updates
            .flow
            .borrow_mut()
            .as_mut()
            .and_then(UpdateFlow::restart);
        match result {
            Some(Ok(restart)) => self.restart_to_update(restart),
            Some(Err(e)) => {
                warn(format_args!("cannot install the update: {e:?}"));
                self.updates.dismissed.set(false);
                self.push_update();
            }
            None => {}
        }
    }

    /// "Get it": the new version's page in the browser.
    fn open_update_page(&self) {
        let Some(page) = self.updates.flow.borrow().as_ref().map(UpdateFlow::page) else {
            return;
        };
        if let Err(e) = opener::open(page.as_str()) {
            warn(format_args!("cannot open {page}: {e}"));
        }
    }

    /// Shows the update state in `UpdateUi`.
    fn push_update(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let flow = self.updates.flow.borrow();
        let state = flow.as_ref().map_or(&UpdateState::Idle, UpdateFlow::state);
        let (name, version, percent) = ui_state(state);
        let global = ui.global::<UpdateUi>();
        global.set_state(name.into());
        global.set_version(version.into());
        global.set_percent(percent);
        let offers = matches!(name, "available" | "ready" | "install-failed");
        global.set_banner(offers && !self.updates.dismissed.get());
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use taurus_core::update;
    use taurus_testkit::FakeRelease;

    use super::*;

    fn config(manifest_url: Url, public_key: String, kind: &InstallKind) -> UpdateConfig {
        UpdateConfig {
            manifest_url,
            public_key,
            platform: crate::update_apply::platform_for(kind),
            current: semver::Version::new(1, 0, 0),
        }
    }

    fn flow(kind: InstallKind) -> UpdateFlow {
        let url = Url::parse("https://example.org/latest.json").unwrap();
        UpdateFlow::new(
            config(url, "key".into(), &kind),
            kind,
            PathBuf::from("/nowhere"),
        )
    }

    fn release(asset: bool) -> Release {
        Release {
            version: semver::Version::new(1, 1, 0),
            notes: String::new(),
            page: Url::parse("https://example.org/v1.1.0").unwrap(),
            asset: asset.then(|| update::Asset {
                url: Url::parse("https://example.org/a.bin").unwrap(),
                size: 3,
                sha256: "0".repeat(64),
            }),
        }
    }

    fn appimage() -> InstallKind {
        InstallKind::AppImage {
            path: PathBuf::from("/apps/Hyades.AppImage"),
        }
    }

    #[test]
    fn automatic_check_failure_is_quiet() {
        let mut f = flow(appimage());
        let offline = Err(UpdateError::Offline("no route".into()));
        assert_eq!(f.on_check_result(false, offline), UpdateState::Idle);
        assert_eq!(
            f.on_check_result(false, Err(UpdateError::BadSignature)),
            UpdateState::Idle
        );
        assert_eq!(f.on_check_result(false, Ok(None)), UpdateState::Idle);
    }

    #[test]
    fn manual_check_failure_is_reported() {
        let mut f = flow(appimage());
        let offline = Err(UpdateError::Offline("no route".into()));
        assert_eq!(f.on_check_result(true, offline), UpdateState::CheckFailed);
        assert_eq!(f.state(), &UpdateState::CheckFailed);
    }

    #[test]
    fn manual_none_is_up_to_date() {
        let mut f = flow(appimage());
        assert_eq!(f.on_check_result(true, Ok(None)), UpdateState::UpToDate);
    }

    #[test]
    fn notify_only_kind_never_downloads() {
        let page = Url::parse("https://example.org/v1.1.0").unwrap();
        let available = UpdateState::Available {
            version: "1.1.0".into(),
            page,
        };
        let mut f = flow(InstallKind::NotifyOnly);
        assert_eq!(f.on_check_result(false, Ok(Some(release(true)))), available);
        assert_eq!(f.download_job(), None);
        // A release without this platform's asset is only reported too.
        let mut f = flow(appimage());
        assert_eq!(f.on_check_result(true, Ok(Some(release(false)))), available);
        assert_eq!(f.download_job(), None);
    }

    #[test]
    fn download_failure_after_a_check() {
        let mut f = flow(appimage());
        let downloading = UpdateState::Downloading {
            version: "1.1.0".into(),
            percent: 0,
        };
        assert_eq!(
            f.on_check_result(false, Ok(Some(release(true)))),
            downloading
        );
        assert_eq!(
            f.on_download_result(false, Err(UpdateError::Mismatch)),
            UpdateState::Idle
        );
        f.on_check_result(true, Ok(Some(release(true))));
        assert_eq!(
            f.on_download_result(true, Err(UpdateError::Mismatch)),
            UpdateState::CheckFailed
        );
    }

    /// A flow of `kind` with 1.1.0 found, downloaded and ready.
    fn ready_flow(kind: InstallKind) -> UpdateFlow {
        let mut f = flow(kind);
        assert!(f.begin_check(false));
        f.on_check_result(false, Ok(Some(release(true))));
        let file = PathBuf::from("/updates/a.bin");
        assert_eq!(
            f.on_download_result(false, Ok(file.clone())),
            UpdateState::Ready {
                version: "1.1.0".into(),
                file
            }
        );
        f
    }

    /// Review #1: "Check now" while an update is ready does not start a
    /// check, and an answer arriving anyway leaves it ready.
    #[test]
    fn check_now_keeps_a_ready_update() {
        let mut f = ready_flow(appimage());
        let ready = f.state().clone();
        assert!(!f.begin_check(true), "a check started while ready");
        assert!(!f.begin_check(false));
        assert_eq!(f.state(), &ready);
        let offline = Err(UpdateError::Offline("no route".into()));
        assert_eq!(f.on_check_result(true, offline), ready);
        assert_eq!(f.on_check_result(true, Ok(None)), ready);
        assert_eq!(f.on_check_result(true, Ok(Some(release(true)))), ready);
        assert_eq!(f.download_job(), None, "downloads it again");
    }

    /// Review Focus 3's last step: `apply` fails (the AppImage's folder
    /// does not exist, so it cannot be written) and Restart offers the
    /// release's page instead.
    #[test]
    fn failed_install_offers_the_website() {
        let mut f = ready_flow(InstallKind::AppImage {
            path: PathBuf::from("/no-such-folder-hyades/Hyades.AppImage"),
        });
        assert_eq!(f.restart(), Some(Err(ApplyError::NotWritable)));
        let page = Url::parse("https://example.org/v1.1.0").unwrap();
        assert_eq!(
            f.state(),
            &UpdateState::InstallFailed { page: page.clone() }
        );
        assert_eq!(f.page(), page);
        assert_eq!(f.restart(), None, "nothing ready any more");
    }

    /// Review #2: after a failed install, later automatic checks neither
    /// download the same version again nor hide the website offer; a
    /// newer version is offered from the website too.
    #[test]
    fn no_download_again_after_a_failed_install() {
        let mut f = ready_flow(InstallKind::AppImage {
            path: PathBuf::from("/no-such-folder-hyades/Hyades.AppImage"),
        });
        assert!(f.restart().is_some_and(|r| r.is_err()));
        let failed = f.state().clone();
        assert!(matches!(failed, UpdateState::InstallFailed { .. }));

        assert!(f.begin_check(false));
        assert_eq!(f.on_check_result(false, Ok(Some(release(true)))), failed);
        assert_eq!(f.download_job(), None, "the same version again");
        assert!(f.begin_check(false));
        let offline = Err(UpdateError::Offline("no route".into()));
        assert_eq!(f.on_check_result(false, offline), failed, "offer hidden");
        assert_eq!(f.on_check_result(false, Ok(None)), failed);

        let mut newer = release(true);
        newer.version = semver::Version::new(1, 2, 0);
        let available = f.on_check_result(false, Ok(Some(newer)));
        assert!(
            matches!(&available, UpdateState::Available { version, .. } if version == "1.2.0"),
            "{available:?}"
        );
        assert_eq!(f.download_job(), None);
        let offline = Err(UpdateError::Offline("no route".into()));
        assert_eq!(f.on_check_result(false, offline), available);
    }

    /// Against a signed fake release: the check finds 1.1.0 with this
    /// AppImage's asset, the download lands in the updates folder and
    /// the state is Ready with that file.
    #[tokio::test]
    async fn asset_release_downloads_then_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let platform = update::platform_key("linux", std::env::consts::ARCH, true).unwrap();
        let bytes = b"new AppImage".to_vec();
        let host = FakeRelease::start("1.1.0", &[(platform.as_str(), bytes.clone())]).await;
        let image = tmp.path().join("Hyades.AppImage");
        std::fs::write(&image, "old").unwrap();
        let kind = InstallKind::AppImage { path: image };
        let mut cfg = config(host.manifest_url.clone(), host.public_key.clone(), &kind);
        // The test runs on every OS; the AppImage key is the one asked.
        cfg.platform = Some(platform);
        let dir = tmp.path().join("updates");
        let mut f = UpdateFlow::new(cfg, kind, dir.clone());

        let client = taurus_core::network::build_client(&taurus_core::ProxySetting::None);
        let checked = update::check(&client, f.config()).await;
        assert_eq!(
            f.on_check_result(false, checked),
            UpdateState::Downloading {
                version: "1.1.0".into(),
                percent: 0
            }
        );
        let asset = f.download_job().expect("an asset to download");
        let file = update::download(&client, &asset, f.updates_dir(), |_, _| {}).await;
        let state = f.on_download_result(false, file);
        let UpdateState::Ready { version, file } = state else {
            panic!("not ready: {state:?}");
        };
        assert_eq!(version, "1.1.0");
        assert!(file.starts_with(&dir), "{file:?}");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn updates_dir_keeps_only_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        for (name, age) in [
            ("Hyades-1.0.1.AppImage", 300),
            ("Hyades-1.1.0.AppImage", 10),
            ("Hyades-1.0.2.AppImage", 100),
            ("Hyades-1.2.0.AppImage.part", 1),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, name).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - Duration::from_secs(age)).unwrap();
        }
        clean_updates_dir(dir);
        let left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["Hyades-1.1.0.AppImage"]);
        // A folder that is not there yet is no error.
        clean_updates_dir(&dir.join("missing"));
    }
}
