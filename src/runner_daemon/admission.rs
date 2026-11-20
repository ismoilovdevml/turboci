//! Admission control: before the runner asks GitLab for a job, it checks that
//! the host has the memory, disk space and CPU headroom the config asks for
//! (`min_free_memory`, `min_free_disk`, `max_load_avg`). While a limit is
//! violated no job is requested; running jobs are never touched.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::config::RunnerConfig;

/// Admission limits from the config; each one is optional
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    pub min_free_memory: Option<u64>,
    pub min_free_disk: Option<u64>,
    pub max_load_avg: Option<f64>,
}

impl Limits {
    /// The limits of a config (checked by `RunnerConfig::validate`)
    pub fn from_config(config: &RunnerConfig) -> Result<Self> {
        Ok(Self {
            min_free_memory: config.min_free_memory_bytes()?,
            min_free_disk: config.min_free_disk_bytes()?,
            max_load_avg: config.max_load_avg,
        })
    }

    /// No limit is set: every job request goes ahead without a check
    pub fn is_off(&self) -> bool {
        self.min_free_memory.is_none()
            && self.min_free_disk.is_none()
            && self.max_load_avg.is_none()
    }

    /// Config keys of limits this platform cannot check (they are ignored)
    pub fn unsupported(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if !cfg!(target_os = "linux") {
            if self.min_free_memory.is_some() {
                keys.push("min_free_memory");
            }
            if self.max_load_avg.is_some() {
                keys.push("max_load_avg");
            }
        }
        keys
    }

    /// The limits in effect, for the log
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(bytes) = self.min_free_memory {
            parts.push(format!("min_free_memory {}", human(bytes)));
        }
        if let Some(bytes) = self.min_free_disk {
            parts.push(format!("min_free_disk {}", human(bytes)));
        }
        if let Some(load) = self.max_load_avg {
            parts.push(format!("max_load_avg {}", load));
        }
        if parts.is_empty() {
            "off".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// What a limit is about
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    Memory,
    Disk,
    Load,
}

impl Resource {
    pub const ALL: [Resource; 3] = [Resource::Memory, Resource::Disk, Resource::Load];

    /// Label value in metrics
    pub fn name(self) -> &'static str {
        match self {
            Resource::Memory => "memory",
            Resource::Disk => "disk",
            Resource::Load => "load",
        }
    }
}

/// A limit the host does not meet right now
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub resource: Resource,
    pub message: String,
}

/// Reads the host's resources; `None` when a value cannot be read, and the
/// limit on it is then not enforced
pub trait Probe: Send + Sync {
    /// Memory available for new work, in bytes
    fn available_memory(&self) -> Option<u64>;
    /// 1-minute load average
    fn load_average(&self) -> Option<f64>;
    /// Space unprivileged users can still write on the file system of `path`
    fn available_disk(&self, path: &Path) -> Option<u64>;
}

/// The runner's host. Memory and load come from /proc (Linux only); disk
/// space from statvfs (every platform)
pub struct HostProbe;

impl Probe for HostProbe {
    fn available_memory(&self) -> Option<u64> {
        if cfg!(target_os = "linux") {
            parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
        } else {
            None
        }
    }

    fn load_average(&self) -> Option<f64> {
        if cfg!(target_os = "linux") {
            parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
        } else {
            None
        }
    }

    fn available_disk(&self, path: &Path) -> Option<u64> {
        // A directory not created yet will be on its closest existing
        // parent's file system
        let existing = path.ancestors().find(|dir| dir.exists())?;
        let stat = rustix::fs::statvfs(existing).ok()?;
        Some(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
}

/// `MemAvailable` of /proc/meminfo in bytes: what the kernel can hand out
/// without swapping, page cache it can drop included
fn parse_meminfo(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    let mut fields = line.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    let multiplier = match fields.next() {
        Some("kB") => 1024,
        None => 1,
        Some(_) => return None,
    };
    value.checked_mul(multiplier)
}

/// The 1-minute load average, the first field of /proc/loadavg
fn parse_loadavg(loadavg: &str) -> Option<f64> {
    loadavg
        .split_whitespace()
        .next()?
        .parse()
        .ok()
        .filter(|load: &f64| load.is_finite())
}

/// The limits the host violates now; `dirs` are the directories jobs write
/// to (builds and cache)
pub fn check(limits: &Limits, probe: &dyn Probe, dirs: &[PathBuf]) -> Vec<Violation> {
    let mut violations = Vec::new();
    if let Some(min) = limits.min_free_memory {
        if let Some(available) = probe.available_memory().filter(|bytes| *bytes < min) {
            violations.push(Violation {
                resource: Resource::Memory,
                message: format!(
                    "available memory {} is below min_free_memory {}",
                    human(available),
                    human(min)
                ),
            });
        }
    }
    if let Some(min) = limits.min_free_disk {
        for dir in dirs {
            if let Some(free) = probe.available_disk(dir).filter(|bytes| *bytes < min) {
                violations.push(Violation {
                    resource: Resource::Disk,
                    message: format!(
                        "free disk space for {} {} is below min_free_disk {}",
                        dir.display(),
                        human(free),
                        human(min)
                    ),
                });
            }
        }
    }
    if let Some(max) = limits.max_load_avg {
        if let Some(load) = probe.load_average().filter(|load| *load > max) {
            violations.push(Violation {
                resource: Resource::Load,
                message: format!("load average {:.2} is above max_load_avg {}", load, max),
            });
        }
    }
    violations
}

/// A change in admission worth a log line
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// No jobs are taken (anew, or for other resources than before)
    Blocked(String),
    /// Jobs are taken again
    Admitted,
}

/// What blocked the last check, so the log shows changes instead of a line
/// every poll
#[derive(Debug, Default)]
pub struct Gate {
    blocked: Vec<Resource>,
}

impl Gate {
    /// Record a check; a transition when the set of violated resources changed
    pub fn update(&mut self, violations: &[Violation]) -> Option<Transition> {
        let mut blocked: Vec<Resource> = violations.iter().map(|v| v.resource).collect();
        blocked.sort();
        blocked.dedup();
        if blocked == self.blocked {
            return None;
        }
        self.blocked = blocked;
        Some(if violations.is_empty() {
            Transition::Admitted
        } else {
            Transition::Blocked(
                violations
                    .iter()
                    .map(|v| v.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })
    }

    pub fn is_blocked(&self, resource: Resource) -> bool {
        self.blocked.contains(&resource)
    }
}

/// Bytes for people: "1.5 GB", "512.0 MB" (1 GB = 1024 MB, as in the config)
pub fn human(bytes: u64) -> String {
    const MB: f64 = 1_048_576.0;
    let bytes = bytes as f64;
    if bytes >= 1024.0 * MB {
        format!("{:.1} GB", bytes / (1024.0 * MB))
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

/// A probe that reports what a test sets
#[cfg(test)]
#[derive(Default)]
pub struct FixedProbe {
    values: std::sync::Mutex<(Option<u64>, Option<f64>, Option<u64>)>,
}

#[cfg(test)]
impl FixedProbe {
    pub fn set(&self, memory: Option<u64>, load: Option<f64>, disk: Option<u64>) {
        *self.values.lock().unwrap() = (memory, load, disk);
    }
}

#[cfg(test)]
impl Probe for FixedProbe {
    fn available_memory(&self) -> Option<u64> {
        self.values.lock().unwrap().0
    }

    fn load_average(&self) -> Option<f64> {
        self.values.lock().unwrap().1
    }

    fn available_disk(&self, _path: &Path) -> Option<u64> {
        self.values.lock().unwrap().2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn limits() -> Limits {
        Limits {
            min_free_memory: Some(2 * GB),
            min_free_disk: Some(10 * GB),
            max_load_avg: Some(8.0),
        }
    }

    fn probe(memory: Option<u64>, load: Option<f64>, disk: Option<u64>) -> FixedProbe {
        let probe = FixedProbe::default();
        probe.set(memory, load, disk);
        probe
    }

    fn resources(violations: &[Violation]) -> Vec<Resource> {
        violations.iter().map(|v| v.resource).collect()
    }

    #[test]
    fn reads_meminfo_and_loadavg() {
        let meminfo = "MemTotal:       16303428 kB\nMemFree:          508124 kB\nMemAvailable:    8151714 kB\nBuffers:          102400 kB\n";
        assert_eq!(parse_meminfo(meminfo), Some(8151714 * 1024));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: lots kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: 5 pages\n"), None);

        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/1024 12345\n"), Some(0.52));
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("nan 1 1"), None);
    }

    #[test]
    fn the_host_probe_reads_this_host() {
        let dir = tempfile::tempdir().unwrap();
        // A directory that does not exist yet counts on its parent's file system
        assert!(HostProbe
            .available_disk(&dir.path().join("not/yet"))
            .is_some());
        if cfg!(target_os = "linux") {
            assert!(HostProbe.available_memory().is_some_and(|bytes| bytes > 0));
            assert!(HostProbe.load_average().is_some());
        } else {
            assert_eq!(HostProbe.available_memory(), None);
            assert_eq!(HostProbe.load_average(), None);
        }
    }

    #[test]
    fn admits_when_every_limit_is_met() {
        let dirs = [PathBuf::from("/builds"), PathBuf::from("/cache")];
        let healthy = probe(Some(4 * GB), Some(1.5), Some(50 * GB));
        assert!(check(&limits(), &healthy, &dirs).is_empty());
        // Without limits nothing is checked, however bad the host looks
        let starved = probe(Some(0), Some(500.0), Some(0));
        assert!(check(&Limits::default(), &starved, &dirs).is_empty());
        assert!(Limits::default().is_off());
        assert!(!limits().is_off());
    }

    #[test]
    fn blocks_on_each_violated_limit() {
        let dirs = [PathBuf::from("/builds"), PathBuf::from("/cache")];

        let low_memory = check(&limits(), &probe(Some(GB), Some(1.0), Some(50 * GB)), &dirs);
        assert_eq!(resources(&low_memory), [Resource::Memory]);
        assert!(
            low_memory[0]
                .message
                .contains("1.0 GB is below min_free_memory 2.0 GB"),
            "{}",
            low_memory[0].message
        );

        // Each directory is checked: both are reported
        let low_disk = check(&limits(), &probe(Some(4 * GB), Some(1.0), Some(GB)), &dirs);
        assert_eq!(resources(&low_disk), [Resource::Disk, Resource::Disk]);
        assert!(low_disk[0].message.contains("/builds"));
        assert!(low_disk[1].message.contains("/cache"));

        let busy = check(
            &limits(),
            &probe(Some(4 * GB), Some(9.25), Some(50 * GB)),
            &dirs,
        );
        assert_eq!(resources(&busy), [Resource::Load]);
        assert!(busy[0].message.contains("9.25 is above max_load_avg 8"));

        let all = check(&limits(), &probe(Some(0), Some(99.0), Some(0)), &dirs[..1]);
        assert_eq!(
            resources(&all),
            [Resource::Memory, Resource::Disk, Resource::Load]
        );
    }

    #[test]
    fn values_that_cannot_be_read_do_not_block() {
        let dirs = [PathBuf::from("/builds")];
        assert!(check(&limits(), &probe(None, None, None), &dirs).is_empty());
        // Exactly at the limit is enough
        let at_limit = probe(Some(2 * GB), Some(8.0), Some(10 * GB));
        assert!(check(&limits(), &at_limit, &dirs).is_empty());
    }

    #[test]
    fn the_gate_reports_only_changes() {
        let violation = |resource, message: &str| Violation {
            resource,
            message: message.to_string(),
        };
        let mut gate = Gate::default();

        assert_eq!(gate.update(&[]), None, "admitted from the start");
        assert_eq!(
            gate.update(&[violation(Resource::Disk, "disk full")]),
            Some(Transition::Blocked("disk full".to_string()))
        );
        assert!(gate.is_blocked(Resource::Disk));
        assert!(!gate.is_blocked(Resource::Load));
        // Still blocked by the same resource, with other numbers: no new line
        assert_eq!(
            gate.update(&[violation(Resource::Disk, "disk still full")]),
            None
        );
        assert_eq!(
            gate.update(&[
                violation(Resource::Disk, "disk full"),
                violation(Resource::Load, "busy")
            ]),
            Some(Transition::Blocked("disk full; busy".to_string()))
        );
        assert_eq!(gate.update(&[]), Some(Transition::Admitted));
        assert_eq!(gate.update(&[]), None);
    }

    #[test]
    fn describes_and_flags_limits() {
        assert_eq!(Limits::default().describe(), "off");
        assert_eq!(
            limits().describe(),
            "min_free_memory 2.0 GB, min_free_disk 10.0 GB, max_load_avg 8"
        );
        let unsupported = limits().unsupported();
        if cfg!(target_os = "linux") {
            assert!(unsupported.is_empty());
        } else {
            assert_eq!(unsupported, ["min_free_memory", "max_load_avg"]);
        }
        assert_eq!(human(512 << 20), "512.0 MB");
    }
}
