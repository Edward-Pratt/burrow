//! The user's terminal: answering a session's questions, and raw mode for
//! `burrow ssh`.

use std::io::{BufRead, IsTerminal, Write};
use std::os::fd::AsRawFd;

use burrow_core::{Prompt, PromptAnswer, PromptKind, SecretValue};

use crate::clean;

/// Asks the user `prompt` on the terminal. `None` when there is no
/// terminal to ask on.
pub fn ask(prompt: &Prompt) -> Option<PromptAnswer> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{}", clean(&prompt.title));
    let _ = writeln!(err, "{}", clean(&prompt.message));
    let answer = match prompt.kind {
        PromptKind::Confirm => {
            let _ = write!(err, "Trust and connect? [y/N] ");
            let _ = err.flush();
            let line = read_line(false)?;
            if matches!(line.trim(), "y" | "Y" | "yes") {
                PromptAnswer::Confirm
            } else {
                PromptAnswer::Cancel
            }
        }
        PromptKind::Secret { .. } => {
            let _ = write!(err, "> ");
            let _ = err.flush();
            let line = read_line(true)?;
            let _ = writeln!(err);
            PromptAnswer::Text {
                value: SecretValue::new(line.trim_end_matches(['\r', '\n'])),
                save: false,
            }
        }
        PromptKind::Text => {
            let _ = write!(err, "> ");
            let _ = err.flush();
            let line = read_line(false)?;
            PromptAnswer::Text {
                value: SecretValue::new(line.trim_end_matches(['\r', '\n'])),
                save: false,
            }
        }
    };
    Some(answer)
}

/// One line from the terminal, without echoing it when `hidden`.
fn read_line(hidden: bool) -> Option<String> {
    let _echo = hidden
        .then(|| Mode::set(|t| t.c_lflag &= !libc::ECHO))
        .flatten();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line)
}

/// A changed terminal mode, put back when dropped.
pub struct Mode {
    saved: libc::termios,
}

impl Mode {
    /// Changes stdin's terminal mode with `change`. `None` when stdin is
    /// not a terminal.
    fn set(change: impl FnOnce(&mut libc::termios)) -> Option<Mode> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: termios is plain data, filled in by tcgetattr.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is stdin and `saved` is a valid termios to write to.
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut changed = saved;
        change(&mut changed);
        // SAFETY: as above, with a termios tcgetattr filled in.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &changed) } != 0 {
            return None;
        }
        Some(Mode { saved })
    }

    /// Raw mode: keys go to the remote shell as they are typed, Ctrl+C
    /// included.
    pub fn raw() -> Option<Mode> {
        // SAFETY: cfmakeraw only changes the fields of the termios given.
        Mode::set(|t| unsafe { libc::cfmakeraw(t) })
    }
}

impl Drop for Mode {
    fn drop(&mut self) {
        // SAFETY: puts back the termios tcgetattr returned.
        unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &self.saved) };
    }
}

/// The terminal's size in columns and rows, or 80×24 when unknown.
pub fn size() -> (u16, u16) {
    // SAFETY: winsize is plain data, filled in by the ioctl.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize to the pointer given.
    let ok = unsafe { libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    if ok != 0 || size.ws_col == 0 || size.ws_row == 0 {
        return (80, 24);
    }
    (size.ws_col, size.ws_row)
}
