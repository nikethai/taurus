fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            // Element names for the testing backend's ElementHandle queries
            // (tests/ui.rs finds controls by accessible label).
            .with_debug_info(true)
            // translations/<lang>/LC_MESSAGES/taurus-desktop.po, compiled in;
            // src/i18n.rs selects one (see TRANSLATING.md).
            .with_bundled_translations("translations"),
    )
    .expect("slint build failed");
}
