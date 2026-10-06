//! Questions the backend puts to the user in the desktop UI and blocks on, such
//! as "an orchestrator wants to remove worktree N, OK?". The asker announces the
//! request (an event the UI turns into a dialog) and waits; the UI answers
//! through a Tauri command. No answer within the timeout counts as "no".

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
pub struct PendingConfirms {
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, Sender<bool>>>,
}

impl PendingConfirms {
    /// Register a request, hand its id to `announce` (which tells the UI), and
    /// block until it's answered or `timeout` passes. `Some(answer)`, or `None`
    /// on timeout. Call from a blocking context.
    pub fn ask(&self, timeout: Duration, announce: impl FnOnce(u64)) -> Option<bool> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        announce(id);
        let answer = rx.recv_timeout(timeout).ok();
        self.waiting.lock().unwrap().remove(&id);
        answer
    }

    /// Deliver the user's answer. False when the request is no longer pending
    /// (already answered or timed out), so a late click changes nothing.
    pub fn answer(&self, id: u64, approve: bool) -> bool {
        match self.waiting.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(approve).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PendingConfirms;
    use std::sync::Arc;
    use std::time::Duration;

    /// Ask on a worker thread and answer from this one, like the API handler
    /// (blocking pool) and the UI command do.
    fn ask_and_answer(approve: bool) -> Option<bool> {
        let pc = Arc::new(PendingConfirms::default());
        let (id_tx, id_rx) = std::sync::mpsc::channel();
        let asker = {
            let pc = pc.clone();
            std::thread::spawn(move || pc.ask(Duration::from_secs(10), |id| id_tx.send(id).unwrap()))
        };
        let id = id_rx.recv().unwrap();
        assert!(pc.answer(id, approve));
        asker.join().unwrap()
    }

    #[test]
    fn approve_and_decline_reach_the_asker() {
        assert_eq!(ask_and_answer(true), Some(true));
        assert_eq!(ask_and_answer(false), Some(false));
    }

    #[test]
    fn no_answer_times_out_and_a_late_answer_is_ignored() {
        let pc = PendingConfirms::default();
        let mut asked = 0;
        assert_eq!(pc.ask(Duration::from_millis(20), |id| asked = id), None);
        assert!(!pc.answer(asked, true), "a late approval must not count");
    }

    #[test]
    fn unknown_request_is_not_answered() {
        assert!(!PendingConfirms::default().answer(42, true));
    }
}
