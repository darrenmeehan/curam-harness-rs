//! ui.rs — the agent ↔ UI bus.
//!
//! The TUI runs the agent on a worker thread and stays live by pulling
//! events off an mpsc channel: thinking deltas, tool calls, results, ask
//! prompts. The worker half of the bus (owned by the Agent) also answers
//! approval/judgment questions by waiting on a reply channel, and honours a
//! shared cancel flag (Ctrl-C).
//!
//! One-shot and REPL runs install no bus: the agent falls back to the plain
//! POC-style prints, so the single run_task() path serves every front end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// One event on the agent → UI channel.
#[derive(Debug)]
pub struct UiEv {
    pub kind: String, // delta | tool | result | status | ask | stuck | error | done
    pub text: String,
}

/// The bus half an agent owns: emits events, receives ask replies, shares
/// the cancel flag with the UI thread.
#[derive(Debug)]
pub struct UiBus {
    pub tx: mpsc::Sender<UiEv>,
    pub reply_rx: Option<mpsc::Receiver<String>>,
    pub cancel: Arc<AtomicBool>,
}

impl UiBus {
    /// Emit one event; false when the UI died (its receiver dropped).
    pub fn send(&self, kind: &str, text: &str) -> bool {
        self.tx
            .send(UiEv {
                kind: kind.into(),
                text: text.into(),
            })
            .is_ok()
    }

    /// Whether the UI asked the run to stop (Ctrl-C once).
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Synchronous ask: emit an "ask" event, wait for the modal reply.
    /// Timeouts after ASK_QUIET_MS of no reply (or a closed UI channel):
    /// stale approvals must never hang the agent. None = deny.
    pub fn ask(&self, question: &str) -> Option<String> {
        if !self.send("ask", question) {
            return None;
        }
        let mut quiet = 0;
        loop {
            if self.cancelled() {
                return None;
            }
            match &self.reply_rx {
                Some(rx) => match rx.recv_timeout(Duration::from_millis(ASK_POLL_MS)) {
                    Ok(r) => return Some(r),
                    Err(_) => {
                        quiet += 1;
                        if quiet * ASK_POLL_MS >= ASK_QUIET_MS {
                            return None;
                        }
                    }
                },
                None => return None,
            }
        }
    }
}

const ASK_POLL_MS: u64 = 200;
const ASK_QUIET_MS: u64 = 60_000; // 60s of silence denies a stale ask
