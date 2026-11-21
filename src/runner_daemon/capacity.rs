//! Job slots: how many jobs run, and how many may (`concurrent`). A config
//! reload changes the limit while jobs run; lowering it never stops a running
//! job, the runner just takes no new one until enough have ended.
//!
//! With several runners each has its own slots (its `limit`) within the
//! process-wide ones (`concurrent`), as in gitlab-runner: a job needs a slot
//! of both.

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
    /// Slots shared with other runners, of which a job needs one as well
    parent: Option<Arc<Capacity>>,
}

/// A taken job slot, given back when dropped
#[derive(Debug)]
pub struct Slot {
    capacity: Arc<Capacity>,
    /// Holds a slot of the capacity's parent too
    with_parent: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.capacity.release();
        if let (true, Some(parent)) = (self.with_parent, &self.capacity.parent) {
            parent.release();
        }
    }
}

impl Capacity {
    pub fn new(limit: u32) -> Arc<Self> {
        Arc::new(Self {
            state: watch::Sender::new(State { running: 0, limit }),
            parent: None,
        })
    }

    /// `limit` slots of which each job also needs one of `parent`'s
    pub fn within(limit: u32, parent: Arc<Capacity>) -> Arc<Self> {
        Arc::new(Self {
            state: watch::Sender::new(State { running: 0, limit }),
            parent: Some(parent),
        })
    }

    /// The slots shared with other runners; itself when there are none
    pub fn shared(self: &Arc<Self>) -> Arc<Capacity> {
        self.parent.clone().unwrap_or_else(|| self.clone())
    }

    fn try_take(&self) -> bool {
        self.state.send_if_modified(|state| {
            let free = state.running < state.limit;
            if free {
                state.running += 1;
            }
            free
        })
    }

    fn release(&self) {
        self.state
            .send_modify(|state| state.running = state.running.saturating_sub(1));
    }

    /// Wait until a slot is free (without taking it)
    async fn free(&self) {
        // `self` holds the sender, so this never fails
        let _ = self
            .state
            .subscribe()
            .wait_for(|state| state.running < state.limit)
            .await
            .map(|_| ());
    }

    /// Wait for a free slot (and one of the parent's) and take it.
    /// Cancel-safe: a slot is only taken when this returns.
    pub async fn acquire(self: &Arc<Self>) -> Slot {
        loop {
            if !self.try_take() {
                self.free().await;
                continue;
            }
            let Some(parent) = &self.parent else {
                return Slot {
                    capacity: self.clone(),
                    with_parent: false,
                };
            };
            if parent.try_take() {
                return Slot {
                    capacity: self.clone(),
                    with_parent: true,
                };
            }
            // Not held while waiting for the shared slots
            self.release();
            parent.free().await;
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
    async fn runners_get_slots_within_their_limit_and_the_shared_one() {
        let shared = Capacity::new(3);
        let a = Capacity::within(2, shared.clone());
        let b = Capacity::within(u32::MAX, shared.clone());
        assert!(Arc::ptr_eq(&a.shared(), &shared));
        assert!(Arc::ptr_eq(&shared.shared(), &shared));

        let a1 = a.acquire().await;
        let _a2 = a.acquire().await;
        assert!(can_take(&a).await.is_none(), "runner limit 2");
        let b1 = b.acquire().await;
        assert_eq!((a.running(), b.running(), shared.running()), (2, 1, 3));
        // concurrent = 3 is reached: no runner gets a slot, and a waiting
        // runner holds none of its own meanwhile
        assert!(can_take(&b).await.is_none(), "concurrent 3");
        assert_eq!(b.running(), 1);

        let waiting = tokio::spawn({
            let b = b.clone();
            async move { b.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        drop(a1);
        let b2 = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("woken by a freed shared slot")
            .unwrap();
        assert_eq!((a.running(), b.running(), shared.running()), (1, 2, 3));

        // A higher concurrent lets a waiting runner in
        let waiting = tokio::spawn({
            let b = b.clone();
            async move { b.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        shared.set_limit(4);
        let b3 = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("woken by the new concurrent")
            .unwrap();
        drop((b1, b2, b3));
        assert_eq!((b.running(), shared.running()), (0, 1));
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
