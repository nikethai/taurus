//! Hyades desktop app: Slint UI plus the modules behind it.

slint::include_modules!();

pub mod add_flow;
mod adv_add;
mod adv_fields;
mod adv_flow;
mod adv_pick;
mod adv_settings;
mod adv_view;
pub mod app;
pub mod batch;
mod batch_flow;
pub mod bridge;
pub mod clipboard;
pub mod detail;
pub mod filters;
pub mod format;
pub mod grab_flow;
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
mod torrent_pick;
pub mod tray;
mod whendone_flow;

#[cfg(test)]
mod testutil;
