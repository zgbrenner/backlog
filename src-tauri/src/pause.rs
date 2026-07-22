//! Durable pause coordination for watcher-delivered work.
//!
//! A discovered file waits on this gate instead of returning early, so a pause
//! cannot consume the only filesystem event and strand the file until restart.

use tokio::sync::watch;

#[derive(Clone)]
pub struct PauseGate {
    sender: watch::Sender<bool>,
}

impl PauseGate {
    pub fn new(paused: bool) -> Self {
        let (sender, _receiver) = watch::channel(paused);
        Self { sender }
    }

    pub fn is_paused(&self) -> bool {
        *self.sender.borrow()
    }

    pub fn set_paused(&self, paused: bool) {
        self.sender.send_replace(paused);
    }

    pub async fn wait_until_resumed(&self) {
        let mut receiver = self.sender.subscribe();
        loop {
            if !*receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn discovered_work_waits_and_wakes_on_resume() {
        let gate = PauseGate::new(true);
        let waiter = gate.clone();
        let task = tokio::spawn(async move {
            waiter.wait_until_resumed().await;
            "delivered"
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!task.is_finished(), "paused work must remain queued");

        gate.set_paused(false);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("resume should wake the queued task")
            .expect("queued task should not panic");
        assert_eq!(result, "delivered");
    }

    #[tokio::test]
    async fn unpaused_gate_does_not_delay_work() {
        let gate = PauseGate::new(false);
        tokio::time::timeout(Duration::from_millis(50), gate.wait_until_resumed())
            .await
            .expect("unpaused work should pass immediately");
    }
}
