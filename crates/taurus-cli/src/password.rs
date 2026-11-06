//! Asking for the password of a protected download. It comes only from the
//! terminal, never from a flag or the environment (a debug build's test
//! hook aside, see `test_hooks`), and is never printed, logged or put into
//! an error message.

use std::io::IsTerminal;

use taurus_core::Credentials;

use crate::Result;
use crate::test_hooks;

const NO_TERMINAL: &str = "a password can only be typed on a terminal; \
     run hyades-cli from one (it is never taken from an option or a pipe)";

/// The user name `user` with the password typed (not echoed) on the
/// terminal. Fails, saying so, when there is no terminal or nothing was
/// typed.
pub fn credentials(user: &str) -> Result<Credentials> {
    let user = check_user(user)?;
    let password = match test_hooks::password() {
        Some(password) => password,
        None => prompt(user)?,
    };
    if password.is_empty() {
        return Err("no password was entered".into());
    }
    Ok(Credentials {
        username: user.to_string(),
        password,
    })
}

/// The user name, trimmed, if HTTP Basic sign-in can carry it: RFC 7617
/// allows no `:` in it (so `--user alice:secret` is refused, not stored)
/// and no control characters. The text is not repeated in the error.
fn check_user(user: &str) -> Result<&str> {
    let user = user.trim();
    if user.is_empty() {
        return Err("the user name is empty".into());
    }
    if user.contains(':') || user.chars().any(char::is_control) {
        return Err("the user name cannot contain ':' or control characters; \
             the password is asked for on the terminal"
            .into());
    }
    Ok(user)
}

/// Asks on the terminal. Stdin must be a terminal too: a script that pipes
/// text must not be mistaken for a person typing, and rpassword reads the
/// terminal device itself, which could wait on a person the script knows
/// nothing about.
fn prompt(user: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(NO_TERMINAL.into());
    }
    // rpassword writes the prompt to the terminal device itself (not stdout,
    // which carries only results, nor stderr, which may be redirected). The
    // error of the terminal read names no password, but it is not needed: the
    // person is told what to do instead.
    rpassword::prompt_password(format!("Password for {user}: "))
        .map_err(|_| "could not read the password from the terminal".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_that_basic_sign_in_cannot_carry_are_refused() {
        assert_eq!(check_user("  alice ").unwrap(), "alice");
        for bad in ["", "  ", "alice:secret", ":", "a\tb", "a\nb"] {
            let e = check_user(bad).unwrap_err().to_string();
            assert!(!e.contains("secret"), "{bad:?}: {e}");
            assert!(
                e.contains("empty") || e.contains("cannot contain"),
                "{bad:?}: {e}"
            );
        }
    }
}
