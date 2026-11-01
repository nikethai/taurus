//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use slint::ComponentHandle;
use taurus_desktop::{AppWindow, Theme};

#[test]
fn ui_smoke() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = AppWindow::new().unwrap();
    window_title(&ui);
    theme_defaults(&ui);
    // later tasks append their checks here
}

fn window_title(ui: &AppWindow) {
    assert_eq!(ui.get_window_title(), "Hyades");
}

fn theme_defaults(ui: &AppWindow) {
    let theme = ui.global::<Theme>();
    let color = |r, g, b| slint::Color::from_rgb_u8(r, g, b);
    theme.set_dark(false);
    assert_eq!(theme.get_bg().color(), color(0xFB, 0xFB, 0xFC));
    theme.set_dark(true);
    assert_eq!(theme.get_bg().color(), color(0x1C, 0x1C, 0x1F));
    assert_eq!(theme.get_accent_base().color(), color(0xE8, 0x59, 0x0C));
}
