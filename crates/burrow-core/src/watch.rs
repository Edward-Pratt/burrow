//! Picks up edits made outside Burrow (an editor, `git pull`, a file manager)
//! and turns them into events.

use std::time::Duration;

use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::engine::Core;
use crate::error::{Error, Result};

/// Editors often write a file in several steps; wait for them to settle.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Keeps watching while it is alive. Drop it to stop.
pub struct Watcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

impl Core {
    /// Starts watching the tree directory. Must be called from inside a
    /// tokio runtime.
    pub fn watch(&self) -> Result<Watcher> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
            let _ = tx.send(result);
        })
        .map_err(|e| Error::Watch(e.to_string()))?;
        debouncer
            .watch(self.tree_dir(), RecursiveMode::Recursive)
            .map_err(|e| Error::Watch(e.to_string()))?;

        // The loop ends when the debouncer, and with it `tx`, is dropped.
        let core = self.clone();
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                if let Err(errors) = result {
                    for e in errors {
                        tracing::warn!("file watcher: {e}");
                    }
                }
                // Reload even after watcher errors: a full reload is cheap
                // and always ends in the right state.
                let core = core.clone();
                let reloaded = tokio::task::spawn_blocking(move || core.reload()).await;
                match reloaded {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("reloading config: {e}"),
                    Err(e) => tracing::warn!("reloading config: {e}"),
                }
            }
        });

        Ok(Watcher {
            _debouncer: debouncer,
        })
    }
}
