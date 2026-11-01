//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, ModelRc, VecModel};
use taurus_desktop::{AppWindow, RowVm, Screen, Theme};

#[test]
fn ui_smoke() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = AppWindow::new().unwrap();
    window_title(&ui);
    theme_defaults(&ui);
    welcome_has_paste_field(&ui);
    downloads_rows_show_status_and_retry(&ui);
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

/// Elements whose accessible label is exactly `label`.
fn labelled(ui: &AppWindow, label: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(ui, label).collect()
}

fn welcome_has_paste_field(ui: &AppWindow) {
    ui.set_screen(Screen::Welcome);
    let field = labelled(ui, "Paste a link");
    assert!(
        field
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::TextInput)),
        "welcome_has_paste_field: no text input labelled \"Paste a link\""
    );
}

fn row(id: i32, state: &str) -> RowVm {
    RowVm {
        id,
        name: format!("file-{id}.zip").into(),
        kind: "archive".into(),
        state: state.into(),
        progress: 0.3,
        done_text: "120 MB".into(),
        total_text: "400 MB".into(),
        eta_kind: "unknown".into(),
        ..Default::default()
    }
}

fn downloads_rows_show_status_and_retry(ui: &AppWindow) {
    let downloading = RowVm {
        eta_kind: "minutes".into(),
        eta_value: 3,
        ..row(1, "downloading")
    };
    let waiting = RowVm {
        retry_seconds: 65,
        error_kind: "connection_lost".into(),
        ..row(2, "waiting")
    };
    let failed = RowVm {
        error_kind: "link_expired".into(),
        ..row(3, "failed")
    };
    let rows = VecModel::from(vec![downloading, waiting, failed]);
    ui.set_downloading_rows(ModelRc::new(rows));
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);

    let retried = Rc::new(RefCell::new(Vec::new()));
    let sink = retried.clone();
    ui.on_retry(move |id| sink.borrow_mut().push(id));

    let visible = |label: &str| {
        let found = labelled(ui, label);
        assert!(
            found.iter().any(|e| e.size().width > 0.0),
            "downloads_rows_show_status_and_retry: no visible text {label:?}"
        );
    };
    visible("120 MB of 400 MB · About 3 min left");
    visible("Waiting for a connection. Trying again in 1:05.");
    visible("This link stopped working. It may have expired.");

    let buttons: Vec<ElementHandle> = labelled(ui, "Try again")
        .into_iter()
        .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .collect();
    assert_eq!(
        buttons.len(),
        1,
        "downloads_rows_show_status_and_retry: expected one \"Try again\" button"
    );
    buttons[0].invoke_accessible_default_action();
    assert_eq!(
        *retried.borrow(),
        vec![3],
        "downloads_rows_show_status_and_retry: retry(id) not called once with the failed row"
    );
}
