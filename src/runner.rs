//! How a command line becomes a process, and how its end is reported.
//! Kept free of GTK so it can be unit-tested.

use std::env;

/// The shell and arguments that run `command` the way a terminal would.
/// A login shell (`-l`) picks up the same PATH and profile as a new terminal tab.
pub fn shell_argv(command: &str) -> Vec<String> {
    let shell = env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string());
    vec![shell, "-lc".to_string(), command.to_string()]
}

/// Extra variables layered on top of the inherited environment.
pub fn extra_env() -> Vec<String> {
    vec![
        "TERM=xterm-256color".to_string(),
        "COLORTERM=truecolor".to_string(),
    ]
}

/// How a child ended, decoded from the raw wait status VTE reports.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    Signaled(i32),
}

impl Outcome {
    pub fn from_wait_status(status: i32) -> Self {
        let signal = status & 0x7f;
        if signal == 0 {
            Outcome::Exited((status >> 8) & 0xff)
        } else {
            Outcome::Signaled(signal)
        }
    }

    pub fn succeeded(&self) -> bool {
        *self == Outcome::Exited(0)
    }

    pub fn describe(&self) -> String {
        match self {
            Outcome::Exited(code) => format!("exit {code}"),
            Outcome::Signaled(sig) => format!("killed by signal {sig}"),
        }
    }

    /// The exit code a shell would report for this outcome (128 + signal for a kill).
    pub fn shell_exit_code(&self) -> u8 {
        match self {
            Outcome::Exited(code) => *code as u8,
            Outcome::Signaled(sig) => (128 + sig) as u8,
        }
    }
}

/// `runbox --request` exit codes that aren't the command's own.
pub mod request_exit {
    /// Declined: the window closed before the command finished, or it never ran.
    pub const NOT_RUN: u8 = 125;
    /// Another request is pending or a command is already running.
    pub const BUSY: u8 = 75;
    /// Bad arguments.
    pub const USAGE: u8 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_normal_exit() {
        assert_eq!(Outcome::from_wait_status(0), Outcome::Exited(0));
        assert_eq!(Outcome::from_wait_status(1 << 8), Outcome::Exited(1));
        assert_eq!(Outcome::from_wait_status(100 << 8), Outcome::Exited(100));
    }

    #[test]
    fn decodes_signal_death() {
        assert_eq!(Outcome::from_wait_status(2), Outcome::Signaled(2));
        assert_eq!(Outcome::from_wait_status(9), Outcome::Signaled(9));
        assert!(!Outcome::from_wait_status(2).succeeded());
    }

    #[test]
    fn shell_exit_codes_match_bash() {
        assert_eq!(Outcome::Exited(4).shell_exit_code(), 4);
        assert_eq!(Outcome::Signaled(2).shell_exit_code(), 130);
        assert_eq!(Outcome::Signaled(1).shell_exit_code(), 129);
    }

    #[test]
    fn command_reaches_the_shell_unchanged() {
        let argv = shell_argv("echo 'a b' | wc -c; sudo apt update");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[1], "-lc");
        assert_eq!(argv[2], "echo 'a b' | wc -c; sudo apt update");
    }
}
