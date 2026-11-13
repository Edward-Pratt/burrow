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
    /// For a session started [`unattended`], what it would have asked.
    unasked: Option<Arc<Mutex<Vec<Unasked>>>>,
}

/// A question an unattended session skipped. Only Burrow's own title is
/// kept: the message can hold text the server sent.
struct Unasked {
    title: String,
    kind: PromptKind,
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

    /// `error`, with the questions it skipped and what the user can do so
    /// they aren't asked, for a session started [`unattended`]. Says it
    /// once: the questions are forgotten here.
    pub fn explain(&self, error: String) -> String {
        let Some(unasked) = &self.unasked else {
            return error;
        };
        let unasked = std::mem::take(&mut *unasked.lock().unwrap());
        if unasked.is_empty() {
            return error;
        }
        let mut text = format!("{error}. Burrow had questions nobody here can answer:");
        for question in unasked {
            let title: String = question.title.chars().filter(|c| !c.is_control()).collect();
            let fix = match (question.kind, title.as_str()) {
                (_, "Key passphrase") => {
                    "add the key to ssh-agent with `ssh-add`, so Burrow needn't ask"
                }
                (PromptKind::Confirm, _) => "connect to it once in Burrow and answer there",
                (PromptKind::Secret { can_save: true }, _) => {
                    "connect to it once in Burrow and save the password"
                }
                _ => "the server asks this each time, so only a session in Burrow can answer it",
            };
            text.push_str(&format!("\n- \"{title}\": {fix}"));
        }
        text
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
            let title = title.to_string();
            unasked.lock().unwrap().push(Unasked { title, kind });
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
