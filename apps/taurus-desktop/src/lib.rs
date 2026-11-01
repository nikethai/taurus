//! Hyades desktop app: Slint UI plus the pure modules behind it.

slint::include_modules!();

pub mod app;
pub mod bridge;
pub mod format;
pub mod links;
pub mod rows;

#[cfg(test)]
mod testutil;
