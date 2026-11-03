//! Hyades desktop app: Slint UI plus the modules behind it.

slint::include_modules!();

pub mod add_flow;
mod adv_flow;
mod adv_view;
pub mod app;
pub mod bridge;
pub mod clipboard;
pub mod detail;
pub mod filters;
pub mod format;
pub mod icon;
pub mod instance;
pub mod links;
pub mod login;
mod media_add;
pub mod mode;
pub mod notify;
mod pump;
pub mod rows;
pub mod settings_flow;
pub mod shell_text;
pub mod tray;

#[cfg(test)]
mod testutil;
