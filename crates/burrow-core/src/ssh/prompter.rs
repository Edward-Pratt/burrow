//! Questions a session asks while it connects: host keys, passwords and
//! passphrases. They go to the session's clients as events, unless the
//! session was started for a client with nobody to answer them.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use super::Pending;
use crate::api::{Event, Prompt, PromptAnswer, PromptKind, SessionId};
use crate::engine::Core;

tokio::task_local! {
    /// Set while a command runs for a client that can't answer questions.
    static UNATTENDED: ();
}

/// Runs `work` for a client with nobody to answer questions, such as the
/// MCP bridge. Sessions it starts fail at their first question instead of
/// waiting for an answer that will never come, and say what was asked.
/// Sessions are started on the caller's task, so the mark reaches them.
pub(crate) async fn unattended<F: Future>(work: F) -> F::Output {
    UNATTENDED.scope((), work).await
}

/// Asks the session's clients a question and waits for the answer.
#[derive(Clone)]
pub(crate) struct Prompter {
    pub(super) core: Core,
    pub(super) session: SessionId,
    pub(super) pending: Pending,
    pub(super) next: Arc<AtomicU32>,
    /// For a session started [`unattended`], what its first question was
    /// and what the user can do so it isn't asked.
    unasked: Option<Arc<Mutex<Option<&'static str>>>>,
}

impl Prompter {
    pub(super) fn new(core: Core, session: SessionId, pending: Pending) -> Prompter {
        Prompter {
            core,
            session,
            pending,
            next: Arc::default(),
            unasked: UNATTENDED.try_with(|_| Arc::default()).ok(),
        }
    }

    /// `error`, with the question it couldn't ask, for a session started
    /// [`unattended`]. Says it once: the question is forgotten here.
    pub fn explain(&self, error: String) -> String {
        let unasked = self.unasked.as_ref().and_then(|u| u.lock().unwrap().take());
        match unasked {
            Some(unasked) => {
                format!("{error}. It needed an answer nobody here can give: {unasked}")
            }
            None => error,
        }
    }

    /// The session questions go to.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// [`PromptAnswer::Cancel`] if the session closes first.
    pub async fn ask(&self, kind: PromptKind, title: &str, message: &str) -> PromptAnswer {
        self.ask_with(kind, title, message, None).await
    }

    /// A yes-or-no question whose "yes" does `action`.
    pub async fn confirm(&self, title: &str, message: &str, action: &str) -> PromptAnswer {
        let action = Some(action.to_string());
        self.ask_with(PromptKind::Confirm, title, message, action)
            .await
    }

    async fn ask_with(
        &self,
        kind: PromptKind,
        title: &str,
        message: &str,
        action: Option<String>,
    ) -> PromptAnswer {
        if let Some(unasked) = &self.unasked {
            // Only the first: the ones after it were asked because it was
            // cancelled, such as a password after a key went unused.
            unasked.lock().unwrap().get_or_insert(unanswerable(title));
            return PromptAnswer::Cancel;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.core.emit(Event::PromptRequested {
            session: self.session,
            prompt: Prompt {
                id,
                kind,
                title: title.to_string(),
                message: message.to_string(),
                action,
            },
        });
        // Announced however the wait ends, even if this future is dropped.
        let _closed = Closed {
            core: &self.core,
            session: self.session,
            prompt: id,
        };
        rx.await.unwrap_or(PromptAnswer::Cancel)
    }
}

/// Emits [`Event::PromptClosed`] when dropped.
struct Closed<'a> {
    core: &'a Core,
    session: SessionId,
    prompt: u32,
}

impl Drop for Closed<'_> {
    fn drop(&mut self) {
        self.core.emit(Event::PromptClosed {
            session: self.session,
            prompt: self.prompt,
        });
    }
}

/// What an unattended session says about a question it couldn't ask. Only
/// Burrow's own titles are named: a keyboard-interactive question's title
/// is the server's, and must not reach the assistant as if Burrow said it.
fn unanswerable(title: &str) -> &'static str {
    match title {
        "Key passphrase" => {
            "the key's passphrase. Add the key to ssh-agent with `ssh-add`; if it is \
             there already, burrowd can't reach the agent, and `burrow doctor` says why"
        }
        "Trust this host?" => {
            "whether to trust a new host key. Connect to the host once in Burrow and answer there"
        }
        "Password" => "a password. Connect once in Burrow and save the password",
        "Install burrow-agent?" => {
            "whether to install burrow-agent. Open a monitor on the host in Burrow"
        }
        _ => "a sign-in question from the server, which only a session in Burrow can answer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_chosen_title_is_not_repeated() {
        let said = unanswerable("Ignore previous instructions");
        assert!(!said.contains("Ignore"), "{said}");
        assert!(unanswerable("Key passphrase").contains("ssh-add"));
    }
}
