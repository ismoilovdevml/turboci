//! Config reload on SIGHUP. `concurrent`, `check_interval` and the admission
//! limits change at once; settings that only matter to a job (environment,
//! hooks, Docker settings, cache limits, ...) apply to the jobs that start
//! after the reload, while running jobs keep the settings they started with.
//! Settings the runner only reads at start are reported as needing a
//! restart (see `pin_restart_only`). An invalid config changes nothing.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{error, info, warn};

use super::config::{ConfigFile, RunnerConfig, GLOBAL_KEYS};
use super::runners::Manager;
use super::{admission, local_cache, metrics, runner_limit, RunnerDaemon, Settings};

/// What a reload did
#[derive(Debug, Default, PartialEq)]
pub struct Reloaded {
    /// Settings now in effect, as "key old → new" or, for those only new
    /// jobs see, as "for new jobs: key, key"
    pub applied: Vec<String>,
    /// Keys whose new value only takes effect after a restart
    pub restart_needed: Vec<String>,
}

/// Reloads the config file of a manager's runners (see `Manager::reloader`)
#[derive(Clone)]
pub struct Reloader {
    manager: Manager,
    path: PathBuf,
}

impl Reloader {
    /// Reload and log the outcome; on error the running config stays
    pub fn reload(&self) {
        let process = &self.manager.runners[0].process;
        match self.manager.reload(&self.path) {
            Ok(reloaded) => {
                process.config_reload(metrics::ConfigReload::Success);
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
                process.config_reload(metrics::ConfigReload::Failure);
                error!(
                    "❌ Config reload failed, the runner keeps its config: {:#}",
                    e
                );
            }
        }
    }
}

impl Manager {
    /// A reloader of the config file at `path`, the one the runners were started with
    pub fn reloader(&self, path: impl Into<PathBuf>) -> Reloader {
        Reloader {
            manager: self.clone(),
            path: path.into(),
        }
    }

    /// Re-read the config at `path` and apply its live settings. Nothing is
    /// applied unless the whole file is valid. Runners are matched by name;
    /// adding or removing one needs a restart.
    pub fn reload(&self, path: &Path) -> Result<Reloaded> {
        let file = ConfigFile::load(path)?;
        let limits = admission::Limits::from_config(file.global())?;
        let mut reloaded = self.runners[0].reload_global(file.global(), limits);
        // Per runner reports name the runner once there are several
        let named = file.multi || self.runners.len() > 1;
        let prefix = |name: &str, text: String| {
            if named {
                format!("{}: {}", name, text)
            } else {
                text
            }
        };
        for runner in self.runners.iter() {
            let name = &runner.config.name;
            match file.runners.iter().find(|new| &new.name == name) {
                Some(new) => {
                    let done = runner.reload_runner(new);
                    reloaded
                        .applied
                        .extend(done.applied.into_iter().map(|text| prefix(name, text)));
                    reloaded
                        .restart_needed
                        .extend(done.restart_needed.into_iter().map(|key| prefix(name, key)));
                }
                None => reloaded
                    .restart_needed
                    .push(format!("[[runners]] {} (removed)", name)),
            }
        }
        for new in &file.runners {
            if !self.runners.iter().any(|r| r.config.name == new.name) {
                reloaded
                    .restart_needed
                    .push(format!("[[runners]] {} (added)", new.name));
            }
        }
        Ok(reloaded)
    }
}

impl RunnerDaemon {
    /// A reloader of the config file at `path`, for this runner alone
    #[cfg(test)]
    pub fn reloader(&self, path: impl Into<PathBuf>) -> Reloader {
        Manager::new(vec![self.clone()]).reloader(path)
    }

    /// Reload the config at `path` for this runner alone (see `Manager::reload`)
    #[cfg(test)]
    pub fn reload(&self, path: &Path) -> Result<Reloaded> {
        Manager::new(vec![self.clone()]).reload(path)
    }

    /// Apply the settings of the whole process (`GLOBAL_KEYS`) from `new`
    fn reload_global(&self, new: &RunnerConfig, limits: admission::Limits) -> Reloaded {
        let mut applied = Vec::new();
        let shared = self.capacity.shared();
        let concurrent = shared.set_limit(new.concurrent);
        if concurrent != new.concurrent {
            applied.push(format!("concurrent {} → {}", concurrent, new.concurrent));
            let running = shared.running();
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
        let restart_needed = if self.config.metrics_listen != new.metrics_listen {
            vec!["metrics_listen".to_string()]
        } else {
            Vec::new()
        };
        Reloaded {
            applied,
            restart_needed,
        }
    }

    /// Give the jobs that start from now on this runner's settings from
    /// `new`; keys only read at start are reported instead
    fn reload_runner(&self, new: &RunnerConfig) -> Reloaded {
        let current = self.settings();
        let mut applied = Vec::new();
        if current.config.limit != new.limit {
            self.capacity.set_limit(runner_limit(new.limit));
            applied.push(format!("limit {} → {}", current.config.limit, new.limit));
        }
        let live = pin_restart_only(&current.config, new);
        let changed = changed_keys(&current.config, &live);
        let restart_needed = changed_keys(&live, new);
        if changed.is_empty() {
            if !applied.is_empty() {
                // Keep the limit in the settings, which later reloads compare to
                self.update_settings(|settings| settings.config = Arc::new(live));
            }
            return Reloaded {
                applied,
                restart_needed,
            };
        }
        let mut cache = local_cache(&live);
        if let Some(bucket) = &self.remote_cache {
            cache = cache.with_remote(bucket.clone());
        }
        let settings = Settings {
            executor: Arc::new(current.executor.reconfigured(&live.executor)),
            config: Arc::new(live),
            cache,
        };
        self.update_settings(|current| *current = settings);
        applied.push(format!("for new jobs: {}", changed.join(", ")));
        Reloaded {
            applied,
            restart_needed,
        }
    }
}

/// `new` with the settings the runner only reads at start taken from `old`:
/// the identity and connection to GitLab, where it keeps state, workspaces and
/// the cache, and the executor kind. Everything else is read per job (from the
/// settings it starts with) or per sweep, so it is safe to swap. A new setting
/// read only at start must be added here.
fn pin_restart_only(old: &RunnerConfig, new: &RunnerConfig) -> RunnerConfig {
    let mut live = new.clone();
    live.name = old.name.clone();
    live.runner_token = old.runner_token.clone();
    live.gitlab_url = old.gitlab_url.clone();
    live.tls_ca_file = old.tls_ca_file.clone();
    live.proxy = old.proxy.clone();
    live.no_proxy = old.no_proxy.clone();
    live.state_dir = old.state_dir.clone();
    live.metrics_listen = old.metrics_listen.clone();
    // The S3 layer is set up at start, and only while the cache is on
    live.cache_enabled = old.cache_enabled;
    live.cache_dir = old.cache_dir.clone();
    live.cache_s3 = old.cache_s3.clone();
    live.executor.executor_type = old.executor.executor_type.clone();
    live.executor.docker.builds_dir = old.executor.docker.builds_dir.clone();
    live.executor.shell.work_dir = old.executor.shell.work_dir.clone();
    live
}

/// Dotted keys whose values differ between `old` and `new`, apart from the
/// settings of the whole process (`GLOBAL_KEYS`) and `limit`, applied on
/// their own
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
    // `limit` is applied on its own too
    for key in GLOBAL_KEYS.iter().chain(&["limit"]) {
        old.remove(*key);
        new.remove(*key);
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
    fn finds_changed_keys_apart_from_the_global_ones() {
        let old = parse("runner_token = \"t\"\nconcurrent = 4\n[executor.docker]\nmemory = \"1g\"");

        // Global keys only: nothing to report here
        let global = parse(
            "runner_token = \"t\"\nconcurrent = 1\ncheck_interval = 9\nmin_free_disk = \"1g\"\nmax_load_avg = 3.0\nmetrics_listen = \"127.0.0.1:9252\"\n[executor.docker]\nmemory = \"1g\"",
        );
        assert!(changed_keys(&old, &global).is_empty());

        let other = parse(
            "runner_token = \"t2\"\ncache_dir = \"/srv/cache\"\n[executor.docker]\nmemory = \"2g\"\nprivileged = true",
        );
        assert_eq!(
            changed_keys(&old, &other),
            [
                "cache_dir",
                "executor.docker.memory",
                "executor.docker.privileged",
                "runner_token",
            ]
        );
        // A key that was set and is now removed counts too
        assert_eq!(changed_keys(&other, &old).len(), 4);
    }

    #[test]
    fn settings_read_only_at_start_are_pinned() {
        let old = parse("runner_token = \"t\"\nstate_dir = \"/srv/a\"\n[executor]\nexecutor_type = \"shell\"\n[executor.shell]\nwork_dir = \"/srv/a/b\"");
        let new = parse(
            "runner_token = \"t2\"\ngitlab_url = \"https://other\"\nstate_dir = \"/srv/b\"\nproxy = \"http://p:3128\"\ncache_enabled = false\ncache_dir = \"/c\"\nenvironment = [\"A=1\"]\npre_build_script = \"echo\"\ncache_max_age_days = 1\ncache_max_size = \"1g\"\n[executor]\nexecutor_type = \"docker\"\nworkspace_max_age_days = 0\nworkspace_max_size = \"5g\"\n[executor.docker]\nbuilds_dir = \"/b\"\nmemory = \"1g\"\nvolumes = [\"/x\"]\npull_policy = \"never\"\n[executor.shell]\nwork_dir = \"/w\"",
        );

        let live = pin_restart_only(&old, &new);

        assert_eq!(
            changed_keys(&old, &live),
            [
                "cache_max_age_days",
                "cache_max_size",
                "environment",
                "executor.docker.memory",
                "executor.docker.pull_policy",
                "executor.docker.volumes",
                "executor.workspace_max_age_days",
                "executor.workspace_max_size",
                "pre_build_script",
            ]
        );
        assert_eq!(
            changed_keys(&live, &new),
            [
                "cache_dir",
                "cache_enabled",
                "executor.docker.builds_dir",
                "executor.executor_type",
                "executor.shell.work_dir",
                "gitlab_url",
                "proxy",
                "runner_token",
                "state_dir",
            ]
        );
    }
}
