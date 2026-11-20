//! A kept terminal's output. A kept terminal outlives the client that
//! opened it, such as a phone whose screen locked, so its output can't
//! wait on that client: the keeper reads it all the time, keeps the latest
//! [`RING`] bytes for whoever attaches next, and passes it on to the one
//! reader attached now, if any.
//!
//! A reader that stops reading for [`STALL`] is detached rather than
//! waited on, so a build in the shell never stops because a phone is in a
//! pocket. Until then it slows the session, as any terminal's reader does.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use super::OUTPUT_BUFFER;

/// The output a kept terminal holds for the next reader. The replay is
/// raw bytes, so a full-screen program may look partly drawn until it
/// next redraws, which attaching with a resize usually makes it do.
pub(crate) const RING: usize = 256 * 1024;

/// How long a reader may leave output unread before it is detached.
const STALL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct Keeper {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    ring: VecDeque<u8>,
    reader: Option<Reader>,
    /// Counts attaches, so a stalled reader is only detached if no newer
    /// one took its place meanwhile.
    attaches: u64,
}

struct Reader {
    tx: mpsc::Sender<Vec<u8>>,
    attach: u64,
}

impl Keeper {
    /// Starts keeping `output` until the session ends it.
    pub(crate) fn start(output: mpsc::Receiver<Vec<u8>>) -> Arc<Keeper> {
        let keeper = Arc::new(Keeper::default());
        tokio::spawn(keeper.clone().run(output));
        keeper
    }

    async fn run(self: Arc<Self>, mut output: mpsc::Receiver<Vec<u8>>) {
        while let Some(chunk) = output.recv().await {
            // Into the ring and to the reader under one lock, so an attach
            // sees each chunk either in its replay or as output, never both.
            let reader = {
                let mut state = self.state.lock().unwrap();
                state.ring.extend(&chunk);
                let over = state.ring.len().saturating_sub(RING);
                state.ring.drain(..over);
                state.reader.as_ref().map(|r| (r.tx.clone(), r.attach))
            };
            let Some((tx, attach)) = reader else { continue };
            let sent = tokio::time::timeout(STALL, tx.send(chunk)).await;
            if !matches!(sent, Ok(Ok(()))) {
                self.detach(attach);
            }
        }
        // The session ended: so does the reader's stream.
        self.state.lock().unwrap().reader = None;
    }

    /// Becomes the one reader: returns what the ring holds, and a stream of
    /// the output from there on. An earlier reader's stream ends.
    pub(crate) fn attach(&self) -> (Vec<u8>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
        let mut state = self.state.lock().unwrap();
        state.attaches += 1;
        let attach = state.attaches;
        state.reader = Some(Reader { tx, attach });
        (state.ring.iter().copied().collect(), rx)
    }

    /// Whether a reader is attached and still reading.
    pub(crate) fn attached(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.reader.as_ref().is_some_and(|r| !r.tx.is_closed())
    }

    fn detach(&self, attach: u64) {
        let mut state = self.state.lock().unwrap();
        if state.reader.as_ref().is_some_and(|r| r.attach == attach) {
            state.reader = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn output_with_no_reader_is_kept_for_the_next() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        tx.send(b"one ".to_vec()).await.unwrap();
        tx.send(b"two".to_vec()).await.unwrap();
        settle().await;
        let (replay, _stream) = keeper.attach();
        assert_eq!(replay, b"one two");
    }

    #[tokio::test]
    async fn the_ring_keeps_only_the_latest_bytes() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        tx.send(vec![b'a'; RING]).await.unwrap();
        tx.send(b"end".to_vec()).await.unwrap();
        settle().await;
        let (replay, _stream) = keeper.attach();
        assert_eq!(replay.len(), RING);
        assert!(replay.ends_with(b"aaend"));
    }

    #[tokio::test]
    async fn a_reader_gets_what_follows_its_replay() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        tx.send(b"before".to_vec()).await.unwrap();
        settle().await;
        let (replay, mut stream) = keeper.attach();
        assert_eq!(replay, b"before");
        tx.send(b"after".to_vec()).await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"after");
        assert!(keeper.attached());
    }

    #[tokio::test]
    async fn a_second_reader_takes_over() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        let (_, mut first) = keeper.attach();
        let (_, mut second) = keeper.attach();
        assert_eq!(first.recv().await, None);
        tx.send(b"x".to_vec()).await.unwrap();
        assert_eq!(second.recv().await.unwrap(), b"x");
    }

    #[tokio::test]
    async fn a_dropped_reader_detaches_without_ending_the_session() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        let (_, stream) = keeper.attach();
        drop(stream);
        assert!(!keeper.attached());
        tx.send(b"still here".to_vec()).await.unwrap();
        settle().await;
        let (replay, _stream) = keeper.attach();
        assert_eq!(replay, b"still here");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_reader_is_detached_and_output_goes_on() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        let (_, _stalled) = keeper.attach();
        // More than the reader's buffer, which it never reads. The session
        // can still send all of it.
        for _ in 0..OUTPUT_BUFFER + 8 {
            tx.send(b"x".to_vec()).await.unwrap();
        }
        settle().await;
        assert!(!keeper.attached());
        let (replay, _stream) = keeper.attach();
        assert_eq!(replay.len(), OUTPUT_BUFFER + 8);
    }

    #[tokio::test]
    async fn the_stream_ends_with_the_session() {
        let (tx, rx) = mpsc::channel(4);
        let keeper = Keeper::start(rx);
        let (_, mut stream) = keeper.attach();
        drop(tx);
        assert_eq!(stream.recv().await, None);
    }
}
