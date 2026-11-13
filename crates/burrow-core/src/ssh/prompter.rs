//! Questions a session asks while it connects: host keys, passwords and
//! passphrases. They go to the session's clients as events.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::oneshot;

use super::Pending;
use crate::api::{Event, Prompt, PromptAnswer, PromptKind, SessionId};
use crate::engine::Core;

/// Asks the session's clients a question and waits for the answer.
#[derive(Clone)]
pub(crate) struct Prompter {
    pub(super) core: Core,
    pub(super) session: SessionId,
    pub(super) pending: Pending,
    pub(super) next: Arc<AtomicU32>,
}

impl Prompter {
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
