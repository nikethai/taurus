use slint::ComponentHandle;
use taurus_desktop::AppWindow;

fn main() -> Result<(), slint::PlatformError> {
    AppWindow::new()?.run()
}
