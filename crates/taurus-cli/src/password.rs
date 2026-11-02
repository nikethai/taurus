//! Asking for the password of a protected download. It comes only from the
//! terminal, never from a flag or the environment (a debug build's test
//! hook aside, see `test_hooks`), and is never printed, logged or put into
//! an error message.

use std::io::IsTerminal;

use taurus_core::Credentials;

use crate::Result;
use crate::test_hooks;

const NO_TERMINAL: &str = "a password can only be typed on a terminal; \
     run taurus from one (it is never taken from an option or a pipe)";

/// The user name `user` with the password typed (not echoed) on the
/// terminal. Fails, saying so, when there is no terminal or nothing was
/// typed.
pub fn credentials(user: &str) -> Result<Credentials> {
    let user = user.trim();
    if user.is_empty() {
        return Err("the user name is empty".into());
    }
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

/// Asks on the terminal. Stdin must be a terminal too: a script that pipes
/// text must not be mistaken for a person typing, and rpassword reads the
/// terminal device itself, which could wait on a person the script knows
/// nothing about.
fn prompt(user: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(NO_TERMINAL.into());
    }
    // The error of the terminal read names no password, but it is not
    // needed: the person is told what to do instead.
    rpassword::prompt_password(format!("Password for {user}: "))
        .map_err(|_| "could not read the password from the terminal".into())
}
