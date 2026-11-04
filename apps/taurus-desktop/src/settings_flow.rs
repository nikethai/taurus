//! The Settings screen: `Settings` to `SettingsVm` and back, and the theme.
//!
//! Every change is echoed to the window at once (the controls show only
//! what `settings` holds) and saved with `Engine::set_settings` by one
//! writer task, in the order the changes were made.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use slint::ComponentHandle;
use taurus_core::{Engine, Settings, SpeedLimit, ThemeChoice, UiMode, WhenDone};
use tokio::sync::mpsc;

use crate::app::{App, AppSender, UiMsg};
use crate::{SettingsVm, Theme, format, login};

/// Accent names as stored in `UiSettings.accent`.
const ACCENTS: [&str; 6] = ["Aldebaran", "Cobalt", "Jade", "Grape", "Rose", "Sky"];

/// What the Settings screen shows for `s`. Values it has no control for
/// (a custom speed, the Quit action) show as their own name, so no choice
/// is marked; the screen shows them read-only (a custom speed with its
/// limit in `speed-custom`).
pub fn settings_vm(s: &Settings) -> SettingsVm {
    let speed = match s.speed {
        SpeedLimit::Gentle => "gentle",
        SpeedLimit::Balanced => "balanced",
        SpeedLimit::Fastest => "fastest",
        SpeedLimit::Custom { .. } => "custom",
    };
    let when_done = match s.when_done {
        WhenDone::Nothing => "nothing",
        WhenDone::Notify => "notify",
        WhenDone::Sleep => "sleep",
        WhenDone::Shutdown => "shutdown",
        WhenDone::Quit => "quit",
    };
    SettingsVm {
        folder: s
            .ui
            .folder
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
            .into(),
        sort_by_type: s.ui.sort_by_type,
        speed: speed.into(),
        when_done: when_done.into(),
        offer_copied_links: s.ui.offer_copied_links,
        open_at_login: s.ui.open_at_login,
        theme: theme_name(s.ui.theme).into(),
        accent: s.ui.accent.to_lowercase().into(),
        speed_custom: match s.speed {
            SpeedLimit::Custom { bytes_per_sec } => format::speed(bytes_per_sec).into(),
            _ => Default::default(),
        },
        advanced: s.ui.mode == UiMode::Advanced,
    }
}

fn theme_name(t: ThemeChoice) -> &'static str {
    match t {
        ThemeChoice::System => "system",
        ThemeChoice::Light => "light",
        ThemeChoice::Dark => "dark",
    }
}

/// Applies one `setting-changed(key, value)` to `s`. Keys are the
/// `SettingsVm` field names, then Advanced Settings' own
/// (`adv_settings::apply_adv_change`); names are matched ignoring case.
/// False when the key or value is not one the screens send (`s` is left as
/// it was).
pub fn apply_change(s: &mut Settings, key: &str, value: &str) -> bool {
    let value = value.trim();
    let flag = || match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    let lower = value.to_ascii_lowercase();
    match key {
        "folder" => s.ui.folder = (!value.is_empty()).then(|| PathBuf::from(value)),
        "sort-by-type" => match flag() {
            Some(b) => s.ui.sort_by_type = b,
            None => return false,
        },
        "offer-copied-links" => match flag() {
            Some(b) => s.ui.offer_copied_links = b,
            None => return false,
        },
        "open-at-login" => match flag() {
            Some(b) => s.ui.open_at_login = b,
            None => return false,
        },
        "advanced" => match flag() {
            Some(true) => s.ui.mode = UiMode::Advanced,
            Some(false) => s.ui.mode = UiMode::Simple,
            None => return false,
        },
        "speed" => {
            s.speed = match lower.as_str() {
                "gentle" => SpeedLimit::Gentle,
                "balanced" => SpeedLimit::Balanced,
                "fastest" => SpeedLimit::Fastest,
                _ => return false,
            }
        }
        "when-done" => {
            s.when_done = match lower.as_str() {
                "nothing" => WhenDone::Nothing,
                "notify" => WhenDone::Notify,
                "sleep" => WhenDone::Sleep,
                "shutdown" => WhenDone::Shutdown,
                "quit" => WhenDone::Quit,
                _ => return false,
            }
        }
        "theme" => {
            s.ui.theme = match lower.as_str() {
                "system" => ThemeChoice::System,
                "light" => ThemeChoice::Light,
                "dark" => ThemeChoice::Dark,
                _ => return false,
            }
        }
        "accent" => match ACCENTS.iter().find(|a| a.eq_ignore_ascii_case(value)) {
            Some(a) => s.ui.accent = a.to_string(),
            None => return false,
        },
        // Advanced Settings' own keys.
        _ => return crate::adv_settings::apply_adv_change(s, key, value),
    }
    true
}

/// How many of the latest saves `RecentSaves` remembers.
const RECENT_SAVES: usize = 8;

/// The latest settings handed to the writer, oldest first: an engine echo
/// equal to one of them but the newest is an older save's, late.
#[derive(Default)]
pub(crate) struct RecentSaves(VecDeque<Settings>);

impl RecentSaves {
    pub(crate) fn saved(&mut self, settings: Settings) {
        if self.0.len() == RECENT_SAVES {
            self.0.pop_front();
        }
        self.0.push_back(settings);
    }

    /// Whether the engine's `SettingsChanged` with `settings` is to be
    /// shown: not when it is the echo of a save older than the newest.
    /// The newest one's echo means every older echo has come (the engine
    /// sends them in order): those are forgotten. Anything else is a
    /// change made in the engine (the when-done reset, another program).
    /// Forgets every save but `settings`, the engine's state after a failed
    /// save: its echoes and later ones are shown again.
    pub(crate) fn reset_to(&mut self, settings: &Settings) {
        self.0.clear();
        self.0.push_back(settings.clone());
    }

    pub(crate) fn take_echo(&mut self, settings: &Settings) -> bool {
        match self.0.iter().rposition(|s| s == settings) {
            Some(i) if i + 1 == self.0.len() => {
                self.0.drain(..i);
                true
            }
            Some(_) => false,
            None => true,
        }
    }
}

/// Saves the UI's settings one at a time, in order; of a burst of changes
/// only the newest is saved. Each change has a generation number, so the
/// app can tell whether a save is still on its way. An echo older than
/// the newest save is never shown or saved again (`RecentSaves`).
pub(crate) struct SettingsWriter {
    tx: mpsc::UnboundedSender<(u64, Settings)>,
    /// Generation of the newest change handed to `save`.
    made: Cell<u64>,
    /// Generation of the newest save that finished (saved or failed).
    saved: Arc<AtomicU64>,
    recent: RefCell<RecentSaves>,
}

impl SettingsWriter {
    /// Starts the writer task on `rt`; it ends when the writer is dropped.
    pub fn spawn(engine: &Engine, rt: &tokio::runtime::Handle, app: AppSender) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, Settings)>();
        let saved = Arc::new(AtomicU64::new(0));
        let done = saved.clone();
        let engine = engine.clone();
        rt.spawn(async move {
            while let Some(mut change) = rx.recv().await {
                // Only the newest of the changes waiting matters.
                while let Ok(newer) = rx.try_recv() {
                    change = newer;
                }
                let (generation, settings) = change;
                let result = engine.set_settings(settings).await;
                done.store(generation, Ordering::SeqCst);
                if let Err(e) = result {
                    eprintln!("hyades: saving settings failed: {e}");
                    app.send(UiMsg::SettingsFailed);
                }
            }
        });
        Self {
            tx,
            made: Cell::new(0),
            saved,
            recent: RefCell::default(),
        }
    }

    pub fn save(&self, settings: Settings) {
        let generation = self.made.get() + 1;
        self.made.set(generation);
        self.recent.borrow_mut().saved(settings.clone());
        let _ = self.tx.send((generation, settings));
    }

    /// See `RecentSaves::take_echo`.
    fn take_echo(&self, settings: &Settings) -> bool {
        self.recent.borrow_mut().take_echo(settings)
    }

    /// A save failed: `settings` (read back from the engine) are what it
    /// holds now, so every remembered save is forgotten in their favour.
    fn resync(&self, settings: &Settings) {
        self.recent.borrow_mut().reset_to(settings);
    }

    /// Every change has been saved (or failed): the engine's settings are
    /// as new as the UI's. While not idle, the engine's `SettingsChanged`
    /// echoes are older than what the UI shows and are ignored; an echo
    /// that trails the last save is recognized by `take_echo`.
    pub fn idle(&self) -> bool {
        self.saved.load(Ordering::SeqCst) >= self.made.get()
    }
}

impl App {
    /// Shows `self.settings` on the Settings screen and in the theme.
    pub(crate) fn push_settings(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let vm = settings_vm(&self.settings.borrow());
        self.offer_links
            .store(vm.offer_copied_links, std::sync::atomic::Ordering::Relaxed);
        let theme = ui.global::<Theme>();
        theme.set_scheme(vm.theme.clone());
        theme.set_accent(vm.accent.clone());
        ui.set_settings(vm);
        self.push_whendone(&ui);
        self.push_adv_settings(&ui);
        // ui.mode may have changed: Simple, compact or wide.
        self.apply_layout();
    }

    pub(crate) fn setting_changed(&self, key: &str, value: &str) {
        let mut next = self.settings.borrow().clone();
        if !apply_change(&mut next, key, value) {
            eprintln!("hyades: ignored setting {key:?} = {value:?}");
            return self.push_settings();
        }
        self.change_settings(key, next);
    }

    /// A folder picked in the OS dialog for Settings: kept as the path it is
    /// (the screen only shows its display text, which may be lossy).
    pub(crate) fn set_folder(&self, folder: PathBuf) {
        let mut next = self.settings.borrow().clone();
        next.ui.folder = Some(folder);
        self.change_settings("folder", next);
    }

    /// The when-done action ran and the engine set it back to "nothing":
    /// the UI follows at once, so no later save of the UI's copy (or one
    /// still on its way) can bring Sleep or Shut down back.
    pub(crate) fn when_done_fired(&self) {
        let next = Settings {
            when_done: WhenDone::Nothing,
            ..self.settings.borrow().clone()
        };
        let in_flight = !self.writer.idle();
        *self.settings.borrow_mut() = next.clone();
        self.push_settings();
        if in_flight {
            self.writer.save(next);
        }
    }

    /// `next` (changed at `key`) becomes the UI's settings and is saved.
    pub(crate) fn change_settings(&self, key: &str, next: Settings) {
        if key == "open-at-login"
            && self.os_shell.get()
            && let Err(e) = login::set_open_at_login(next.ui.open_at_login)
        {
            eprintln!("hyades: open at login: {e}");
            return self.push_settings();
        }
        if !next.ui.offer_copied_links {
            self.hide_banner();
        }
        *self.settings.borrow_mut() = next.clone();
        self.push_settings();
        self.writer.save(next);
    }

    /// Settings from the engine (its `SettingsChanged`), unless a change
    /// made here is still being saved or they echo an older save.
    pub(crate) fn engine_settings(&self, settings: Settings) {
        if self.writer.idle() && self.writer.take_echo(&settings) {
            *self.settings.borrow_mut() = settings;
            self.push_settings();
        }
    }

    /// A save failed: show the engine's settings again (the failed change is
    /// not kept on screen as if saved), unless a newer change is on its way.
    /// They usually equal an older save, so the echo check is bypassed.
    pub(crate) fn settings_failed(&self, settings: Settings) {
        if self.writer.idle() {
            self.writer.resync(&settings);
            *self.settings.borrow_mut() = settings;
            self.push_settings();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_the_screen() {
        let mut s = Settings::default();
        let vm = settings_vm(&s);
        assert_eq!(
            (vm.theme.as_str(), vm.accent.as_str(), vm.speed.as_str()),
            ("system", "aldebaran", "fastest")
        );
        assert_eq!((vm.when_done.as_str(), vm.folder.as_str()), ("nothing", ""));

        for (key, value) in [
            ("theme", "dark"),
            ("accent", "jade"),
            ("speed", "gentle"),
            ("when-done", "sleep"),
            ("sort-by-type", "false"),
            ("offer-copied-links", "false"),
            ("open-at-login", "true"),
            ("folder", "/tmp/dl"),
        ] {
            assert!(apply_change(&mut s, key, value), "{key}");
        }
        assert_eq!(s.ui.theme, ThemeChoice::Dark);
        assert_eq!(s.ui.accent, "Jade");
        assert_eq!(s.speed, SpeedLimit::Gentle);
        assert_eq!(s.when_done, WhenDone::Sleep);
        assert_eq!(s.ui.folder, Some(PathBuf::from("/tmp/dl")));
        assert!(apply_change(&mut s, "when-done", "Quit"));
        assert_eq!(s.when_done, WhenDone::Quit);
        assert_eq!(settings_vm(&s).when_done.as_str(), "quit");
        assert!(apply_change(&mut s, "when-done", "sleep"));
        let vm = settings_vm(&s);
        assert_eq!(
            (vm.theme.as_str(), vm.accent.as_str(), vm.when_done.as_str()),
            ("dark", "jade", "sleep")
        );
        assert!(!vm.sort_by_type && !vm.offer_copied_links && vm.open_at_login);

        // Case does not matter; unknown keys and values change nothing.
        assert!(apply_change(&mut s, "accent", "ROSE"));
        assert_eq!(s.ui.accent, "Rose");
        let before = s.clone();
        assert!(!apply_change(&mut s, "accent", "mauve"));
        assert!(!apply_change(&mut s, "speed", "warp"));
        assert!(!apply_change(&mut s, "sort-by-type", "yes"));
        assert!(!apply_change(&mut s, "volume", "11"));
        assert_eq!(s, before);
        assert!(apply_change(&mut s, "folder", ""));
        assert_eq!(s.ui.folder, None);
    }

    /// Save A, save B, then A's echo arrives late: it is not shown, so the
    /// next change starts from B and keeps B's fields. B's own echo, and a
    /// change made in the engine, are shown.
    #[test]
    fn a_late_echo_of_an_older_save_is_ignored() {
        let mut recent = RecentSaves::default();
        let mut ui = Settings::default();
        let change = |ui: &mut Settings, recent: &mut RecentSaves, key, value| {
            assert!(apply_change(ui, key, value));
            recent.saved(ui.clone());
        };
        change(&mut ui, &mut recent, "theme", "dark");
        let a = ui.clone();
        change(&mut ui, &mut recent, "accent", "jade");
        let b = ui.clone();
        assert!(!recent.take_echo(&a), "A's echo shown over B");
        change(&mut ui, &mut recent, "speed", "gentle");
        assert_eq!(
            (ui.ui.theme, ui.ui.accent.as_str(), &ui.speed),
            (ThemeChoice::Dark, "Jade", &SpeedLimit::Gentle),
            "B's fields lost"
        );
        assert!(!recent.take_echo(&b), "B's echo shown over C");
        assert!(recent.take_echo(&ui), "C's echo");
        // Every older echo has come: one equal to A now is the engine's.
        assert!(recent.take_echo(&a), "the engine's own change");
        let reset = Settings {
            when_done: WhenDone::Nothing,
            ..ui.clone()
        };
        assert!(recent.take_echo(&reset));
        // Only the latest few are remembered.
        for i in 0..20 {
            let mut s = ui.clone();
            s.max_active = i + 1;
            recent.saved(s);
        }
        assert_eq!(recent.0.len(), RECENT_SAVES);
    }

    /// After a failed save the engine still holds an older save; reading it
    /// back must be shown (not ignored as an old echo), and so must the
    /// engine's later changes.
    #[test]
    fn a_failed_save_shows_the_engines_settings_again() {
        let mut recent = RecentSaves::default();
        let a = Settings::default();
        let mut b = a.clone();
        b.ui.theme = taurus_core::ThemeChoice::Dark;
        recent.saved(a.clone());
        recent.saved(b.clone());
        assert!(!recent.take_echo(&a), "an old echo stays hidden");
        recent.reset_to(&a);
        assert!(recent.take_echo(&a), "the engine's state after a failure");
        let mut c = a.clone();
        c.ui.theme = taurus_core::ThemeChoice::Light;
        assert!(recent.take_echo(&c), "a later change in the engine");
    }

    /// The mode switch changes `ui.mode` only; a custom speed shows as
    /// "custom" with its limit until a preset replaces it.
    #[test]
    fn advanced_switch_and_custom_speed() {
        let mut s = Settings {
            speed: SpeedLimit::Custom {
                bytes_per_sec: 3_000_000,
            },
            ..Settings::default()
        };
        let vm = settings_vm(&s);
        assert_eq!(
            (vm.speed.as_str(), vm.speed_custom.as_str(), vm.advanced),
            ("custom", "3.0 MB/s", false)
        );
        let before = s.clone();
        assert!(apply_change(&mut s, "advanced", "true"));
        assert_eq!(s.ui.mode, UiMode::Advanced);
        assert!(settings_vm(&s).advanced);
        assert!(!apply_change(&mut s, "advanced", "on"));
        assert!(apply_change(&mut s, "advanced", "false"));
        assert_eq!(s, before);
        assert!(apply_change(&mut s, "speed", "gentle"));
        assert_eq!(settings_vm(&s).speed_custom.as_str(), "");
    }
}
