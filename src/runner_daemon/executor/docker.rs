//! Docker executor: one container per job, steps run as execs in it, sources
//! checked out by a helper container, services on the job's own network

use super::*;
use crate::gitlab::DockerOptions;

#[derive(Clone)]
pub struct DockerExecutor {
    docker: Arc<Docker>,
    config: DockerConfig,
    /// Custom CA of the GitLab server, given to jobs
    pub(super) ca_pem: Option<String>,
    /// Value of the `turboci.runner` label on every container and network this
    /// runner creates, so leftovers of a crash can be found and removed
    owner: String,
    /// Image pulls in progress, shared by the jobs of this runner
    pulls: Arc<Flights<PullOutcome>>,
    /// Workspaces found to belong wholly to their owner when their last job ended
    clean_workspaces: Arc<CleanWorkspaces>,
    /// Which images jobs use, for the image collection
    images: Arc<ImageUsage>,
}

/// A workspace directory as found on disk: a workspace that was removed
/// and created again, or given to another user, does not match its stamp
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceStamp {
    dev: u64,
    ino: u64,
    uid: u32,
    gid: u32,
}

impl WorkspaceStamp {
    /// Whether the files the runner itself creates belong to the
    /// workspace's owner (see `CleanWorkspaces`)
    #[cfg(unix)]
    fn owned_by_runner(self) -> bool {
        rustix::process::geteuid().as_raw() == self.uid
            && rustix::process::getegid().as_raw() == self.gid
    }

    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }
}

/// Workspaces in which everything belonged to the workspace's owner once
/// the last job in them ended and all its containers were gone. Between
/// jobs only the runner writes to a workspace, and only workspaces the
/// runner's own user and group own are marked, so its files belong to the
/// owner too; only a job's containers give files to others. The next job in
/// a marked workspace need not walk it again before its checkout (see
/// `sources_user`). A mark is used once: every job that starts containers
/// in a workspace has to earn it anew when it ends. Kept in memory only, so
/// a restarted runner checks every workspace once more.
#[derive(Default)]
struct CleanWorkspaces(std::sync::Mutex<std::collections::HashMap<PathBuf, WorkspaceStamp>>);

impl CleanWorkspaces {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<PathBuf, WorkspaceStamp>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mark(&self, dir: &Path, stamp: WorkspaceStamp) {
        self.lock().insert(dir.to_path_buf(), stamp);
    }

    fn forget(&self, dir: &Path) {
        self.lock().remove(dir);
    }

    /// Whether `dir`, as `stamp` shows it now, was marked clean; the mark is gone after
    fn take(&self, dir: &Path, stamp: WorkspaceStamp) -> bool {
        self.lock().remove(dir) == Some(stamp)
    }
}

/// When this runner's jobs last used each image and which images jobs are
/// using right now, for `collect_images` (dockerd records neither)
#[derive(Default)]
struct ImageUsage {
    /// Held by a job while it marks its images and by the collection around
    /// each removal, so no job pulls or starts an image the collection has
    /// just decided to remove
    removal: tokio::sync::Mutex<()>,
    state: std::sync::Mutex<UsageState>,
}

#[derive(Default)]
struct UsageState {
    /// Image (see `usage_key`) → when a job last used it
    last_used: std::collections::HashMap<String, std::time::SystemTime>,
    /// Image (see `usage_key`) → how many jobs use it now
    held: std::collections::HashMap<String, usize>,
    /// Image ID → when the collection first saw it
    first_seen: std::collections::HashMap<String, std::time::SystemTime>,
}

impl ImageUsage {
    fn state(&self) -> std::sync::MutexGuard<'_, UsageState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mark `images` as used by a job until the returned guard is dropped
    async fn hold(self: &Arc<Self>, images: &[&str]) -> HeldImages {
        let keys: Vec<String> = images.iter().map(|image| usage_key(image)).collect();
        let _removal = self.removal.lock().await;
        let now = std::time::SystemTime::now();
        let mut state = self.state();
        for key in &keys {
            *state.held.entry(key.clone()).or_default() += 1;
            state.last_used.insert(key.clone(), now);
        }
        HeldImages {
            usage: Arc::clone(self),
            keys,
        }
    }
}

/// Images a job uses; they count as used until it is dropped
struct HeldImages {
    usage: Arc<ImageUsage>,
    keys: Vec<String>,
}

impl Drop for HeldImages {
    fn drop(&mut self) {
        let now = std::time::SystemTime::now();
        let mut state = self.usage.state();
        for key in &self.keys {
            if let Some(count) = state.held.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.held.remove(key);
                }
            }
            state.last_used.insert(key.clone(), now);
        }
    }
}

/// An image reference as `GET /images/json` lists it: Docker Hub images
/// without `docker.io/` and `library/`, and with the default tag
fn usage_key(reference: &str) -> String {
    let mut name = reference;
    for prefix in ["docker.io/", "index.docker.io/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest;
            break;
        }
    }
    if image::registry(name) == "docker.io" {
        name = name.strip_prefix("library/").unwrap_or(name);
    }
    image::with_default_tag(name)
}

/// The real tags of a listed image (dockerd lists `<none>:<none>` for an
/// untagged one, or nothing)
fn image_tags(image: &docker_api::ImageSummary) -> Vec<String> {
    image
        .repo_tags
        .iter()
        .flatten()
        .filter(|tag| !tag.starts_with("<none>"))
        .cloned()
        .collect()
}

/// A listed image's tags and digests as `usage_key` gives them
fn image_references(image: &docker_api::ImageSummary) -> Vec<String> {
    image
        .repo_tags
        .iter()
        .flatten()
        .chain(image.repo_digests.iter().flatten())
        .filter(|reference| !reference.starts_with("<none>"))
        .map(|reference| usage_key(reference))
        .collect()
}

/// How a pull ended, as every job waiting for it sees it
type PullOutcome = std::result::Result<(), JobFailure>;

/// An image reference and the identity of the registry login it is pulled
/// with (see `login_identity`)
type PullKey = (String, String);

type Flight<T> = futures_util::future::Shared<futures_util::future::BoxFuture<'static, T>>;
type WeakFlight<T> = futures_util::future::WeakShared<futures_util::future::BoxFuture<'static, T>>;
type FlightMap<T> = std::collections::HashMap<PullKey, WeakFlight<T>>;

/// Work in progress by key, so concurrent callers with the same key wait for
/// one run of it and all get its output, errors included. The work runs while
/// at least one caller still waits for it: when the last one gives up (a
/// canceled job), it is dropped, and the next caller starts it anew. Entries
/// leave the map as soon as their work ended or was dropped.
struct Flights<T: Clone> {
    running: std::sync::Mutex<FlightMap<T>>,
}

impl<T: Clone> Default for Flights<T> {
    fn default() -> Self {
        Self {
            running: std::sync::Mutex::new(FlightMap::new()),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Flights<T> {
    /// Output of the work running under `key`, or of `start()` when none is
    async fn join(
        &self,
        key: PullKey,
        start: impl FnOnce() -> futures_util::future::BoxFuture<'static, T>,
    ) -> T {
        use futures_util::FutureExt;

        // Declared before `flight`, so it is dropped after it: by then this
        // caller no longer keeps the work alive
        let _forget = ForgetEnded(&self.running);
        let flight: Flight<T> = {
            let mut running = lock_flights(&self.running);
            match running.get(&key).and_then(|weak| weak.upgrade()) {
                Some(flight) => flight,
                None => {
                    let flight = start().shared();
                    // Not polled yet, so it cannot have ended
                    if let Some(weak) = flight.downgrade() {
                        running.insert(key, weak);
                    }
                    flight
                }
            }
        };
        flight.await
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock_flights(&self.running).len()
    }
}

/// The map of running work; a panic elsewhere cannot leave it inconsistent
fn lock_flights<T: Clone>(
    running: &std::sync::Mutex<FlightMap<T>>,
) -> std::sync::MutexGuard<'_, FlightMap<T>> {
    running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Removes the entries of work that ended or that nobody waits for any more
struct ForgetEnded<'a, T: Clone>(&'a std::sync::Mutex<FlightMap<T>>);

impl<T: Clone> Drop for ForgetEnded<'_, T> {
    fn drop(&mut self) {
        lock_flights(self.0).retain(|_, weak| weak.upgrade().is_some());
    }
}

/// Identifies a registry login without holding its secret: concurrent pulls
/// are only shared between jobs that use the same one, so no job gets an
/// image through another job's login
fn login_identity(credentials: Option<&docker_api::DockerCredentials>) -> String {
    let Some(credentials) = credentials else {
        return "anonymous".to_string();
    };
    let mut hasher = blake3::Hasher::new();
    for part in [
        &credentials.serveraddress,
        &credentials.username,
        &credentials.password,
        &credentials.identitytoken,
        &credentials.registrytoken,
    ] {
        // Length-prefixed, so ("ab", "c") and ("a", "bc") differ
        let part = part.as_deref().unwrap_or("");
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// An image a job needs: its name, the platform to run it for (dockerd's
/// own when `None`) and its pull policies (`None` for the runner's helper
/// image, which is pulled only when missing)
struct ImageNeed<'i> {
    name: &'i str,
    platform: Option<&'i str>,
    policies: Option<&'i [String]>,
}

/// A pull a job needs: of `image` for `platform` by `policy`, which is
/// attempt `attempt` of the image's policies, and the policies to fall back
/// to, in order, when it fails
#[derive(Debug, Clone)]
struct PlannedPull<'i> {
    image: &'i str,
    platform: Option<&'i str>,
    policy: PullPolicy,
    attempt: usize,
    fallback: Vec<PullPolicy>,
}

impl PlannedPull<'_> {
    fn is_for(&self, need: &ImageNeed<'_>) -> bool {
        self.platform == need.platform
            && image::with_default_tag(self.image) == image::with_default_tag(need.name)
    }

    /// The image, and its platform when one is asked for
    fn describe(&self) -> String {
        match self.platform {
            Some(platform) => format!("{} for {}", self.image, platform),
            None => self.image.to_string(),
        }
    }
}

/// The log line of trying another pull policy, like gitlab-runner's
fn attempt_note(attempt: usize, policy: PullPolicy) -> String {
    format!(
        "Attempt #{}: Trying \"{}\" pull policy\n",
        attempt,
        policy.name()
    )
}

/// Operating system, architecture and variant of a platform such as
/// `linux/arm64/v8`, lower-cased and with Docker's aliases resolved: e.g.
/// `x86_64` is `amd64`, `aarch64` is `arm64` (whose `v8` is no variant) and
/// `arm` without a variant is `arm/v7`. A lone part is an operating system
/// when it names one, else an architecture. `None` stands for any.
fn platform_parts(platform: &str) -> [Option<String>; 3] {
    const SYSTEMS: [&str; 7] = [
        "linux", "windows", "darwin", "freebsd", "netbsd", "openbsd", "solaris",
    ];
    let lower = platform.trim().to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').filter(|part| !part.is_empty()).collect();
    let (os, arch, variant) = match parts.as_slice() {
        [] => (None, None, None),
        [os] if SYSTEMS.contains(os) => (Some(*os), None, None),
        [arch] => (None, Some(*arch), None),
        [os, arch] => (Some(*os), Some(*arch), None),
        [os, arch, variant, ..] => (Some(*os), Some(*arch), Some(*variant)),
    };
    let (arch, variant) = match (arch, variant) {
        (Some("x86_64" | "x86-64"), variant) => (Some("amd64"), variant),
        (Some("aarch64" | "arm64"), Some("v8") | None) => (Some("arm64"), None),
        (Some("aarch64"), variant) => (Some("arm64"), variant),
        (Some("armhf"), _) => (Some("arm"), Some("v7")),
        (Some("armel"), _) => (Some("arm"), Some("v6")),
        (Some("arm"), None) => (Some("arm"), Some("v7")),
        (Some("i386" | "i686" | "x86"), variant) => (Some("386"), variant),
        other => other,
    };
    [os, arch, variant].map(|part| part.map(str::to_string))
}

/// Whether an image found locally is for `wanted`, as far as dockerd tells
/// (a part it leaves out matches)
fn platform_matches(found: &docker_api::ImageInspect, wanted: &str) -> bool {
    let wanted = platform_parts(wanted);
    let [os, arch, variant] = platform_parts(&format!(
        "{}/{}/{}",
        found.os.as_deref().unwrap_or_default(),
        found.architecture.as_deref().unwrap_or_default(),
        found.variant.as_deref().unwrap_or_default()
    ));
    let found = match (&os, &arch) {
        // Nothing to compare with: dockerd did not say
        (None, None) => return true,
        _ => [os, arch, variant],
    };
    wanted
        .iter()
        .zip(&found)
        .all(|(wanted, found)| match (wanted, found) {
            (Some(wanted), Some(found)) => wanted == found,
            _ => true,
        })
}

/// Pull `image` for `platform` (dockerd's own when `None`) from its registry
async fn pull_image(
    docker: &Docker,
    image: &str,
    platform: Option<&str>,
    credentials: Option<docker_api::DockerCredentials>,
) -> PullOutcome {
    let reference = image::with_default_tag(image);
    let mut stream = docker.create_image(&reference, platform, credentials.as_ref());
    while let Some(progress) = stream.next().await {
        if let Err(e) = progress {
            let error = e.to_string();
            let hint = image::registry_pull_hint(&error, image::registry(image))
                .map(|hint| format!("\n{}", hint))
                .unwrap_or_default();
            return Err(pull_failure(format!(
                "failed to pull image {}: {}{}",
                image, error, hint
            )));
        }
    }
    Ok(())
}

/// Label marking containers and networks with the runner that created them
const OWNER_LABEL: &str = "turboci.runner";
/// How long a killed job container may take to stop
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

impl std::fmt::Debug for DockerExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerExecutor")
            .field("config", &self.config)
            .finish()
    }
}

/// TCP ports that show a service is up: its HEALTHCHECK_TCP_PORT, or its
/// exposed TCP ports (lowest 20), as gitlab-runner checks them
fn service_ports(config: Option<&docker_api::ContainerConfig>) -> Vec<u16> {
    let Some(config) = config else {
        return Vec::new();
    };
    let health_check = config.env.iter().flatten().find_map(|entry| {
        let (key, value) = entry.split_once('=')?;
        key.eq_ignore_ascii_case("HEALTHCHECK_TCP_PORT")
            .then(|| value.trim().parse::<u16>().ok())
            .flatten()
    });
    if let Some(port) = health_check {
        return vec![port];
    }
    let mut ports: Vec<u16> = config
        .exposed_ports
        .iter()
        .flat_map(|ports| ports.keys())
        .filter_map(|port| port.strip_suffix("/tcp")?.parse().ok())
        .collect();
    ports.sort_unstable();
    ports.truncate(20);
    ports
}

/// How long a service may take to accept connections by default (see
/// `DockerConfig::service_wait`)
#[cfg(test)]
const SERVICE_WAIT: Duration = Duration::from_secs(30);
/// Longest pause between connection attempts: a service that is ready after
/// 1.1s must not hold the job until the next full second
const SERVICE_POLL: Duration = Duration::from_millis(200);
/// First pause between connection attempts from the host, doubled after
/// each attempt up to `SERVICE_POLL`: most services open their ports within
/// a few hundred milliseconds, and an attempt that is refused costs little
const SERVICE_FIRST_POLL: Duration = Duration::from_millis(25);
/// Limit of one connection attempt (like `nc -w1`)
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// A service to wait for: its container name (its host name on the job
/// network), the TCP ports that show it is up and, when the runner's host
/// can reach the job network, its address there
struct ServiceCheck {
    host: String,
    ports: Vec<u16>,
    address: Option<std::net::IpAddr>,
}

/// The warning for a service that did not accept connections within `wait`
fn service_timeout_warning(check: &ServiceCheck, wait: Duration) -> String {
    format!(
        "WARNING: service {} (port {}) did not respond within {}s",
        check.host,
        check
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        wait.as_secs()
    )
}

/// What one connection attempt to a service port showed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortProbe {
    /// Accepted: the service is up
    Open,
    /// Refused or reset by the service's own network stack: it is reachable,
    /// but nothing listens on the port yet
    Closed,
    /// No answer in time, no route, or any other error: the host may not
    /// reach the job network at all
    Silent,
}

impl PortProbe {
    fn of_error(error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted => PortProbe::Closed,
            _ => PortProbe::Silent,
        }
    }
}

/// Try to connect to `address` once
async fn probe_port(address: std::net::SocketAddr) -> PortProbe {
    match tokio::time::timeout(
        SERVICE_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(address),
    )
    .await
    {
        Ok(Ok(_)) => PortProbe::Open,
        Ok(Err(e)) => PortProbe::of_error(&e),
        Err(_) => PortProbe::Silent,
    }
}

/// How waiting for a service from the runner's host ended
#[derive(Debug, PartialEq, Eq)]
enum HostWait {
    Ready,
    TimedOut,
    /// The first attempt neither connected nor was refused: the host does not
    /// reach the service, which has to be checked from the job network
    Unreachable,
}

/// The pause after one of `poll` between connection attempts (see
/// `SERVICE_FIRST_POLL`)
fn next_poll(poll: Duration) -> Duration {
    (poll * 2).min(SERVICE_POLL)
}

/// Wait from the runner's host until one of `ports` at `ip` accepts
/// connections (docker:dind exposes 2376 for TLS even when it only listens
/// on 2375), for up to `timeout`
async fn wait_from_host(ip: std::net::IpAddr, ports: &[u16], timeout: Duration) -> HostWait {
    let give_up = Instant::now() + timeout;
    let mut first = true;
    let mut poll = SERVICE_FIRST_POLL;
    loop {
        let probes = futures_util::future::join_all(
            ports
                .iter()
                .map(|&port| probe_port(std::net::SocketAddr::new(ip, port))),
        )
        .await;
        if probes.contains(&PortProbe::Open) {
            return HostWait::Ready;
        }
        // Later on, a service that stops answering for a moment (busy
        // booting) is only not ready yet
        if first && !probes.contains(&PortProbe::Closed) {
            return HostWait::Unreachable;
        }
        first = false;
        if Instant::now() >= give_up {
            return HostWait::TimedOut;
        }
        tokio::time::sleep(poll).await;
        poll = next_poll(poll);
    }
}

/// Whether `gateway`, the job network's address on its bridge, belongs to
/// this host: the network is then a bridge here, whose containers the runner
/// reaches directly. Not with Docker Desktop, a remote or rootless daemon or
/// rootless Podman, whose networks are in another machine or namespace.
fn bridge_is_local(gateway: std::net::IpAddr) -> bool {
    // Binding succeeds only to an address of one of the host's interfaces
    cfg!(target_os = "linux") && std::net::UdpSocket::bind((gateway, 0)).is_ok()
}

/// The service's address on `network` when the runner's host reaches it
fn host_reachable_address(
    info: &docker_api::ContainerInspectResponse,
    network: &str,
) -> Option<std::net::IpAddr> {
    let endpoint = info
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?
        .get(network)?;
    let parse = |value: &Option<String>| value.as_deref()?.parse::<std::net::IpAddr>().ok();
    let gateway = parse(&endpoint.gateway)?;
    let address = parse(&endpoint.ip_address)?;
    bridge_is_local(gateway).then_some(address)
}

/// `uid:gid` owning a host directory, to run a helper as that user
async fn workspace_owner(dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        tokio::fs::metadata(dir)
            .await
            .ok()
            .map(|meta| format!("{}:{}", meta.uid(), meta.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// User the sources helper runs as: the workspace's owner, so the checkout
/// belongs to the runner's user right away. Root (`None`) when the runner's or
/// the job's pre/post_get_sources_script hooks run in the helper (they may
/// need root, e.g. to install packages), when the workspace belongs to root,
/// or when anything in it belongs to someone else (an earlier job's files
/// that could not be handed back), which only root may replace. That takes
/// a walk of the whole workspace, unless the last job in it left it `clean`.
async fn sources_user(job: &Job, job_dir: &Path, clean: &CleanWorkspaces) -> Option<String> {
    let hooks = ["pre_get_sources_script", "post_get_sources_script"];
    if hooks.iter().any(|hook| !hook_lines(job, hook).is_empty()) {
        return None;
    }
    #[cfg(unix)]
    {
        let meta = tokio::fs::metadata(job_dir).await.ok()?;
        let stamp = WorkspaceStamp::of(&meta);
        let (uid, gid) = (stamp.uid, stamp.gid);
        if uid == 0 {
            return None;
        }
        if !clean.take(job_dir, stamp) {
            let dir = job_dir.to_path_buf();
            let walk = tokio::task::spawn_blocking(move || owned_by_others(&dir, uid, gid));
            let others = metrics::timed(Phase::WorkspaceCheck, walk)
                .await
                .unwrap_or(true);
            if others {
                return None;
            }
        }
        Some(format!("{}:{}", uid, gid))
    }
    #[cfg(not(unix))]
    {
        let _ = (job_dir, clean);
        None
    }
}

fn pull_failure(message: String) -> JobFailure {
    JobFailure {
        reason: FailureReason::ImagePullFailure,
        exit_code: None,
        message,
    }
}

/// `work`, dropped with an error once `limits` (when given) are reached
async fn within<T>(
    limits: Option<&Limits<'_>>,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match limits {
        Some(limits) => limits
            .bound(work)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the job timed out or was canceled"))),
        None => work.await,
    }
}

/// Settings of the host dockerd that the runner's config needs but that only
/// dockerd's own configuration can provide; each entry says how to fix it.
/// `resolve` returns a host name's addresses, none when it does not resolve.
pub fn daemon_warnings(
    info: &docker_api::SystemInfo,
    config: &DockerConfig,
    proxy: bool,
    certs_dir: &Path,
    resolve: &dyn Fn(&str) -> Vec<std::net::IpAddr>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let dockerd_proxy = [&info.http_proxy, &info.https_proxy]
        .iter()
        .any(|value| value.as_deref().is_some_and(|v| !v.is_empty()));
    if proxy && !dockerd_proxy {
        warnings.push(
            "proxy is set but dockerd has none, so image pulls bypass it. Add \
             /etc/systemd/system/docker.service.d/proxy.conf with [Service] \
             Environment=\"HTTP_PROXY=...\" \"HTTPS_PROXY=...\" \"NO_PROXY=...\", then \
             systemctl daemon-reload && systemctl restart docker"
                .to_string(),
        );
    }
    let registry_config = info.registry_config.clone().unwrap_or_default();
    let indexes = registry_config.index_configs.unwrap_or_default();
    let cidrs = registry_config.insecure_registry_cidrs.unwrap_or_default();
    for registry in &config.insecure_registries {
        let listed = indexes
            .get(registry)
            .is_some_and(|index| index.secure == Some(false));
        // Like dockerd: insecure when one of the host's addresses is in a CIDR
        // (127.0.0.0/8 is by default); a name that does not resolve is not
        let in_cidr = || {
            let host = registry_host(registry);
            let addrs = match host.parse::<std::net::IpAddr>() {
                Ok(ip) => vec![ip],
                // No CIDR could match, so a lookup would only cost time
                Err(_) if cidrs.is_empty() => Vec::new(),
                Err(_) => resolve(host),
            };
            addrs
                .into_iter()
                .any(|ip| cidrs.iter().any(|cidr| image::ip_in_cidr(ip, cidr)))
        };
        if !(listed || in_cidr()) {
            warnings.push(format!(
                "dockerd does not treat {registry} as insecure, so pulls from it fail. Add it \
                 to \"insecure-registries\" in /etc/docker/daemon.json and restart dockerd"
            ));
        }
    }
    for (registry, file) in &config.registry_ca {
        if !certs_dir.join(registry).join("ca.crt").exists() {
            warnings.push(format!(
                "dockerd has no CA for {registry}, so pulls from it fail. Run: install -D -m 0644 \
                 {file} {}/{registry}/ca.crt (no dockerd restart needed)",
                certs_dir.display()
            ));
        }
    }
    warnings
}

/// Host of a registry given as `host`, `host:port` or `[v6]:port`, split like
/// dockerd's net.SplitHostPort; anything else (`::1`, `[::1]`) is kept whole
fn registry_host(registry: &str) -> &str {
    if let Some((host, _)) = registry
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]:"))
    {
        return host;
    }
    match registry.split_once(':') {
        Some((host, port)) if !port.contains(':') => host,
        _ => registry,
    }
}

/// Read-only mounts of registry CAs where dockerd looks for them. Mounts, not
/// binds: a registry port (`host:8443`) would break the `src:dst` bind syntax.
pub fn dind_ca_mounts(
    registry_ca: &std::collections::BTreeMap<String, String>,
) -> Vec<docker_api::Mount> {
    registry_ca
        .iter()
        .map(|(registry, file)| docker_api::Mount {
            target: Some(format!("/etc/docker/certs.d/{}/ca.crt", registry)),
            source: Some(file.clone()),
            typ: Some(docker_api::MountType::Bind),
            read_only: Some(true),
        })
        .collect()
}

/// Containers and network created for one job, removed when it ends
#[derive(Default)]
struct JobContainers {
    /// Checks out sources, so job images do not need git (like gitlab-runner's helper)
    helper: Option<String>,
    run: RunContainers,
}

/// What the job's scripts run with: the job container next to its services
#[derive(Default)]
struct RunContainers {
    network: Option<String>,
    services: Vec<String>,
    job: Option<String>,
}

/// What starting the containers of a job needs
struct JobSetup<'a> {
    job: &'a Job,
    job_dir: &'a Path,
    image: &'a str,
    /// `image:docker:` of the job (platform and user)
    image_options: &'a DockerOptions,
    services: &'a [crate::gitlab::Service],
    service_images: &'a [String],
    /// `docker:` of each service
    service_options: &'a [DockerOptions],
    /// The job's variables, for service variables that refer to them
    values: &'a std::collections::HashMap<String, String>,
    env: &'a JobEnv,
}

/// Where starting the job's containers reports progress: straight to the
/// job log, or to the branch that owns the log while it shows the checkout
/// that runs at the same time (see `forward_setup_log`)
enum SetupLog<'t, 'a> {
    Trace(&'t mut TraceWriter<'a>),
    Channel(tokio::sync::mpsc::UnboundedSender<String>),
}

impl SetupLog<'_, '_> {
    async fn write(&mut self, text: &str) {
        match self {
            SetupLog::Trace(trace) => trace.write(text).await,
            // The receiver lives until the sender is dropped
            SetupLog::Channel(sender) => {
                let _ = sender.send(text.to_string());
            }
        }
    }
}

/// Write what the services branch reported while the get_sources section
/// was open, then the rest as it comes, until that branch is done (drops
/// its sender). Output that sits in the log is sent to GitLab on time, so a
/// long pull after a quick checkout does not hide the checkout's last lines.
async fn forward_setup_log(
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    trace: &mut TraceWriter<'_>,
) {
    let mut flush = FlushTimer::default();
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(line) => trace.write(&line).await,
                None => return,
            },
            _ = flush.wait(trace.next_flush_at()) => {
                trace.flush().await;
            }
        }
    }
}

impl DockerExecutor {
    pub fn new(config: DockerConfig) -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;
        Ok(Self {
            docker: Arc::new(docker),
            config: Self::prepared(config)?,
            owner: "turboci".to_string(),
            ca_pem: None,
            pulls: Arc::default(),
            clean_workspaces: Arc::default(),
            images: Arc::default(),
        })
    }

    /// Host directory holding the workspaces (`builds_dir` in the config)
    pub(super) fn builds_root(&self) -> PathBuf {
        PathBuf::from(
            self.config
                .builds_dir
                .as_deref()
                .unwrap_or(DOCKER_BUILDS_ROOT),
        )
    }

    /// Identify this runner (its system ID) in container and network labels
    pub fn with_owner(mut self, owner: &str) -> Self {
        self.owner = owner.to_string();
        self
    }

    /// The same executor (Docker connection, owner, image pulls in flight)
    /// with other settings, for jobs that start from now on
    pub fn reconfigured(&self, config: DockerConfig) -> Result<Self> {
        Ok(Self {
            config: Self::prepared(config)?,
            ..self.clone()
        })
    }

    /// `config` checked, with its seccomp profile files read (once, here)
    fn prepared(mut config: DockerConfig) -> Result<DockerConfig> {
        config.validate()?;
        config.security_opt = config
            .security_opt
            .iter()
            .map(|option| crate::runner_daemon::config::security_opt(option))
            .collect::<Result<_>>()?;
        Ok(config)
    }

    fn labels(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([(OWNER_LABEL.to_string(), self.owner.clone())])
    }

    /// `git ls-files --others -z` of the project as the workspace's owner:
    /// an exec in the job's sources `helper` when there is one, else a
    /// helper container of its own
    pub(super) async fn untracked_files(
        &self,
        job_id: u64,
        job_dir: &Path,
        subdir: &str,
        helper: Option<&str>,
    ) -> Result<String> {
        let dir = format!("/builds/{}", subdir);
        if let Some(helper) = helper {
            let user = workspace_owner(job_dir).await;
            match self.untracked_in(helper, &dir, user.as_deref()).await {
                Ok(files) => return Ok(files),
                Err(e) => warn!(
                    "Listing untracked files in the sources helper failed, trying a new container: {:#}",
                    e
                ),
            }
        }
        let script = format!(
            "GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null git -c safe.directory={dir} {} -C {dir} ls-files --others -z",
            NO_REPO_COMMANDS.join(" "),
        );
        self.run_helper(
            &format!("turboci-job-{}-untracked", job_id),
            &script,
            HostConfig {
                binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                network_mode: Some("none".to_string()),
                ..Default::default()
            },
            true,
            // Not root: the workspace belongs to the runner again
            workspace_owner(job_dir).await,
            None,
        )
        .await
    }

    /// `git ls-files --others -z` of `dir` by an exec in the running
    /// `helper` as `user` (the container's when `None`). The helper was
    /// started with the job's variables: git gets a clean environment
    /// instead, as a helper container of its own would have.
    async fn untracked_in(&self, helper: &str, dir: &str, user: Option<&str>) -> Result<String> {
        const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
        let safe_directory = format!("safe.directory={}", dir);
        let mut cmd = vec![
            "env",
            "-i",
            PATH,
            "GIT_CONFIG_NOSYSTEM=1",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "git",
            "-c",
            &safe_directory,
        ];
        cmd.extend(NO_REPO_COMMANDS);
        cmd.extend(["-C", dir, "ls-files", "--others", "-z"]);
        let exec = self
            .docker
            .create_exec(
                helper,
                &ExecConfig {
                    cmd: Some(cmd),
                    env: Some(vec![PATH]),
                    user,
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        let run = async {
            let mut output = self.docker.start_exec(&exec.id).await?;
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(chunk) = output.next().await {
                match chunk? {
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        stdout.extend_from_slice(&message)
                    }
                    LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                    LogOutput::StdIn { .. } => {}
                }
            }
            let code = self.docker.inspect_exec(&exec.id).await?.exit_code;
            if code != Some(0) {
                anyhow::bail!(
                    "git exited with {:?}: {}",
                    code,
                    String::from_utf8_lossy(&stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&stdout).into_owned())
        };
        tokio::time::timeout(CLEANUP_TIMEOUT, run)
            .await
            .context("timed out")?
    }

    /// Remove containers and networks this runner left behind (crash, kill,
    /// power loss); other runners on the same Docker host are not touched
    pub async fn sweep_orphans(&self) {
        let filter = std::collections::HashMap::from([(
            "label".to_string(),
            vec![format!("{}={}", OWNER_LABEL, self.owner)],
        )]);
        let containers = self
            .docker
            .list_containers(true, &filter)
            .await
            .unwrap_or_default();
        for container in containers {
            if let Some(id) = container.id {
                match self.force_remove(&id).await {
                    Ok(()) => info!("🧹 Removed leftover container {}", id),
                    Err(e) => warn!("Failed to remove leftover container {}: {}", id, e),
                }
            }
        }
        let networks = self.docker.list_networks(&filter).await.unwrap_or_default();
        for network in networks {
            if let Some(name) = network.name {
                if self.docker.remove_network(&name).await.is_ok() {
                    info!("🧹 Removed leftover network {}", name);
                }
            }
        }
    }

    /// Remove the images of the Docker host that no container uses, past
    /// `image_max_age_days` and, least recently used first, beyond
    /// `image_max_size`. The helper and default images, the services of the
    /// runner's config and the images of running jobs are kept; removals are
    /// never forced, so dockerd keeps an image a container uses. An image's
    /// last use is the last job of this runner that used it, or when the
    /// collection first saw it: dockerd records no last use, and an image's
    /// build time says nothing about it (a pull of `postgres:16` may bring
    /// an image built months ago). Returns the names of the removed images.
    pub async fn collect_images(&self) -> Vec<String> {
        self.collect_images_where(|_| true).await
    }

    /// `collect_images` among the images `eligible` accepts
    async fn collect_images_where(
        &self,
        eligible: impl Fn(&docker_api::ImageSummary) -> bool,
    ) -> Vec<String> {
        use crate::runner_daemon::housekeeping::{self, UnusedImage};

        let max_age = self
            .config
            .image_max_age_days
            .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60)));
        let max_size = self.config.image_max_size_bytes().ok().flatten();
        if max_age.is_none() && max_size.is_none() {
            return Vec::new();
        }
        let started = std::time::SystemTime::now();
        let images = match self.docker.list_images().await {
            Ok(images) => images,
            Err(e) => {
                warn!("Image collection: cannot list the Docker images: {}", e);
                return Vec::new();
            }
        };
        // Without the containers any image may be in use: nothing is removed
        let containers = match self
            .docker
            .list_containers(true, &std::collections::HashMap::new())
            .await
        {
            Ok(containers) => containers,
            Err(e) => {
                warn!("Image collection: cannot list the containers: {}", e);
                return Vec::new();
            }
        };
        let in_use: std::collections::HashSet<String> = containers
            .into_iter()
            .filter_map(|container| container.image_id)
            .collect();
        let kept: std::collections::HashSet<String> =
            [&self.config.helper_image, &self.config.default_image]
                .into_iter()
                .chain(self.config.services.iter().map(|service| &service.name))
                .map(|image| usage_key(image))
                .collect();

        let mut unused = Vec::new();
        {
            let mut state = self.images.state();
            state
                .first_seen
                .retain(|id, _| images.iter().any(|image| &image.id == id));
            for image in images.iter().filter(|image| eligible(image)) {
                let references = image_references(image);
                let first_seen = *state.first_seen.entry(image.id.clone()).or_insert(started);
                if in_use.contains(&image.id)
                    || references.iter().any(|reference| {
                        kept.contains(reference) || state.held.contains_key(reference)
                    })
                {
                    continue;
                }
                let last_used = references
                    .iter()
                    .filter_map(|reference| state.last_used.get(reference).copied())
                    .chain(std::iter::once(first_seen))
                    .max()
                    .unwrap_or(first_seen);
                unused.push(UnusedImage {
                    id: image.id.clone(),
                    tags: image_tags(image),
                    references,
                    size: u64::try_from(image.size).unwrap_or(0),
                    created: image.created,
                    last_used,
                });
            }
        }

        let total: u64 = unused.iter().map(|image| image.size).sum();
        let mut removed = Vec::new();
        let mut freed = 0;
        for image in housekeeping::images_to_remove(&unused, started, max_age, max_size) {
            if self.remove_unused_image(image, started).await {
                removed.push(image.name());
                freed += image.size;
            }
        }
        if !removed.is_empty() {
            info!(
                "🧹 Removed {} unused Docker images ({} of {} unused): {}",
                removed.len(),
                crate::runner_daemon::admission::human(freed),
                crate::runner_daemon::admission::human(total),
                removed.join(", ")
            );
        }
        if let Some(max_size) = max_size {
            if total - freed > max_size {
                warn!(
                    "Unused Docker images still take {}, above image_max_size {}",
                    crate::runner_daemon::admission::human(total - freed),
                    crate::runner_daemon::admission::human(max_size)
                );
            }
        }
        removed
    }

    /// Remove `image`, picked by the collection that started at `started`,
    /// unless a job started using it since; true when it is gone
    async fn remove_unused_image(
        &self,
        image: &crate::runner_daemon::housekeeping::UnusedImage,
        started: std::time::SystemTime,
    ) -> bool {
        // No job marks its images while an image is checked and removed
        let _removal = self.images.removal.lock().await;
        {
            let state = self.images.state();
            let used_since = image.references.iter().any(|reference| {
                state.held.contains_key(reference)
                    || state
                        .last_used
                        .get(reference)
                        .is_some_and(|used| *used >= started)
            });
            if used_since {
                return false;
            }
        }
        // Untagging the last tag removes the image; an untagged one goes by ID
        let targets = if image.tags.is_empty() {
            vec![image.id.clone()]
        } else {
            image.tags.clone()
        };
        for target in &targets {
            match self.docker.remove_image(target).await {
                Ok(()) => {}
                Err(e) if e.status_code() == Some(404) => {}
                // In use by a container created meanwhile, or a parent of
                // another image: dockerd keeps it
                Err(e) if e.status_code() == Some(409) => {
                    info!("Image collection: kept {}: {}", target, e);
                    return false;
                }
                Err(e) => {
                    warn!("Image collection: cannot remove {}: {}", target, e);
                    return false;
                }
            }
        }
        true
    }

    /// `daemon_warnings` for the Docker daemon this runner uses
    pub async fn daemon_warnings(&self, proxy: bool) -> Vec<String> {
        let info = match self.docker.info().await {
            Ok(info) => info,
            Err(e) => return vec![format!("Cannot read the Docker daemon's settings: {}", e)],
        };
        let config = self.config.clone();
        // Resolving insecure registry names blocks (getaddrinfo)
        tokio::task::spawn_blocking(move || {
            let resolve = |host: &str| -> Vec<std::net::IpAddr> {
                crate::net::lookup(host)
                    .map(|addrs| addrs.iter().map(|addr| addr.ip()).collect())
                    .unwrap_or_default()
            };
            let certs_dir = Path::new("/etc/docker/certs.d");
            daemon_warnings(&info, &config, proxy, certs_dir, &resolve)
        })
        .await
        .unwrap_or_else(|e| vec![format!("Cannot check the Docker daemon's settings: {}", e)])
    }

    /// Run the job; what it leaves is returned (see `Teardown`), including
    /// after failures, timeouts, cancellation and panics
    pub(super) async fn execute(
        &self,
        job: &Job,
        job_dir: &Path,
        deadline: Instant,
        trace: &mut TraceWriter<'_>,
        restore: &impl Restore,
    ) -> (JobOutcome, Leftovers) {
        use futures_util::FutureExt;

        let mut containers = JobContainers::default();
        // Closed by start_and_run once the job's containers are ready, here
        // when it fails before
        let mut preparing = true;
        trace
            .section_start("prepare_executor", "Preparing the \"docker\" executor")
            .await;
        let outcome = std::panic::AssertUnwindSafe(self.start_and_run(
            job,
            job_dir,
            deadline,
            trace,
            &mut containers,
            &mut preparing,
            restore,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(JobFailure::system("internal runner error")));
        if preparing {
            trace.section_end("prepare_executor").await;
        }
        let left = Leftovers {
            executor: self.clone(),
            job_id: job.id,
            job_dir: job_dir.to_path_buf(),
            containers,
            settled: None,
        };
        (outcome, left)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_and_run(
        &self,
        job: &Job,
        job_dir: &Path,
        deadline: Instant,
        trace: &mut TraceWriter<'_>,
        containers: &mut JobContainers,
        preparing: &mut bool,
        restore: &impl Restore,
    ) -> JobOutcome {
        let mut env = script::job_env(
            job,
            "/builds",
            job_dir,
            &script::RunnerVars {
                disposable: true,
                ca_pem: self.ca_pem.clone(),
            },
        );
        env.trust_checkouts();
        // Image and service names may use variables, e.g. $CI_REGISTRY_IMAGE/ci
        let values: std::collections::HashMap<String, String> = env.vars.iter().cloned().collect();
        let expand = |name: &str| script::expand(name, &values);

        let (image, policies, image_options) = match &job.image {
            Some(img) => (
                expand(&img.name),
                img.pull_policy.clone(),
                img.docker_options(),
            ),
            None => (
                self.config.default_image.clone(),
                Vec::new(),
                DockerOptions::default(),
            ),
        };
        trace
            .write(&format!("Using Docker executor with image {} ...\n", image))
            .await;
        info!("🐳 Using Docker image: {}", image);
        // Services from the runner config come first, then the job's own
        let services: Vec<crate::gitlab::Service> = self
            .config
            .services
            .iter()
            .map(|s| crate::gitlab::Service {
                name: s.name.clone(),
                alias: s.alias.clone(),
                entrypoint: s.entrypoint.clone(),
                command: s.command.clone(),
                pull_policy: Vec::new(),
                variables: Vec::new(),
                executor_opts: None,
            })
            .chain(job.services.iter().cloned())
            .collect();
        let service_images: Vec<String> = services.iter().map(|s| expand(&s.name)).collect();
        let service_options: Vec<DockerOptions> =
            services.iter().map(|s| s.docker_options()).collect();
        let checkout = job.git_info.is_some()
            && git::strategy(&job.variables, job.allow_git_fetch) != GitStrategy::None;

        // Every image is checked against its pull policy first, so a job
        // fails before anything starts when one is not allowed
        let job_images: Vec<ImageNeed> = std::iter::once(ImageNeed {
            name: &image,
            platform: image_options.platform.as_deref(),
            policies: Some(policies.as_slice()),
        })
        .chain(
            service_images
                .iter()
                .zip(&services)
                .zip(&service_options)
                .map(|((name, service), options)| ImageNeed {
                    name,
                    platform: options.platform.as_deref(),
                    policies: Some(service.pull_policy.as_slice()),
                }),
        )
        .collect();
        let helper = ImageNeed {
            name: &self.config.helper_image,
            platform: None,
            policies: None,
        };
        let mut images: Vec<&ImageNeed> = job_images.iter().take(1).collect();
        if checkout {
            images.push(&helper);
        }
        images.extend(job_images.iter().skip(1));
        self.check_allowed_images(job.image.as_ref().map(|_| image.as_str()), &service_images)
            .map_err(pull_failure)?;
        // Until the job ends, the image collection leaves its images alone
        let _held = self
            .images
            .hold(&images.iter().map(|need| need.name).collect::<Vec<_>>())
            .await;
        let pulls = metrics::timed(Phase::Pull, self.plan_pulls(&images, trace)).await?;
        // Pulled by the branch that needs them; an image both need is pulled
        // once (see `pull_shared`)
        let needed_by = |wanted: &[&ImageNeed]| -> Vec<PlannedPull> {
            pulls
                .iter()
                .filter(|pull| wanted.iter().any(|need| pull.is_for(need)))
                .cloned()
                .collect()
        };
        let job_pulls = needed_by(&job_images.iter().collect::<Vec<_>>());
        let helper_pulls = needed_by(&[&helper]);

        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;
        let subdir = script::project_subdir(job).map_err(JobFailure::system)?;
        tokio::fs::create_dir_all(job_dir.join(&subdir))
            .await
            .map_err(|e| JobFailure::system(format!("Failed to create host workspace: {}", e)))?;
        let dirs = JobDirs {
            builds: "/builds".to_string(),
            project: format!("/builds/{}", subdir),
            host_project: job_dir.join(&subdir),
        };
        let setup = JobSetup {
            job,
            job_dir,
            image: &image,
            image_options: &image_options,
            services: &services,
            service_images: &service_images,
            service_options: &service_options,
            values: &values,
            env: &env,
        };
        let JobContainers { helper, run } = containers;

        // Like the scripts, pulls and service startup stop at the job timeout
        // and once the job is being canceled
        let cancel = trace.cancel();
        if !checkout {
            // Hooks of get_sources run in the job container, which has to come first
            let limits = Limits {
                deadline,
                cancel: &cancel,
                stop_at: RemoteState::Canceling,
            };
            let id = self
                .start_job_container(
                    &setup,
                    &job_pulls,
                    &limits,
                    &mut SetupLog::Trace(trace),
                    run,
                )
                .await?;
            trace.section_end("prepare_executor").await;
            *preparing = false;
            let runner = self.runner(&id, &env);
            return run_job_steps(job, deadline, &runner, &runner, trace, &dirs, restore).await;
        }

        // Getting sources needs neither the job image nor the services nor the
        // job container: the checkout runs while they are pulled and started.
        // Both branches stop on `setup_stop`, which a cancel of the job sets,
        // and which a branch that fails sets to stop the other one; the first
        // failure is the job's. A failing checkout does not stop the job
        // container: after_script still runs in it, as it would without
        // services. Container API calls are never interrupted (a dropped
        // create could leave a container nobody removes): every container is
        // recorded as soon as it exists, and removed by the job's teardown.
        let setup_stop = CancelSignal::default();
        let limits = Limits {
            deadline,
            cancel: &setup_stop,
            stop_at: RemoteState::Canceling,
        };
        let failed = std::sync::OnceLock::<JobFailure>::new();
        let fail = |failure: JobFailure| {
            let _ = failed.set(failure);
            setup_stop.update(RemoteState::Canceling);
        };
        // The services branch's lines wait while the get_sources section is
        // open, and follow it live once it is closed
        let (log_sender, mut log_lines) = tokio::sync::mpsc::unbounded_channel();
        let sources = async {
            let started = metrics::timed(Phase::Helper, async {
                let mut pull_log = SetupLog::Trace(&mut *trace);
                // The workspace check runs while the helper image is pulled;
                // a canceled job does not wait for it
                let (pulled, user) = tokio::join!(
                    self.pull_all(&helper_pulls, job, &limits, &mut pull_log),
                    limits.bound(sources_user(job, job_dir, &self.clean_workspaces))
                );
                pulled?;
                limits.check(job)?;
                let user = user.unwrap_or(None);
                // Kept until the job's teardown, which hands the workspace
                // back with it
                self.start_helper(job, job_dir, &env, user, helper)
                    .await
                    .map_err(|e| JobFailure::system(format!("{:#}", e)))
            })
            .await;
            let sourced = match started {
                Ok(id) => {
                    trace.section_end("prepare_executor").await;
                    *preparing = false;
                    let runner = self.runner(&id, &env);
                    Ok(get_sources_section(job, &runner, trace, &dirs, &limits).await)
                }
                Err(failure) => {
                    fail(failure.clone());
                    Err(failure)
                }
            };
            // What the containers branch reported, and the wait for it to
            // end, follow the checkout in a section of their own
            let waiting = !(log_lines.is_closed() && log_lines.is_empty());
            if *preparing || !waiting {
                forward_setup_log(&mut log_lines, trace).await;
            } else {
                let header = if services.is_empty() {
                    "Starting the job container"
                } else {
                    "Starting services and the job container"
                };
                trace.section_start("prepare_containers", header).await;
                metrics::timed(Phase::SetupWait, forward_setup_log(&mut log_lines, trace)).await;
                trace.section_end("prepare_containers").await;
            }
            sourced
        };
        let containers = async {
            // Dropped when this branch is done, which ends the forwarding
            let mut log = SetupLog::Channel(log_sender);
            let started = self
                .start_job_container(&setup, &job_pulls, &limits, &mut log, run)
                .await;
            if let Err(failure) = &started {
                fail(failure.clone());
            }
            started
        };
        let forward_cancel = async {
            cancel.reached(RemoteState::Canceling).await;
            setup_stop.update(RemoteState::Canceling);
            std::future::pending::<()>().await
        };
        let (sourced, started) = tokio::select! {
            both = async { tokio::join!(sources, containers) } => both,
            () = forward_cancel => unreachable!("waits forever once forwarded"),
        };
        if let Some(failure) = failed.into_inner() {
            return Err(failure);
        }
        let id = started?;
        let sources_failure = sourced?;
        let runner = self.runner(&id, &env);
        run_steps(
            job,
            deadline,
            sources_failure,
            &runner,
            trace,
            &dirs,
            restore,
        )
        .await
    }

    /// Runs scripts in container `id`
    fn runner<'r>(&'r self, id: &'r str, env: &JobEnv) -> DockerRunner<'r> {
        DockerRunner {
            docker: &self.docker,
            container_id: id,
            env: env.to_docker(),
        }
    }

    /// Pull the job image and the service images in `pulls`, start the
    /// services on a network of the job's own and wait for them while the
    /// job container is created and started on that network; returns its id
    /// once both are done. Everything is recorded in `run` as soon as it
    /// exists, so a failure leaves nothing the teardown misses.
    async fn start_job_container(
        &self,
        setup: &JobSetup<'_>,
        pulls: &[PlannedPull<'_>],
        limits: &Limits<'_>,
        log: &mut SetupLog<'_, '_>,
        run: &mut RunContainers,
    ) -> std::result::Result<String, JobFailure> {
        let job = setup.job;
        metrics::timed(Phase::Pull, self.pull_all(pulls, job, limits, log)).await?;
        let volumes = self
            .job_volumes(job)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
        if setup.services.is_empty() {
            return self
                .create_job_container(setup, &volumes, None, limits, &mut run.job)
                .await;
        }

        // Services need a network of their own to be reachable by alias;
        // the job container joins it too
        limits.check(job)?;
        let started = std::time::Instant::now();
        let network = format!("turboci-job-{}", job.id);
        let created = self
            .create_network(&network)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)));
        if let Err(failure) = created {
            metrics::record(Phase::Services, started.elapsed());
            return Err(failure);
        }
        run.network = Some(network.clone());
        let RunContainers {
            services, job: id, ..
        } = run;
        // Only this branch writes to the log; the job container only runs a
        // sleep until the steps, which start once both branches are done
        let services_up = async {
            let up = async {
                self.start_services(setup, &volumes, &network, limits, log, services)
                    .await?;
                self.wait_for_services(job.id, &network, services, log, limits)
                    .await;
                Ok::<_, JobFailure>(())
            }
            .await;
            metrics::record(Phase::Services, started.elapsed());
            up
        };
        let container = self.create_job_container(setup, &volumes, Some(&network), limits, id);
        let (services_up, container) = tokio::join!(services_up, container);
        services_up?;
        let id = container?;
        // A wait for services that the job timeout or a cancel ended
        limits.check(job)?;
        Ok(id)
    }

    /// Create and start the job container on `network` (the configured
    /// `network_mode` when `None`); recorded in `slot` once it exists
    async fn create_job_container(
        &self,
        setup: &JobSetup<'_>,
        volumes: &[String],
        network: Option<&str>,
        limits: &Limits<'_>,
        slot: &mut Option<String>,
    ) -> std::result::Result<String, JobFailure> {
        let job = setup.job;
        limits.check(job)?;
        let started = std::time::Instant::now();
        let name = format!("turboci-job-{}", job.id);
        let config = ContainerCreateBody {
            image: Some(setup.image.to_string()),
            // `image: {docker: {user: ...}}`; the image's own user otherwise
            user: setup.image_options.user.clone(),
            working_dir: Some("/builds".to_string()),
            env: Some(setup.env.to_docker()),
            // `image: {entrypoint: [""]}` clears an entrypoint that is not a shell
            entrypoint: job.image.as_ref().and_then(|img| img.entrypoint.clone()),
            // Keep the container alive for the whole job; steps run as execs
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "while :; do sleep 3600; done".to_string(),
            ]),
            host_config: Some(
                self.job_host_config(
                    std::iter::once(format!("{}:/builds", setup.job_dir.display()))
                        .chain(volumes.iter().cloned())
                        .collect(),
                    network,
                ),
            ),
            ..Default::default()
        };
        let id = self
            .create_named_for(&name, config, setup.image_options.platform.as_deref())
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
        *slot = Some(id.clone());
        info!("📦 Created container: {}", id);

        self.docker
            .start_container(&id)
            .await
            .map_err(|e| JobFailure::system(format!("Failed to start container: {}", e)))?;
        metrics::record(Phase::Container, started.elapsed());
        Ok(id)
    }

    /// Create and start the job's services on `network`, all at the same
    /// time; each is recorded in `ids` (in the services' order) once it
    /// exists. A failing
    /// service, a cancel or the job timeout stops the services that have
    /// not been created or started yet, but no create or start call that is
    /// under way: a dropped create could leave a container nobody removes.
    async fn start_services(
        &self,
        setup: &JobSetup<'_>,
        volumes: &[String],
        network: &str,
        limits: &Limits<'_>,
        log: &mut SetupLog<'_, '_>,
        ids: &mut Vec<String>,
    ) -> JobOutcome {
        let job = setup.job;
        if setup.services.is_empty() {
            return Ok(());
        }
        for service_image in setup.service_images {
            log.write(&format!("Starting service {} ...\n", service_image))
                .await;
        }
        limits.check(job)?;
        let given_up = std::sync::atomic::AtomicBool::new(false);
        let go_on = || {
            if given_up.load(std::sync::atomic::Ordering::SeqCst) {
                // Another service failed: its error is the job's
                return Err(None);
            }
            limits.check(job).map_err(Some)
        };
        let network = Some(network);
        let starts = setup
            .services
            .iter()
            .zip(setup.service_images)
            .zip(setup.service_options)
            .enumerate()
            .map(|(index, ((service, service_image), options))| {
                let go_on = &go_on;
                let given_up = &given_up;
                async move {
                    let mut host_config = self.host_config(volumes.to_vec(), network);
                    let mut service = service.clone();
                    if image::is_dind(service_image) {
                        service.command = image::dind_command(
                            service.command.as_deref(),
                            &self.config.insecure_registries,
                        );
                        let mounts = dind_ca_mounts(&self.config.registry_ca);
                        host_config.mounts = (!mounts.is_empty()).then_some(mounts);
                    }
                    if let Err(stop) = go_on() {
                        return (None, stop);
                    }
                    let created = self
                        .create_service(
                            job,
                            &format!("turboci-job-{}-svc-{}", job.id, index),
                            service_image,
                            &service,
                            options,
                            setup.values,
                            // Configured volumes too, e.g. /certs/client for docker:dind with TLS
                            host_config,
                        )
                        .await;
                    let id = match created {
                        Ok(id) => id,
                        Err(e) => {
                            given_up.store(true, std::sync::atomic::Ordering::SeqCst);
                            let message = format!("service {}: {:#}", service.name, e);
                            return (None, Some(JobFailure::system(message)));
                        }
                    };
                    if let Err(stop) = go_on() {
                        return (Some(id), stop);
                    }
                    match self.docker.start_container(&id).await {
                        Ok(()) => (Some(id), None),
                        Err(e) => {
                            given_up.store(true, std::sync::atomic::Ordering::SeqCst);
                            let message = format!("service {}: {}", service_image, e);
                            (Some(id), Some(JobFailure::system(message)))
                        }
                    }
                }
            });
        let started = futures_util::future::join_all(starts).await;
        let mut failure = None;
        for (id, failed) in started {
            // Every service that exists is removed by the teardown, started or not
            ids.extend(id);
            if failure.is_none() {
                failure = failed;
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Start the container that checks out sources as `user` (root when
    /// `None`, see `sources_user`), with the workspace mounted; recorded in
    /// `helper` as soon as it exists, so a failed start leaves nothing
    async fn start_helper(
        &self,
        job: &Job,
        job_dir: &Path,
        env: &JobEnv,
        user: Option<String>,
        helper: &mut Option<String>,
    ) -> Result<String> {
        let config = ContainerCreateBody {
            image: Some(self.config.helper_image.clone()),
            user,
            working_dir: Some("/builds".to_string()),
            env: Some(env.to_docker()),
            // Helper images may have their own entrypoint (alpine/git's is `git`)
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: Some(vec!["while :; do sleep 3600; done".to_string()]),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                network_mode: Some(self.config.network_mode.clone()),
                ..self.name_resolution()
            }),
            ..Default::default()
        };
        let id = self
            .create_named(&format!("turboci-job-{}-sources", job.id), config)
            .await?;
        *helper = Some(id.clone());
        self.docker
            .start_container(&id)
            .await
            .context("Failed to start helper container")?;
        Ok(id)
    }

    /// Host settings shared by job and service containers
    fn host_config(&self, binds: Vec<String>, network: Option<&str>) -> HostConfig {
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            network_mode: Some(
                network
                    .map(str::to_string)
                    .unwrap_or_else(|| self.config.network_mode.clone()),
            ),
            privileged: Some(self.config.privileged),
            // Validated when the executor is created
            memory: self.config.memory_bytes().ok().flatten(),
            memory_swap: self.config.memory_swap_bytes().ok().flatten(),
            memory_reservation: self.config.memory_reservation_bytes().ok().flatten(),
            shm_size: self.config.shm_size_bytes().ok().flatten(),
            oom_score_adj: self.config.oom_score_adjust,
            pids_limit: self.config.pids_limit,
            nano_cpus: self.config.cpus.map(|cpus| (cpus * 1e9) as i64),
            cpu_shares: self.config.cpu_shares,
            storage_opt: (!self.config.storage_opt.is_empty())
                .then(|| self.config.storage_opt.clone().into_iter().collect()),
            runtime: self.config.runtime.clone(),
            ..self.name_resolution()
        }
    }

    /// `extra_hosts`, `dns` and `dns_search`: job, service and checkout
    /// containers need them alike (e.g. to reach a GitLab only /etc/hosts
    /// knows)
    fn name_resolution(&self) -> HostConfig {
        let list = |values: Vec<String>| (!values.is_empty()).then_some(values);
        HostConfig {
            extra_hosts: list(self.config.extra_host_entries()),
            dns: list(self.config.dns.clone()),
            dns_search: list(self.config.dns_search.clone()),
            ..Default::default()
        }
    }

    /// Host settings of the job container: those it shares with services,
    /// plus the workspace and the options gitlab-runner gives job containers
    /// only (tmpfs, capabilities, security options, devices, GPUs, sysctls
    /// and ulimits)
    fn job_host_config(&self, binds: Vec<String>, network: Option<&str>) -> HostConfig {
        fn list<T>(values: Vec<T>) -> Option<Vec<T>> {
            (!values.is_empty()).then_some(values)
        }
        let config = &self.config;
        HostConfig {
            tmpfs: (!config.tmpfs.is_empty()).then(|| config.tmpfs.clone().into_iter().collect()),
            cap_add: list(config.cap_add.clone()),
            cap_drop: list(config.cap_drop.clone()),
            security_opt: list(config.security_opt.clone()),
            devices: list(config.device_mappings()),
            device_requests: list(config.gpu_requests()),
            sysctls: (!config.sysctls.is_empty())
                .then(|| config.sysctls.clone().into_iter().collect()),
            ulimits: list(config.ulimits()),
            ..self.host_config(binds, network)
        }
    }

    /// Binds for `volumes`: `host:container[:mode]` entries as they are, and a
    /// bare container path (e.g. "/cache") as a volume that persists between
    /// the project's jobs, like gitlab-runner's cache volumes. Each concurrent
    /// slot of a project gets its own, so parallel jobs do not share one.
    async fn job_volumes(&self, job: &Job) -> Result<Vec<String>> {
        let project = job.job_info.as_ref().map_or(0, |info| info.project_id);
        let slot = variable_value(job, "CI_CONCURRENT_PROJECT_ID").unwrap_or("0");
        let mut binds = Vec::new();
        for volume in &self.config.volumes {
            if volume.contains(':') {
                binds.push(volume.clone());
                continue;
            }
            let hash = blake3::hash(volume.as_bytes()).to_hex();
            let name = format!("turboci-cache-p{}-c{}-{}", project, slot, &hash[..12]);
            if self.docker.inspect_volume(&name).await.is_err() {
                let mut labels = self.labels();
                labels.insert("turboci.volume".to_string(), volume.clone());
                self.docker
                    .create_volume(&docker_api::VolumeCreateOptions {
                        name: Some(name.clone()),
                        labels: Some(labels),
                    })
                    .await
                    .with_context(|| format!("Failed to create volume for {}", volume))?;
            }
            binds.push(format!("{}:{}", name, volume));
        }
        Ok(binds)
    }

    /// Registry login for `image`, in gitlab-runner's order: the job's
    /// DOCKER_AUTH_CONFIG, the runner's Docker config file, then the
    /// credentials GitLab sent (its container registry). Each Docker config
    /// may name credential helpers (see `credential_helper`); a job's only
    /// those `allowed_docker_credential_helpers` lists. Also returns what
    /// went wrong on the way (never a secret).
    async fn registry_login(
        &self,
        image: &str,
        job: &Job,
    ) -> (Option<docker_api::DockerCredentials>, Vec<String>) {
        use crate::runner_daemon::credential_helper::{self as helpers, ConfigSource};

        let mut warnings = Vec::new();
        if let Some(json) = variable_value(job, "DOCKER_AUTH_CONFIG") {
            let source = ConfigSource {
                name: "DOCKER_AUTH_CONFIG",
                json,
                allowed_helpers: Some(&self.config.allowed_docker_credential_helpers),
            };
            let login = helpers::login(&source, image, Default::default(), &mut warnings).await;
            if login.is_some() {
                return (login, warnings);
            }
        }
        let file = self.config.auth_config_file.clone().or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{}/.docker/config.json", home))
        });
        if let Some(file) = file {
            if let Ok(json) = tokio::fs::read_to_string(&file).await {
                // The operator's own config: its helpers always run
                let source = ConfigSource {
                    name: &file,
                    json: &json,
                    allowed_helpers: None,
                };
                let login = helpers::login(&source, image, Default::default(), &mut warnings).await;
                if login.is_some() {
                    return (login, warnings);
                }
            }
        }
        let login = image::credentials_for(image, &job.credentials).map(|c| {
            docker_api::DockerCredentials {
                username: Some(c.username.clone()),
                password: Some(c.password.clone()),
                serveraddress: Some(image::registry(image).to_string()),
                ..Default::default()
            }
        });
        (login, warnings)
    }

    /// gitlab-runner's `allowed_images` and `allowed_services`: when patterns
    /// are set, the job's own image (`None` when it uses `default_image`) and
    /// its own services must match one of them; `default_image` and the
    /// services of the runner's config are always allowed. Names are matched
    /// as the job wrote them, variables expanded (`ruby:*` does not match
    /// `docker.io/library/ruby:3` nor `ruby`).
    fn check_allowed_images(
        &self,
        job_image: Option<&str>,
        service_images: &[String],
    ) -> std::result::Result<(), String> {
        use crate::runner_daemon::config::glob_matches;

        let disallowed = |image: &str, kind: &str, patterns: &[String]| {
            format!(
                "The {:?} image is not present on list of allowed {}:\n{}\nPlease check the runner's executor.docker.allowed_{} configuration",
                image,
                kind,
                patterns
                    .iter()
                    .map(|pattern| format!("- {}", pattern))
                    .collect::<Vec<_>>()
                    .join("\n"),
                kind
            )
        };
        let allowed_images = &self.config.allowed_images;
        if let Some(image) = job_image {
            if !allowed_images.is_empty()
                && image != self.config.default_image
                && !glob_matches(allowed_images, image)
            {
                return Err(disallowed(image, "images", allowed_images));
            }
        }
        // Services of the runner's config come first (see `start_and_run`)
        let (configured, own) =
            service_images.split_at(self.config.services.len().min(service_images.len()));
        let allowed_services = &self.config.allowed_services;
        if allowed_services.is_empty() {
            return Ok(());
        }
        match own.iter().find(|service| {
            !configured.contains(service) && !glob_matches(allowed_services, service)
        }) {
            Some(service) => Err(disallowed(service, "services", allowed_services)),
            None => Ok(()),
        }
    }

    /// Which of `images` need a pull according to their pull policies (each
    /// image once per platform), and the policies to fall back to when that
    /// pull fails. The policies are tried in order, like gitlab-runner: one
    /// that is satisfied by an image already present ends the search, one
    /// that pulls is planned, and `never` without the image moves on to the
    /// next. Fails when an image may not be pulled and is missing.
    async fn plan_pulls<'i>(
        &self,
        images: &[&ImageNeed<'i>],
        trace: &mut TraceWriter<'_>,
    ) -> std::result::Result<Vec<PlannedPull<'i>>, JobFailure> {
        let mut pulls: Vec<PlannedPull> = Vec::new();
        for need in images {
            let policies = match need.policies {
                // The runner's own helper image
                None => vec![PullPolicy::IfNotPresent],
                Some(policies) => PullPolicy::resolve(
                    policies,
                    &self.config.pull_policy,
                    &self.config.allowed_pull_policies,
                )
                .map_err(pull_failure)?,
            };
            let mut notes = String::new();
            let mut planned = None;
            for (index, &policy) in policies.iter().enumerate() {
                let attempt = index + 1;
                if attempt > 1 {
                    notes.push_str(&attempt_note(attempt, policy));
                }
                // `always` pulls whether the image is present or not
                if policy != PullPolicy::Always && self.present(need.name, need.platform).await {
                    notes.push_str(&format!("Using locally found image {}\n", need.name));
                    break;
                }
                if policy == PullPolicy::Never {
                    let error = format!(
                        "image {} is not present and pull_policy is never",
                        need.name
                    );
                    if attempt == policies.len() {
                        trace.write(&notes).await;
                        return Err(pull_failure(error));
                    }
                    notes.push_str(&format!(
                        "WARNING: Failed to pull image with policy \"never\": {}\n",
                        error
                    ));
                    continue;
                }
                planned = Some(PlannedPull {
                    image: need.name,
                    platform: need.platform,
                    policy,
                    attempt,
                    fallback: policies[attempt..].to_vec(),
                });
                break;
            }
            trace.write(&notes).await;
            if let Some(pull) = planned {
                if !pulls.iter().any(|planned| planned.is_for(need)) {
                    trace
                        .write(&format!("Pulling image {} ...\n", pull.describe()))
                        .await;
                    pulls.push(pull);
                }
            }
        }
        Ok(pulls)
    }

    /// Whether `image` is present, for `platform` when one is given
    async fn present(&self, image: &str, platform: Option<&str>) -> bool {
        match self
            .docker
            .inspect_image(&image::with_default_tag(image))
            .await
        {
            Ok(found) => platform.is_none_or(|wanted| platform_matches(&found, wanted)),
            Err(_) => false,
        }
    }

    /// Make `pulls` at the same time, with the registry credentials of
    /// `registry_login`, until `limits` stop them; what their fallbacks did
    /// goes to `log` as each pull ends
    async fn pull_all(
        &self,
        pulls: &[PlannedPull<'_>],
        job: &Job,
        limits: &Limits<'_>,
        log: &mut SetupLog<'_, '_>,
    ) -> JobOutcome {
        let pulling = async {
            let mut running: futures_util::stream::FuturesUnordered<_> = pulls
                .iter()
                .map(|pull| self.pull_with_fallback(pull, job))
                .collect();
            while let Some((outcome, notes)) = running.next().await {
                if !notes.is_empty() {
                    log.write(&notes).await;
                }
                outcome?;
            }
            Ok(())
        };
        limits
            .bound(pulling)
            .await
            .map_err(|stop| stop.failure(job))?
    }

    /// Make `pull`; when it fails, try its fallback policies in order (like
    /// gitlab-runner): `if-not-present` and `never` use the image when it is
    /// present, `always` and `if-not-present` pull again. Returns how the
    /// pull ended and what to log about the fallbacks.
    async fn pull_with_fallback(&self, pull: &PlannedPull<'_>, job: &Job) -> (JobOutcome, String) {
        let mut outcome = self.pull(pull.image, pull.platform, job).await;
        let mut notes = String::new();
        let mut tried = vec![pull.policy];
        for (index, &policy) in pull.fallback.iter().enumerate() {
            let Err(failure) = &outcome else {
                break;
            };
            notes.push_str(&format!(
                "WARNING: Failed to pull image with policy \"{}\": {}\n",
                tried[tried.len() - 1].name(),
                failure.message
            ));
            notes.push_str(&attempt_note(pull.attempt + index + 1, policy));
            tried.push(policy);
            if policy != PullPolicy::Always && self.present(pull.image, pull.platform).await {
                notes.push_str(&format!("Using locally found image {}\n", pull.image));
                outcome = Ok(());
            } else if policy != PullPolicy::Never {
                outcome = self.pull(pull.image, pull.platform, job).await;
            }
        }
        if tried.len() > 1 {
            outcome = outcome.map_err(|failure| {
                let tried: Vec<&str> = tried.iter().map(|policy| policy.name()).collect();
                pull_failure(format!(
                    "{} (pull policies tried: {})",
                    failure.message,
                    tried.join(", ")
                ))
            });
        }
        (outcome, notes)
    }

    /// Pull `image` for `platform` from its registry with the job's login for it
    async fn pull(&self, image: &str, platform: Option<&str>, job: &Job) -> JobOutcome {
        let (credentials, warnings) = self.registry_login(image, job).await;
        for warning in &warnings {
            warn!("Job {}: {}", job.id, warning);
        }
        // A pull that fails says why a login it may have needed was not used
        self.pull_shared(image, platform, credentials)
            .await
            .map_err(|mut failure| {
                for warning in warnings {
                    failure.message.push_str(&format!("\nWARNING: {}", warning));
                }
                failure
            })
    }

    /// Pull `image` for `platform` with `credentials`, or wait for the pull
    /// of the same image for the same platform with the same login that
    /// another job already started
    async fn pull_shared(
        &self,
        image: &str,
        platform: Option<&str>,
        credentials: Option<docker_api::DockerCredentials>,
    ) -> PullOutcome {
        use futures_util::FutureExt;

        let reference = image::with_default_tag(image);
        let key = (
            match platform {
                Some(platform) => format!("{} {}", reference, platform),
                None => reference,
            },
            login_identity(credentials.as_ref()),
        );
        let docker = Arc::clone(&self.docker);
        let image = image.to_string();
        let platform = platform.map(str::to_string);
        self.pulls
            .join(key, move || {
                async move { pull_image(&docker, &image, platform.as_deref(), credentials).await }
                    .boxed()
            })
            .await
    }

    /// Create a bridge network for the job. Usually the name is free; a
    /// leftover network of the name (409 Conflict) is removed first.
    async fn create_network(&self, name: &str) -> Result<()> {
        let request = docker_api::NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            labels: Some(self.labels()),
            check_duplicate: Some(true),
        };
        match self.docker.create_network(&request).await {
            Ok(()) => return Ok(()),
            Err(e) if e.status_code() == Some(409) => {}
            Err(e) => return Err(e).with_context(|| format!("Failed to create network {}", name)),
        }
        let _ = self.docker.remove_network(name).await;
        self.docker
            .create_network(&request)
            .await
            .with_context(|| format!("Failed to create network {}", name))?;
        Ok(())
    }

    /// Create a service container reachable by its aliases on the job
    /// network, for the platform and as the user of its `docker:` options
    #[allow(clippy::too_many_arguments)]
    async fn create_service(
        &self,
        job: &Job,
        name: &str,
        image: &str,
        service: &crate::gitlab::Service,
        options: &DockerOptions,
        values: &std::collections::HashMap<String, String>,
        host_config: HostConfig,
    ) -> Result<String> {
        use docker_api::{EndpointConfig, NetworkingConfig};

        let aliases = image::service_aliases(image, service.alias.as_deref());
        // Services run on the job's network, reachable there by their aliases
        let networking_config = host_config
            .network_mode
            .clone()
            .map(|network| NetworkingConfig {
                endpoints_config: Some(std::collections::HashMap::from([(
                    network,
                    EndpointConfig {
                        aliases: Some(aliases),
                    },
                )])),
            });
        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            user: options.user.clone(),
            env: Some(script::service_env(job, &service.variables, values)),
            entrypoint: service.entrypoint.clone(),
            cmd: service.command.clone(),
            host_config: Some(host_config),
            networking_config,
            ..Default::default()
        };
        self.create_named_for(name, config, options.platform.as_deref())
            .await
    }

    /// Wait until the services' exposed TCP ports accept connections, like
    /// gitlab-runner (up to `wait_for_services_timeout`, 30s by default, then
    /// a warning; not at all when it is -1), so scripts do not start against
    /// a database that is still booting. Services are checked in parallel,
    /// from the runner's host when it reaches the job network, otherwise from
    /// a container on that network. Returns how many services were checked
    /// from a container.
    async fn wait_for_services(
        &self,
        job_id: u64,
        network: &str,
        services: &[String],
        log: &mut SetupLog<'_, '_>,
        limits: &Limits<'_>,
    ) -> usize {
        let Some(wait) = self.config.service_wait() else {
            return 0;
        };
        let inspected = futures_util::future::join_all(
            services.iter().map(|id| self.docker.inspect_container(id)),
        )
        .await;
        let checks: Vec<ServiceCheck> = inspected
            .into_iter()
            .filter_map(|info| {
                let info = info.ok()?;
                let ports = service_ports(info.config.as_ref());
                (!ports.is_empty()).then(|| ServiceCheck {
                    host: info
                        .name
                        .as_deref()
                        .unwrap_or_default()
                        .trim_start_matches('/')
                        .to_string(),
                    address: host_reachable_address(&info, network),
                    ports,
                })
            })
            .collect();
        if checks.is_empty() {
            return 0;
        }
        log.write(&format!(
            "Waiting for services to be up and running (timeout {} seconds)...\n",
            wait.as_secs()
        ))
        .await;
        let from_host = futures_util::future::join_all(checks.iter().map(|check| async move {
            match check.address {
                Some(ip) => wait_from_host(ip, &check.ports, wait).await,
                None => HostWait::Unreachable,
            }
        }));
        // Stopped by the job timeout or a cancel, which the caller reports
        let Ok(waited) = limits.bound(from_host).await else {
            return 0;
        };
        let mut unreachable = Vec::new();
        for (check, waited) in checks.iter().zip(waited) {
            match waited {
                HostWait::Ready => {}
                HostWait::TimedOut => {
                    log.write(&format!("{}\n", service_timeout_warning(check, wait)))
                        .await
                }
                HostWait::Unreachable => unreachable.push(check),
            }
        }
        if !unreachable.is_empty() {
            self.wait_in_container(job_id, network, &unreachable, wait, log, limits)
                .await;
        }
        unreachable.len()
    }

    /// `wait_for_services` for `checks`, from a short-lived helper container
    /// on the job network, for up to `wait`
    async fn wait_in_container(
        &self,
        job_id: u64,
        network: &str,
        checks: &[&ServiceCheck],
        wait: Duration,
        log: &mut SetupLog<'_, '_>,
        limits: &Limits<'_>,
    ) {
        let tries = (wait.as_millis() / SERVICE_POLL.as_millis()).max(1);
        let mut script: Vec<String> = checks
            .iter()
            .map(|check| {
                // The service is up once any of its ports accepts connections
                let any_port = check
                    .ports
                    .iter()
                    .map(|port| format!("nc -z -w1 {} {}", check.host, port))
                    .collect::<Vec<_>>()
                    .join(" || ");
                format!(
                    "( i=0; until {any_port}; do i=$((i+1)); \
                     if [ $i -ge {tries} ]; then echo {warning}; break; fi; \
                     sleep {poll}; done ) &",
                    warning = script::quote(&service_timeout_warning(check, wait)),
                    poll = SERVICE_POLL.as_secs_f64(),
                )
            })
            .collect();
        script.push("wait".to_string());
        match self
            .run_helper(
                &format!("turboci-job-{}-svc-wait", job_id),
                &script.join("\n"),
                HostConfig {
                    network_mode: Some(network.to_string()),
                    ..Default::default()
                },
                false,
                None,
                Some(limits),
            )
            .await
        {
            Ok(output) => log.write(&output).await,
            Err(e) => warn!("Could not check services: {:#}", e),
        }
    }

    /// Run a shell script in a short-lived container of the helper image and
    /// return its output (only stdout when `stdout_only`). With `limits`, it
    /// is given up on (and its container removed) like a script.
    async fn run_helper(
        &self,
        name: &str,
        script: &str,
        host_config: HostConfig,
        stdout_only: bool,
        user: Option<String>,
        limits: Option<&Limits<'_>>,
    ) -> Result<String> {
        within(limits, self.pull_if_missing(&self.config.helper_image)).await?;
        let config = ContainerCreateBody {
            image: Some(self.config.helper_image.clone()),
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: Some(vec![script.to_string()]),
            host_config: Some(host_config),
            user,
            ..Default::default()
        };
        let id = self.create_named(name, config).await?;
        // The container is removed below however this ends
        let result = within(limits, async {
            self.docker.start_container(&id).await?;
            let mut wait = self.docker.wait_container(&id);
            tokio::time::timeout(CLEANUP_TIMEOUT, async {
                while let Some(status) = wait.next().await {
                    status?;
                }
                anyhow::Ok(())
            })
            .await
            .context("service check timed out")??;
            let mut logs = self.docker.logs(&id, true, !stdout_only);
            let mut output = String::new();
            while let Some(chunk) = logs.next().await {
                output.push_str(&chunk?.to_string());
            }
            anyhow::Ok(output)
        })
        .await;
        let _ = self.force_remove(&id).await;
        result
    }

    /// Create a container under a fixed name, replacing a leftover one
    async fn create_named(&self, name: &str, config: ContainerCreateBody) -> Result<String> {
        self.create_named_for(name, config, None).await
    }

    /// `create_named` of the image's variant for `platform` (dockerd's
    /// default when `None`)
    async fn create_named_for(
        &self,
        name: &str,
        mut config: ContainerCreateBody,
        platform: Option<&str>,
    ) -> Result<String> {
        config
            .labels
            .get_or_insert_with(Default::default)
            .extend(self.labels());

        // Usually the name is free; 409 Conflict when a container has it
        match self.docker.create_container(name, platform, &config).await {
            Ok(response) => return Ok(response.id),
            Err(e) if e.status_code() == Some(409) => {}
            Err(e) => return Err(e).context("Failed to create container"),
        }

        // A container with this name from another runner on the same Docker host
        // (same job id, different GitLab) must not be killed
        if let Ok(existing) = self.docker.inspect_container(name).await {
            let owner = existing
                .config
                .and_then(|config| config.labels)
                .and_then(|labels| labels.get(OWNER_LABEL).cloned());
            if owner.as_deref() != Some(self.owner.as_str()) {
                anyhow::bail!("container name {} is used by another runner", name);
            }
        }

        let _ = self.docker.remove_container(name, true, true).await;
        let response = self
            .docker
            .create_container(name, platform, &config)
            .await
            .context("Failed to create container")?;
        Ok(response.id)
    }

    /// Stop what of the job could still change the workspace and, unless the
    /// runner's user can read what the uploads need (`reads`) as it is, give
    /// the workspace back to its owner, the runner's user (files created in
    /// containers belong to root), so the runner can read them. The job
    /// container is killed, not removed: removing it, its volumes (a
    /// docker:dind's images) and the other containers, and a hand-back left
    /// for later, can wait until the job has been reported (see
    /// `Leftovers::finish`).
    async fn settle(
        &self,
        job_id: u64,
        job_dir: &Path,
        containers: &mut JobContainers,
        reads: &UploadReads,
    ) -> Settled {
        // The job's containers may have changed anything
        self.clean_workspaces.forget(job_dir);
        if containers.run.job.is_none() && containers.helper.is_none() {
            return Settled::Untouched;
        }
        if let Some(id) = &containers.run.job {
            if let Err(e) = self.stop(id).await {
                // A container that may still run can still change the workspace
                warn!("Failed to stop container {}: {:#}", id, e);
                match self.force_remove(id).await {
                    Ok(()) => containers.run.job = None,
                    Err(e) => {
                        warn!("Failed to remove container {}: {:#}", id, e);
                        return Settled::Not;
                    }
                }
            }
        }
        if uploads_can_read(job_dir, reads).await {
            tracing::debug!(
                "Job #{}: the uploads can read the workspace, handed back after the report",
                job_id
            );
            return Settled::Stopped;
        }
        self.hand_back(job_id, job_dir, containers.helper.as_deref(), false)
            .await
    }

    /// Give the workspace to its owner once nothing of the job runs any
    /// more. The sources `helper` (the trusted helper image, only the
    /// workspace mounted) does it, after a walk of the workspace found
    /// something to hand back when `walk_first`; without a helper, a fresh
    /// helper container does it when the walk finds something.
    async fn hand_back(
        &self,
        job_id: u64,
        job_dir: &Path,
        helper: Option<&str>,
        walk_first: bool,
    ) -> Settled {
        #[cfg(unix)]
        {
            let Ok(meta) = tokio::fs::metadata(job_dir).await else {
                return Settled::Not;
            };
            let stamp = WorkspaceStamp::of(&meta);
            if let Some(helper) = helper {
                if walk_first {
                    let (dir, uid, gid) = (job_dir.to_path_buf(), stamp.uid, stamp.gid);
                    let walk = tokio::task::spawn_blocking(move || owned_by_others(&dir, uid, gid));
                    if !walk.await.unwrap_or(true) {
                        return Settled::Owned(stamp);
                    }
                }
                let script = chown_script(stamp.uid, stamp.gid);
                let cmd = ["sh", "-c", script.as_str()];
                let chown = exec_as_root(&self.docker, helper, &cmd);
                match tokio::time::timeout(CLEANUP_TIMEOUT, chown).await {
                    Ok(Ok(0)) => return Settled::Owned(stamp),
                    Ok(Ok(code)) => warn!("Resetting workspace ownership exited with {}", code),
                    Ok(Err(e)) => warn!("Failed to reset workspace ownership: {:#}", e),
                    Err(_) => warn!("Resetting workspace ownership timed out"),
                }
            }
            match self.reset_ownership(job_id, job_dir, stamp).await {
                Ok(()) => Settled::Owned(stamp),
                Err(e) => {
                    warn!("Failed to reset workspace ownership: {:#}", e);
                    Settled::Not
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (job_id, job_dir, helper, walk_first);
            Settled::Not
        }
    }

    /// Kill container `id` and wait until it has stopped (one that is gone
    /// or not running counts as stopped)
    async fn stop(&self, id: &str) -> Result<()> {
        match self.docker.kill_container(id).await {
            Ok(()) => {}
            Err(e) if matches!(e.status_code(), Some(404 | 409)) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let mut wait = self.docker.wait_container(id);
        tokio::time::timeout(STOP_TIMEOUT, async {
            while let Some(status) = wait.next().await {
                match status {
                    // Killed: its exit code is not zero
                    Ok(_) | Err(docker_api::Error::ContainerWait { .. }) => {}
                    Err(e) if e.status_code() == Some(404) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
        .await
        .context("timed out")??;
        Ok(())
    }

    /// Remove the job's containers, all at the same time, and its network;
    /// true when every container is gone
    async fn remove(&self, containers: &JobContainers) -> bool {
        let removals = containers
            .run
            .job
            .iter()
            .chain(&containers.helper)
            .chain(&containers.run.services)
            .map(|id| async move {
                match self.force_remove(id).await {
                    Ok(()) => {
                        info!("🗑️  Removed container: {}", id);
                        true
                    }
                    Err(e) => {
                        warn!("Failed to remove container {}: {}", id, e);
                        false
                    }
                }
            });
        let all_removed = futures_util::future::join_all(removals)
            .await
            .into_iter()
            .all(|removed| removed);
        if let Some(network) = &containers.run.network {
            if let Err(e) = self.docker.remove_network(network).await {
                warn!("Failed to remove network {}: {}", network, e);
            }
        }
        all_removed
    }

    /// Pull a runner-internal image (no job credentials) when it is not present
    async fn pull_if_missing(&self, image: &str) -> Result<()> {
        if self.present(image, None).await {
            return Ok(());
        }
        self.pull_shared(image, None, None)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))
    }

    async fn force_remove(&self, id: &str) -> Result<()> {
        force_remove(&self.docker, id).await
    }

    /// Give the workspace back to its owner's uid/gid (`stamp`) from a
    /// fresh container of the (trusted) helper image, never from the job's
    /// own image. Skipped when everything already belongs to it, e.g. jobs
    /// running as the runner's user, or Docker Desktop.
    #[cfg(unix)]
    async fn reset_ownership(
        &self,
        job_id: u64,
        job_dir: &Path,
        stamp: WorkspaceStamp,
    ) -> Result<()> {
        let (uid, gid) = (stamp.uid, stamp.gid);
        let dir = job_dir.to_path_buf();
        let needed = tokio::task::spawn_blocking(move || owned_by_others(&dir, uid, gid)).await?;
        if !needed {
            return Ok(());
        }
        // Jobs without sources to check out never pulled the helper image
        self.pull_if_missing(&self.config.helper_image).await?;
        let config = ContainerCreateBody {
            image: Some(self.config.helper_image.clone()),
            user: Some("0".to_string()),
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: Some(vec![chown_script(uid, gid)]),
            network_disabled: Some(true),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = self
            .create_named(&format!("turboci-job-{}-cleanup", job_id), config)
            .await?;
        let result = async {
            self.docker.start_container(&id).await?;
            let mut wait = self.docker.wait_container(&id);
            tokio::time::timeout(CLEANUP_TIMEOUT, async {
                while let Some(status) = wait.next().await {
                    status?;
                }
                anyhow::Ok(())
            })
            .await
            .context("chown timed out")?
        }
        .await;
        let _ = self.force_remove(&id).await;
        // The script exited with 0: it gave everything to the owner
        result
    }
}

/// How the workspace was handed back once the job's processes stopped
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// No container of the job ran: nothing in it changed
    Untouched,
    /// Nothing of the job runs any more, and the runner's user can read
    /// what the uploads need: handing the workspace back is left to
    /// `Leftovers::finish`
    Stopped,
    /// Everything belongs to the workspace's owner, as `stamp` shows it,
    /// and nothing of the job runs any more
    #[cfg_attr(not(unix), allow(dead_code))]
    Owned(WorkspaceStamp),
    /// Handing back failed, or a container could not be stopped
    Not,
}

/// What a Docker job leaves once its steps ran (see `Teardown`): its
/// containers (the job container still idling, the sources helper, the
/// services), network and volumes
pub struct Leftovers {
    executor: DockerExecutor,
    job_id: u64,
    job_dir: PathBuf,
    containers: JobContainers,
    /// Once `prepare_uploads` ran
    settled: Option<Settled>,
}

impl Leftovers {
    /// Stop the job's container and, unless the runner's user can read what
    /// the uploads need (`reads`) as it is, give the workspace to that
    /// user, so cache and artifacts can be read
    pub(super) async fn prepare_uploads(&mut self, reads: &UploadReads) {
        if self.settled.is_none() {
            let settled = self
                .executor
                .settle(self.job_id, &self.job_dir, &mut self.containers, reads)
                .await;
            self.settled = Some(settled);
        }
    }

    /// The job's sources helper, which runs until `finish`
    pub(super) fn helper(&self) -> Option<&str> {
        self.containers.helper.as_deref()
    }

    /// Remove the containers, the network and the job container's volumes,
    /// and give the workspace to its owner when `prepare_uploads` did not.
    /// A workspace that belongs wholly to its owner afterwards, when that is
    /// the runner's own user and group, is marked clean for the next job's
    /// checkout (see `CleanWorkspaces`).
    pub(super) async fn finish(mut self) {
        let executor = self.executor.clone();
        let all_removed = if !matches!(self.settled, None | Some(Settled::Stopped)) {
            executor.remove(&self.containers).await
        } else {
            // Without uploads, or with uploads that did not need it, the
            // job container and the services are removed first, which
            // stops the job; the helper stays for handing the workspace
            // back, should a walk find it needed
            executor.clean_workspaces.forget(&self.job_dir);
            let containers = &self.containers;
            let touched = containers.run.job.is_some() || containers.helper.is_some();
            let job = JobContainers {
                helper: None,
                run: RunContainers {
                    network: None,
                    services: containers.run.services.clone(),
                    job: containers.run.job.clone(),
                },
            };
            let rest = JobContainers {
                helper: containers.helper.clone(),
                run: RunContainers {
                    network: containers.run.network.clone(),
                    ..RunContainers::default()
                },
            };
            let hand_back =
                executor.hand_back(self.job_id, &self.job_dir, rest.helper.as_deref(), true);
            let removed = if self.settled == Some(Settled::Stopped) {
                // The job container is stopped already (services do not
                // mount the workspace): hand back while they are removed
                let (removed, settled) = tokio::join!(executor.remove(&job), hand_back);
                self.settled = Some(settled);
                removed
            } else {
                let removed = executor.remove(&job).await;
                self.settled = Some(if touched {
                    hand_back.await
                } else {
                    Settled::Untouched
                });
                removed
            };
            executor.remove(&rest).await && removed
        };
        #[cfg(unix)]
        if let Some(Settled::Owned(stamp)) = self.settled {
            if all_removed && stamp.owned_by_runner() {
                self.executor.clean_workspaces.mark(&self.job_dir, stamp);
            }
        }
        #[cfg(not(unix))]
        let _ = all_removed;
    }

    /// Run `finish` in the background (a teardown that was dropped)
    pub(super) fn finish_later(self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(self.finish());
        }
    }
}

/// Script that gives everything below /builds to `uid:gid`, touching only
/// what belongs to someone else (a kept workspace is mostly the runner's
/// already). Symbolic links are changed themselves, never followed. A helper
/// image whose find cannot do this falls back to chown -R.
fn chown_script(uid: u32, gid: u32) -> String {
    format!(
        "find /builds \\( ! -user {uid} -o ! -group {gid} \\) -exec chown -h {uid}:{gid} {{}} + \
         || chown -R -h {uid}:{gid} /builds"
    )
}

/// Whether anything in `dir` (symbolic links not followed) belongs to
/// another user or group than `uid:gid`, or cannot be checked
#[cfg(unix)]
fn owned_by_others(dir: &Path, uid: u32, gid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    walkdir::WalkDir::new(dir).into_iter().any(|entry| {
        entry
            .and_then(|entry| entry.metadata())
            .map_or(true, |meta| meta.uid() != uid || meta.gid() != gid)
    })
}

/// Whether the runner's process (`reads.reader`) and git listing untracked
/// files (as the workspace's owner) can read what the uploads need in
/// `job_dir` as it is (see `uploads::readable`), off the async runtime
async fn uploads_can_read(job_dir: &Path, reads: &UploadReads) -> bool {
    use std::os::unix::fs::MetadataExt;

    let git = match tokio::fs::metadata(job_dir).await {
        Ok(meta) => uploads::Reader {
            uid: meta.uid(),
            groups: vec![meta.gid()],
        },
        Err(_) => return false,
    };
    let (job_dir, reads) = (job_dir.to_path_buf(), reads.clone());
    tokio::task::spawn_blocking(move || uploads::readable(&job_dir, &reads, Some(&git)))
        .await
        .unwrap_or(false)
}

/// Kill and remove a container with its anonymous volumes; one that is
/// already gone counts as removed
async fn force_remove(docker: &Docker, id: &str) -> Result<()> {
    match docker.remove_container(id, true, true).await {
        Ok(()) => Ok(()),
        Err(e) if e.status_code() == Some(404) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

struct DockerRunner<'a> {
    docker: &'a Docker,
    container_id: &'a str,
    env: Vec<String>,
}

impl DockerRunner<'_> {
    /// Kill every process in the container except its init (the idle loop), so a
    /// timed-out or canceled script stops before after_script runs. Runs the
    /// `kill` binary directly (no shell), as root, bounded in time; images
    /// without one (distroless, scratch-based) use sh's builtin `kill`
    /// instead: sh is always there, the container's idle loop runs in it.
    async fn stop_scripts(&self) {
        let mut problem = String::new();
        for cmd in [&["kill", "-KILL", "-1"][..], &["sh", "-c", "kill -9 -1"]] {
            match tokio::time::timeout(Duration::from_secs(10), self.exec_as_root(cmd)).await {
                Ok(Ok(0)) => return,
                Ok(Ok(code)) => problem = format!("{} exited with {}", cmd[0], code),
                Ok(Err(e)) => problem = format!("{}: {:#}", cmd[0], e),
                Err(_) => problem = format!("{}: timed out", cmd[0]),
            }
        }
        warn!("Could not stop the script: {}", problem);
    }

    /// Run `cmd` as root in the container and return its exit code
    async fn exec_as_root(&self, cmd: &[&str]) -> Result<i64> {
        exec_as_root(self.docker, self.container_id, cmd).await
    }
}

/// Run `cmd` as root in container `id` and return its exit code; its
/// output is discarded
async fn exec_as_root(docker: &Docker, id: &str, cmd: &[&str]) -> Result<i64> {
    let exec = docker
        .create_exec(
            id,
            &ExecConfig {
                cmd: Some(cmd.to_vec()),
                user: Some("0"),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await?;
    let mut output = docker.start_exec(&exec.id).await?;
    while output.next().await.is_some() {}
    let inspect = docker.inspect_exec(&exec.id).await?;
    Ok(inspect.exit_code.unwrap_or(-1))
}

impl ScriptRunner for DockerRunner<'_> {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) -> Result<RunStatus> {
        let exec = self
            .docker
            .create_exec(
                self.container_id,
                &ExecConfig {
                    cmd: Some(vec!["sh", "-c", SHELL_DETECT, "sh", script]),
                    env: Some(self.env.iter().map(String::as_str).collect()),
                    working_dir: Some(workdir),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let mut stdout = Utf8Decoder::default();
        let mut stderr = Utf8Decoder::default();
        let mut output = self.docker.start_exec(&exec.id).await?;
        // Created once, not for every chunk of output
        let deadline = tokio::time::sleep_until(limits.deadline);
        let cancel = limits.cancel.reached(limits.stop_at);
        tokio::pin!(deadline, cancel);
        let mut flush = FlushTimer::default();
        loop {
            let next = tokio::select! {
                next = output.next() => next,
                _ = &mut deadline => {
                    self.stop_scripts().await;
                    return Ok(RunStatus::TimedOut);
                }
                _ = &mut cancel => {
                    self.stop_scripts().await;
                    return Ok(RunStatus::Canceled);
                }
                // A quiet script must not keep its last lines from GitLab
                _ = flush.wait(trace.next_flush_at()) => {
                    trace.flush().await;
                    continue;
                }
            };
            let Some(chunk) = next else { break };
            let chunk = chunk?;
            if trace.is_truncated() {
                // Past the log limit the output is dropped: drain it, skip decoding
                trace.flush_if_due().await;
                continue;
            }
            let text = match chunk {
                LogOutput::StdErr { message } => stderr.decode(&message),
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    stdout.decode(&message)
                }
                LogOutput::StdIn { .. } => continue,
            };
            trace.write(&text).await;
        }
        trace.write(&stdout.finish()).await;
        trace.write(&stderr.finish()).await;

        let inspect = self.docker.inspect_exec(&exec.id).await?;
        let code = inspect.exit_code.unwrap_or(-1);
        Ok(RunStatus::Exited(i32::try_from(code).unwrap_or(-1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner_daemon::executor::tests::{job, steps};
    use crate::security::secret_scrubber::SecretScrubber;

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_executor_runs_job_and_cleans_up() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                default_image: "alpine:3.20".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 900_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "variables": [
                {"key": "GREETING", "value": "hello"},
                {"key": "API_KEY", "value": "top-secret-key", "masked": true},
                {"key": "CONFIG", "value": "file-content", "file": true}
            ],
            "steps": steps(
                &[
                    "cd /tmp",
                    "echo \"$GREETING from $PWD as $(id -u)\"",
                    "echo key=$API_KEY",
                    "cat \"$CONFIG\"",
                    "mkdir -p \"$CI_PROJECT_DIR/out\" && echo artifact > \"$CI_PROJECT_DIR/out/a.txt\"",
                    "exit 4"
                ],
                &["echo after-ran"]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&j);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, FailureReason::ScriptFailure, "{}", log);
        assert_eq!(failure.exit_code, Some(4), "{}", log);
        assert!(log.contains("hello from /tmp as 0"), "{}", log);
        assert!(log.contains("key=[MASKED]"), "{}", log);
        assert!(!log.contains("top-secret-key"));
        assert!(log.contains("file-content"), "{}", log);
        assert!(log.contains("after-ran"), "{}", log);

        // Files the container created as root are left for artifact upload...
        let job_dir = executor.job_dir(&j);
        assert_eq!(
            std::fs::read_to_string(job_dir.join("project/out/a.txt")).unwrap(),
            "artifact\n"
        );
        // ...the container is gone...
        let docker = Docker::connect_with_local_defaults().unwrap();
        assert!(docker
            .inspect_container(&format!("turboci-job-{}", job_id))
            .await
            .is_err());
        // ...and the runner's user can delete the workspace
        executor.cleanup(&j, false).await;
        assert!(!job_dir.exists());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_gets_tmpfs_mounts() {
        let docker_executor = DockerExecutor::new(DockerConfig {
            pull_policy: "if-not-present".to_string(),
            tmpfs: std::collections::BTreeMap::from([(
                "/scratch".to_string(),
                "rw,size=16m".to_string(),
            )]),
            ..DockerConfig::default()
        })
        .unwrap();
        docker_executor
            .pull_if_missing("alpine:3.20")
            .await
            .unwrap();
        let executor = ExecutorType::Docker(docker_executor);
        let job_id = 912_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "steps": steps(
                &[
                    "grep ' /scratch ' /proc/mounts",
                    "echo x > /scratch/f && echo tmpfs-writable",
                ],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("tmpfs /scratch tmpfs"), "{}", log);
        assert!(log.contains("size=16384k"), "{}", log);
        assert!(log.contains("tmpfs-writable"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    /// Only created, not started: a CPU weight needs a cgroup that nested
    /// test daemons cannot always provide
    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_accepts_the_cpu_weight_of_job_containers() {
        let executor = DockerExecutor::new(DockerConfig {
            cpu_shares: Some(512),
            ..DockerConfig::default()
        })
        .unwrap()
        .with_owner(&format!("cpu-shares-test-{}", std::process::id()));
        executor.pull_if_missing("alpine:3.20").await.unwrap();
        let id = executor
            .create_named(
                &format!("turboci-cpu-shares-test-{}", std::process::id()),
                ContainerCreateBody {
                    image: Some("alpine:3.20".to_string()),
                    host_config: Some(executor.job_host_config(Vec::new(), None)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let info = executor.docker.inspect_container(&id).await;
        executor.force_remove(&id).await.unwrap();

        let shares = info
            .unwrap()
            .host_config
            .and_then(|config| config.cpu_shares);
        assert_eq!(shares, Some(512));
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_gets_the_container_options() {
        let docker_executor = DockerExecutor::new(DockerConfig {
            pull_policy: "if-not-present".to_string(),
            extra_hosts: vec!["gitlab.turboci.test:10.1.2.3".to_string()],
            dns: vec!["10.9.8.7".to_string()],
            dns_search: vec!["corp.turboci.test".to_string()],
            cap_add: vec!["SYS_PTRACE".to_string()],
            cap_drop: vec!["NET_RAW".to_string()],
            security_opt: vec!["no-new-privileges".to_string()],
            devices: vec!["/dev/null:/dev/turboci-null".to_string()],
            sysctls: std::collections::BTreeMap::from([(
                "net.ipv4.ip_unprivileged_port_start".to_string(),
                "0".to_string(),
            )]),
            ulimit: std::collections::BTreeMap::from([(
                "nofile".to_string(),
                "1024:2048".to_string(),
            )]),
            runtime: Some("runc".to_string()),
            ..DockerConfig::default()
        })
        .unwrap();
        docker_executor
            .pull_if_missing("alpine:3.20")
            .await
            .unwrap();
        let executor = ExecutorType::Docker(docker_executor);
        let job_id = 913_000_000 + u64::from(std::process::id());
        // Bits of CapEff: 13 is CAP_NET_RAW, 19 CAP_SYS_PTRACE
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "steps": steps(
                &[
                    "grep gitlab.turboci.test /etc/hosts",
                    "cat /etc/resolv.conf",
                    "eff=$(awk '/^CapEff/ {print $2}' /proc/self/status)",
                    "echo net_raw=$(( (0x$eff >> 13) & 1 )) sys_ptrace=$(( (0x$eff >> 19) & 1 ))",
                    "grep NoNewPrivs /proc/self/status",
                    "test -c /dev/turboci-null && echo device-present",
                    "echo port_start=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)",
                    "echo nofile=$(ulimit -n)/$(ulimit -Hn)",
                ],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();
        executor.cleanup(&j, false).await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("10.1.2.3\tgitlab.turboci.test"), "{}", log);
        assert!(log.contains("nameserver 10.9.8.7"), "{}", log);
        assert!(log.contains("search corp.turboci.test"), "{}", log);
        assert!(log.contains("net_raw=0 sys_ptrace=1"), "{}", log);
        assert!(log.contains("NoNewPrivs:\t1"), "{}", log);
        assert!(log.contains("device-present"), "{}", log);
        assert!(log.contains("port_start=0"), "{}", log);
        assert!(log.contains("nofile=1024/2048"), "{}", log);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_image_collection_removes_only_unused_images() {
        let executor = DockerExecutor::new(DockerConfig {
            default_image: "alpine:3.20".to_string(),
            image_max_age_days: Some(0),
            ..DockerConfig::default()
        })
        .unwrap()
        .with_owner(&format!("image-gc-test-{}", std::process::id()));
        executor.pull_if_missing("alpine:3.20").await.unwrap();
        let docker = &executor.docker;
        let repo = format!("turboci-gc-test-{}", std::process::id());
        // Three images of their own: committed containers with different commands
        let mut ids = Vec::new();
        for tag in ["old", "inuse", "held"] {
            let container = executor
                .create_named(
                    &format!("{}-{}", repo, tag),
                    ContainerCreateBody {
                        image: Some("alpine:3.20".to_string()),
                        cmd: Some(vec!["echo".to_string(), tag.to_string()]),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            ids.push(
                docker
                    .commit_container(&container, &repo, tag)
                    .await
                    .unwrap(),
            );
            executor.force_remove(&container).await.unwrap();
        }
        // A (stopped) container uses one, a running job another
        let user = executor
            .create_named(
                &format!("{}-user", repo),
                ContainerCreateBody {
                    image: Some(format!("{}:inuse", repo)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let held = executor.images.hold(&[&format!("{}:held", repo)]).await;
        let ours = |image: &docker_api::ImageSummary| {
            image_tags(image)
                .iter()
                .any(|tag| tag.starts_with(&format!("{}:", repo)))
        };

        let removed = executor.collect_images_where(ours).await;
        let present = |tag: &str| {
            let name = format!("{}:{}", repo, tag);
            async move { docker.inspect_image(&name).await.is_ok() }
        };
        let (old, inuse, kept) = (
            present("old").await,
            present("inuse").await,
            present("held").await,
        );
        let alpine = docker.inspect_image("alpine:3.20").await.is_ok();
        // Once the job ended, its image is collected too
        drop(held);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let later = executor.collect_images_where(ours).await;
        let held_gone = !present("held").await;
        executor.force_remove(&user).await.unwrap();
        for tag in ["inuse", "held"] {
            let _ = docker.remove_image(&format!("{}:{}", repo, tag)).await;
        }

        assert_eq!(removed, [format!("{}:old", repo)]);
        assert!(
            !old && inuse && kept && alpine,
            "{} {} {} {}",
            old,
            inuse,
            kept,
            alpine
        );
        assert_eq!(later, [format!("{}:held", repo)]);
        assert!(held_gone);
        // No limits configured: nothing is collected
        let off = DockerExecutor::new(DockerConfig::default()).unwrap();
        assert!(off.collect_images().await.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_executor_runs_services_with_limits() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                memory: Some("256m".to_string()),
                pids_limit: Some(128),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 910_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine", "pull_policy": ["if-not-present"]},
            "services": [{"name": "redis:7-alpine", "alias": "cache"}],
            "steps": steps(
                &[
                    "for i in $(seq 1 20); do redis-cli -h redis ping >/dev/null 2>&1 && break; sleep 0.5; done",
                    "echo \"by-image $(redis-cli -h redis ping)\"",
                    "echo \"by-alias $(redis-cli -h cache ping)\"",
                    "echo \"memory $(cat /sys/fs/cgroup/memory.max 2>/dev/null || cat /sys/fs/cgroup/memory/memory.limit_in_bytes)\"",
                    "echo \"pids $(cat /sys/fs/cgroup/pids.max 2>/dev/null || cat /sys/fs/cgroup/pids/pids.max)\""
                ],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("by-image PONG"), "{}", log);
        assert!(log.contains("by-alias PONG"), "{}", log);
        assert!(log.contains("memory 268435456"), "{}", log);
        assert!(log.contains("pids 128"), "{}", log);

        // Job container, service container and job network are all gone
        let docker = Docker::connect_with_local_defaults().unwrap();
        for name in [
            format!("turboci-job-{}", job_id),
            format!("turboci-job-{}-svc-0", job_id),
        ] {
            assert!(docker.inspect_container(&name).await.is_err());
        }
        assert!(docker
            .inspect_network(&format!("turboci-job-{}", job_id))
            .await
            .is_err());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_runner_services_shm_and_persistent_volumes() {
        use crate::runner_daemon::config::ServiceConfig;

        let owner = format!("test-volumes-{}", std::process::id());
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                shm_size: Some("256m".to_string()),
                volumes: vec!["/persist".to_string()],
                services: vec![ServiceConfig {
                    name: "redis:7-alpine".to_string(),
                    alias: Some("runnercache".to_string()),
                    ..ServiceConfig::default()
                }],
                ..DockerConfig::default()
            })
            .unwrap()
            .with_owner(&owner),
        );
        let project = 900_000 + u64::from(std::process::id());
        let run = |id: u64, script: &'static [&'static str]| {
            job(serde_json::json!({
                "id": id, "token": "t",
                "job_info": {"name": "t", "stage": "test", "project_id": project, "project_name": "p"},
                "image": {"name": "redis:7-alpine"},
                "steps": steps(script, &[])
            }))
        };
        let first_id = 980_000_000 + u64::from(std::process::id());
        let first = run(
            first_id,
            &[
                "for i in $(seq 1 20); do redis-cli -h runnercache ping >/dev/null 2>&1 && break; sleep 0.5; done",
                "echo \"runner-service $(redis-cli -h runnercache ping)\"",
                "echo \"shm $(df -k /dev/shm | awk 'NR==2{print $2}')\"",
                "echo kept > /persist/marker",
            ],
        );
        let second = run(first_id + 1, &["echo \"persisted $(cat /persist/marker)\""]);

        let scrubber = SecretScrubber::new(vec![]);
        let mut log = String::new();
        for j in [&first, &second] {
            let mut trace = TraceWriter::new(None, j.id, "t", &scrubber);
            let outcome = executor.execute(j, &mut trace, &NoRestore).await;
            trace.finish().await;
            assert!(outcome.is_ok(), "{:?}\n{}", outcome, trace.text());
            log.push_str(&trace.text());
            executor.cleanup(j, false).await;
        }

        assert!(log.contains("runner-service PONG"), "{}", log);
        assert!(log.contains("shm 262144"), "{}", log);
        assert!(log.contains("persisted kept"), "{}", log);

        // The volume is labelled with the runner, so it can be found and removed
        let docker = Docker::connect_with_local_defaults().unwrap();
        let volumes = docker
            .list_volumes(&std::collections::HashMap::from([(
                "label".to_string(),
                vec![format!("{}={}", OWNER_LABEL, owner)],
            )]))
            .await
            .unwrap()
            .volumes
            .unwrap_or_default();
        assert_eq!(volumes.len(), 1, "{:?}", volumes);
        for volume in volumes {
            docker.remove_volume(&volume.name).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_executor_checks_out_sources_without_git_in_job_image() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 920_000_000 + u64::from(std::process::id());
        // A job without git_info.protected gets a workspace of its own
        let job_dir = executor.builds_root().join(format!("job-{}", job_id));

        // Origin repository inside the workspace, visible to containers as /builds/origin.git
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = job_dir.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello from git\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &job_dir,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );

        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "git_info": {
                "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                "sha": sha, "before_sha": "", "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
            },
            // The origin repo belongs to the host user, not root in the helper
            "variables": [
                {"key": "GIT_CONFIG_COUNT", "value": "1"},
                {"key": "GIT_CONFIG_KEY_0", "value": "safe.directory"},
                {"key": "GIT_CONFIG_VALUE_0", "value": "*"}
            ],
            "steps": steps(&["cat file.txt", "command -v git || echo no-git-in-job-image"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("hello from git"), "{}", log);
        assert!(log.contains("no-git-in-job-image"), "{}", log);
        let docker = Docker::connect_with_local_defaults().unwrap();
        assert!(docker
            .inspect_container(&format!("turboci-job-{}-sources", job_id))
            .await
            .is_err());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_as_root_uses_git_on_the_runners_checkout() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let project = 937_000_000 + u64::from(std::process::id());
        let workspace = executor
            .builds_root()
            .join(format!("project-{}-0", project));
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = workspace.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &workspace,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );
        // The checkout belongs to the runner's user (uid 1000 plays it when
        // the tests run as root), the job's git runs as root
        std::fs::create_dir_all(workspace.join("project")).unwrap();
        if std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&workspace).unwrap()) == 0 {
            for entry in walkdir::WalkDir::new(&workspace) {
                std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
            }
        }

        let j = job(serde_json::json!({
            "id": project, "token": "t", "allow_git_fetch": true,
            "image": {"name": DockerConfig::default().helper_image, "entrypoint": [""]},
            "job_info": {"name": "build", "stage": "test", "project_id": project, "project_name": "app"},
            "git_info": {
                "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                "sha": sha, "before_sha": "", "protected": false,
                "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
            },
            // The job's own git settings still apply next to the runner's
            "variables": [
                {"key": "GIT_CONFIG_COUNT", "value": "1"},
                {"key": "GIT_CONFIG_KEY_0", "value": "core.abbrev"},
                {"key": "GIT_CONFIG_VALUE_0", "value": "12"}
            ],
            "steps": steps(&[
                "echo \"job-user=$(id -u) checkout-owner=$(stat -c %u .git)\"",
                "echo \"short=$(git rev-parse --short HEAD)\"",
                "git status --porcelain"
            ], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, j.id, "t", &scrubber);
        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();
        executor.cleanup(&j, false).await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("job-user=0 checkout-owner="), "{}", log);
        assert!(!log.contains("checkout-owner=0\n"), "{}", log);
        assert!(log.contains(&format!("short={}\n", &sha[..12])), "{}", log);
        assert!(!log.contains("dubious ownership"), "{}", log);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_next_job_fetches_into_the_workspace_root_files_included() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let project = 930_000_000 + u64::from(std::process::id());
        let workspace = executor
            .builds_root()
            .join(format!("project-{}-0", project));
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // Origin next to the project in the workspace, seen as /builds/origin.git
        let work = workspace.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello from git\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &workspace,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );

        let run = |id: u64, script: &'static [&'static str]| {
            let j = job(serde_json::json!({
                "id": id, "token": "t", "allow_git_fetch": true,
                "image": {"name": "alpine:3.20"},
                "job_info": {"name": "build", "stage": "test", "project_id": project, "project_name": "app"},
                "git_info": {
                    "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                    "sha": sha, "before_sha": "", "protected": false,
                    "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
                },
                // The origin repo belongs to the host user, not root in the helper
                "variables": [
                    {"key": "GIT_CONFIG_COUNT", "value": "1"},
                    {"key": "GIT_CONFIG_KEY_0", "value": "safe.directory"},
                    {"key": "GIT_CONFIG_VALUE_0", "value": "*"}
                ],
                "steps": steps(script, &[])
            }));
            let executor = executor.clone();
            async move {
                let scrubber = SecretScrubber::new(vec![]);
                let mut trace = TraceWriter::new(None, j.id, "t", &scrubber);
                let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
                trace.finish().await;
                (outcome, trace.text(), j)
            }
        };

        // Files the job creates belong to root in the container
        let (outcome, log, _) = run(project, &["echo x > leftover"]).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        let (outcome, log, last) = run(project + 1, &["test ! -e leftover", "cat file.txt"]).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(
            log.contains("Reusing the checkout of an earlier job"),
            "{}",
            log
        );
        assert!(!log.contains("cloning again"), "{}", log);
        assert!(log.contains("hello from git"), "{}", log);
        executor.cleanup(&last, false).await;
        assert!(!workspace.exists());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_checkout_belongs_to_the_workspace_owner() {
        use std::os::unix::fs::MetadataExt;

        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let project = 935_000_000 + u64::from(std::process::id());
        let workspace = executor
            .builds_root()
            .join(format!("project-{}-0", project));
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = workspace.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello from git\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &workspace,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );
        // The runner's user owns the workspace; as root, uid 1000 plays it,
        // and the directories the runner creates have to be its too
        std::fs::create_dir_all(workspace.join("project")).unwrap();
        let meta = std::fs::metadata(&workspace).unwrap();
        let owner = if meta.uid() == 0 {
            for entry in walkdir::WalkDir::new(&workspace) {
                std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
            }
            "1000:1000".to_string()
        } else {
            format!("{}:{}", meta.uid(), meta.gid())
        };

        let run = |id: u64, hooks: serde_json::Value| {
            let j = job(serde_json::json!({
                "id": id, "token": "t", "allow_git_fetch": true,
                "image": {"name": "alpine:3.20"},
                "job_info": {"name": "build", "stage": "test", "project_id": project, "project_name": "app"},
                "git_info": {
                    "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                    "sha": sha, "before_sha": "", "protected": false,
                    "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
                },
                // Root in the helper reads the origin of another user
                "variables": [
                    {"key": "GIT_CONFIG_COUNT", "value": "1"},
                    {"key": "GIT_CONFIG_KEY_0", "value": "safe.directory"},
                    {"key": "GIT_CONFIG_VALUE_0", "value": "*"}
                ],
                "hooks": hooks,
                // .git/config is written anew by every checkout
                "steps": steps(&["stat -c 'config-owner=%u:%g' .git/config", "cat file.txt"], &[])
            }));
            let executor = executor.clone();
            async move {
                let scrubber = SecretScrubber::new(vec![]);
                let mut trace = TraceWriter::new(None, j.id, "t", &scrubber);
                let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
                trace.finish().await;
                (outcome, trace.text(), j)
            }
        };

        // A clone, then a fetch into the kept checkout, both as the owner
        let (outcome, log, _) = run(project, serde_json::json!([])).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains(&format!("config-owner={}", owner)), "{}", log);
        assert!(log.contains("hello from git"), "{}", log);
        // Left clean for the next checkout, when the runner's user owns the
        // workspace (as root, uid 1000 only plays the owner)
        let ExecutorType::Docker(docker_executor) = &executor else {
            unreachable!()
        };
        let stamp = WorkspaceStamp::of(&std::fs::metadata(&workspace).unwrap());
        assert_eq!(
            docker_executor.clean_workspaces.lock().get(&workspace),
            stamp.owned_by_runner().then_some(&stamp)
        );
        let (outcome, log, _) = run(project + 1, serde_json::json!([])).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(
            log.contains("Reusing the checkout of an earlier job"),
            "{}",
            log
        );
        assert!(!log.contains("cloning again"), "{}", log);
        assert!(log.contains(&format!("config-owner={}", owner)), "{}", log);

        // With a get-sources hook the helper stays root
        let (outcome, log, last) = run(
            project + 2,
            serde_json::json!([{"name": "pre_get_sources_script", "script": ["echo hook-ran"]}]),
        )
        .await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("hook-ran"), "{}", log);
        assert!(log.contains("config-owner=0:0"), "{}", log);
        assert!(!log.contains("cloning again"), "{}", log);

        executor.cleanup(&last, false).await;
        assert!(!workspace.exists());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_replacing_sh_cannot_hang_cleanup() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 931_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "steps": [{"name": "script", "when": "on_success", "timeout": 60, "script": [
                "mkdir -p out && echo x > out/root-owned",
                "rm /bin/sh && printf '#!/bin/busybox ash\\nexec sleep 100000\\n' > /bin/sh && chmod +x /bin/sh"
            ]}]
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = tokio::time::timeout(
            Duration::from_secs(90),
            executor.execute(&j, &mut trace, &NoRestore),
        )
        .await
        .expect("cleanup hung on the job's /bin/sh");
        trace.finish().await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, trace.text());
        executor.cleanup(&j, false).await;
        assert!(!executor.job_dir(&j).exists(), "workspace not removable");
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_jobs_die_quietly_on_a_closed_pipe() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 941_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "steps": steps(&[crate::runner_daemon::executor::tests::PIPE_CHECK], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        crate::runner_daemon::executor::tests::assert_default_sigpipe(&log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_uses_bash_and_expands_image_variables() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 940_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "${BASE_IMAGE}"},
            "variables": [{"key": "BASE_IMAGE", "value": "debian:bookworm-slim"}],
            "steps": steps(
                &["[[ 1 == 1 ]] && echo \"bash-syntax-ok\"", "arr=(a b); echo \"array=${arr[1]}\""],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("image debian:bookworm-slim"), "{}", log);
        assert!(log.contains("bash-syntax-ok"), "{}", log);
        assert!(log.contains("array=b"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_services_wait_get_their_variables_and_volumes_but_no_secrets() {
        let shared = tempfile::tempdir().unwrap();
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                volumes: vec![format!("{}:/shared", shared.path().display())],
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 950_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine"},
            "variables": [
                {"key": "PUBLIC_VAR", "value": "pub", "public": true},
                {"key": "SECRET_VAR", "value": "hidden-secret", "masked": true}
            ],
            "services": [{
                "name": "redis:7-alpine",
                "command": ["sh", "-c",
                    "umask 022; echo \"svc=$SVC_ONLY public=$PUBLIC_VAR secret=$SECRET_VAR\" > /shared/env.txt; sleep 3; exec redis-server"],
                "variables": [{"key": "SVC_ONLY", "value": "svc-value"}]
            }],
            // No retry loop: the runner must wait for the service's port
            "steps": steps(&["echo \"ping=$(redis-cli -h redis ping)\""], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("Waiting for services"), "{}", log);
        assert!(log.contains("ping=PONG"), "{}", log);
        assert_eq!(
            std::fs::read_to_string(shared.path().join("env.txt")).unwrap(),
            "svc=svc-value public=pub secret=\n"
        );
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_sweep_removes_only_own_leftovers() {
        let owner = format!("sweep-test-{}", std::process::id());
        let mine = DockerExecutor::new(DockerConfig::default())
            .unwrap()
            .with_owner(&owner);
        let other = DockerExecutor::new(DockerConfig::default())
            .unwrap()
            .with_owner(&format!("{}-other", owner));
        mine.pull_if_missing("alpine:3.20").await.unwrap();
        let idle = |name: &str| ContainerCreateBody {
            image: Some("alpine:3.20".to_string()),
            cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
            labels: Some(std::collections::HashMap::from([(
                "name".to_string(),
                name.to_string(),
            )])),
            ..Default::default()
        };
        let leftover = mine
            .create_named(&format!("{}-a", owner), idle("a"))
            .await
            .unwrap();
        mine.create_network(&format!("{}-net", owner))
            .await
            .unwrap();
        let foreign = other
            .create_named(&format!("{}-b", owner), idle("b"))
            .await
            .unwrap();

        mine.sweep_orphans().await;

        let docker = Docker::connect_with_local_defaults().unwrap();
        let gone = docker.inspect_container(&leftover).await.is_err();
        let kept = docker.inspect_container(&foreign).await.is_ok();
        let net_gone = docker
            .inspect_network(&format!("{}-net", owner))
            .await
            .is_err();
        let _ = other.force_remove(&foreign).await;
        assert!(gone, "own leftover container not removed");
        assert!(net_gone, "own leftover network not removed");
        assert!(kept, "another runner's container was removed");
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_concurrent_pulls_of_one_image_share_the_pull_and_its_error() {
        let executor = DockerExecutor::new(DockerConfig::default()).unwrap();
        let image = "busybox:1.36";
        let _ = executor.docker.remove_image(image).await;

        let (a, b) = tokio::join!(
            executor.pull_shared(image, None, None),
            executor.pull_shared(image, None, None)
        );
        assert!(a.is_ok() && b.is_ok(), "{:?} {:?}", a, b);
        assert!(executor.docker.inspect_image(image).await.is_ok());
        assert_eq!(executor.pulls.len(), 0);

        let missing = "turboci-missing.invalid/none:1";
        let (a, b) = tokio::join!(
            executor.pull_shared(missing, None, None),
            executor.pull_shared(missing, None, None)
        );
        let (a, b) = (a.unwrap_err(), b.unwrap_err());
        assert_eq!(a.reason, FailureReason::ImagePullFailure);
        assert_eq!(a.message, b.message);
        assert!(a.message.contains(missing), "{}", a.message);
        assert_eq!(executor.pulls.len(), 0);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_service_ports_are_probed_from_the_host_or_a_container() {
        let owner = format!("svc-wait-test-{}", std::process::id());
        let executor = DockerExecutor::new(DockerConfig::default())
            .unwrap()
            .with_owner(&owner);
        executor.pull_if_missing("redis:7-alpine").await.unwrap();
        let job_id = 913_000_000 + u64::from(std::process::id());
        let network = format!("turboci-job-{}", job_id);
        executor.create_network(&network).await.unwrap();
        let j = job(serde_json::json!({"id": job_id, "token": "t"}));
        let start_service = |index: usize| {
            let executor = &executor;
            let j = &j;
            let network = &network;
            async move {
                let service: crate::gitlab::Service = serde_json::from_value(serde_json::json!({
                    "name": "redis:7-alpine",
                    "command": ["sh", "-c", "sleep 2; exec redis-server"]
                }))
                .unwrap();
                let id = executor
                    .create_service(
                        j,
                        &format!("turboci-job-{}-svc-{}", job_id, index),
                        "redis:7-alpine",
                        &service,
                        &DockerOptions::default(),
                        &std::collections::HashMap::new(),
                        executor.host_config(Vec::new(), Some(network.as_str())),
                    )
                    .await
                    .unwrap();
                executor.docker.start_container(&id).await.unwrap();
                id
            }
        };
        let cancel = CancelSignal::default();
        let limits = Limits {
            deadline: Instant::now() + Duration::from_secs(120),
            cancel: &cancel,
            stop_at: RemoteState::Canceling,
        };
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        // From the host, which reaches a local bridge (Linux)
        let first = start_service(0).await;
        let started = Instant::now();
        let in_container = executor
            .wait_for_services(
                job_id,
                &network,
                std::slice::from_ref(&first),
                &mut SetupLog::Trace(&mut trace),
                &limits,
            )
            .await;
        let host_wait = started.elapsed();

        // From a container on the job network, as without a local bridge
        let second = start_service(1).await;
        let started = Instant::now();
        let check = ServiceCheck {
            host: format!("turboci-job-{}-svc-1", job_id),
            ports: vec![6379],
            address: None,
        };
        executor
            .wait_in_container(
                job_id,
                &network,
                &[&check],
                SERVICE_WAIT,
                &mut SetupLog::Trace(&mut trace),
                &limits,
            )
            .await;
        let container_wait = started.elapsed();
        trace.finish().await;
        let log = trace.text();

        for id in [&first, &second] {
            executor.force_remove(id).await.unwrap();
        }
        executor.docker.remove_network(&network).await.unwrap();
        assert_eq!(in_container, 0, "{}", log);
        assert!(host_wait >= Duration::from_millis(1500), "{:?}", host_wait);
        assert!(
            container_wait >= Duration::from_millis(1500),
            "{:?}",
            container_wait
        );
        assert!(log.contains("Waiting for services"), "{}", log);
        assert!(!log.contains("WARNING"), "{}", log);
    }

    /// Names of containers left of job `job_id`
    async fn leftovers(job_id: u64) -> Vec<String> {
        let docker = Docker::connect_with_local_defaults().unwrap();
        docker
            .list_containers(
                true,
                &std::collections::HashMap::from([(
                    "name".to_string(),
                    vec![format!("turboci-job-{}", job_id)],
                )]),
            )
            .await
            .unwrap()
            .into_iter()
            .flat_map(|container| container.names.unwrap_or_default())
            .collect()
    }

    /// A job checking out a local repository (its workspace's origin.git)
    /// next to a redis service that starts listening after `service_delay`
    /// seconds; `hooks` and `script` as given
    fn pipelined_job(
        executor: &ExecutorType,
        job_id: u64,
        service: &str,
        service_delay: u32,
        hooks: serde_json::Value,
        script: &[&str],
    ) -> Job {
        let job_dir = executor.builds_root().join(format!("job-{}", job_id));
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = job_dir.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello from git\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &job_dir,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );
        job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine"},
            "git_info": {
                "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                "sha": sha, "before_sha": "", "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
            },
            "variables": [
                {"key": "GIT_CONFIG_COUNT", "value": "1"},
                {"key": "GIT_CONFIG_KEY_0", "value": "safe.directory"},
                {"key": "GIT_CONFIG_VALUE_0", "value": "*"}
            ],
            "services": [{
                "name": service, "alias": "cache",
                "command": ["sh", "-c", format!("sleep {}; exec redis-server", service_delay)]
            }],
            "hooks": hooks,
            "steps": steps(script, &["echo after-ran"])
        }))
    }

    /// An executor whose job, helper and service images are present
    async fn with_pipeline_images() -> ExecutorType {
        let executor = DockerExecutor::new(DockerConfig {
            pull_policy: "if-not-present".to_string(),
            ..DockerConfig::default()
        })
        .unwrap();
        for image in ["redis:7-alpine", "alpine/git:latest"] {
            executor.pull_if_missing(image).await.unwrap();
        }
        ExecutorType::Docker(executor)
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_checkout_runs_while_services_start() {
        let executor = with_pipeline_images().await;
        let job_id = 914_000_000 + u64::from(std::process::id());
        // Run one after the other, the hook and the service delay would
        // take at least 20s; overlapped about 10s plus setup, which a busy CI
        // host can stretch by several seconds
        let j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            10,
            serde_json::json!([{"name": "pre_get_sources_script", "script": ["sleep 10"]}]),
            &["cat file.txt", "echo \"ping=$(redis-cli -h cache ping)\""],
        );
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let started = Instant::now();
        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        let took = started.elapsed();
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("hello from git"), "{}", log);
        assert!(log.contains("ping=PONG"), "{}", log);
        assert!(took < Duration::from_secs(18), "took {:?}\n{}", took, log);
        // The services' progress follows the checkout's section, in a
        // section of its own, after the executor's preparation
        let at = |text: &str| {
            log.find(text)
                .unwrap_or_else(|| panic!("no {:?} in\n{}", text, log))
        };
        let prepared = at(":prepare_executor\r");
        let sources_start = at(":get_sources\r");
        let sources_end = log.rfind(":get_sources\r").unwrap();
        let containers = at(":prepare_containers\r");
        let service = at("Starting service");
        assert!(prepared < sources_start, "{}", log);
        assert!(sources_start < sources_end, "{}", log);
        assert!(sources_end < containers && containers < service, "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_failed_service_stops_the_checkout_and_leaves_nothing() {
        let executor = with_pipeline_images().await;
        let job_id = 915_000_000 + u64::from(std::process::id());
        let j = pipelined_job(
            &executor,
            job_id,
            "turboci-missing.invalid/redis:1",
            0,
            serde_json::json!([{"name": "pre_get_sources_script", "script": ["sleep 60"]}]),
            &["echo script-ran"],
        );
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let started = Instant::now();
        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        let took = started.elapsed();
        trace.finish().await;
        let log = trace.text();

        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, FailureReason::ImagePullFailure, "{}", log);
        assert!(
            failure.message.contains("turboci-missing.invalid"),
            "{}",
            failure.message
        );
        assert!(took < Duration::from_secs(30), "took {:?}\n{}", took, log);
        assert!(
            !log.contains("script-ran") && !log.contains("after-ran"),
            "{}",
            log
        );
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_failed_checkout_still_runs_after_script_next_to_services() {
        let executor = with_pipeline_images().await;
        let job_id = 916_000_000 + u64::from(std::process::id());
        let mut j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            0,
            serde_json::json!([]),
            &["echo script-ran"],
        );
        j.git_info.as_mut().unwrap().repo_url = "file:///builds/missing.git".to_string();
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::ScriptFailure,
            "{}",
            log
        );
        assert!(!log.contains("\nscript-ran"), "{}", log);
        assert!(log.contains("after-ran"), "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_cancel_stops_checkout_and_service_startup() {
        let executor = with_pipeline_images().await;
        let job_id = 917_000_000 + u64::from(std::process::id());
        // Neither the hook nor the service would be done before the cancel
        let j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            60,
            serde_json::json!([{"name": "pre_get_sources_script", "script": ["sleep 60"]}]),
            &["echo script-ran"],
        );
        let scrubber = SecretScrubber::new(vec![]);
        let cancel = CancelSignal::default();
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber).with_cancel(cancel.clone());
        let trigger = async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            cancel.update(RemoteState::Canceling);
        };

        let started = Instant::now();
        let (outcome, ()) = tokio::join!(executor.execute(&j, &mut trace, &NoRestore), trigger);
        let took = started.elapsed();
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobCanceled,
            "{}",
            log
        );
        assert!(took < Duration::from_secs(20), "took {:?}\n{}", took, log);
        assert!(!log.contains("script-ran"), "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_timeout_stops_checkout_and_service_startup() {
        let executor = with_pipeline_images().await;
        let job_id = 918_000_000 + u64::from(std::process::id());
        let mut j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            60,
            serde_json::json!([{"name": "pre_get_sources_script", "script": ["sleep 60"]}]),
            &["echo script-ran"],
        );
        j.runner_info = serde_json::from_value(serde_json::json!({"timeout": 4})).unwrap();
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let started = Instant::now();
        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        let took = started.elapsed();
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout,
            "{}",
            log
        );
        assert!(took < Duration::from_secs(20), "took {:?}\n{}", took, log);
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    /// An executor with alpine:3.20 already present: the job timeout includes
    /// image pulls, which short timeouts in tests must not depend on
    async fn with_alpine_pulled() -> ExecutorType {
        let executor = DockerExecutor::new(DockerConfig {
            pull_policy: "if-not-present".to_string(),
            ..DockerConfig::default()
        })
        .unwrap();
        executor.pull_if_missing("alpine:3.20").await.unwrap();
        ExecutorType::Docker(executor)
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_timeout_stops_the_script_before_after_script() {
        let executor = with_alpine_pulled().await;
        let job_id = 960_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "runner_info": {"timeout": 4},
            "steps": steps(
                &["sleep 30"],
                &["if ps | grep -v grep | grep -q 'sleep 30'; then echo script-still-running; else echo script-stopped; fi"]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout
        );
        assert!(log.contains("script-stopped"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_timeout_stops_the_script_in_an_image_without_kill() {
        let executor = with_alpine_pulled().await;
        let job_id = 961_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "runner_info": {"timeout": 4},
            "steps": steps(
                &["rm -f /bin/kill /usr/bin/kill", "sleep 30"],
                &["if ps | grep -v grep | grep -q 'sleep 30'; then echo script-still-running; else echo script-stopped; fi"]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout
        );
        assert!(log.contains("script-stopped"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    /// A job that builds an image in docker:dind and pushes it to the
    /// `registry` service
    fn dind_push_job(job_id: u64, registry: &str, registry_service: serde_json::Value) -> Job {
        job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "docker:27-cli"},
            "services": [
                registry_service,
                // Without an empty DOCKER_TLS_CERTDIR the entrypoint adds --tlsverify
                {"name": "docker:27-dind", "alias": "docker",
                 "command": ["--host=tcp://0.0.0.0:2375", "--tls=false"],
                 "variables": [{"key": "DOCKER_TLS_CERTDIR", "value": ""}]}
            ],
            "variables": [{"key": "DOCKER_HOST", "value": "tcp://docker:2375"}],
            "steps": steps(
                &[
                    "for i in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 1; done",
                    "printf 'FROM alpine:3.20\\n' | docker build -t turboci-push-test -",
                    &format!("docker tag turboci-push-test {}/turboci/push-test:1", registry),
                    &format!("docker push {}/turboci/push-test:1 && echo pushed-ok", registry),
                ],
                &[]
            )
        }))
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_dind_pushes_to_an_insecure_registry() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                privileged: true,
                insecure_registries: vec!["registry:5000".to_string()],
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 990_000_000 + u64::from(std::process::id());
        let j = dind_push_job(
            job_id,
            "registry:5000",
            serde_json::json!({"name": "registry:2", "alias": "registry"}),
        );
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("pushed-ok"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_dind_trusts_a_registry_ca() {
        // A certificate for host "registry", which is also its own CA
        let certs = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=registry",
                "-addext",
                "subjectAltName=DNS:registry",
            ])
            .arg("-keyout")
            .arg(certs.path().join("key.pem"))
            .arg("-out")
            .arg(certs.path().join("cert.pem"))
            .output()
            .unwrap()
            .status;
        assert!(status.success());
        // Readable by the registry container's user
        for file in ["key.pem", "cert.pem"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                certs.path().join(file),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let dir = certs.path().to_str().unwrap().to_string();
        // With the port: a bare "registry/..." would be a Docker Hub repository
        let registry = "registry:443";
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                privileged: true,
                volumes: vec![format!("{}:/registry-certs:ro", dir)],
                registry_ca: std::collections::BTreeMap::from([(
                    registry.to_string(),
                    format!("{}/cert.pem", dir),
                )]),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 991_000_000 + u64::from(std::process::id());
        let j = dind_push_job(
            job_id,
            registry,
            serde_json::json!({"name": "registry:2", "alias": "registry", "variables": [
                {"key": "REGISTRY_HTTP_ADDR", "value": "0.0.0.0:443"},
                {"key": "REGISTRY_HTTP_TLS_CERTIFICATE", "value": "/registry-certs/cert.pem"},
                {"key": "REGISTRY_HTTP_TLS_KEY", "value": "/registry-certs/key.pem"},
                {"key": "HEALTHCHECK_TCP_PORT", "value": "443"}
            ]}),
        );
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("pushed-ok"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_and_services_run_for_their_platform_and_as_their_user() {
        let executor = with_pipeline_images().await;
        let host = match std::env::consts::ARCH {
            "aarch64" => "linux/arm64",
            _ => "linux/amd64",
        };
        let job_id = 962_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {
                "name": "alpine:3.20",
                "executor_opts": {"docker": {"platform": host, "user": "1000:1000"}}
            },
            "services": [{
                "name": "redis:7-alpine", "alias": "cache",
                "executor_opts": {"docker": {"platform": host, "user": "redis"}}
            }],
            "steps": steps(
                &[
                    "echo \"uid=$(id -u) gid=$(id -g)\"",
                    "for i in $(seq 1 20); do nc -z cache 6379 && break; sleep 0.5; done",
                    "nc -z cache 6379 && echo service-up",
                ],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("uid=1000 gid=1000"), "{}", log);
        assert!(log.contains("service-up"), "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_failed_pull_falls_back_to_a_local_image() {
        let executor = DockerExecutor::new(DockerConfig {
            pull_policy: "always".to_string(),
            allowed_pull_policies: vec!["always".to_string(), "if-not-present".to_string()],
            ..DockerConfig::default()
        })
        .unwrap();
        executor.pull_if_missing("alpine:3.20").await.unwrap();
        // Present locally, but its registry does not exist
        let local = "turboci-fallback.invalid/alpine";
        executor
            .docker
            .tag_image("alpine:3.20", local, "1")
            .await
            .unwrap();
        let executor = ExecutorType::Docker(executor);
        let job_id = 963_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": format!("{}:1", local), "pull_policy": ["always", "if-not-present"]},
            "steps": steps(&["echo ran-from-local-image"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();
        let ExecutorType::Docker(docker_executor) = &executor else {
            unreachable!()
        };
        let _ = docker_executor
            .docker
            .remove_image(&format!("{}:1", local))
            .await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(
            log.contains("WARNING: Failed to pull image with policy \"always\": "),
            "{}",
            log
        );
        assert!(
            log.contains(
                "Attempt #2: Trying \"if-not-present\" pull policy\nUsing locally found image"
            ),
            "{}",
            log
        );
        assert!(log.contains("ran-from-local-image"), "{}", log);
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_service_that_fails_to_start_fails_the_job_and_leaves_nothing() {
        let executor = with_pipeline_images().await;
        let job_id = 964_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine"},
            "services": [
                {"name": "redis:7-alpine", "alias": "one"},
                {"name": "redis:7-alpine", "alias": "broken", "entrypoint": ["/nonexistent"]},
                {"name": "redis:7-alpine", "alias": "three"}
            ],
            "steps": steps(&["echo script-ran"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        let failure = outcome.unwrap_err();
        assert_eq!(
            failure.reason,
            FailureReason::RunnerSystemFailure,
            "{}",
            log
        );
        assert!(
            failure.message.starts_with("service redis:7-alpine: "),
            "{}",
            failure.message
        );
        assert_eq!(log.matches("Starting service redis:7-alpine").count(), 3);
        assert!(!log.contains("script-ran"), "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        assert!(leftover_networks(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    /// Names of networks left of job `job_id`
    async fn leftover_networks(job_id: u64) -> Vec<String> {
        let docker = Docker::connect_with_local_defaults().unwrap();
        docker
            .list_networks(&std::collections::HashMap::from([(
                "name".to_string(),
                vec![format!("turboci-job-{}", job_id)],
            )]))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|network| network.name)
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_network_replaces_a_leftover_one() {
        let executor = DockerExecutor::new(DockerConfig::default()).unwrap();
        let name = format!(
            "turboci-job-{}",
            970_000_000 + u64::from(std::process::id())
        );
        let names = || async {
            let found = executor
                .docker
                .list_networks(&std::collections::HashMap::from([(
                    "name".to_string(),
                    vec![name.clone()],
                )]))
                .await
                .unwrap();
            found
                .into_iter()
                .filter_map(|network| network.name)
                .collect::<Vec<_>>()
        };

        // A new name, then the same name again as a crashed job's leftover
        executor.create_network(&name).await.unwrap();
        executor.create_network(&name).await.unwrap();
        let found = names().await;

        executor.docker.remove_network(&name).await.unwrap();
        assert_eq!(found, std::slice::from_ref(&name));
        assert!(names().await.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_container_starts_while_services_are_awaited() {
        let executor = with_pipeline_images().await;
        let job_id = 967_000_000 + u64::from(std::process::id());
        // How long the job container's first process has been running (from
        // its start time since boot in clock ticks), and the service's answer
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine"},
            "services": [{
                "name": "redis:7-alpine", "alias": "cache",
                "command": ["sh", "-c", "sleep 3; exec redis-server"]
            }],
            "steps": steps(&[
                "echo \"age=$(awk -v up=\"$(cut -d' ' -f1 /proc/uptime)\" '{printf \"%d\", up - $22 / 100}' /proc/1/stat)\"",
                "echo \"ping=$(redis-cli -h cache ping)\"",
            ], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        // The script started once the service answered...
        assert!(log.contains("ping=PONG"), "{}", log);
        assert!(log.contains("Waiting for services"), "{}", log);
        assert!(!log.contains("WARNING"), "{}", log);
        // ...in a job container started while the service was awaited
        let age: u64 = log
            .lines()
            .find_map(|line| line.strip_prefix("age="))
            .and_then(|age| age.trim().parse().ok())
            .unwrap_or_else(|| panic!("no age in\n{}", log));
        assert!(
            age >= 2,
            "job container started {}s before the script\n{}",
            age,
            log
        );
        assert!(leftovers(job_id).await.is_empty());
        assert!(leftover_networks(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_wait_for_services_timeout_bounds_or_skips_the_wait() {
        let job_id = 969_000_000 + u64::from(std::process::id());
        let mut logs = Vec::new();
        for (index, timeout) in [-1, 1].into_iter().enumerate() {
            let executor = DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                wait_for_services_timeout: timeout,
                ..DockerConfig::default()
            })
            .unwrap();
            executor.pull_if_missing("redis:7-alpine").await.unwrap();
            let executor = ExecutorType::Docker(executor);
            let id = job_id + index as u64;
            let j = job(serde_json::json!({
                "id": id, "token": "t",
                "image": {"name": "redis:7-alpine"},
                "services": [{
                    "name": "redis:7-alpine", "alias": "cache",
                    "command": ["sh", "-c", "sleep 6; exec redis-server"]
                }],
                "steps": steps(&["nc -z cache 6379 && echo service-up || echo service-not-up"], &[])
            }));
            let scrubber = SecretScrubber::new(vec![]);
            let mut trace = TraceWriter::new(None, id, "t", &scrubber);
            let started = Instant::now();
            let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
            let took = started.elapsed();
            trace.finish().await;
            let log = trace.text();
            assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
            // Not held until the service is up
            assert!(took < Duration::from_secs(5), "took {:?}\n{}", took, log);
            assert!(log.contains("Starting service redis:7-alpine"), "{}", log);
            assert!(log.contains("service-not-up"), "{}", log);
            assert!(leftovers(id).await.is_empty());
            assert!(leftover_networks(id).await.is_empty());
            executor.cleanup(&j, false).await;
            logs.push(log);
        }
        // -1: no wait at all
        assert!(!logs[0].contains("Waiting for services"), "{}", logs[0]);
        assert!(!logs[0].contains("WARNING"), "{}", logs[0]);
        // 1: a wait of a second, then the warning
        assert!(
            logs[1].contains("Waiting for services to be up and running (timeout 1 seconds)..."),
            "{}",
            logs[1]
        );
        assert!(logs[1].contains("did not respond within 1s"), "{}", logs[1]);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_job_container_that_fails_to_start_next_to_services_leaves_nothing() {
        let executor = with_pipeline_images().await;
        let job_id = 968_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine", "entrypoint": ["/nonexistent"]},
            "services": [
                {"name": "redis:7-alpine", "alias": "one",
                 "command": ["sh", "-c", "sleep 2; exec redis-server"]},
                {"name": "redis:7-alpine", "alias": "two"}
            ],
            "steps": steps(&["echo script-ran"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();

        let failure = outcome.unwrap_err();
        assert_eq!(
            failure.reason,
            FailureReason::RunnerSystemFailure,
            "{}",
            log
        );
        assert!(
            failure.message.starts_with("Failed to start container"),
            "{}",
            failure.message
        );
        assert_eq!(log.matches("Starting service redis:7-alpine").count(), 2);
        assert!(!log.contains("script-ran"), "{}", log);
        assert!(leftovers(job_id).await.is_empty());
        assert!(leftover_networks(job_id).await.is_empty());
        executor.cleanup(&j, false).await;
    }

    #[tokio::test]
    async fn setup_lines_wait_for_the_log_and_then_follow_live() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 1, "t", &scrubber);
        let (sender, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let mut log = SetupLog::Channel(sender);
        // Written while the log still showed the checkout
        log.write("Starting service a ...\n").await;
        trace.write("checkout done\n").await;

        let services = async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            log.write("Waiting for services ...\n").await;
            // Dropping the sender ends the forwarding
        };
        tokio::join!(forward_setup_log(&mut lines, &mut trace), services);
        trace.finish().await;

        assert_eq!(
            trace.text(),
            "checkout done\nStarting service a ...\nWaiting for services ...\n"
        );
    }

    /// Names of the running containers of job `job_id`
    async fn running(job_id: u64) -> Vec<String> {
        let docker = Docker::connect_with_local_defaults().unwrap();
        docker
            .list_containers(
                false,
                &std::collections::HashMap::from([(
                    "name".to_string(),
                    vec![format!("turboci-job-{}", job_id)],
                )]),
            )
            .await
            .unwrap()
            .into_iter()
            .flat_map(|container| container.names.unwrap_or_default())
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_uploads_read_a_handed_back_workspace_and_containers_go_after() {
        use std::os::unix::fs::MetadataExt;

        let executor = with_pipeline_images().await;
        let job_id = 975_000_000 + u64::from(std::process::id());
        let j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            0,
            serde_json::json!([]),
            // Root-owned and unreadable for anyone else, and a process that
            // would go on writing
            &[
                "mkdir -m 700 secret && echo s > secret/file && chmod 600 secret/file",
                "(i=0; while :; do i=$((i+1)); echo $i > late; sleep 0.1; done) &",
            ],
        );
        let job_dir = executor.job_dir(&j);
        let project = job_dir.join("project");
        // The runner's user owns the workspace; as root, uid 1000 plays it
        let owner = std::fs::metadata(&job_dir).unwrap().uid();
        let (uid, gid) = if owner == 0 {
            for entry in walkdir::WalkDir::new(&job_dir) {
                std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
            }
            (1000, 1000)
        } else {
            let meta = std::fs::metadata(&job_dir).unwrap();
            (meta.uid(), meta.gid())
        };
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let (outcome, mut teardown) = executor.execute_deferred(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        let log = trace.text();
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        // Until the teardown, the job container idles and the helper and
        // the service live on
        assert_eq!(
            running(job_id).await.len(),
            3,
            "{:?}",
            leftovers(job_id).await
        );

        // The artifacts would hold secret/, which the owner cannot read
        teardown
            .prepare_uploads(&reads_as(&project, &["secret/"], (uid, gid)))
            .await;
        assert!(matches!(
            teardown.0.as_ref().unwrap().settled,
            Some(Settled::Owned(_))
        ));
        // The job is stopped, and all of its files belong to the owner
        assert_eq!(
            running(job_id).await.len(),
            2,
            "{:?}",
            running(job_id).await
        );
        assert!(archived_as(&project, "secret", (uid, gid)));
        let late = std::fs::read_to_string(job_dir.join("project/late")).unwrap();
        for entry in walkdir::WalkDir::new(&job_dir) {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            assert_eq!(
                (meta.uid(), meta.gid()),
                (uid, gid),
                "{}",
                entry.path().display()
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_to_string(job_dir.join("project/late")).unwrap(),
            late,
            "the job's process still runs"
        );

        teardown.finish().await;
        assert!(leftovers(job_id).await.is_empty());
        let docker = Docker::connect_with_local_defaults().unwrap();
        assert!(docker
            .inspect_network(&format!("turboci-job-{}", job_id))
            .await
            .is_err());
        executor.cleanup(&j, false).await;
        assert!(!job_dir.exists());
    }

    /// Uploads of `patterns` in `project`, read by `uid:gid`
    fn reads_as(project: &Path, patterns: &[&str], (uid, gid): (u32, u32)) -> UploadReads {
        let mut reads = UploadReads::new(
            project.to_path_buf(),
            patterns.iter().map(|p| p.to_string()).collect(),
            false,
        );
        reads.reader = uploads::Reader {
            uid,
            groups: vec![gid],
        };
        reads
    }

    /// Whether `uid:gid` can archive `path` in `project`, as the uploads
    /// would (as root, a tar run as that user tells)
    fn archived_as(project: &Path, path: &str, (uid, gid): (u32, u32)) -> bool {
        use std::os::unix::process::CommandExt;

        let mut tar = std::process::Command::new("tar");
        tar.arg("-cf")
            .arg("/dev/null")
            .arg("-C")
            .arg(project)
            .arg(path);
        if rustix::process::geteuid().is_root() {
            tar.uid(uid).gid(gid);
        }
        tar.status().unwrap().success()
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_uploads_read_a_readable_workspace_before_it_is_handed_back() {
        use std::os::unix::fs::MetadataExt;

        let executor = with_pipeline_images().await;
        let job_id = 976_000_000 + u64::from(std::process::id());
        // As root, with the image's umask: 0644 files, 0755 directories
        let j = pipelined_job(
            &executor,
            job_id,
            "redis:7-alpine",
            0,
            serde_json::json!([]),
            &["mkdir -p out/sub cache && echo a > out/sub/a && echo c > cache/c"],
        );
        let job_dir = executor.job_dir(&j);
        // The runner's user owns the workspace; as root, uid 1000 plays it
        let owner = std::fs::metadata(&job_dir).unwrap();
        let (uid, gid) = if owner.uid() == 0 {
            for entry in walkdir::WalkDir::new(&job_dir) {
                std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
            }
            (1000, 1000)
        } else {
            (owner.uid(), owner.gid())
        };
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);
        let (outcome, mut teardown) = executor.execute_deferred(&j, &mut trace, &NoRestore).await;
        trace.finish().await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, trace.text());

        let project = job_dir.join("project");
        let mut reads = reads_as(&project, &["out/", "cache/*"], (uid, gid));
        reads.untracked = true;
        teardown.prepare_uploads(&reads).await;
        assert!(matches!(
            teardown.0.as_ref().unwrap().settled,
            Some(Settled::Stopped)
        ));
        // The job container is stopped, the helper and the service run
        assert_eq!(running(job_id).await.len(), 2);
        // Nothing was handed back: the job's files still belong to root
        let created = std::fs::metadata(job_dir.join("project/out/sub/a")).unwrap();
        assert_eq!((created.uid(), created.gid()), (0, 0));
        assert!(archived_as(&project, "out", (uid, gid)));
        assert!(archived_as(&project, "cache", (uid, gid)));
        // git in the sources helper lists the untracked files
        let ExecutorType::Docker(docker) = &executor else {
            unreachable!()
        };
        let helper = teardown.0.as_ref().unwrap().helper().unwrap().to_string();
        let listed = docker
            .untracked_in(
                &helper,
                "/builds/project",
                Some(&format!("{}:{}", uid, gid)),
            )
            .await
            .unwrap();
        let mut listed: Vec<&str> = listed.split('\0').filter(|f| !f.is_empty()).collect();
        listed.sort();
        assert_eq!(listed, ["cache/c", "out/sub/a"]);
        let files = executor.untracked_files(&j, Some(&teardown)).await.unwrap();
        assert_eq!(files.len(), 2, "{:?}", files);

        // After the report, the workspace is handed back and the containers go
        teardown.finish().await;
        assert!(leftovers(job_id).await.is_empty());
        for entry in walkdir::WalkDir::new(&job_dir) {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            assert_eq!(
                (meta.uid(), meta.gid()),
                (uid, gid),
                "{}",
                entry.path().display()
            );
        }
        executor.cleanup(&j, false).await;
        assert!(!job_dir.exists());
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn docker_lists_untracked_files_with_the_helper() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 970_000_000 + u64::from(std::process::id());
        let project = executor
            .builds_root()
            .join(format!("job-{}", job_id))
            .join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&project, &["init", "-q"]);
        std::fs::write(project.join("tracked"), "t").unwrap();
        git(&project, &["add", "tracked"]);
        git(&project, &["commit", "-q", "-m", "init"]);
        std::fs::write(project.join("new.txt"), "n").unwrap();
        // The job's repository config asks git to run a command
        git(
            &project,
            &[
                "config",
                "core.fsmonitor",
                "touch /builds/project/fsmonitor-ran",
            ],
        );

        let j = job(serde_json::json!({"id": job_id, "token": "t"}));
        let files = executor.untracked_files(&j, None).await.unwrap();

        let ran = project.join("fsmonitor-ran").exists();
        executor.cleanup(&j, false).await;
        assert_eq!(files, vec!["new.txt".to_string()]);
        assert!(!ran, "the job's core.fsmonitor command ran");
    }

    #[tokio::test]
    async fn sources_run_as_the_workspace_owner_only_when_safe() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        std::fs::create_dir_all(workspace.join("project/.git")).unwrap();
        let plain = job(serde_json::json!({"id": 1, "token": "t"}));
        let hooked = |name: &str| {
            job(serde_json::json!({
                "id": 1, "token": "t",
                "hooks": [{"name": name, "script": ["apk add git-lfs"]}]
            }))
        };
        let clean = CleanWorkspaces::default();

        for name in ["pre_get_sources_script", "post_get_sources_script"] {
            assert_eq!(
                sources_user(&hooked(name), workspace, &clean).await,
                None,
                "{}",
                name
            );
        }
        let meta = std::fs::metadata(workspace).unwrap();
        if meta.uid() != 0 {
            assert_eq!(
                sources_user(&plain, workspace, &clean).await,
                Some(format!("{}:{}", meta.uid(), meta.gid()))
            );
            return;
        }
        // As root: a root-owned workspace keeps root
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        for entry in walkdir::WalkDir::new(workspace) {
            std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
        }
        assert_eq!(
            sources_user(&plain, workspace, &clean).await,
            Some("1000:1000".to_string())
        );
        // A leftover of someone else's that only root may remove
        std::fs::write(workspace.join("project/.git/index.lock"), "").unwrap();
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        assert_eq!(
            sources_user(&plain, &workspace.join("missing"), &clean).await,
            None
        );
    }

    /// A workspace that is not wholly its owner's, so that only a skipped
    /// walk lets the sources helper run as the owner: as root, a file of
    /// another user; otherwise a directory the walk cannot read
    fn unclean_workspace() -> (tempfile::TempDir, String) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        std::fs::create_dir_all(workspace.join("project/.git")).unwrap();
        let mut meta = std::fs::metadata(workspace).unwrap();
        if meta.uid() == 0 {
            for entry in walkdir::WalkDir::new(workspace) {
                std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
            }
            std::fs::write(workspace.join("project/.git/index.lock"), "").unwrap();
            meta = std::fs::metadata(workspace).unwrap();
        } else {
            let locked = workspace.join("project/locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::create_dir(locked.join("inside")).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let owner = format!("{}:{}", meta.uid(), meta.gid());
        (dir, owner)
    }

    #[tokio::test]
    async fn a_workspace_left_clean_is_not_walked_again_before_the_next_checkout() {
        let (dir, owner) = unclean_workspace();
        let workspace = dir.path();
        let plain = job(serde_json::json!({"id": 1, "token": "t"}));
        let clean = CleanWorkspaces::default();
        let stamp = || WorkspaceStamp::of(&std::fs::metadata(workspace).unwrap());

        // Walked: the workspace is not wholly the owner's
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        // Marked clean when the last job ended: not walked
        clean.mark(workspace, stamp());
        assert_eq!(sources_user(&plain, workspace, &clean).await, Some(owner));
        // The mark is used up by that job
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        // A mark of the workspace as it was before it was created anew, or
        // before it changed hands, does not count
        let mut old = stamp();
        old.ino += 1;
        clean.mark(workspace, old);
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        let mut old = stamp();
        old.uid += 1;
        clean.mark(workspace, old);
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);
        // Nor does a mark that was forgotten (a job started containers in it)
        clean.mark(workspace, stamp());
        clean.forget(workspace);
        assert_eq!(sources_user(&plain, workspace, &clean).await, None);

        // Readable again, so the temporary directory can be removed
        let locked = workspace.join("project/locked");
        if locked.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn ownership_check_does_not_follow_symlinks() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(dir.path()).unwrap();
        let (uid, gid) = (meta.uid(), meta.gid());
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::write(dir.path().join("project/file"), "x").unwrap();
        // Points at a file of root's: the link itself is the runner's
        std::os::unix::fs::symlink("/etc/hosts", dir.path().join("project/link")).unwrap();

        assert!(!owned_by_others(dir.path(), uid, gid));
        assert!(owned_by_others(dir.path(), uid + 1, gid));
        assert!(owned_by_others(dir.path(), uid, gid + 1));
        assert!(owned_by_others(&dir.path().join("missing"), uid, gid));
    }

    #[test]
    fn chown_touches_only_what_others_own_and_never_follows_links() {
        let script = chown_script(1000, 1001);
        assert!(
            script.starts_with(
                "find /builds \\( ! -user 1000 -o ! -group 1001 \\) -exec chown -h 1000:1001 {} +"
            ),
            "{}",
            script
        );
        assert!(
            script.ends_with("|| chown -R -h 1000:1001 /builds"),
            "{}",
            script
        );
    }

    /// An executor whose Docker client is never used: it points nowhere
    #[test]
    fn jobs_without_git_get_an_empty_workspace_of_their_own() {
        let docker = ExecutorType::Docker(offline(DockerConfig::default()));
        let shell = ExecutorType::Shell(crate::runner_daemon::executor::ShellExecutor::new(Some(
            "/tmp/turboci-shell-test".to_string(),
        )));
        let with_strategy = |strategy: Option<&str>| {
            let variables: Vec<serde_json::Value> = strategy
                .map(|s| vec![serde_json::json!({"key": "GIT_STRATEGY", "value": s})])
                .unwrap_or_default();
            job(serde_json::json!({
                "id": 77, "token": "t",
                "job_info": {"name": "deploy", "stage": "deploy", "project_id": 5, "project_name": "p"},
                "git_info": {
                    "repo_url": "https://example.com/g/p.git", "ref": "main", "ref_type": "branch",
                    "sha": "0123456789abcdef0123456789abcdef01234567", "before_sha": "",
                    "refspecs": [], "protected": false
                },
                "variables": variables
            }))
        };
        let leaf = |executor: &ExecutorType, j: &Job| {
            executor
                .job_dir(j)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };

        // A deploy job that clones another repository must not find the clone
        // an earlier job of the project left in the kept workspace
        assert_eq!(leaf(&docker, &with_strategy(Some("none"))), "job-77");
        assert_eq!(leaf(&docker, &with_strategy(Some("fetch"))), "project-5-0");
        assert_eq!(leaf(&docker, &with_strategy(None)), "project-5-0");
        // gitlab-runner's shell executor shares the project directory
        assert_eq!(leaf(&shell, &with_strategy(Some("none"))), "project-5-0");
    }

    fn offline(config: DockerConfig) -> DockerExecutor {
        let docker = Docker::with_socket("/nonexistent/turboci-test/docker.sock");
        DockerExecutor {
            docker: Arc::new(docker),
            config,
            ca_pem: None,
            owner: "turboci".to_string(),
            pulls: Arc::default(),
            clean_workspaces: Arc::default(),
            images: Arc::default(),
        }
    }

    #[test]
    fn a_reconfigured_executor_keeps_its_state_and_reads_seccomp_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("seccomp.json");
        std::fs::write(&profile, r#"{"defaultAction": "SCMP_ACT_ALLOW"}"#).unwrap();
        let running = offline(DockerConfig::default()).with_owner("s_owner");
        let new = DockerConfig {
            default_image: "debian:12".to_string(),
            image_max_age_days: Some(3),
            allowed_images: vec!["debian:*".to_string()],
            security_opt: vec![format!("seccomp={}", profile.display())],
            ..DockerConfig::default()
        };

        let next = running.reconfigured(new.clone()).unwrap();

        assert_eq!(next.config.default_image, "debian:12");
        assert_eq!(next.config.image_max_age_days, Some(3));
        assert_eq!(next.config.allowed_images, ["debian:*"]);
        // The profile is read, as at start
        assert_eq!(
            next.config.security_opt,
            [r#"seccomp={"defaultAction":"SCMP_ACT_ALLOW"}"#]
        );
        // Pulls in flight, image usage and workspace checks stay shared
        assert_eq!(next.owner, "s_owner");
        assert!(Arc::ptr_eq(&next.pulls, &running.pulls));
        assert!(Arc::ptr_eq(&next.images, &running.images));
        assert!(Arc::ptr_eq(
            &next.clean_workspaces,
            &running.clean_workspaces
        ));
        assert_eq!(
            running.config.default_image,
            DockerConfig::default().default_image
        );

        let missing = DockerConfig {
            security_opt: vec!["seccomp=/nonexistent/profile.json".to_string()],
            ..new
        };
        assert!(running.reconfigured(missing).is_err());
    }

    fn key(image: &str, login: &str) -> PullKey {
        (image.to_string(), login.to_string())
    }

    #[tokio::test]
    async fn concurrent_callers_with_one_key_share_one_run_and_its_error() {
        use futures_util::FutureExt;

        let flights = Flights::<std::result::Result<(), String>>::default();
        let starts = std::sync::atomic::AtomicUsize::new(0);
        let start = || {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err("registry said no".to_string())
            }
            .boxed()
        };

        let (a, b, c) = tokio::join!(
            flights.join(key("alpine:3.20", "anonymous"), start),
            flights.join(key("alpine:3.20", "anonymous"), start),
            flights.join(key("alpine:3.20", "anonymous"), start),
        );

        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        for outcome in [a, b, c] {
            assert_eq!(outcome, Err("registry said no".to_string()));
        }
        assert_eq!(flights.len(), 0, "an ended run stayed in the map");

        // Ended runs are not reused: the next caller starts a new one
        let _ = flights.join(key("alpine:3.20", "anonymous"), start).await;
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn another_login_or_image_gets_a_run_of_its_own() {
        use futures_util::FutureExt;

        let flights = Flights::<u32>::default();
        let starts = std::sync::atomic::AtomicUsize::new(0);
        let start = |value: u32| {
            let starts = &starts;
            move || {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    value
                }
                .boxed()
            }
        };

        let outcomes = tokio::join!(
            flights.join(key("app:1", "anonymous"), start(1)),
            flights.join(key("app:1", "login-a"), start(2)),
            flights.join(key("app:1", "login-b"), start(3)),
            flights.join(key("app:2", "login-a"), start(4)),
        );

        assert_eq!(outcomes, (1, 2, 3, 4));
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(flights.len(), 0);
    }

    /// Sets its flag when dropped
    struct OnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn work_nobody_waits_for_is_dropped_and_forgotten() {
        use futures_util::FutureExt;

        let flights = Flights::<u32>::default();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let endless = {
            let dropped = Arc::clone(&dropped);
            move || {
                async move {
                    let _guard = OnDrop(dropped);
                    std::future::pending::<u32>().await
                }
                .boxed()
            }
        };

        // Both waiters give up, as canceled jobs do
        let gave_up = tokio::time::timeout(Duration::from_millis(50), async {
            tokio::join!(
                flights.join(key("app:1", "anonymous"), endless.clone()),
                flights.join(key("app:1", "anonymous"), endless),
            )
        })
        .await;

        assert!(gave_up.is_err());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(flights.len(), 0, "abandoned work stayed in the map");
        let fresh = flights
            .join(key("app:1", "anonymous"), || async { 7 }.boxed())
            .await;
        assert_eq!(fresh, 7);
    }

    #[tokio::test]
    async fn work_goes_on_while_one_waiter_remains() {
        use futures_util::FutureExt;

        let flights = Flights::<u32>::default();
        let start = || {
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                5
            }
            .boxed()
        };

        let gives_up = tokio::time::timeout(
            Duration::from_millis(10),
            flights.join(key("app:1", "anonymous"), start),
        );
        let (gave_up, stayed) =
            tokio::join!(gives_up, flights.join(key("app:1", "anonymous"), start));

        assert!(gave_up.is_err());
        assert_eq!(stayed, 5);
        assert_eq!(flights.len(), 0);
    }

    #[test]
    fn login_identities_differ_by_login_and_hide_the_secret() {
        let login = |user: &str, password: &str| docker_api::DockerCredentials {
            username: Some(user.to_string()),
            password: Some(password.to_string()),
            serveraddress: Some("registry.corp".to_string()),
            ..Default::default()
        };
        let a = login_identity(Some(&login("ci", "secret-one")));

        assert_eq!(a, login_identity(Some(&login("ci", "secret-one"))));
        assert_ne!(a, login_identity(Some(&login("ci", "secret-two"))));
        assert_ne!(a, login_identity(Some(&login("other", "secret-one"))));
        assert_ne!(a, login_identity(None));
        // Length-prefixed parts: moving a character between them changes it
        assert_ne!(login_identity(Some(&login("cis", "ecret-one"))), a);
        assert!(!a.contains("secret-one"));
    }

    fn need<'i>(
        name: &'i str,
        platform: Option<&'i str>,
        policies: Option<&'i [String]>,
    ) -> ImageNeed<'i> {
        ImageNeed {
            name,
            platform,
            policies,
        }
    }

    fn policies(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// An executor that finds no image locally and whose pulls all fail
    fn without_images() -> DockerExecutor {
        offline(DockerConfig {
            pull_policy: "always".to_string(),
            allowed_pull_policies: policies(&["always", "if-not-present", "never"]),
            ..DockerConfig::default()
        })
    }

    #[tokio::test]
    async fn pull_policies_are_tried_in_order_until_one_pulls() {
        let executor = without_images();
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 1, "t", &scrubber);
        let never_then_always = policies(&["never", "always"]);
        let always_then_local = policies(&["always", "if-not-present"]);
        let default = Vec::new();
        let images = [
            need("app:1", None, Some(never_then_always.as_slice())),
            need("db:1", None, Some(always_then_local.as_slice())),
            // The same image again, and for another platform
            need("db:1", None, Some(default.as_slice())),
            need("db:1", Some("linux/arm64"), Some(default.as_slice())),
            need("alpine/git", None, None),
        ];

        let pulls = executor
            .plan_pulls(&images.iter().collect::<Vec<_>>(), &mut trace)
            .await
            .unwrap();
        trace.finish().await;
        let log = trace.text();

        let planned: Vec<_> = pulls
            .iter()
            .map(|p| (p.image, p.platform, p.policy, p.attempt, p.fallback.clone()))
            .collect();
        assert_eq!(
            planned,
            vec![
                ("app:1", None, PullPolicy::Always, 2, vec![]),
                (
                    "db:1",
                    None,
                    PullPolicy::Always,
                    1,
                    vec![PullPolicy::IfNotPresent]
                ),
                ("db:1", Some("linux/arm64"), PullPolicy::Always, 1, vec![]),
                ("alpine/git", None, PullPolicy::IfNotPresent, 1, vec![]),
            ]
        );
        assert!(
            log.contains(
                "WARNING: Failed to pull image with policy \"never\": image app:1 is not \
                 present and pull_policy is never\nAttempt #2: Trying \"always\" pull policy\n\
                 Pulling image app:1 ...\n"
            ),
            "{}",
            log
        );
        assert!(
            log.contains("Pulling image db:1 for linux/arm64 ...\n"),
            "{}",
            log
        );
        assert_eq!(log.matches("Pulling image db:1 ...").count(), 1, "{}", log);
    }

    #[tokio::test]
    async fn an_image_no_policy_can_get_fails_the_job_before_anything_starts() {
        let executor = without_images();
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 1, "t", &scrubber);
        let never = policies(&["never"]);
        let not_allowed = policies(&["sometimes"]);

        let failure = executor
            .plan_pulls(&[&need("app:1", None, Some(never.as_slice()))], &mut trace)
            .await
            .unwrap_err();
        assert_eq!(failure.reason, FailureReason::ImagePullFailure);
        assert_eq!(
            failure.message,
            "image app:1 is not present and pull_policy is never"
        );
        let failure = executor
            .plan_pulls(
                &[&need("app:1", None, Some(not_allowed.as_slice()))],
                &mut trace,
            )
            .await
            .unwrap_err();
        assert!(
            failure.message.contains("not one of the runner's allowed"),
            "{}",
            failure.message
        );
    }

    #[tokio::test]
    async fn a_failed_pull_falls_back_to_the_next_policies() {
        let executor = without_images();
        let j = job(serde_json::json!({"id": 1, "token": "t"}));
        let pull = PlannedPull {
            image: "app:1",
            platform: None,
            policy: PullPolicy::Always,
            attempt: 1,
            fallback: vec![PullPolicy::IfNotPresent, PullPolicy::Never],
        };

        let (outcome, notes) = executor.pull_with_fallback(&pull, &j).await;

        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, FailureReason::ImagePullFailure);
        assert!(
            failure.message.starts_with("failed to pull image app:1: ")
                && failure
                    .message
                    .ends_with(" (pull policies tried: always, if-not-present, never)"),
            "{}",
            failure.message
        );
        let lines: Vec<&str> = notes.lines().collect();
        assert_eq!(lines.len(), 4, "{}", notes);
        assert!(
            lines[0].starts_with("WARNING: Failed to pull image with policy \"always\": "),
            "{}",
            notes
        );
        assert_eq!(
            lines[1],
            "Attempt #2: Trying \"if-not-present\" pull policy"
        );
        assert!(
            lines[2].starts_with("WARNING: Failed to pull image with policy \"if-not-present\": "),
            "{}",
            notes
        );
        assert_eq!(lines[3], "Attempt #3: Trying \"never\" pull policy");

        // Without fallbacks the pull's own error is the job's
        let pull = PlannedPull {
            fallback: Vec::new(),
            ..pull
        };
        let (outcome, notes) = executor.pull_with_fallback(&pull, &j).await;
        assert!(!outcome.unwrap_err().message.contains("pull policies tried"));
        assert!(notes.is_empty());
    }

    #[test]
    fn local_images_match_the_platform_docker_would_run() {
        let image = |os: &str, arch: &str, variant: Option<&str>| docker_api::ImageInspect {
            os: Some(os.to_string()),
            architecture: Some(arch.to_string()),
            variant: variant.map(str::to_string),
        };
        let amd64 = image("linux", "amd64", None);
        let arm64 = image("linux", "arm64", Some("v8"));
        let armv6 = image("linux", "arm", Some("v6"));

        for wanted in [
            "linux/amd64",
            "LINUX/X86_64",
            "amd64",
            "linux",
            "linux/amd64/v3",
        ] {
            assert!(platform_matches(&amd64, wanted), "{}", wanted);
        }
        for wanted in [
            "linux/arm64",
            "linux/arm64/v8",
            "linux/aarch64",
            "windows/amd64",
        ] {
            assert!(!platform_matches(&amd64, wanted), "{}", wanted);
        }
        for wanted in ["linux/arm64", "linux/arm64/v8", "linux/aarch64", "arm64"] {
            assert!(platform_matches(&arm64, wanted), "{}", wanted);
        }
        assert!(platform_matches(&armv6, "linux/arm/v6"));
        assert!(platform_matches(&armv6, "linux/armel"));
        // `arm` alone is arm/v7
        assert!(!platform_matches(&armv6, "linux/arm"));
        assert!(platform_matches(
            &image("linux", "arm", None),
            "linux/arm/v7"
        ));
        // dockerd that does not say matches anything
        assert!(platform_matches(
            &docker_api::ImageInspect::default(),
            "linux/arm64"
        ));
    }

    #[test]
    fn cpu_shares_and_storage_options_reach_jobs_and_services_tmpfs_only_jobs() {
        let executor = offline(DockerConfig {
            cpu_shares: Some(512),
            tmpfs: std::collections::BTreeMap::from([(
                "/tmp".to_string(),
                "rw,size=64m".to_string(),
            )]),
            storage_opt: std::collections::BTreeMap::from([(
                "size".to_string(),
                "20G".to_string(),
            )]),
            ..DockerConfig::default()
        });

        let service = executor.host_config(Vec::new(), Some("net"));
        let job = executor.job_host_config(vec!["/w:/builds".to_string()], Some("net"));

        for config in [&service, &job] {
            assert_eq!(config.cpu_shares, Some(512));
            assert_eq!(
                config
                    .storage_opt
                    .as_ref()
                    .unwrap()
                    .get("size")
                    .map(String::as_str),
                Some("20G")
            );
            assert_eq!(config.network_mode.as_deref(), Some("net"));
        }
        assert!(service.tmpfs.is_none());
        assert_eq!(
            job.tmpfs.as_ref().unwrap().get("/tmp").map(String::as_str),
            Some("rw,size=64m")
        );
        assert_eq!(job.binds, Some(vec!["/w:/builds".to_string()]));

        let unset = offline(DockerConfig::default());
        let job = unset.job_host_config(Vec::new(), None);
        assert!(job.cpu_shares.is_none() && job.tmpfs.is_none() && job.storage_opt.is_none());
    }

    #[test]
    fn container_options_reach_jobs_and_services_as_in_gitlab_runner() {
        let executor = offline(DockerConfig {
            extra_hosts: vec![
                "gitlab.corp=10.0.0.5".to_string(),
                "v6.corp:::1".to_string(),
            ],
            dns: vec!["10.0.0.53".to_string()],
            dns_search: vec!["corp.local".to_string()],
            cap_add: vec!["SYS_PTRACE".to_string()],
            cap_drop: vec!["NET_RAW".to_string()],
            security_opt: vec!["no-new-privileges".to_string()],
            devices: vec!["/dev/fuse".to_string(), "/dev/sda:/dev/xvda:r".to_string()],
            gpus: Some("all".to_string()),
            sysctls: std::collections::BTreeMap::from([(
                "net.ipv4.ip_unprivileged_port_start".to_string(),
                "0".to_string(),
            )]),
            ulimit: std::collections::BTreeMap::from([
                ("nofile".to_string(), "1024:4096".to_string()),
                ("core".to_string(), "0".to_string()),
            ]),
            runtime: Some("nvidia".to_string()),
            ..DockerConfig::default()
        });

        let job = executor.job_host_config(Vec::new(), None);
        let service = executor.host_config(Vec::new(), Some("net"));
        let helper = executor.name_resolution();

        // Name resolution and the runtime: jobs and services alike
        for config in [&job, &service] {
            assert_eq!(
                config.extra_hosts,
                Some(vec![
                    "gitlab.corp:10.0.0.5".to_string(),
                    "v6.corp:::1".to_string()
                ])
            );
            assert_eq!(config.dns, Some(vec!["10.0.0.53".to_string()]));
            assert_eq!(config.dns_search, Some(vec!["corp.local".to_string()]));
            assert_eq!(config.runtime.as_deref(), Some("nvidia"));
        }
        // The checkout gets name resolution only
        assert_eq!(helper.extra_hosts, job.extra_hosts);
        assert_eq!(helper.dns, job.dns);
        assert!(helper.runtime.is_none() && helper.cap_drop.is_none());
        // The rest: jobs only
        assert!(service.cap_add.is_none() && service.cap_drop.is_none());
        assert!(service.security_opt.is_none() && service.devices.is_none());
        assert!(service.device_requests.is_none() && service.sysctls.is_none());
        assert!(service.ulimits.is_none());

        let body = serde_json::to_value(&job).unwrap();
        assert_eq!(body["CapAdd"], serde_json::json!(["SYS_PTRACE"]));
        assert_eq!(body["CapDrop"], serde_json::json!(["NET_RAW"]));
        assert_eq!(
            body["SecurityOpt"],
            serde_json::json!(["no-new-privileges"])
        );
        assert_eq!(
            body["Devices"],
            serde_json::json!([
                {"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/fuse", "CgroupPermissions": "rwm"},
                {"PathOnHost": "/dev/sda", "PathInContainer": "/dev/xvda", "CgroupPermissions": "r"}
            ])
        );
        assert_eq!(
            body["DeviceRequests"],
            serde_json::json!([
                {"Driver": "", "Count": -1, "Capabilities": [["gpu"]], "Options": {}}
            ])
        );
        assert_eq!(
            body["Sysctls"],
            serde_json::json!({"net.ipv4.ip_unprivileged_port_start": "0"})
        );
        assert_eq!(
            body["Ulimits"],
            serde_json::json!([
                {"Name": "core", "Soft": 0, "Hard": 0},
                {"Name": "nofile", "Soft": 1024, "Hard": 4096}
            ])
        );
        assert_eq!(body["Runtime"], "nvidia");

        // Unset: nothing is sent, dockerd's defaults apply
        let body = serde_json::to_value(
            offline(DockerConfig::default()).job_host_config(Vec::new(), None),
        )
        .unwrap();
        for key in [
            "ExtraHosts",
            "Dns",
            "DnsSearch",
            "CapAdd",
            "CapDrop",
            "SecurityOpt",
            "Devices",
            "DeviceRequests",
            "Sysctls",
            "Ulimits",
            "Runtime",
        ] {
            assert!(body.get(key).is_none(), "{} in {}", key, body);
        }
    }

    #[test]
    fn gpu_requests_of_device_ids_and_counts() {
        let request = |gpus: &str| {
            offline(DockerConfig {
                gpus: Some(gpus.to_string()),
                ..DockerConfig::default()
            })
            .job_host_config(Vec::new(), None)
            .device_requests
            .unwrap()
        };
        let ids = &request("\"device=0,2\"")[0];
        assert_eq!(ids.device_ids, Some(vec!["0".to_string(), "2".to_string()]));
        assert_eq!(ids.count, 0);
        assert_eq!(ids.capabilities, vec![vec!["gpu".to_string()]]);
        assert_eq!(request("2")[0].count, 2);
        let utility = &request("count=1,capabilities=utility,driver=nvidia")[0];
        assert_eq!(
            utility.capabilities,
            vec![vec!["utility".to_string(), "gpu".to_string()]]
        );
        assert_eq!(utility.driver, "nvidia");
        assert!(offline(DockerConfig {
            gpus: Some(" ".to_string()),
            ..DockerConfig::default()
        })
        .job_host_config(Vec::new(), None)
        .device_requests
        .is_none());
    }

    #[test]
    fn allowed_images_and_services_follow_gitlab_runner() {
        let executor = offline(DockerConfig {
            default_image: "alpine:3.20".to_string(),
            allowed_images: vec!["ruby:*".to_string(), "registry.corp/**".to_string()],
            allowed_services: vec!["postgres:*".to_string()],
            services: vec![crate::runner_daemon::config::ServiceConfig {
                name: "docker:27-dind".to_string(),
                ..Default::default()
            }],
            ..DockerConfig::default()
        });
        let check = |image: Option<&str>, services: &[&str]| {
            let services: Vec<String> = services.iter().map(|s| s.to_string()).collect();
            executor.check_allowed_images(image, &services)
        };

        assert!(check(Some("ruby:3.3"), &["docker:27-dind"]).is_ok());
        assert!(check(Some("registry.corp/group/sub/app:1"), &["docker:27-dind"]).is_ok());
        // The default image, chosen by the job or not
        assert!(check(Some("alpine:3.20"), &["docker:27-dind"]).is_ok());
        assert!(check(None, &["docker:27-dind", "postgres:16"]).is_ok());

        let error = check(Some("python:3.12"), &["docker:27-dind"]).unwrap_err();
        assert!(error.contains("\"python:3.12\" image is not present on list of allowed images"));
        assert!(error.contains("- ruby:*\n- registry.corp/**"), "{}", error);
        assert!(
            error.contains("executor.docker.allowed_images"),
            "{}",
            error
        );
        // `*` stops at `/`, names are not normalized, and a tag is needed
        assert!(check(Some("ruby/evil:1"), &["docker:27-dind"]).is_err());
        assert!(check(Some("docker.io/library/ruby:3"), &["docker:27-dind"]).is_err());
        assert!(check(Some("ruby"), &["docker:27-dind"]).is_err());

        let error = check(None, &["docker:27-dind", "mysql:8"]).unwrap_err();
        assert!(error.contains("\"mysql:8\" image is not present on list of allowed services"));
        // A job naming the configured service again is fine
        assert!(check(None, &["docker:27-dind", "docker:27-dind"]).is_ok());

        // No patterns: anything goes
        let open = offline(DockerConfig::default());
        assert!(open
            .check_allowed_images(Some("anything/at:all"), &["any:service".to_string()])
            .is_ok());
    }

    #[test]
    fn usage_keys_match_how_dockerd_lists_images() {
        assert_eq!(usage_key("alpine"), "alpine:latest");
        assert_eq!(usage_key("docker.io/library/alpine:3.20"), "alpine:3.20");
        assert_eq!(usage_key("index.docker.io/library/alpine"), "alpine:latest");
        assert_eq!(usage_key("library/alpine:3"), "alpine:3");
        assert_eq!(usage_key("docker.io/myorg/app:1"), "myorg/app:1");
        assert_eq!(
            usage_key("registry.corp:5000/app"),
            "registry.corp:5000/app:latest"
        );
        assert_eq!(usage_key("alpine@sha256:abc"), "alpine@sha256:abc");

        let listed = docker_api::ImageSummary {
            id: "sha256:1".to_string(),
            repo_tags: Some(vec!["alpine:3.20".to_string(), "<none>:<none>".to_string()]),
            repo_digests: Some(vec!["alpine@sha256:abc".to_string()]),
            created: 0,
            size: 0,
        };
        assert_eq!(image_tags(&listed), ["alpine:3.20"]);
        assert_eq!(
            image_references(&listed),
            ["alpine:3.20", "alpine@sha256:abc"]
        );
    }

    #[tokio::test]
    async fn held_images_count_as_used_until_the_job_ends() {
        let usage = Arc::new(ImageUsage::default());
        let before = std::time::SystemTime::now();

        let first = usage.hold(&["alpine:3.20", "postgres"]).await;
        let second = usage.hold(&["docker.io/library/alpine:3.20"]).await;
        assert_eq!(usage.state().held["alpine:3.20"], 2);
        assert_eq!(usage.state().held["postgres:latest"], 1);
        drop(first);
        assert_eq!(usage.state().held["alpine:3.20"], 1);
        assert!(!usage.state().held.contains_key("postgres:latest"));
        drop(second);
        let state = usage.state();
        assert!(state.held.is_empty());
        assert!(state.last_used["alpine:3.20"] >= before);
    }

    #[test]
    fn service_polls_start_short_and_double_up_to_the_longest() {
        let mut poll = SERVICE_FIRST_POLL;
        let mut polls = Vec::new();
        for _ in 0..6 {
            polls.push(poll.as_millis());
            poll = next_poll(poll);
        }
        assert_eq!(polls, [25, 50, 100, 200, 200, 200]);
    }

    #[tokio::test]
    async fn host_wait_sees_a_port_soon_after_it_opens() {
        let localhost = std::net::IpAddr::from([127, 0, 0, 1]);
        let port = closed_port();
        let late = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            tokio::net::TcpListener::bind((localhost, port))
                .await
                .unwrap()
        };
        let ports = [port];
        let started = Instant::now();

        let (waited, _listener) =
            tokio::join!(wait_from_host(localhost, &ports, SERVICE_WAIT), late);

        assert_eq!(waited, HostWait::Ready);
        // Attempts at about 0, 25 and 75ms: at a fixed SERVICE_POLL the
        // second attempt would come only after 200ms
        assert!(
            started.elapsed() < Duration::from_millis(180),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn port_probe_errors_tell_closed_from_unreachable() {
        use std::io::{Error, ErrorKind};

        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
        ] {
            assert_eq!(PortProbe::of_error(&Error::from(kind)), PortProbe::Closed);
        }
        for kind in [
            ErrorKind::HostUnreachable,
            ErrorKind::NetworkUnreachable,
            ErrorKind::AddrNotAvailable,
            ErrorKind::TimedOut,
            ErrorKind::PermissionDenied,
        ] {
            assert_eq!(PortProbe::of_error(&Error::from(kind)), PortProbe::Silent);
        }
    }

    /// A local port nothing listens on
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn host_wait_is_ready_once_any_port_accepts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap().port();
        let localhost = std::net::IpAddr::from([127, 0, 0, 1]);

        let waited = wait_from_host(localhost, &[closed_port(), open], SERVICE_WAIT).await;

        assert_eq!(waited, HostWait::Ready);
    }

    #[tokio::test]
    async fn host_wait_keeps_trying_a_refusing_port_until_it_opens_or_times_out() {
        let localhost = std::net::IpAddr::from([127, 0, 0, 1]);
        let started = Instant::now();
        let waited = wait_from_host(localhost, &[closed_port()], Duration::from_millis(500)).await;
        assert_eq!(waited, HostWait::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(500));

        // The service starts listening after a few attempts
        let port = closed_port();
        let late = async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap()
        };
        let ports = [port];
        let (waited, _listener) =
            tokio::join!(wait_from_host(localhost, &ports, SERVICE_WAIT), late);
        assert_eq!(waited, HostWait::Ready);
    }

    #[tokio::test]
    async fn host_wait_gives_up_at_once_on_an_address_it_cannot_reach() {
        // TEST-NET-1 is never assigned: no route, or no answer
        let unreachable = std::net::IpAddr::from([192, 0, 2, 1]);
        let started = Instant::now();

        let waited = wait_from_host(unreachable, &[5432], SERVICE_WAIT).await;

        assert_eq!(waited, HostWait::Unreachable);
        assert!(started.elapsed() < SERVICE_CONNECT_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn only_bridges_of_this_host_are_reached_directly() {
        let linux = cfg!(target_os = "linux");
        assert_eq!(
            bridge_is_local(std::net::IpAddr::from([127, 0, 0, 1])),
            linux
        );
        assert!(!bridge_is_local(std::net::IpAddr::from([192, 0, 2, 1])));

        let inspected = |gateway: &str| docker_api::ContainerInspectResponse {
            network_settings: Some(docker_api::NetworkSettings {
                networks: Some(std::collections::HashMap::from([(
                    "turboci-job-1".to_string(),
                    docker_api::EndpointSettings {
                        gateway: Some(gateway.to_string()),
                        ip_address: Some("127.0.0.2".to_string()),
                    },
                )])),
            }),
            ..Default::default()
        };
        let local = inspected("127.0.0.1");
        assert_eq!(
            host_reachable_address(&local, "turboci-job-1"),
            linux.then(|| std::net::IpAddr::from([127, 0, 0, 2]))
        );
        assert_eq!(host_reachable_address(&local, "bridge"), None);
        assert_eq!(
            host_reachable_address(&inspected("192.0.2.1"), "turboci-job-1"),
            None
        );
        assert_eq!(
            host_reachable_address(&inspected(""), "turboci-job-1"),
            None
        );
    }

    #[test]
    fn service_warnings_name_the_service_and_its_ports() {
        let check = ServiceCheck {
            host: "turboci-job-1-svc-0".to_string(),
            ports: vec![2375, 2376],
            address: None,
        };
        assert_eq!(
            service_timeout_warning(&check, SERVICE_WAIT),
            "WARNING: service turboci-job-1-svc-0 (port 2375, 2376) did not respond within 30s"
        );
    }

    #[test]
    fn service_ports_follow_gitlab_runner() {
        let config = |env: Vec<&str>, ports: &[&str]| docker_api::ContainerConfig {
            env: Some(env.into_iter().map(str::to_string).collect()),
            exposed_ports: Some(
                ports
                    .iter()
                    .map(|p| (p.to_string(), serde::de::IgnoredAny))
                    .collect(),
            ),
            ..Default::default()
        };
        assert_eq!(
            service_ports(Some(&config(vec![], &["2376/tcp", "2375/tcp", "53/udp"]))),
            vec![2375, 2376]
        );
        assert_eq!(
            service_ports(Some(&config(
                vec!["HEALTHCHECK_TCP_PORT=8080"],
                &["80/tcp"]
            ))),
            vec![8080]
        );
        assert!(service_ports(None).is_empty());
    }

    /// A resolver for which no host name resolves
    fn no_dns(_: &str) -> Vec<std::net::IpAddr> {
        Vec::new()
    }

    #[test]
    fn warns_when_dockerd_has_no_proxy() {
        let config = DockerConfig::default();
        let certs = std::path::Path::new("/nonexistent");
        let bare = docker_api::SystemInfo::default();
        let with_proxy = docker_api::SystemInfo {
            https_proxy: Some("http://proxy.corp:3128".to_string()),
            ..Default::default()
        };

        let warnings = daemon_warnings(&bare, &config, true, certs, &no_dns);
        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(warnings[0].contains("docker.service.d"), "{}", warnings[0]);
        assert!(daemon_warnings(&with_proxy, &config, true, certs, &no_dns).is_empty());
        assert!(daemon_warnings(&bare, &config, false, certs, &no_dns).is_empty());
    }

    /// dockerd's registry settings with these insecure CIDRs and no index configs
    fn with_insecure_cidrs(cidrs: &[&str]) -> docker_api::SystemInfo {
        docker_api::SystemInfo {
            registry_config: Some(docker_api::RegistryServiceConfig {
                insecure_registry_cidrs: Some(cidrs.iter().map(|c| c.to_string()).collect()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn registry_names_resolving_into_an_insecure_cidr_do_not_warn() {
        let config = DockerConfig {
            insecure_registries: vec![
                "localhost:5000".to_string(),
                "mirror.corp".to_string(),
                "[fd00::5]:5000".to_string(),
            ],
            ..DockerConfig::default()
        };
        let info = with_insecure_cidrs(&["127.0.0.0/8", "10.0.0.0/8", "fd00::/8"]);
        let resolve = |host: &str| -> Vec<std::net::IpAddr> {
            match host {
                "localhost" => vec!["127.0.0.1".parse().unwrap()],
                "mirror.corp" => vec!["192.168.1.9".parse().unwrap(), "10.1.2.3".parse().unwrap()],
                _ => Vec::new(),
            }
        };

        let warnings = daemon_warnings(&info, &config, false, Path::new("/nonexistent"), &resolve);

        assert!(warnings.is_empty(), "{:?}", warnings);
    }

    #[test]
    fn registry_names_are_not_resolved_when_listed_or_without_cidrs() {
        use docker_api::IndexInfo;

        let never = |host: &str| -> Vec<std::net::IpAddr> { panic!("{host} was resolved") };
        let certs = Path::new("/nonexistent");
        let listed = DockerConfig {
            insecure_registries: vec!["listed.corp:5000".to_string()],
            ..DockerConfig::default()
        };
        let mut info = with_insecure_cidrs(&["127.0.0.0/8"]);
        info.registry_config.as_mut().unwrap().index_configs =
            Some(std::collections::HashMap::from([(
                "listed.corp:5000".to_string(),
                IndexInfo {
                    secure: Some(false),
                },
            )]));
        assert!(daemon_warnings(&info, &listed, false, certs, &never).is_empty());

        let unlisted = DockerConfig {
            insecure_registries: vec!["registry.corp:5000".to_string()],
            ..DockerConfig::default()
        };
        let warnings = daemon_warnings(&with_insecure_cidrs(&[]), &unlisted, false, certs, &never);
        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(
            warnings[0].contains("registry.corp:5000"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn registry_names_resolving_outside_the_insecure_cidrs_warn() {
        let config = DockerConfig {
            insecure_registries: vec!["registry.corp:5000".to_string()],
            ..DockerConfig::default()
        };
        let info = with_insecure_cidrs(&["127.0.0.0/8", "10.0.0.0/8"]);
        let resolve = |host: &str| -> Vec<std::net::IpAddr> {
            match host {
                "registry.corp" => vec!["192.168.1.10".parse().unwrap()],
                _ => Vec::new(),
            }
        };

        let warnings = daemon_warnings(&info, &config, false, Path::new("/nonexistent"), &resolve);

        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(
            warnings[0].contains("registry.corp:5000") && warnings[0].contains("daemon.json"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn warns_about_registries_dockerd_does_not_trust() {
        use docker_api::{IndexInfo, RegistryServiceConfig, SystemInfo};

        let certs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(certs.path().join("harbor.ok")).unwrap();
        std::fs::write(certs.path().join("harbor.ok/ca.crt"), "pem").unwrap();
        let config = DockerConfig {
            insecure_registries: vec![
                "insecure.ok".to_string(),
                "10.0.0.5:5000".to_string(),
                "missing.insecure".to_string(),
            ],
            registry_ca: std::collections::BTreeMap::from([
                ("harbor.ok".to_string(), "/x.pem".to_string()),
                ("harbor.missing".to_string(), "/y.pem".to_string()),
            ]),
            ..DockerConfig::default()
        };
        let info = SystemInfo {
            registry_config: Some(RegistryServiceConfig {
                insecure_registry_cidrs: Some(vec!["10.0.0.0/8".to_string()]),
                index_configs: Some(std::collections::HashMap::from([(
                    "insecure.ok".to_string(),
                    IndexInfo {
                        secure: Some(false),
                    },
                )])),
            }),
            ..Default::default()
        };

        let warnings = daemon_warnings(&info, &config, false, certs.path(), &no_dns);

        assert_eq!(warnings.len(), 2, "{:?}", warnings);
        assert!(warnings[0].contains("missing.insecure") && warnings[0].contains("daemon.json"));
        assert!(warnings[1].contains("harbor.missing") && warnings[1].contains("install -D"));
    }

    #[test]
    fn dind_ca_mounts_keep_ports_in_the_target() {
        let registry_ca = std::collections::BTreeMap::from([(
            "harbor.corp:8443".to_string(),
            "/etc/turboci-registry-ca/harbor.corp:8443.pem".to_string(),
        )]);

        let mounts = dind_ca_mounts(&registry_ca);

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].target.as_deref(),
            Some("/etc/docker/certs.d/harbor.corp:8443/ca.crt")
        );
        assert_eq!(
            mounts[0].source.as_deref(),
            Some("/etc/turboci-registry-ca/harbor.corp:8443.pem")
        );
        assert_eq!(mounts[0].read_only, Some(true));
        assert_eq!(mounts[0].typ, Some(docker_api::MountType::Bind));
    }
}
