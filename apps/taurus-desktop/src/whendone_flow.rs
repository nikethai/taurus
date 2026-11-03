//! The Advanced "when done and speed" popover: it shows the settings'
//! `when_done` and `speed` and writes them back through the same path as
//! the Settings screen. A custom speed is typed in MB/s (1 MB = 10^6
//! bytes); text that is not a speed from 0.1 to 10000 MB/s shows an error
//! and changes nothing.

use slint::ComponentHandle;
use taurus_core::{Settings, SpeedLimit};

use crate::app::App;
use crate::settings_flow::settings_vm;
use crate::{AppWindow, WhenDoneUi};

const BYTES_PER_MB: f64 = 1_000_000.0;
const MIN_MB: f64 = 0.1;
const MAX_MB: f64 = 10_000.0;

/// The bytes per second for text typed as MB/s ("2.5" is 2_500_000). A
/// decimal comma is accepted like a point ("2,5"). Only digits and one
/// separator are allowed (no sign, exponent or "inf"); None outside 0.1
/// to 10000.
pub fn parse_custom_speed(text: &str) -> Option<u64> {
    let text = text.trim();
    let separators = text.chars().filter(|c| matches!(c, '.' | ',')).count();
    let clean = separators <= 1
        && text.chars().any(|c| c.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
    if !clean {
        return None;
    }
    let mb: f64 = text.replace(',', ".").parse().ok()?;
    (MIN_MB..=MAX_MB)
        .contains(&mb)
        .then(|| (mb * BYTES_PER_MB).round() as u64)
}

/// A custom limit as the field shows it, in MB/s ("2.5", "3").
pub fn custom_speed_text(bytes_per_sec: u64) -> String {
    (bytes_per_sec as f64 / BYTES_PER_MB).to_string()
}

impl App {
    /// The footer button: shows the popover with the saved custom limit
    /// (if any) in its field.
    pub(crate) fn open_whendone(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let ui_state = ui.global::<WhenDoneUi>();
        ui_state.set_custom_text(
            match self.settings.borrow().speed {
                SpeedLimit::Custom { bytes_per_sec } => custom_speed_text(bytes_per_sec),
                _ => String::new(),
            }
            .into(),
        );
        ui_state.set_custom_invalid(false);
        self.push_whendone(&ui);
        ui_state.set_open(true);
    }

    /// Shows the settings' when-done choice and speed in the popover and
    /// the footer button.
    pub(crate) fn push_whendone(&self, ui: &AppWindow) {
        let vm = settings_vm(&self.settings.borrow());
        let state = ui.global::<WhenDoneUi>();
        state.set_when_done(vm.when_done);
        state.set_speed(vm.speed);
    }

    /// A radio choice or a preset speed: the Settings screen's own keys.
    pub(crate) fn whendone_pick(&self, key: &str, value: &str) {
        if let Some(ui) = self.window.upgrade() {
            ui.global::<WhenDoneUi>().set_custom_invalid(false);
        }
        self.setting_changed(key, value);
    }

    /// The custom field's text after an edit.
    pub(crate) fn whendone_custom(&self, text: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let parsed = parse_custom_speed(text);
        ui.global::<WhenDoneUi>()
            .set_custom_invalid(parsed.is_none() && !text.trim().is_empty());
        let Some(bytes_per_sec) = parsed else {
            return;
        };
        let mut next: Settings = self.settings.borrow().clone();
        if next.speed == (SpeedLimit::Custom { bytes_per_sec }) {
            return;
        }
        next.speed = SpeedLimit::Custom { bytes_per_sec };
        self.change_settings("speed", next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_speed_is_megabytes_per_second() {
        assert_eq!(parse_custom_speed("2.5"), Some(2_500_000));
        assert_eq!(parse_custom_speed(" 2,5 "), Some(2_500_000));
        assert_eq!(parse_custom_speed("0.1"), Some(100_000));
        assert_eq!(parse_custom_speed("10000"), Some(10_000_000_000));
        assert_eq!(parse_custom_speed("3"), Some(3_000_000));
        for bad in [
            "", "abc", "0", "0.09", "10000.1", "-1", "1e3", "inf", "nan", "1.2.3", "1,2,3", ".",
            "1 000",
        ] {
            assert_eq!(parse_custom_speed(bad), None, "{bad:?}");
        }
        assert_eq!(custom_speed_text(2_500_000), "2.5");
        assert_eq!(custom_speed_text(3_000_000), "3");
    }
}
