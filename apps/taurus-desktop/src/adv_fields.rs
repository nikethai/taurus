//! Advanced Settings' typed fields: the custom speed, the torrent limits,
//! peers, port and the manual proxy's URL. Each edit is only checked; a
//! field is applied on Enter, when it loses focus, and, for edits still
//! pending, when the wide layout's section changes or the screen is left
//! (the field is about to go away without losing focus). A field shows the
//! saved value unless it holds an edit not applied yet (pending), so what
//! it shows is either saved or still marked as typed (and flagged when it
//! is no value the field takes).

use std::cell::RefCell;
use std::collections::BTreeSet;

use slint::{ComponentHandle, SharedString};
use taurus_core::{ProxySetting, Settings, SpeedLimit};

use crate::SettingsUi;
use crate::adv_settings::{apply_adv_change, limit_text};
use crate::app::App;
use crate::whendone_flow::custom_speed_text;

/// The typed fields' keys (`field-edited` / `field-commit`).
const FIELDS: [&str; 6] = [
    "speed-custom",
    "upload-limit",
    "torrent-down-limit",
    "peers",
    "port",
    "proxy-url",
];

/// The typed fields edited since they were last applied (or whose text
/// was refused).
#[derive(Default)]
pub(crate) struct FieldsState {
    pending: RefCell<BTreeSet<&'static str>>,
}

impl FieldsState {
    fn add(&self, key: &str) {
        if let Some(key) = FIELDS.into_iter().find(|k| *k == key) {
            self.pending.borrow_mut().insert(key);
        }
    }

    fn remove(&self, key: &str) {
        self.pending.borrow_mut().remove(key);
    }

    fn has(&self, key: &str) -> bool {
        self.pending.borrow().contains(key)
    }
}

/// What the field `key` shows for `s`; `None` when nothing is saved for
/// it (the custom speed while a preset is on, the URL while the proxy is
/// not manual).
pub fn saved_text(s: &Settings, key: &str) -> Option<String> {
    let t = &s.torrent;
    let number = |n: Option<String>| Some(n.unwrap_or_default());
    match key {
        "speed-custom" => match s.speed {
            SpeedLimit::Custom { bytes_per_sec } => Some(custom_speed_text(bytes_per_sec)),
            _ => None,
        },
        "upload-limit" => Some(limit_text(t.upload_limit_bps)),
        "torrent-down-limit" => Some(limit_text(t.download_limit_bps)),
        "peers" => number(t.peer_limit.map(|n| n.to_string())),
        "port" => number(t.listen_port.map(|n| n.to_string())),
        "proxy-url" => match &s.network.proxy {
            ProxySetting::Manual { url } => Some(url.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn field_text(state: &SettingsUi<'_>, key: &str) -> SharedString {
    match key {
        "speed-custom" => state.get_custom_text(),
        "upload-limit" => state.get_upload_text(),
        "torrent-down-limit" => state.get_down_text(),
        "peers" => state.get_peers_text(),
        "port" => state.get_port_text(),
        _ => state.get_proxy_text(),
    }
}

fn set_field_text(state: &SettingsUi<'_>, key: &str, text: SharedString) {
    match key {
        "speed-custom" => state.set_custom_text(text),
        "upload-limit" => state.set_upload_text(text),
        "torrent-down-limit" => state.set_down_text(text),
        "peers" => state.set_peers_text(text),
        "port" => state.set_port_text(text),
        _ => state.set_proxy_text(text),
    }
}

/// Flags (or clears) the typed field `key`'s error; `comma` for the custom
/// speed's "use a dot" note. The proxy's error is its own text.
fn set_invalid(state: &SettingsUi<'_>, key: &str, invalid: bool, comma: bool) {
    match key {
        "speed-custom" => {
            state.set_custom_invalid(invalid);
            state.set_custom_comma(invalid && comma);
        }
        "upload-limit" => state.set_upload_invalid(invalid),
        "torrent-down-limit" => state.set_down_invalid(invalid),
        "peers" => state.set_peers_invalid(invalid),
        "port" => state.set_port_invalid(invalid),
        _ => state.set_proxy_error(Default::default()),
    }
}

impl App {
    /// The screen opened: no edit is pending; every field shows the saved
    /// value (empty when there is none), with no error.
    pub(crate) fn reset_fields(&self, state: &SettingsUi<'_>) {
        self.fields.pending.borrow_mut().clear();
        let s = self.settings.borrow().clone();
        for key in FIELDS {
            set_field_text(state, key, saved_text(&s, key).unwrap_or_default().into());
            set_invalid(state, key, false, false);
        }
    }

    /// The settings changed (here, or the engine's replaced them): every
    /// field without a pending edit shows the saved value.
    pub(crate) fn fill_fields(&self, state: &SettingsUi<'_>) {
        let s = self.settings.borrow().clone();
        for key in FIELDS {
            if self.fields.has(key) {
                continue;
            }
            if let Some(text) = saved_text(&s, key) {
                set_field_text(state, key, text.into());
            }
        }
    }

    /// `changed(key, value)` from Advanced Settings: a preset speed drops a
    /// custom speed still being typed, System or None a proxy URL.
    pub(crate) fn adv_changed(&self, key: &str, value: &str) {
        let dropped = match key {
            "speed" => Some("speed-custom"),
            "proxy" => Some("proxy-url"),
            _ => None,
        };
        if let Some(field) = dropped {
            self.fields.remove(field);
            if let Some(ui) = self.window.upgrade() {
                set_invalid(&ui.global::<SettingsUi>(), field, false, false);
            }
        }
        self.setting_changed(key, value);
    }

    /// The Custom speed was picked: the field shows the limit in effect
    /// now (empty for Fastest), as the popover does.
    pub(crate) fn adv_custom_chosen(&self) {
        if let Some(ui) = self.window.upgrade() {
            let state = ui.global::<SettingsUi>();
            let limit = self.engine.effective_speed_limit();
            self.fields.remove("speed-custom");
            state.set_custom_text(limit.map(custom_speed_text).unwrap_or_default().into());
            set_invalid(&state, "speed-custom", false, false);
        }
    }

    /// A typed field changed: it is pending and checked (an empty custom
    /// speed is not flagged; the URL is checked when applied); nothing is
    /// saved.
    pub(crate) fn adv_field_edited(&self, key: &str, text: &str) {
        self.fields.add(key);
        if key != "proxy-url" {
            self.check_field(key, text);
        }
    }

    fn check_field(&self, key: &str, text: &str) -> bool {
        let ok = apply_adv_change(&mut self.settings.borrow().clone(), key, text);
        if let Some(ui) = self.window.upgrade() {
            let blank = key == "speed-custom" && text.trim().is_empty();
            let state = ui.global::<SettingsUi>();
            set_invalid(&state, key, !ok && !blank, text.contains(','));
        }
        ok
    }

    /// A typed field was committed (Enter, or focus left it after an
    /// edit): saved when it is a value it takes, else flagged and nothing
    /// changes (the field stays pending).
    pub(crate) fn adv_field_commit(&self, key: &str, text: &str) {
        if key == "proxy-url" {
            return self.proxy_commit(text);
        }
        if !self.check_field(key, text) {
            return self.fields.add(key);
        }
        self.fields.remove(key);
        let mut next = self.settings.borrow().clone();
        if apply_adv_change(&mut next, key, text) && next != *self.settings.borrow() {
            self.change_settings(key, next);
        } else if let Some(ui) = self.window.upgrade() {
            // Nothing new: the field shows the saved value as written.
            self.fill_fields(&ui.global::<SettingsUi>());
        }
    }

    /// The section is about to change or the screen to close: every
    /// pending edit is committed as Enter would.
    pub(crate) fn adv_settings_leave(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let state = ui.global::<SettingsUi>();
        let pending: Vec<&str> = self.fields.pending.borrow().iter().copied().collect();
        for key in pending {
            let text = field_text(&state, key);
            self.adv_field_commit(key, &text);
        }
    }

    /// A manual proxy: kept only if the engine takes it (its check, the one
    /// `Engine::set_settings` runs); otherwise its reason shows under the
    /// field, the old setting stays and the URL stays pending. An empty
    /// URL changes nothing.
    fn proxy_commit(&self, text: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let state = ui.global::<SettingsUi>();
        let url = text.trim();
        if url.is_empty() {
            self.fields.remove("proxy-url");
            return state.set_proxy_error(Default::default());
        }
        let proxy = ProxySetting::Manual {
            url: url.to_owned(),
        };
        if let Err(e) = proxy.check() {
            self.fields.add("proxy-url");
            return state.set_proxy_error(e.detail.into());
        }
        state.set_proxy_error(Default::default());
        self.fields.remove("proxy-url");
        let mut next = self.settings.borrow().clone();
        if next.network.proxy != proxy {
            next.network.proxy = proxy;
            self.change_settings("proxy", next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_texts() {
        let mut s = Settings::default();
        assert_eq!(saved_text(&s, "speed-custom"), None);
        assert_eq!(saved_text(&s, "proxy-url"), None);
        for key in ["upload-limit", "torrent-down-limit", "peers", "port"] {
            assert_eq!(saved_text(&s, key).as_deref(), Some(""), "{key}");
        }
        s.speed = SpeedLimit::Custom {
            bytes_per_sec: 2_500_000,
        };
        s.torrent.upload_limit_bps = Some(10_000);
        s.torrent.listen_port = Some(6000);
        s.network.proxy = ProxySetting::Manual {
            url: "http://127.0.0.1:1".into(),
        };
        assert_eq!(saved_text(&s, "speed-custom").as_deref(), Some("2.5"));
        assert_eq!(saved_text(&s, "upload-limit").as_deref(), Some("0.01"));
        assert_eq!(saved_text(&s, "port").as_deref(), Some("6000"));
        assert_eq!(
            saved_text(&s, "proxy-url").as_deref(),
            Some("http://127.0.0.1:1")
        );
    }
}
