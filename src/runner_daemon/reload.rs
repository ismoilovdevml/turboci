//! Config reload on SIGHUP: `concurrent`, `check_interval` and the admission
//! limits change while the runner runs; any other change is reported as
//! needing a restart. An invalid config changes nothing.

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

use super::config::RunnerConfig;
use super::{admission, metrics, RunnerDaemon};

/// Keys a reload applies; changes to any other key need a restart
const LIVE_KEYS: [&str; 5] = [
    "concurrent",
    "check_interval",
    "min_free_memory",
    "min_free_disk",
    "max_load_avg",
];

/// What a reload did
#[derive(Debug, Default, PartialEq)]
pub struct Reloaded {
    /// Settings now in effect, as "key old → new"
    pub applied: Vec<String>,
    /// Keys whose new value only takes effect after a restart
    pub restart_needed: Vec<String>,
}

/// Reloads a daemon's config file (see `RunnerDaemon::reloader`)
#[derive(Clone)]
pub struct Reloader {
    daemon: RunnerDaemon,
    path: PathBuf,
}

impl Reloader {
    /// Reload and log the outcome; on error the running config stays
    pub fn reload(&self) {
        match self.daemon.reload(&self.path) {
            Ok(reloaded) => {
                self.daemon
                    .metrics
                    .config_reload(metrics::ConfigReload::Success);
                if reloaded.applied.is_empty() {
                    info!("🔄 Config reloaded: no change to the live settings");
                } else {
                    info!("🔄 Config reloaded: {}", reloaded.applied.join(", "));
                }
                if !reloaded.restart_needed.is_empty() {
                    warn!(
                        "Config changes to {} take effect after a restart",
                        reloaded.restart_needed.join(", ")
                    );
                }
            }
            Err(e) => {
                self.daemon
                    .metrics
                    .config_reload(metrics::ConfigReload::Failure);
                error!(
                    "❌ Config reload failed, the runner keeps its config: {:#}",
                    e
                );
            }
        }
    }
}

impl RunnerDaemon {
    /// A reloader of the config file at `path`, the one the daemon was started with
    pub fn reloader(&self, path: impl Into<PathBuf>) -> Reloader {
        Reloader {
            daemon: self.clone(),
            path: path.into(),
        }
    }

    /// Re-read the config at `path` and apply its live settings. Nothing is
    /// applied unless the whole file is valid.
    pub fn reload(&self, path: &Path) -> Result<Reloaded> {
        let new = RunnerConfig::load(path)?;
        let limits = admission::Limits::from_config(&new)?;

        let mut applied = Vec::new();
        let concurrent = self.capacity.set_limit(new.concurrent);
        if concurrent != new.concurrent {
            applied.push(format!("concurrent {} → {}", concurrent, new.concurrent));
            let running = self.capacity.running();
            if running > new.concurrent {
                info!(
                    "   {} jobs are running: they finish, and new jobs start once fewer than {} run",
                    running, new.concurrent
                );
            }
        }
        let interval = self
            .check_interval
            .swap(new.check_interval, std::sync::atomic::Ordering::Relaxed);
        if interval != new.check_interval {
            applied.push(format!(
                "check_interval {}s → {}s",
                interval, new.check_interval
            ));
        }
        for key in limits.unsupported() {
            warn!(
                "{} is only checked on Linux: it is ignored on this host",
                key
            );
        }
        let previous = match self.limits.write() {
            Ok(mut current) => std::mem::replace(&mut *current, limits.clone()),
            Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), limits.clone()),
        };
        if previous != limits {
            applied.push(format!(
                "admission limits {} → {}",
                previous.describe(),
                limits.describe()
            ));
        }

        Ok(Reloaded {
            applied,
            restart_needed: changed_keys(&self.config, &new),
        })
    }
}

/// Dotted keys whose values differ between `old` and `new`, live keys aside
fn changed_keys(old: &RunnerConfig, new: &RunnerConfig) -> Vec<String> {
    fn walk(old: &toml::Table, new: &toml::Table, prefix: &str, out: &mut Vec<String>) {
        let keys: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        for key in keys {
            let path = format!("{}{}", prefix, key);
            match (old.get(key), new.get(key)) {
                (Some(toml::Value::Table(old)), Some(toml::Value::Table(new))) => {
                    walk(old, new, &format!("{}.", path), out)
                }
                (old, new) if old != new => out.push(path),
                _ => {}
            }
        }
    }

    let (Ok(mut old), Ok(mut new)) = (toml::Table::try_from(old), toml::Table::try_from(new))
    else {
        return vec!["(unknown: the config could not be compared)".to_string()];
    };
    for key in LIVE_KEYS {
        old.remove(key);
        new.remove(key);
    }
    let mut out = Vec::new();
    walk(&old, &new, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> RunnerConfig {
        RunnerConfig::parse(content).unwrap()
    }

    #[test]
    fn finds_changes_that_need_a_restart() {
        let old = parse("runner_token = \"t\"\nconcurrent = 4\n[executor.docker]\nmemory = \"1g\"");

        // Live keys only: nothing needs a restart
        let live = parse(
            "runner_token = \"t\"\nconcurrent = 1\ncheck_interval = 9\nmin_free_disk = \"1g\"\nmax_load_avg = 3.0\n[executor.docker]\nmemory = \"1g\"",
        );
        assert!(changed_keys(&old, &live).is_empty());

        let other = parse(
            "runner_token = \"t2\"\ncache_dir = \"/srv/cache\"\nmetrics_listen = \"127.0.0.1:9252\"\n[executor.docker]\nmemory = \"2g\"\nprivileged = true",
        );
        assert_eq!(
            changed_keys(&old, &other),
            [
                "cache_dir",
                "executor.docker.memory",
                "executor.docker.privileged",
                "metrics_listen",
                "runner_token",
            ]
        );
        // A key that was set and is now removed counts too
        assert_eq!(changed_keys(&other, &old).len(), 5);
    }
}
