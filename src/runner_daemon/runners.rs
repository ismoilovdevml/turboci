//! The runners of one config file, run by one process (gitlab-runner's
//! `[[runners]]`). Each runner has its own GitLab connection, executor,
//! state, workspaces, cache and counters; they share `concurrent`, the check
//! interval, the admission limits, the metrics endpoint and the shutdown
//! signal.

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{error, info, warn, Instrument};

use super::{metrics, AbortOnDrop, RunnerDaemon, ShutdownHandle};

/// Runs the runners of a config file
#[derive(Clone)]
pub struct Manager {
    /// Fixed for the life of the process: adding or removing a runner needs
    /// a restart
    pub(super) runners: Arc<Vec<RunnerDaemon>>,
}

impl Manager {
    /// Run `runners` together; they take the shared settings and state of the
    /// first one. Panics without any runner.
    pub fn new(mut runners: Vec<RunnerDaemon>) -> Self {
        let first = runners[0].clone();
        for runner in runners.iter_mut().skip(1) {
            runner.share_from(&first);
        }
        Self {
            runners: Arc::new(runners),
        }
    }

    fn first(&self) -> &RunnerDaemon {
        &self.runners[0]
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.first().shutdown_handle()
    }

    /// Run every runner until a shutdown, then wait for their jobs to end
    pub async fn start(&self) -> Result<()> {
        let first = self.first();
        info!("🚀 TurboCI Runner Daemon starting...");
        info!("   Concurrent jobs: {}", first.config.concurrent);
        info!("   Check interval: {}s", first.config.check_interval);
        let several = self.runners.len() > 1;
        if several {
            let names: Vec<&str> = self
                .runners
                .iter()
                .map(|r| r.config.name.as_str())
                .collect();
            info!("   Runners: {}", names.join(", "));
        }
        let limits = first.current_limits();
        if !limits.is_off() {
            info!("   Admission limits: {}", limits.describe());
        }
        for key in limits.unsupported() {
            warn!(
                "{} is only checked on Linux: it is ignored on this host",
                key
            );
        }
        // Bound before anything else, so a taken port stops the runner at once
        let _metrics = match first.config.metrics_addr()? {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("metrics_listen: cannot listen on {}", addr))?;
                info!("   Metrics: http://{}/metrics", addr);
                let manager = self.clone();
                Some(AbortOnDrop(tokio::spawn(metrics::serve(
                    listener,
                    Arc::new(move || manager.render_metrics()),
                ))))
            }
            None => None,
        };

        let mut running = tokio::task::JoinSet::new();
        for runner in self.runners.iter().cloned() {
            // With several runners, each one's log lines carry its name
            let span = if several {
                tracing::info_span!("runner", name = %runner.config.name)
            } else {
                tracing::Span::none()
            };
            running.spawn(async move { runner.run().await }.instrument(span));
        }
        while let Some(ended) = running.join_next().await {
            if let Err(e) = ended {
                error!("A runner stopped unexpectedly: {}", e);
            }
        }
        info!("✅ Runner stopped");
        Ok(())
    }

    /// The metrics as served on `metrics_listen`
    pub(super) fn render_metrics(&self) -> String {
        let gauges: Vec<metrics::Gauges> = self.runners.iter().map(|r| r.gauges()).collect();
        let runners: Vec<metrics::Runner> = self
            .runners
            .iter()
            .zip(gauges)
            .map(|(runner, gauges)| metrics::Runner {
                name: &runner.config.name,
                metrics: &runner.metrics,
                gauges,
            })
            .collect();
        let first = self.first();
        metrics::render(
            &first.process,
            first.capacity.shared().limit().into(),
            &runners,
        )
    }
}
