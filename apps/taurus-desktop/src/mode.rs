//! Which screen set the window shows: Simple, or Advanced in its compact or
//! wide layout. Pure; the app applies it in `adv_view.rs`.

use taurus_core::UiMode;

/// Advanced mode shows the wide layout (sidebar, list, detail pane) from
/// this window width up, in logical pixels.
pub const WIDE_MIN_PX: f32 = 900.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Simple,
    Compact,
    Wide,
}

impl Layout {
    /// The window's `layout` property value.
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Simple => "simple",
            Layout::Compact => "compact",
            Layout::Wide => "wide",
        }
    }
}

/// Simple mode is always Simple; Advanced is Wide from `WIDE_MIN_PX` up,
/// else Compact.
pub fn layout_for(mode: UiMode, width_px: f32) -> Layout {
    match mode {
        UiMode::Simple => Layout::Simple,
        UiMode::Advanced if width_px >= WIDE_MIN_PX => Layout::Wide,
        UiMode::Advanced => Layout::Compact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_threshold() {
        assert_eq!(layout_for(UiMode::Advanced, 899.0), Layout::Compact);
        assert_eq!(layout_for(UiMode::Advanced, 899.9), Layout::Compact);
        assert_eq!(layout_for(UiMode::Advanced, 900.0), Layout::Wide);
        assert_eq!(layout_for(UiMode::Advanced, 420.0), Layout::Compact);
        assert_eq!(layout_for(UiMode::Simple, 2000.0), Layout::Simple);
        assert_eq!(layout_for(UiMode::Simple, 420.0), Layout::Simple);
        assert_eq!(
            [Layout::Simple, Layout::Compact, Layout::Wide].map(Layout::as_str),
            ["simple", "compact", "wide"]
        );
    }
}
