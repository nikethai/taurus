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
    windows_resources();
}

/// The exe's icon and version info. winresource is a build-dependency only
/// on Windows hosts (`cfg(windows)` is the host here, as the script runs on
/// it); `CARGO_CFG_TARGET_OS` is the platform being built for.
#[cfg(windows)]
fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packaging")
        .join("icons")
        .join("hyades.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    winresource::WindowsResource::new()
        .set_icon(&icon.to_string_lossy())
        .set("ProductName", "Hyades")
        .set("FileDescription", "Hyades")
        .compile()
        .expect("embedding the Windows icon and version info failed");
}

#[cfg(not(windows))]
fn windows_resources() {}
