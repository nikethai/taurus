//! Advanced Settings: `Settings` to `AdvSettingsVm` and back, the typed
//! fields (applied on Enter or focus out, never per keystroke, as the
//! when-done popover's custom speed), the video tools and forgetting saved
//! passwords. Every change goes through the Settings screen's own path
//! (`settings_flow::apply_change`, which falls back to `apply_adv_change`),
//! so both screens write settings the same way.

use std::ops::RangeInclusive;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use taurus_core::tools::{ToolKind, ToolState, ToolStatus};
use taurus_core::{ErrorKind, ProxySetting, Settings, SpeedLimit};

use crate::app::{App, UiMsg};
use crate::settings_flow::settings_vm;
use crate::whendone_flow::{custom_speed_text, parse_custom_speed};
use crate::{AdvSettingsVm, AppWindow, Screen, SettingsUi, Theme, ToolRowVm};

const MAX_ACTIVE: RangeInclusive<u32> = 1..=10;
const CONNECTIONS: [u32; 4] = [1, 4, 8, 16];
const MAX_TORRENTS: RangeInclusive<u32> = 1..=10;
const PEERS: RangeInclusive<u32> = 1..=1000;
/// Ports below 1024 need special rights on most systems.
const PORTS: RangeInclusive<u16> = 1024..=65535;
/// "Stop sharing at ratio" choices; `None` never stops.
const RATIOS: [(&str, Option<f64>); 5] = [
    ("0", Some(0.0)),
    ("0.5", Some(0.5)),
    ("1", Some(1.0)),
    ("2", Some(2.0)),
    ("never", None),
];
/// "Stop sharing after" choices, in seconds.
const STOP_AFTER: [(&str, Option<u64>); 4] = [
    ("off", None),
    ("1h", Some(3600)),
    ("24h", Some(24 * 3600)),
    ("7d", Some(7 * 24 * 3600)),
];

/// What the Advanced screen shows of `s` beyond `SettingsVm`.
pub fn adv_settings_vm(s: &Settings) -> AdvSettingsVm {
    let t = &s.torrent;
    let int = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
    AdvSettingsVm {
        max_active: int(s.max_active),
        connections: int(s.connections),
        auto_checksum: s.auto_checksum,
        keep_awake: s.keep_awake,
        stop_ratio: RATIOS
            .iter()
            .find(|(_, r)| *r == t.stop_ratio)
            .map_or("other", |(name, _)| name)
            .into(),
        stop_after: STOP_AFTER
            .iter()
            .find(|(_, secs)| *secs == t.stop_after_secs)
            .map_or("other", |(name, _)| name)
            .into(),
        max_torrents: int(t.max_active),
        upnp: t.upnp,
        dht: t.dht,
        proxy: match s.network.proxy {
            ProxySetting::System => "system",
            ProxySetting::None => "none",
            ProxySetting::Manual { .. } => "manual",
        }
        .into(),
        progress_by_type: s.ui.progress_by_type,
    }
}

/// A limit typed in MB/s: empty is none, else as the custom speed.
pub fn parse_limit(text: &str) -> Option<Option<u64>> {
    if text.trim().is_empty() {
        return Some(None);
    }
    parse_custom_speed(text).map(Some)
}

/// A limit as its field shows it ("" for none).
pub fn limit_text(limit: Option<u64>) -> String {
    limit.map(custom_speed_text).unwrap_or_default()
}

/// A whole number in `range` typed in a field (digits only); empty is
/// `None` (the field's default).
fn parse_optional<T: std::str::FromStr + PartialOrd>(
    text: &str,
    range: RangeInclusive<T>,
) -> Option<Option<T>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    if !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|n| range.contains(n)).map(Some)
}

/// Applies one Advanced-only change (`changed` or a typed field's commit)
/// to `s`; keys as documented on `AdvSettingsVm` and `SettingsUi`. False
/// when the key or value is not one the screen sends (`s` is unchanged).
pub fn apply_adv_change(s: &mut Settings, key: &str, value: &str) -> bool {
    let value = value.trim();
    let flag = match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    let number = |range: RangeInclusive<u32>| value.parse().ok().filter(|n| range.contains(n));
    let t = &mut s.torrent;
    match key {
        "max-active" => match number(MAX_ACTIVE) {
            Some(n) => s.max_active = n,
            None => return false,
        },
        "connections" => match number(1..=16).filter(|n| CONNECTIONS.contains(n)) {
            Some(n) => s.connections = n,
            None => return false,
        },
        "max-torrents" => match number(MAX_TORRENTS) {
            Some(n) => t.max_active = n,
            None => return false,
        },
        "auto-checksum" | "keep-awake" | "upnp" | "dht" | "progress-by-type" => {
            let Some(on) = flag else {
                return false;
            };
            *match key {
                "auto-checksum" => &mut s.auto_checksum,
                "keep-awake" => &mut s.keep_awake,
                "upnp" => &mut t.upnp,
                "dht" => &mut t.dht,
                _ => &mut s.ui.progress_by_type,
            } = on;
        }
        "stop-ratio" => match RATIOS.iter().find(|(name, _)| *name == value) {
            Some((_, ratio)) => t.stop_ratio = *ratio,
            None => return false,
        },
        "stop-after" => match STOP_AFTER.iter().find(|(name, _)| *name == value) {
            Some((_, secs)) => t.stop_after_secs = *secs,
            None => return false,
        },
        "proxy" => {
            s.network.proxy = match value {
                "system" => ProxySetting::System,
                "none" => ProxySetting::None,
                _ => return false,
            }
        }
        "speed-custom" => match parse_custom_speed(value) {
            Some(bytes_per_sec) => s.speed = SpeedLimit::Custom { bytes_per_sec },
            None => return false,
        },
        "upload-limit" => match parse_limit(value) {
            Some(limit) => t.upload_limit_bps = limit,
            None => return false,
        },
        "torrent-down-limit" => match parse_limit(value) {
            Some(limit) => t.download_limit_bps = limit,
            None => return false,
        },
        "peers" => match parse_optional(value, PEERS) {
            Some(peers) => t.peer_limit = peers,
            None => return false,
        },
        "port" => match parse_optional(value, PORTS) {
            Some(port) => t.listen_port = port,
            None => return false,
        },
        _ => return false,
    }
    true
}

fn tool_key(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::YtDlp => "yt-dlp",
        ToolKind::Ffmpeg => "ffmpeg",
        ToolKind::Deno => "deno",
    }
}

/// A tool's row on the Engines section.
pub fn tool_row(status: &ToolStatus) -> ToolRowVm {
    let (state, progress) = match &status.state {
        ToolState::Missing => ("missing", -1.0),
        ToolState::Installing { downloaded, total } => (
            "installing",
            total
                .filter(|&t| t > 0)
                .map_or(-1.0, |t| (*downloaded).min(t) as f32 / t as f32),
        ),
        ToolState::Ready => ("ready", -1.0),
        ToolState::Failed(_) => ("failed", -1.0),
    };
    ToolRowVm {
        kind: tool_key(status.kind).into(),
        version: status.version.clone().unwrap_or_default().into(),
        state: state.into(),
        progress,
    }
}

/// What a tool action or forgetting the passwords answered.
pub(crate) enum SettingsMsg {
    ToolsDone {
        rollback: bool,
        result: Result<(), ErrorKind>,
    },
    Forgot(Result<u32, ErrorKind>),
}

enum ToolAction {
    Install,
    Check,
    Rollback(ToolKind),
}

impl App {
    /// Shows `self.settings` on Advanced Settings and in the progress bars.
    pub(crate) fn push_adv_settings(&self, ui: &AppWindow) {
        let s = self.settings.borrow();
        ui.global::<Theme>().set_by_type(s.ui.progress_by_type);
        let state = ui.global::<SettingsUi>();
        state.set_settings(settings_vm(&s));
        state.set_vm(adv_settings_vm(&s));
    }

    /// The screen opened: its fields show the saved values, with no error.
    pub(crate) fn adv_settings_opened(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let s = self.settings.borrow().clone();
        let state = ui.global::<SettingsUi>();
        state.set_custom_text(
            match s.speed {
                SpeedLimit::Custom { bytes_per_sec } => custom_speed_text(bytes_per_sec),
                _ => String::new(),
            }
            .into(),
        );
        let t = &s.torrent;
        state.set_upload_text(limit_text(t.upload_limit_bps).into());
        state.set_down_text(limit_text(t.download_limit_bps).into());
        let text = |n: Option<String>| n.unwrap_or_default().into();
        state.set_peers_text(text(t.peer_limit.map(|n| n.to_string())));
        state.set_port_text(text(t.listen_port.map(|n| n.to_string())));
        state.set_proxy_text(match &s.network.proxy {
            ProxySetting::Manual { url } => url.into(),
            _ => Default::default(),
        });
        for key in [
            "speed-custom",
            "upload-limit",
            "torrent-down-limit",
            "peers",
            "port",
        ] {
            set_invalid(&state, key, false, false);
        }
        state.set_proxy_error(Default::default());
        state.set_tools_error(Default::default());
        state.set_forget_state(Default::default());
        self.refresh_tools(&ui);
    }

    /// The Custom speed was picked: the field shows the limit in effect
    /// now (empty for Fastest), as the popover does.
    pub(crate) fn adv_custom_chosen(&self) {
        if let Some(ui) = self.window.upgrade() {
            let state = ui.global::<SettingsUi>();
            let limit = self.engine.effective_speed_limit();
            state.set_custom_text(limit.map(custom_speed_text).unwrap_or_default().into());
            set_invalid(&state, "speed-custom", false, false);
        }
    }

    /// A typed field changed: only checked (an empty custom speed is not
    /// flagged); nothing is saved.
    pub(crate) fn adv_field_edited(&self, key: &str, text: &str) {
        if key == "proxy-url" {
            return;
        }
        let ok = apply_adv_change(&mut self.settings.borrow().clone(), key, text);
        if let Some(ui) = self.window.upgrade() {
            let blank = key == "speed-custom" && text.trim().is_empty();
            set_invalid(
                &ui.global::<SettingsUi>(),
                key,
                !ok && !blank,
                text.contains(','),
            );
        }
    }

    /// A typed field was committed (Enter, or focus left it after an edit):
    /// saved when it is a value it takes, else flagged and nothing changes.
    pub(crate) fn adv_field_commit(&self, key: &str, text: &str) {
        if key == "proxy-url" {
            return self.proxy_commit(text);
        }
        self.adv_field_edited(key, text);
        let mut next = self.settings.borrow().clone();
        if apply_adv_change(&mut next, key, text) && next != *self.settings.borrow() {
            self.change_settings(key, next);
        }
    }

    /// A manual proxy: kept only if the engine takes it (its check, the one
    /// `Engine::set_settings` runs); otherwise its reason shows under the
    /// field and the old setting stays.
    fn proxy_commit(&self, text: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let state = ui.global::<SettingsUi>();
        let url = text.trim();
        if url.is_empty() {
            return state.set_proxy_error(Default::default());
        }
        let proxy = ProxySetting::Manual {
            url: url.to_owned(),
        };
        if let Err(e) = proxy.check() {
            return state.set_proxy_error(e.detail.into());
        }
        state.set_proxy_error(Default::default());
        let mut next = self.settings.borrow().clone();
        if next.network.proxy != proxy {
            next.network.proxy = proxy;
            self.change_settings("proxy", next);
        }
    }

    /// The tools' versions and states, while Settings is open.
    pub(crate) fn refresh_tools(&self, ui: &AppWindow) {
        if ui.get_screen() != Screen::Settings {
            return;
        }
        let statuses = self.engine.tools_status();
        let state = ui.global::<SettingsUi>();
        state.set_tools_missing(statuses.iter().any(|s| s.version.is_none()));
        let rows: Vec<ToolRowVm> = statuses.iter().map(tool_row).collect();
        let model = state.get_tools();
        if model.row_count() != rows.len() {
            return state.set_tools(ModelRc::new(VecModel::from(rows)));
        }
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    }

    pub(crate) fn install_tools(&self) {
        self.tool_action(ToolAction::Install);
    }

    pub(crate) fn check_tools(&self) {
        self.tool_action(ToolAction::Check);
    }

    pub(crate) fn rollback_tool(&self, kind: &str) {
        if let Some(kind) = ToolKind::ALL.into_iter().find(|&k| tool_key(k) == kind) {
            self.tool_action(ToolAction::Rollback(kind));
        }
    }

    fn tool_action(&self, action: ToolAction) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let state = ui.global::<SettingsUi>();
        state.set_tools_busy(true);
        state.set_tools_error(Default::default());
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let rollback = matches!(action, ToolAction::Rollback(_));
            let result = match action {
                ToolAction::Install => engine.install_tools().await,
                ToolAction::Check => engine.update_tools(true).await,
                ToolAction::Rollback(kind) => engine.rollback_tool(kind).await,
            };
            let result = result.map_err(|e| e.kind);
            tx.send(UiMsg::Settings(SettingsMsg::ToolsDone { rollback, result }));
        });
    }

    pub(crate) fn forget_passwords(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        ui.global::<SettingsUi>().set_forget_state("busy".into());
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = engine.forget_saved_passwords().await.map_err(|e| e.kind);
            tx.send(UiMsg::Settings(SettingsMsg::Forgot(result)));
        });
    }

    pub(crate) fn settings_msg(&self, msg: SettingsMsg) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let state = ui.global::<SettingsUi>();
        match msg {
            SettingsMsg::ToolsDone { rollback, result } => {
                state.set_tools_busy(false);
                state.set_tools_error(match result {
                    Ok(()) => Default::default(),
                    Err(ErrorKind::ToolMissing) if rollback => "no-previous".into(),
                    Err(kind) => kind.as_str().into(),
                });
                self.refresh_tools(&ui);
            }
            SettingsMsg::Forgot(Ok(n)) => {
                state.set_forget_count(i32::try_from(n).unwrap_or(i32::MAX));
                state.set_forget_state("done".into());
            }
            SettingsMsg::Forgot(Err(kind)) => {
                eprintln!("hyades: forgetting saved passwords: {kind:?}");
                state.set_forget_state("failed".into());
            }
        }
    }
}

/// Flags (or clears) the typed field `key`'s error; `comma` for the custom
/// speed's "use a dot" note.
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
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_flow::apply_change;

    #[test]
    fn torrent_and_download_choices_round_trip() {
        let mut s = Settings::default();
        let vm = adv_settings_vm(&s);
        assert_eq!((vm.max_active, vm.connections, vm.max_torrents), (3, 16, 3));
        assert_eq!(
            (
                vm.stop_ratio.as_str(),
                vm.stop_after.as_str(),
                vm.proxy.as_str()
            ),
            ("1", "off", "system")
        );
        assert!(vm.auto_checksum && vm.keep_awake && vm.upnp && vm.dht && vm.progress_by_type);

        for (key, value) in [
            ("max-active", "10"),
            ("connections", "4"),
            ("max-torrents", "1"),
            ("stop-ratio", "never"),
            ("stop-after", "24h"),
            ("auto-checksum", "false"),
            ("keep-awake", "false"),
            ("upnp", "false"),
            ("dht", "false"),
            ("progress-by-type", "false"),
            ("proxy", "none"),
        ] {
            assert!(apply_adv_change(&mut s, key, value), "{key}");
        }
        assert_eq!((s.max_active, s.connections), (10, 4));
        assert_eq!(s.torrent.max_active, 1);
        assert_eq!(s.torrent.stop_ratio, None);
        assert_eq!(s.torrent.stop_after_secs, Some(86_400));
        assert!(!s.auto_checksum && !s.keep_awake && !s.torrent.upnp && !s.torrent.dht);
        assert!(!s.ui.progress_by_type);
        assert_eq!(s.network.proxy, ProxySetting::None);
        let vm = adv_settings_vm(&s);
        assert_eq!(
            (
                vm.stop_ratio.as_str(),
                vm.stop_after.as_str(),
                vm.proxy.as_str()
            ),
            ("never", "24h", "none")
        );
        assert!(apply_adv_change(&mut s, "stop-ratio", "0"));
        assert_eq!(s.torrent.stop_ratio, Some(0.0));
        assert!(apply_adv_change(&mut s, "stop-ratio", "0.5"));
        assert_eq!(adv_settings_vm(&s).stop_ratio.as_str(), "0.5");
        // Saved by other means: no choice is marked.
        s.torrent.stop_ratio = Some(1.5);
        s.torrent.stop_after_secs = Some(60);
        let vm = adv_settings_vm(&s);
        assert_eq!(
            (vm.stop_ratio.as_str(), vm.stop_after.as_str()),
            ("other", "other")
        );

        let before = s.clone();
        for (key, value) in [
            ("max-active", "0"),
            ("max-active", "11"),
            ("connections", "3"),
            ("connections", "32"),
            ("max-torrents", "0"),
            ("stop-ratio", "3"),
            ("stop-after", "2h"),
            ("upnp", "yes"),
            ("proxy", "manual"),
            ("proxy", "http://h:1"),
            ("volume", "11"),
        ] {
            assert!(!apply_adv_change(&mut s, key, value), "{key} {value}");
        }
        assert_eq!(s, before);
    }

    /// Typed fields: limits in MB/s (1 MB = 10^6 bytes) or empty for none,
    /// peers and port as whole numbers or empty for the default; anything
    /// else changes nothing.
    #[test]
    fn typed_fields() {
        let mut s = Settings::default();
        assert!(apply_adv_change(&mut s, "upload-limit", "0.5"));
        assert!(apply_adv_change(&mut s, "torrent-down-limit", "2"));
        assert!(apply_adv_change(&mut s, "peers", "80"));
        assert!(apply_adv_change(&mut s, "port", "51413"));
        assert!(apply_adv_change(&mut s, "speed-custom", "2.5"));
        let t = &s.torrent;
        assert_eq!(
            (t.upload_limit_bps, t.download_limit_bps),
            (Some(500_000), Some(2_000_000))
        );
        assert_eq!((t.peer_limit, t.listen_port), (Some(80), Some(51413)));
        assert_eq!(
            s.speed,
            SpeedLimit::Custom {
                bytes_per_sec: 2_500_000
            }
        );
        assert_eq!(limit_text(Some(500_000)), "0.5");
        assert_eq!(limit_text(None), "");

        let before = s.clone();
        for (key, value) in [
            ("upload-limit", "1,5"),
            ("upload-limit", "abc"),
            ("torrent-down-limit", "0"),
            ("peers", "0"),
            ("peers", "1001"),
            ("peers", "+5"),
            ("port", "80"),
            ("port", "65536"),
            ("port", "5e4"),
            ("speed-custom", ""),
            ("speed-custom", "1,000"),
        ] {
            assert!(!apply_adv_change(&mut s, key, value), "{key} {value:?}");
        }
        assert_eq!(s, before);

        for key in ["upload-limit", "torrent-down-limit", "peers", "port"] {
            assert!(apply_adv_change(&mut s, key, " "), "{key}");
        }
        let t = &s.torrent;
        assert_eq!((t.upload_limit_bps, t.download_limit_bps), (None, None));
        assert_eq!((t.peer_limit, t.listen_port), (None, None));
    }

    /// Simple's keys and the Advanced ones go through the same entry point.
    #[test]
    fn one_entry_point_for_both_screens() {
        let mut s = Settings::default();
        assert!(apply_change(&mut s, "accent", "jade"));
        assert!(apply_change(&mut s, "stop-ratio", "never"));
        assert!(apply_change(&mut s, "advanced", "true"));
        assert_eq!(s.ui.accent, "Jade");
        assert_eq!(s.torrent.stop_ratio, None);
        assert_eq!(s.ui.mode, taurus_core::UiMode::Advanced);
    }

    #[test]
    fn tool_rows() {
        let status = |state, version: Option<&str>| ToolStatus {
            kind: ToolKind::Deno,
            version: version.map(str::to_owned),
            path: None,
            state,
        };
        let row = tool_row(&status(ToolState::Ready, Some("2.5.0")));
        assert_eq!(
            (row.kind.as_str(), row.version.as_str(), row.state.as_str()),
            ("deno", "2.5.0", "ready")
        );
        let installing = |total| ToolState::Installing {
            downloaded: 30,
            total,
        };
        let row = tool_row(&status(installing(Some(120)), None));
        assert_eq!((row.state.as_str(), row.progress), ("installing", 0.25));
        assert_eq!(tool_row(&status(installing(None), None)).progress, -1.0);
        assert_eq!(tool_row(&status(installing(Some(0)), None)).progress, -1.0);
        assert_eq!(
            tool_row(&status(ToolState::Missing, None)).state.as_str(),
            "missing"
        );
        let failed = ToolState::Failed(ErrorKind::ConnectionLost);
        assert_eq!(tool_row(&status(failed, None)).state.as_str(), "failed");
    }
}
