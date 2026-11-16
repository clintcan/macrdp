//! What the server does when it can't get a valid password at startup, and the
//! status file that tells the menu-bar controller why (issue #266).
//!
//! Under launchd the LaunchAgent has `KeepAlive`, so a process that exits on a
//! wrong Keychain password is restarted about every 10 s, forever. Each restart
//! is another failed password check against the Mac account (macOS throttles and
//! eventually locks an account after repeated failures) and repeats the startup
//! work. So a headless server does not exit on a credential failure: it logs one
//! error, writes [`STATUS_FILE`] for the controller, and waits. Every
//! [`POLL_INTERVAL`] it re-reads the Keychain or password file (no account
//! check) and runs the check again only when the password differs from the one
//! it rejected ([`should_check`]). So a Keychain that unlocks later, or an entry
//! fixed in Keychain Access, recovers on its own, and waiting adds no failed
//! logins. An interactive run (stdout is a terminal) still exits with the error,
//! as before.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a waiting server re-reads the stored password.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Whether `current` should go to the account check, given the password it
/// last rejected. Only a changed password is checked again.
pub fn should_check(rejected: Option<&str>, current: &str) -> bool {
    rejected != Some(current)
}

/// File name of the status file, in the config directory
/// (`~/Library/Application Support/macrdp`). Its first line is the
/// [`Failure::code`]; the controller keys its menu on it.
pub const STATUS_FILE: &str = "auth-status";

/// Why the startup credential check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The Keychain (or password file) had no usable password.
    NoPassword,
    /// The password was read but the account check rejected it.
    WrongPassword,
}

impl Failure {
    /// The status file's first line. Part of the contract with the controller
    /// (`gui/Sources/macrdptray`), so don't change it in one place only.
    pub fn code(self) -> &'static str {
        match self {
            Failure::NoPassword => "no-password",
            Failure::WrongPassword => "wrong-password",
        }
    }
}

/// Whether a credential failure should park the process instead of exiting.
///
/// - `MACRDP_PARK_ON_AUTH_FAILURE=1/0` (on/off/true/false/yes/no) wins.
/// - Otherwise park when headless (stdout is not a terminal: launchd would
///   restart an exit in a loop) and exit when interactive (nothing restarts a
///   terminal run, and the user is there to read the error).
pub fn should_park(stdout_is_tty: bool, env_override: Option<&str>) -> bool {
    match env_override
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("1") | Some("on") | Some("true") | Some("yes") => true,
        Some("0") | Some("off") | Some("false") | Some("no") => false,
        _ => !stdout_is_tty,
    }
}

/// The status file's contents for `failure`: the code on line 1, then the
/// account on line 2.
pub fn status_contents(failure: Failure, username: &str) -> String {
    format!("{}\n{}\n", failure.code(), username)
}

/// Path of the status file inside `dir`.
pub fn status_path(dir: &Path) -> PathBuf {
    dir.join(STATUS_FILE)
}

/// Record `failure` for the controller. Best effort: a write error is logged by
/// the caller's context and never stops the park.
pub fn write(dir: &Path, failure: Failure, username: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(status_path(dir), status_contents(failure, username))
}

/// Remove a stale status file after a successful check. A missing file is fine.
pub fn clear(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(status_path(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_changed_password_is_checked_again() {
        assert!(should_check(None, "first try"));
        assert!(!should_check(Some("typo"), "typo"));
        assert!(should_check(Some("typo"), "fixed"));
    }

    #[test]
    fn parks_when_headless_and_exits_when_interactive() {
        assert!(should_park(false, None));
        assert!(!should_park(true, None));
    }

    #[test]
    fn env_override_wins_both_ways() {
        assert!(should_park(true, Some("1")));
        assert!(should_park(true, Some(" Yes ")));
        assert!(!should_park(false, Some("0")));
        assert!(!should_park(false, Some("off")));
        // Anything else falls back to the TTY rule.
        assert!(should_park(false, Some("maybe")));
        assert!(!should_park(true, Some("")));
    }

    #[test]
    fn status_codes_are_the_controller_contract() {
        assert_eq!(Failure::NoPassword.code(), "no-password");
        assert_eq!(Failure::WrongPassword.code(), "wrong-password");
        assert_eq!(
            status_contents(Failure::WrongPassword, "alice"),
            "wrong-password\nalice\n"
        );
    }

    #[test]
    fn write_then_clear_round_trips_and_clear_tolerates_missing() {
        let dir = std::env::temp_dir().join(format!("macrdp-auth-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, Failure::NoPassword, "bob").unwrap();
        assert_eq!(
            std::fs::read_to_string(status_path(&dir)).unwrap(),
            "no-password\nbob\n"
        );
        clear(&dir).unwrap();
        assert!(!status_path(&dir).exists());
        clear(&dir).unwrap(); // already gone: still Ok
        let _ = std::fs::remove_dir_all(&dir);
    }
}
