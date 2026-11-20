//! Job slots: how many jobs run, and how many may (`concurrent`). A config
//! reload changes the limit while jobs run; lowering it never stops a running
//! job, the runner just takes no new one until enough have ended.

use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy)]
struct State {
    running: u32,
    limit: u32,
}

#[derive(Debug)]
pub struct Capacity {
    state: watch::Sender<State>,
}

/// A taken job slot, given back when dropped
#[derive(Debug)]
pub struct Slot(Arc<Capacity>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0
            .state
            .send_modify(|state| state.running = state.running.saturating_sub(1));
    }
}

impl Capacity {
    pub fn new(limit: u32) -> Arc<Self> {
        Arc::new(Self {
            state: watch::Sender::new(State { running: 0, limit }),
        })
    }

    /// Wait for a free slot and take it. Cancel-safe: a slot is only taken
    /// when this returns.
    pub async fn acquire(self: &Arc<Self>) -> Slot {
        let mut changes = self.state.subscribe();
        loop {
            let taken = self.state.send_if_modified(|state| {
                let free = state.running < state.limit;
                if free {
                    state.running += 1;
                }
                free
            });
            if taken {
                return Slot(self.clone());
            }
            // `self` holds the sender, so this never fails
            let _ = changes
                .wait_for(|state| state.running < state.limit)
                .await
                .map(|_| ());
        }
    }

    /// Change the number of slots; returns the previous number
    pub fn set_limit(&self, limit: u32) -> u32 {
        let mut previous = limit;
        self.state.send_modify(|state| {
            previous = state.limit;
            state.limit = limit;
        });
        previous
    }

    pub fn limit(&self) -> u32 {
        self.state.borrow().limit
    }

    /// Jobs running now
    pub fn running(&self) -> u32 {
        self.state.borrow().running
    }

    /// Wait until no job runs
    pub async fn idle(&self) {
        let _ = self
            .state
            .subscribe()
            .wait_for(|state| state.running == 0)
            .await
            .map(|_| ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Whether a slot can be taken within a short wait
    async fn can_take(capacity: &Arc<Capacity>) -> Option<Slot> {
        tokio::time::timeout(Duration::from_millis(100), capacity.acquire())
            .await
            .ok()
    }

    #[tokio::test]
    async fn hands_out_at_most_the_limit() {
        let capacity = Capacity::new(2);
        let first = capacity.acquire().await;
        let _second = capacity.acquire().await;
        assert_eq!(capacity.running(), 2);
        assert!(can_take(&capacity).await.is_none(), "third slot handed out");

        drop(first);
        assert_eq!(capacity.running(), 1);
        assert!(can_take(&capacity).await.is_some());
        // Neither the timed-out wait nor the dropped slot holds one now
        assert_eq!(capacity.running(), 1);
    }

    #[tokio::test]
    async fn a_higher_limit_wakes_a_waiting_request() {
        let capacity = Capacity::new(1);
        let _running = capacity.acquire().await;
        let waiting = tokio::spawn({
            let capacity = capacity.clone();
            async move { capacity.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());

        assert_eq!(capacity.set_limit(2), 1);
        let _woken = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("woken by the new limit")
            .unwrap();
        assert_eq!(capacity.running(), 2);
    }

    #[tokio::test]
    async fn a_lower_limit_lets_running_jobs_finish() {
        let capacity = Capacity::new(3);
        let slots: Vec<Slot> = vec![
            capacity.acquire().await,
            capacity.acquire().await,
            capacity.acquire().await,
        ];

        assert_eq!(capacity.set_limit(1), 3);
        // Nothing is taken away from running jobs
        assert_eq!(capacity.running(), 3);
        assert_eq!(capacity.limit(), 1);

        let mut slots = slots.into_iter();
        drop(slots.next());
        assert!(can_take(&capacity).await.is_none(), "2 running, limit 1");
        drop(slots.next());
        assert!(can_take(&capacity).await.is_none(), "1 running, limit 1");
        drop(slots.next());
        let slot = can_take(&capacity).await.expect("free once all ended");
        assert_eq!(capacity.running(), 1);
        drop(slot);
    }

    #[tokio::test]
    async fn idle_waits_for_every_slot() {
        let capacity = Capacity::new(2);
        let slot = capacity.acquire().await;
        // A lower limit does not change what idle waits for
        capacity.set_limit(1);
        let idle = tokio::spawn({
            let capacity = capacity.clone();
            async move { capacity.idle().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!idle.is_finished());

        drop(slot);
        tokio::time::timeout(Duration::from_secs(5), idle)
            .await
            .expect("idle once the job ended")
            .unwrap();
    }
}
