//! The Settings screen: `Settings` to `SettingsVm` and back, and the theme.
//!
//! Every change is saved with `Engine::set_settings` and echoed to the
//! window at once (the controls show only what `settings` holds).

use std::path::PathBuf;

use slint::ComponentHandle;
use taurus_core::{Settings, SpeedLimit, ThemeChoice, WhenDone};

use crate::app::{App, UiMsg};
use crate::{SettingsVm, Theme, login};

/// Accent names as stored in `UiSettings.accent`.
const ACCENTS: [&str; 6] = ["Aldebaran", "Cobalt", "Jade", "Grape", "Rose", "Sky"];

/// What the Settings screen shows for `s`. Values it has no control for
/// (a custom speed, the Quit action) show as their own name, so no choice
/// is marked.
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
/// `SettingsVm` field names; names are matched ignoring case. False when
/// the key or value is not one the screen sends (`s` is left as it was).
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
        _ => return false,
    }
    true
}

impl App {
    /// Shows `self.settings` on the Settings screen and in the theme.
    pub(crate) fn push_settings(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let vm = settings_vm(&self.settings.borrow());
        let theme = ui.global::<Theme>();
        theme.set_scheme(vm.theme.clone());
        theme.set_accent(vm.accent.clone());
        ui.set_settings(vm);
    }

    pub(crate) fn setting_changed(&self, key: &str, value: &str) {
        let mut next = self.settings.borrow().clone();
        if !apply_change(&mut next, key, value) {
            eprintln!("hyades: ignored setting {key:?} = {value:?}");
            return self.push_settings();
        }
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
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(e) = engine.set_settings(next).await {
                eprintln!("hyades: saving settings failed: {e}");
                tx.send(UiMsg::SettingsFailed);
            }
        });
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
}
