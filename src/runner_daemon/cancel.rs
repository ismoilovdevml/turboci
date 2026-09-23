//! Per-job cancellation. The state only escalates (running → canceling →
//! aborted) and can come from GitLab (Job-Status header, 403) or from a runner
//! shutdown.

use tokio::sync::watch;

use crate::gitlab::RemoteState;

#[derive(Clone)]
pub struct CancelSignal {
    tx: std::sync::Arc<watch::Sender<RemoteState>>,
}

impl Default for CancelSignal {
    fn default() -> Self {
        Self {
            tx: std::sync::Arc::new(watch::Sender::new(RemoteState::Running)),
        }
    }
}

impl CancelSignal {
    /// Record a newer state; a weaker state never overrides a stronger one
    pub fn update(&self, state: RemoteState) {
        self.tx.send_if_modified(|current| {
            let escalates = state > *current;
            if escalates {
                *current = state;
            }
            escalates
        });
    }

    pub fn state(&self) -> RemoteState {
        *self.tx.borrow()
    }

    /// Resolve once the state is at least `level`
    pub async fn reached(&self, level: RemoteState) {
        let mut rx = self.tx.subscribe();
        // The sender lives as long as `self`, so this cannot fail
        let _ = rx.wait_for(|state| *state >= level).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn escalates_only_and_wakes_waiters() {
        let cancel = CancelSignal::default();
        let waiter = {
            let cancel = cancel.clone();
            tokio::spawn(async move { cancel.reached(RemoteState::Canceling).await })
        };

        cancel.update(RemoteState::Canceling);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter woken")
            .unwrap();

        cancel.update(RemoteState::Aborted);
        cancel.update(RemoteState::Running);
        assert_eq!(cancel.state(), RemoteState::Aborted);
    }
}
