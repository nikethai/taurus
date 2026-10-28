//! The OS commands behind the when-done actions that touch the machine.

use crate::settings::WhenDone;

/// The program and arguments that put the machine to sleep or shut it down;
/// `None` for the actions front ends carry out (and on other OSes).
pub fn command_for(action: WhenDone) -> Option<(String, Vec<String>)> {
    let (program, args): (&str, &[&str]) = match action {
        WhenDone::Sleep => SLEEP?,
        WhenDone::Shutdown => SHUTDOWN?,
        WhenDone::Nothing | WhenDone::Notify | WhenDone::Quit => return None,
    };
    Some((
        program.to_string(),
        args.iter().map(|a| a.to_string()).collect(),
    ))
}

type Command = Option<(&'static str, &'static [&'static str])>;

#[cfg(target_os = "macos")]
const SLEEP: Command = Some(("pmset", &["sleepnow"]));
#[cfg(target_os = "macos")]
const SHUTDOWN: Command = Some((
    "osascript",
    &["-e", r#"tell application "System Events" to shut down"#],
));

#[cfg(windows)]
const SLEEP: Command = Some(("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]));
#[cfg(windows)]
const SHUTDOWN: Command = Some(("shutdown", &["/s", "/t", "0"]));

#[cfg(target_os = "linux")]
const SLEEP: Command = Some(("systemctl", &["suspend"]));
#[cfg(target_os = "linux")]
const SHUTDOWN: Command = Some(("systemctl", &["poweroff"]));

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
const SLEEP: Command = None;
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
const SHUTDOWN: Command = None;

#[cfg(test)]
mod tests {
    use super::*;

    fn command(program: &str, args: &[&str]) -> Option<(String, Vec<String>)> {
        Some((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ))
    }

    #[test]
    fn commands_per_platform() {
        for action in [WhenDone::Nothing, WhenDone::Notify, WhenDone::Quit] {
            assert_eq!(command_for(action), None, "{action:?}");
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                command_for(WhenDone::Sleep),
                command("pmset", &["sleepnow"])
            );
            assert_eq!(
                command_for(WhenDone::Shutdown),
                command(
                    "osascript",
                    &["-e", r#"tell application "System Events" to shut down"#]
                )
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                command_for(WhenDone::Sleep),
                command("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"])
            );
            assert_eq!(
                command_for(WhenDone::Shutdown),
                command("shutdown", &["/s", "/t", "0"])
            );
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                command_for(WhenDone::Sleep),
                command("systemctl", &["suspend"])
            );
            assert_eq!(
                command_for(WhenDone::Shutdown),
                command("systemctl", &["poweroff"])
            );
        }
    }
}
