//! The tray icon and its menu: "Open Hyades", "Pause all" / "Resume all",
//! "Quit". Clicking the icon opens the window too. Events are read by a
//! 100 ms `slint::Timer` on the UI thread.
//!
//! Linux uses the D-Bus StatusNotifier protocol (tray-icon's `ksni`
//! backend, no GTK). It needs a StatusNotifier host (KDE, most panels;
//! GNOME only with the AppIndicator extension): without one there is no
//! tray, and closing the window quits instead of hiding it.

use std::time::Duration;

use slint::{Timer, TimerMode};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon::app_icon_rgba;
use crate::shell_text::{APP_NAME, TRAY_OPEN, TRAY_PAUSE_ALL, TRAY_QUIT, TRAY_RESUME_ALL};

const POLL: Duration = Duration::from_millis(100);
const ICON_SIZE: u32 = 32;

/// What the tray does; called on the UI thread.
pub struct TrayActions {
    pub open: Box<dyn Fn()>,
    pub pause_all: Box<dyn Fn()>,
    pub resume_all: Box<dyn Fn()>,
    pub quit: Box<dyn Fn()>,
    /// Whether every unfinished download is paused: the menu then offers
    /// "Resume all" instead of "Pause all".
    pub all_paused: Box<dyn Fn() -> bool>,
}

/// The live tray icon; dropping it removes the icon.
pub struct Tray {
    _icon: TrayIcon,
    _timer: Timer,
}

/// Shows the tray icon. `None` when the desktop has no tray (no
/// StatusNotifier host on Linux) or it could not be created; the reason is
/// logged.
pub fn build(on: TrayActions) -> Option<Tray> {
    if !host_available() {
        eprintln!("hyades: no tray on this desktop (no StatusNotifier host)");
        return None;
    }
    let open = MenuItem::new(TRAY_OPEN, true, None);
    let toggle = MenuItem::new(TRAY_PAUSE_ALL, true, None);
    let quit = MenuItem::new(TRAY_QUIT, true, None);
    let menu = Menu::new();
    let appended = menu.append_items(&[&open, &PredefinedMenuItem::separator(), &toggle, &quit]);
    let icon = Icon::from_rgba(app_icon_rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE);
    let built = appended
        .map_err(|e| e.to_string())
        .and(icon.map_err(|e| e.to_string()))
        .and_then(|icon| {
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip(APP_NAME)
                .with_icon(icon)
                .build()
                .map_err(|e| e.to_string())
        });
    let tray = match built {
        Ok(t) => t,
        Err(e) => {
            eprintln!("hyades: no tray icon: {e}");
            return None;
        }
    };

    let (open_id, toggle_id, quit_id) = (open.id().clone(), toggle.id().clone(), quit.id().clone());
    let mut shows_resume = false;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, POLL, move || {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == open_id {
                (on.open)();
            } else if event.id == toggle_id {
                if (on.all_paused)() {
                    (on.resume_all)();
                } else {
                    (on.pause_all)();
                }
            } else if event.id == quit_id {
                (on.quit)();
                return;
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            let clicked = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick { .. }
            );
            if clicked {
                (on.open)();
            }
        }
        let paused = (on.all_paused)();
        if paused != shows_resume {
            shows_resume = paused;
            toggle.set_text(if paused {
                TRAY_RESUME_ALL
            } else {
                TRAY_PAUSE_ALL
            });
        }
    });
    Some(Tray {
        _icon: tray,
        _timer: timer,
    })
}

/// Linux: a StatusNotifier host is registered on the session bus.
#[cfg(target_os = "linux")]
fn host_available() -> bool {
    let has_watcher = || -> zbus::Result<bool> {
        let bus = zbus::blocking::Connection::session()?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&bus)?;
        let name = zbus::names::BusName::try_from("org.kde.StatusNotifierWatcher")?;
        Ok(dbus.name_has_owner(name)?)
    };
    has_watcher().unwrap_or(false)
}

/// Windows and macOS always have a tray (notification area, menu bar).
#[cfg(not(target_os = "linux"))]
fn host_available() -> bool {
    true
}
