//! Docker executor: one container per job, steps run as execs in it, sources
//! checked out by a helper container, services on the job's own network

use super::*;

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
fn login_identity(credentials: Option<&bollard::auth::DockerCredentials>) -> String {
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

/// Pull `image` from its registry
async fn pull_image(
    docker: &Docker,
    image: &str,
    credentials: Option<bollard::auth::DockerCredentials>,
) -> PullOutcome {
    use bollard::image::CreateImageOptions;

    let reference = image::with_default_tag(image);
    let options = Some(CreateImageOptions {
        from_image: reference.as_str(),
        ..Default::default()
    });
    let mut stream = docker.create_image(options, None, credentials);
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

impl std::fmt::Debug for DockerExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerExecutor")
            .field("config", &self.config)
            .finish()
    }
}

/// TCP ports that show a service is up: its HEALTHCHECK_TCP_PORT, or its
/// exposed TCP ports (lowest 20), as gitlab-runner checks them
fn service_ports(config: Option<&bollard::models::ContainerConfig>) -> Vec<u16> {
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

/// How long a service may take to accept connections, like gitlab-runner
const SERVICE_WAIT: Duration = Duration::from_secs(30);
/// Pause between connection attempts: a service that is ready after 1.1s
/// must not hold the job until the next full second
const SERVICE_POLL: Duration = Duration::from_millis(200);
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

/// The warning for a service that did not accept connections in time
fn service_timeout_warning(check: &ServiceCheck) -> String {
    format!(
        "WARNING: service {} (port {}) did not respond within {}s",
        check.host,
        check
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        SERVICE_WAIT.as_secs()
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

/// Wait from the runner's host until one of `ports` at `ip` accepts
/// connections (docker:dind exposes 2376 for TLS even when it only listens
/// on 2375), for up to `timeout`
async fn wait_from_host(ip: std::net::IpAddr, ports: &[u16], timeout: Duration) -> HostWait {
    let give_up = Instant::now() + timeout;
    let mut first = true;
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
        tokio::time::sleep(SERVICE_POLL).await;
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
    info: &bollard::models::ContainerInspectResponse,
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
fn workspace_owner(dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir)
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
/// that could not be handed back), which only root may replace.
async fn sources_user(job: &Job, job_dir: &Path) -> Option<String> {
    let hooks = ["pre_get_sources_script", "post_get_sources_script"];
    if hooks.iter().any(|hook| !hook_lines(job, hook).is_empty()) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let meta = tokio::fs::metadata(job_dir).await.ok()?;
        let (uid, gid) = (meta.uid(), meta.gid());
        if uid == 0 {
            return None;
        }
        let dir = job_dir.to_path_buf();
        let others = tokio::task::spawn_blocking(move || owned_by_others(&dir, uid, gid))
            .await
            .unwrap_or(true);
        (!others).then(|| format!("{}:{}", uid, gid))
    }
    #[cfg(not(unix))]
    {
        let _ = job_dir;
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
    info: &bollard::models::SystemInfo,
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
) -> Vec<bollard::models::Mount> {
    registry_ca
        .iter()
        .map(|(registry, file)| bollard::models::Mount {
            target: Some(format!("/etc/docker/certs.d/{}/ca.crt", registry)),
            source: Some(file.clone()),
            typ: Some(bollard::models::MountTypeEnum::BIND),
            read_only: Some(true),
            ..Default::default()
        })
        .collect()
}

/// Containers and network created for one job, removed when it ends
#[derive(Default)]
struct JobContainers {
    network: Option<String>,
    /// Checks out sources, so job images do not need git (like gitlab-runner's helper)
    helper: Option<String>,
    services: Vec<String>,
    job: Option<String>,
}

impl DockerExecutor {
    pub fn new(config: DockerConfig) -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;
        config.validate()?;

        Ok(Self {
            docker: Arc::new(docker),
            config,
            owner: "turboci".to_string(),
            ca_pem: None,
            pulls: Arc::default(),
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

    fn labels(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([(OWNER_LABEL.to_string(), self.owner.clone())])
    }

    /// Remove containers and networks this runner left behind (crash, kill,
    /// power loss); other runners on the same Docker host are not touched
    /// `git ls-files --others -z` of the project, run by a helper container as
    /// the workspace's owner
    pub(super) async fn untracked_files(
        &self,
        job_id: u64,
        job_dir: &Path,
        subdir: &str,
    ) -> Result<String> {
        let dir = format!("/builds/{}", subdir);
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
            workspace_owner(job_dir),
            None,
        )
        .await
    }

    pub async fn sweep_orphans(&self) {
        use bollard::container::ListContainersOptions;
        use bollard::network::ListNetworksOptions;

        let filter = std::collections::HashMap::from([(
            "label".to_string(),
            vec![format!("{}={}", OWNER_LABEL, self.owner)],
        )]);
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: filter.clone(),
                ..Default::default()
            }))
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
        let networks = self
            .docker
            .list_networks(Some(ListNetworksOptions { filters: filter }))
            .await
            .unwrap_or_default();
        for network in networks {
            if let Some(name) = network.name {
                if self.docker.remove_network(&name).await.is_ok() {
                    info!("🧹 Removed leftover network {}", name);
                }
            }
        }
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

    pub(super) async fn execute(
        &self,
        job: &Job,
        job_dir: &Path,
        deadline: Instant,
        trace: &mut TraceWriter<'_>,
        restore: &impl Restore,
    ) -> JobOutcome {
        use futures_util::FutureExt;

        let mut containers = JobContainers::default();
        let outcome = std::panic::AssertUnwindSafe(self.start_and_run(
            job,
            job_dir,
            deadline,
            trace,
            &mut containers,
            restore,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(JobFailure::system("internal runner error")));
        // Always runs, including after failures, timeouts, cancellation and panics
        self.release(job.id, &containers, job_dir).await;
        outcome
    }

    async fn start_and_run(
        &self,
        job: &Job,
        job_dir: &Path,
        deadline: Instant,
        trace: &mut TraceWriter<'_>,
        containers: &mut JobContainers,
        restore: &impl Restore,
    ) -> JobOutcome {
        // Like the scripts, pulls and service startup stop at the job timeout
        // and once the job is being canceled
        let cancel = trace.cancel();
        let limits = Limits {
            deadline,
            cancel: &cancel,
            stop_at: RemoteState::Canceling,
        };
        let env = script::job_env(
            job,
            "/builds",
            job_dir,
            &script::RunnerVars {
                disposable: true,
                ca_pem: self.ca_pem.clone(),
            },
        );
        // Image and service names may use variables, e.g. $CI_REGISTRY_IMAGE/ci
        let values: std::collections::HashMap<String, String> = env.vars.iter().cloned().collect();
        let expand = |name: &str| script::expand(name, &values);

        let (image, policies) = match &job.image {
            Some(img) => (expand(&img.name), img.pull_policy.clone()),
            None => (self.config.default_image.clone(), Vec::new()),
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
            })
            .chain(job.services.iter().cloned())
            .collect();
        let service_images: Vec<String> = services.iter().map(|s| expand(&s.name)).collect();
        let checkout = job.git_info.is_some()
            && git::strategy(&job.variables, job.allow_git_fetch) != GitStrategy::None;

        // The job image, the helper (only to check out sources) and the
        // service images are pulled at the same time
        let mut images = vec![(image.as_str(), Some(policies.as_slice()))];
        if checkout {
            images.push((self.config.helper_image.as_str(), None));
        }
        images.extend(
            service_images
                .iter()
                .zip(&services)
                .map(|(name, service)| (name.as_str(), Some(service.pull_policy.as_slice()))),
        );
        self.ensure_images(&images, job, trace, &limits).await?;

        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;
        let subdir = script::project_subdir(job).map_err(JobFailure::system)?;
        tokio::fs::create_dir_all(job_dir.join(&subdir))
            .await
            .map_err(|e| JobFailure::system(format!("Failed to create host workspace: {}", e)))?;

        if checkout {
            // Container API calls are not interrupted (a dropped create could
            // leave a container nobody removes), so a cancel is seen between them
            limits.check(job)?;
            let user = sources_user(job, job_dir).await;
            let id = self
                .start_helper(job, job_dir, &env, user)
                .await
                .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
            containers.helper = Some(id);
        }

        let volumes = self
            .job_volumes(job)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;

        // Services need a network of their own to be reachable by alias
        if !services.is_empty() {
            limits.check(job)?;
            let network = format!("turboci-job-{}", job.id);
            self.create_network(&network)
                .await
                .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
            containers.network = Some(network);
        }
        for (index, (service, service_image)) in services.iter().zip(service_images).enumerate() {
            trace
                .write(&format!("Starting service {} ...\n", service_image))
                .await;
            limits.check(job)?;
            let mut host_config = self.host_config(volumes.clone(), containers.network.as_deref());
            let mut service = service.clone();
            if image::is_dind(&service_image) {
                service.command = image::dind_command(
                    service.command.as_deref(),
                    &self.config.insecure_registries,
                );
                let mounts = dind_ca_mounts(&self.config.registry_ca);
                host_config.mounts = (!mounts.is_empty()).then_some(mounts);
            }
            let id = self
                .create_service(
                    job,
                    &format!("turboci-job-{}-svc-{}", job.id, index),
                    &service_image,
                    &service,
                    &values,
                    // Configured volumes too, e.g. /certs/client for docker:dind with TLS
                    host_config,
                )
                .await
                .map_err(|e| JobFailure::system(format!("service {}: {:#}", service.name, e)))?;
            // Recorded before starting, so a service that fails to start is removed too
            containers.services.push(id.clone());
            use bollard::container::StartContainerOptions;
            self.docker
                .start_container(&id, None::<StartContainerOptions<String>>)
                .await
                .map_err(|e| JobFailure::system(format!("service {}: {}", service_image, e)))?;
        }
        if let Some(network) = containers.network.clone() {
            self.wait_for_services(job.id, &network, &containers.services, trace, &limits)
                .await;
        }

        limits.check(job)?;
        let name = format!("turboci-job-{}", job.id);
        let config = ContainerCreateBody {
            image: Some(image.clone()),
            working_dir: Some("/builds".to_string()),
            env: Some(env.to_docker()),
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
                    std::iter::once(format!("{}:/builds", job_dir.display()))
                        .chain(volumes.iter().cloned())
                        .collect(),
                    containers.network.as_deref(),
                ),
            ),
            ..Default::default()
        };
        let id = self
            .create_named(&name, config)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
        containers.job = Some(id.clone());
        info!("📦 Created container: {}", id);

        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| JobFailure::system(format!("Failed to start container: {}", e)))?;

        let runner = DockerRunner {
            docker: &self.docker,
            container_id: &id,
            env: env.to_docker(),
            sources_only: false,
        };
        // Still recorded in `containers`: removing it again at the end is a no-op
        let sources = DockerRunner {
            docker: &self.docker,
            container_id: containers.helper.as_deref().unwrap_or(&id),
            env: env.to_docker(),
            sources_only: containers.helper.is_some(),
        };
        let dirs = JobDirs {
            builds: "/builds".to_string(),
            project: format!("/builds/{}", subdir),
        };
        run_job_steps(job, deadline, &sources, &runner, trace, &dirs, restore).await
    }

    /// Start the container that checks out sources as `user` (root when
    /// `None`, see `sources_user`), with the workspace mounted
    async fn start_helper(
        &self,
        job: &Job,
        job_dir: &Path,
        env: &JobEnv,
        user: Option<String>,
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
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = self
            .create_named(&format!("turboci-job-{}-sources", job.id), config)
            .await?;
        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
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
            ..Default::default()
        }
    }

    /// Host settings of the job container: those it shares with services,
    /// plus the workspace and the job-only tmpfs mounts
    fn job_host_config(&self, binds: Vec<String>, network: Option<&str>) -> HostConfig {
        HostConfig {
            tmpfs: (!self.config.tmpfs.is_empty())
                .then(|| self.config.tmpfs.clone().into_iter().collect()),
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
                    .create_volume(bollard::models::VolumeCreateOptions {
                        name: Some(name.clone()),
                        labels: Some(labels),
                        ..Default::default()
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
    /// credentials GitLab sent (its container registry)
    async fn registry_login(&self, image: &str, job: &Job) -> Option<(String, String)> {
        if let Some(login) = variable_value(job, "DOCKER_AUTH_CONFIG")
            .and_then(|config| image::docker_config_auth(config, image))
        {
            return Some(login);
        }
        let file = self.config.auth_config_file.clone().or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{}/.docker/config.json", home))
        });
        let config = match file {
            Some(file) => tokio::fs::read_to_string(file).await.ok(),
            None => None,
        };
        if let Some(login) = config.and_then(|config| image::docker_config_auth(&config, image)) {
            return Some(login);
        }
        image::credentials_for(image, &job.credentials)
            .map(|c| (c.username.clone(), c.password.clone()))
    }

    /// Make `images` available according to their pull policies, pulling the
    /// ones that need it at the same time (each image once), with the registry
    /// credentials of `registry_login`. A `None` policy is for the runner's
    /// own helper image, which is pulled only when missing.
    async fn ensure_images(
        &self,
        images: &[(&str, Option<&[String]>)],
        job: &Job,
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) -> JobOutcome {
        let mut pulls: Vec<&str> = Vec::new();
        for &(image, job_policies) in images {
            let policy = match job_policies {
                None => PullPolicy::IfNotPresent,
                Some(policies) => PullPolicy::resolve(
                    policies,
                    &self.config.pull_policy,
                    &self.config.allowed_pull_policies,
                )
                .map_err(pull_failure)?,
            };
            let reference = image::with_default_tag(image);
            // `always` pulls whether the image is present or not
            let present =
                policy != PullPolicy::Always && self.docker.inspect_image(&reference).await.is_ok();
            match policy {
                PullPolicy::Never if present => {}
                PullPolicy::Never => {
                    return Err(pull_failure(format!(
                        "image {} is not present and pull_policy is never",
                        image
                    )))
                }
                PullPolicy::IfNotPresent if present => {
                    trace
                        .write(&format!("Using locally found image {}\n", image))
                        .await;
                }
                _ if pulls
                    .iter()
                    .any(|pull| image::with_default_tag(pull) == reference) => {}
                _ => {
                    trace.write(&format!("Pulling image {} ...\n", image)).await;
                    pulls.push(image);
                }
            }
        }
        let pulling =
            futures_util::future::try_join_all(pulls.iter().map(|image| self.pull(image, job)));
        limits
            .bound(pulling)
            .await
            .map_err(|stop| stop.failure(job))?
            .map(drop)
    }

    /// Pull `image` from its registry with the job's login for it
    async fn pull(&self, image: &str, job: &Job) -> JobOutcome {
        let credentials = self
            .registry_login(image, job)
            .await
            .map(|(username, password)| bollard::auth::DockerCredentials {
                username: Some(username),
                password: Some(password),
                serveraddress: Some(image::registry(image).to_string()),
                ..Default::default()
            });
        self.pull_shared(image, credentials).await
    }

    /// Pull `image` with `credentials`, or wait for the pull of the same
    /// image with the same login that another job already started
    async fn pull_shared(
        &self,
        image: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> PullOutcome {
        use futures_util::FutureExt;

        let key = (
            image::with_default_tag(image),
            login_identity(credentials.as_ref()),
        );
        let docker = Arc::clone(&self.docker);
        let image = image.to_string();
        self.pulls
            .join(key, move || {
                async move { pull_image(&docker, &image, credentials).await }.boxed()
            })
            .await
    }

    /// Create a bridge network for the job (replacing a leftover one)
    async fn create_network(&self, name: &str) -> Result<()> {
        let _ = self.docker.remove_network(name).await;
        self.docker
            .create_network(bollard::models::NetworkCreateRequest {
                name: name.to_string(),
                driver: Some("bridge".to_string()),
                labels: Some(self.labels()),
                ..Default::default()
            })
            .await
            .with_context(|| format!("Failed to create network {}", name))?;
        Ok(())
    }

    /// Create a service container reachable by its aliases on the job network
    async fn create_service(
        &self,
        job: &Job,
        name: &str,
        image: &str,
        service: &crate::gitlab::Service,
        values: &std::collections::HashMap<String, String>,
        host_config: HostConfig,
    ) -> Result<String> {
        use bollard::models::{EndpointSettings, NetworkingConfig};

        let aliases = image::service_aliases(image, service.alias.as_deref());
        // Services run on the job's network, reachable there by their aliases
        let networking_config = host_config
            .network_mode
            .clone()
            .map(|network| NetworkingConfig {
                endpoints_config: Some(std::collections::HashMap::from([(
                    network,
                    EndpointSettings {
                        aliases: Some(aliases),
                        ..Default::default()
                    },
                )])),
            });
        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            env: Some(script::service_env(job, &service.variables, values)),
            entrypoint: service.entrypoint.clone(),
            cmd: service.command.clone(),
            host_config: Some(host_config),
            networking_config,
            ..Default::default()
        };
        self.create_named(name, config).await
    }

    /// Wait until the services' exposed TCP ports accept connections, like
    /// gitlab-runner (up to 30s each, then a warning), so scripts do not start
    /// against a database that is still booting. Services are checked in
    /// parallel, from the runner's host when it reaches the job network,
    /// otherwise from a container on that network. Returns how many services
    /// were checked from a container.
    async fn wait_for_services(
        &self,
        job_id: u64,
        network: &str,
        services: &[String],
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) -> usize {
        let mut checks = Vec::new();
        for id in services {
            let Ok(info) = self
                .docker
                .inspect_container(
                    id,
                    None::<bollard::query_parameters::InspectContainerOptions>,
                )
                .await
            else {
                continue;
            };
            let ports = service_ports(info.config.as_ref());
            if ports.is_empty() {
                continue;
            }
            checks.push(ServiceCheck {
                host: info
                    .name
                    .as_deref()
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string(),
                address: host_reachable_address(&info, network),
                ports,
            });
        }
        if checks.is_empty() {
            return 0;
        }
        trace
            .write("Waiting for services to be up and running (timeout 30 seconds)...\n")
            .await;
        let from_host = futures_util::future::join_all(checks.iter().map(|check| async move {
            match check.address {
                Some(ip) => wait_from_host(ip, &check.ports, SERVICE_WAIT).await,
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
                    trace
                        .write(&format!("{}\n", service_timeout_warning(check)))
                        .await
                }
                HostWait::Unreachable => unreachable.push(check),
            }
        }
        if !unreachable.is_empty() {
            self.wait_in_container(job_id, network, &unreachable, trace, limits)
                .await;
        }
        unreachable.len()
    }

    /// `wait_for_services` for `checks`, from a short-lived helper container
    /// on the job network
    async fn wait_in_container(
        &self,
        job_id: u64,
        network: &str,
        checks: &[&ServiceCheck],
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) {
        let tries = SERVICE_WAIT.as_millis() / SERVICE_POLL.as_millis();
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
                    warning = script::quote(&service_timeout_warning(check)),
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
            Ok(output) => trace.write(&output).await,
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
        use bollard::container::{LogsOptions, StartContainerOptions, WaitContainerOptions};

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
            self.docker
                .start_container(&id, None::<StartContainerOptions<String>>)
                .await?;
            let mut wait = self
                .docker
                .wait_container(&id, None::<WaitContainerOptions<String>>);
            tokio::time::timeout(CLEANUP_TIMEOUT, async {
                while let Some(status) = wait.next().await {
                    status?;
                }
                anyhow::Ok(())
            })
            .await
            .context("service check timed out")??;
            let mut logs = self.docker.logs(
                &id,
                Some(LogsOptions::<String> {
                    stdout: true,
                    stderr: !stdout_only,
                    ..Default::default()
                }),
            );
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
    async fn create_named(&self, name: &str, mut config: ContainerCreateBody) -> Result<String> {
        use bollard::container::{CreateContainerOptions, RemoveContainerOptions};

        config
            .labels
            .get_or_insert_with(Default::default)
            .extend(self.labels());
        let create = |config| {
            self.docker.create_container(
                Some(CreateContainerOptions {
                    name: name.to_string(),
                    ..Default::default()
                }),
                config,
            )
        };

        // Usually the name is free; 409 Conflict when a container has it
        match create(config.clone()).await {
            Ok(response) => return Ok(response.id),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            }) => {}
            Err(e) => return Err(e).context("Failed to create container"),
        }

        // A container with this name from another runner on the same Docker host
        // (same job id, different GitLab) must not be killed
        if let Ok(existing) = self
            .docker
            .inspect_container(
                name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
        {
            let owner = existing
                .config
                .and_then(|config| config.labels)
                .and_then(|labels| labels.get(OWNER_LABEL).cloned());
            if owner.as_deref() != Some(self.owner.as_str()) {
                anyhow::bail!("container name {} is used by another runner", name);
            }
        }

        let _ = self
            .docker
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await;
        let response = create(config).await.context("Failed to create container")?;
        Ok(response.id)
    }

    /// Remove the job's containers and network, then hand the workspace back to
    /// the runner's user (files created in containers belong to root)
    async fn release(&self, job_id: u64, containers: &JobContainers, job_dir: &Path) {
        // Removing the containers first stops everything the job started, so
        // nothing the job controls runs during cleanup. They are independent,
        // so they are removed at the same time.
        let removals = containers
            .job
            .iter()
            .chain(&containers.helper)
            .chain(&containers.services)
            .map(|id| async move {
                match self.force_remove(id).await {
                    Ok(()) => info!("🗑️  Removed container: {}", id),
                    Err(e) => warn!("Failed to remove container {}: {}", id, e),
                }
            });
        futures_util::future::join_all(removals).await;
        if let Some(network) = &containers.network {
            if let Err(e) = self.docker.remove_network(network).await {
                warn!("Failed to remove network {}: {}", network, e);
            }
        }

        if containers.job.is_some() || containers.helper.is_some() {
            if let Err(e) = self.reset_ownership(job_id, job_dir).await {
                warn!("Failed to reset workspace ownership: {:#}", e);
            }
        }
    }

    /// Pull a runner-internal image (no job credentials) when it is not present
    async fn pull_if_missing(&self, image: &str) -> Result<()> {
        let reference = image::with_default_tag(image);
        if self.docker.inspect_image(&reference).await.is_ok() {
            return Ok(());
        }
        self.pull_shared(image, None)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))
    }

    async fn force_remove(&self, id: &str) -> Result<()> {
        force_remove(&self.docker, id).await
    }

    /// Give the workspace back to the runner's uid/gid (the owner of
    /// `job_dir`) from a fresh container of the (trusted) helper image, never
    /// from the job's own image. Skipped when everything already belongs to
    /// it, e.g. jobs running as the runner's user, or Docker Desktop.
    async fn reset_ownership(&self, job_id: u64, job_dir: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use bollard::container::{StartContainerOptions, WaitContainerOptions};
            use std::os::unix::fs::MetadataExt;

            let meta = tokio::fs::metadata(job_dir).await?;
            let (uid, gid) = (meta.uid(), meta.gid());
            let dir = job_dir.to_path_buf();
            let needed =
                tokio::task::spawn_blocking(move || owned_by_others(&dir, uid, gid)).await?;
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
                self.docker
                    .start_container(&id, None::<StartContainerOptions<String>>)
                    .await?;
                let mut wait = self
                    .docker
                    .wait_container(&id, None::<WaitContainerOptions<String>>);
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
            result?;
        }
        Ok(())
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

/// Kill and remove a container with its anonymous volumes; one that is
/// already gone counts as removed
async fn force_remove(docker: &Docker, id: &str) -> Result<()> {
    use bollard::container::RemoveContainerOptions;

    match docker
        .remove_container(
            id,
            Some(RemoveContainerOptions {
                force: true,
                v: true,
                ..Default::default()
            }),
        )
        .await
    {
        Ok(())
        | Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

struct DockerRunner<'a> {
    docker: &'a Docker,
    container_id: &'a str,
    env: Vec<String>,
    /// A helper container that only checks out sources: removed once that is
    /// done instead of idling until the job ends
    sources_only: bool,
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
        let exec = self
            .docker
            .create_exec(
                self.container_id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    user: Some("0"),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        if let StartExecResults::Attached { mut output, .. } =
            self.docker.start_exec(&exec.id, None).await?
        {
            while output.next().await.is_some() {}
        }
        let inspect = self.docker.inspect_exec(&exec.id).await?;
        Ok(inspect.exit_code.unwrap_or(-1))
    }
}

impl ScriptRunner for DockerRunner<'_> {
    async fn sources_done(&self) {
        if self.sources_only {
            if let Err(e) = force_remove(self.docker, self.container_id).await {
                warn!("Failed to remove container {}: {:#}", self.container_id, e);
            }
        }
    }

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
                CreateExecOptions {
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
        if let StartExecResults::Attached { mut output, .. } =
            self.docker.start_exec(&exec.id, None).await?
        {
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
            .inspect_container(
                &format!("turboci-job-{}", job_id),
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
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

        let info = executor
            .docker
            .inspect_container(
                &id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await;
        executor.force_remove(&id).await.unwrap();

        let shares = info
            .unwrap()
            .host_config
            .and_then(|config| config.cpu_shares);
        assert_eq!(shares, Some(512));
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
            assert!(docker
                .inspect_container(
                    &name,
                    None::<bollard::query_parameters::InspectContainerOptions>
                )
                .await
                .is_err());
        }
        assert!(docker
            .inspect_network(
                &format!("turboci-job-{}", job_id),
                None::<bollard::query_parameters::InspectNetworkOptions>
            )
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
            .list_volumes(Some(bollard::volume::ListVolumesOptions {
                filters: std::collections::HashMap::from([(
                    "label".to_string(),
                    vec![format!("{}={}", OWNER_LABEL, owner)],
                )]),
            }))
            .await
            .unwrap()
            .volumes
            .unwrap_or_default();
        assert_eq!(volumes.len(), 1, "{:?}", volumes);
        for volume in volumes {
            docker
                .remove_volume(
                    &volume.name,
                    None::<bollard::query_parameters::RemoveVolumeOptions>,
                )
                .await
                .unwrap();
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
            .inspect_container(
                &format!("turboci-job-{}-sources", job_id),
                None::<bollard::query_parameters::InspectContainerOptions>
            )
            .await
            .is_err());
        executor.cleanup(&j, false).await;
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
        let gone = docker
            .inspect_container(
                &leftover,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .is_err();
        let kept = docker
            .inspect_container(
                &foreign,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .is_ok();
        let net_gone = docker
            .inspect_network(
                &format!("{}-net", owner),
                None::<bollard::query_parameters::InspectNetworkOptions>,
            )
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
        let _ = executor
            .docker
            .remove_image(
                image,
                None::<bollard::query_parameters::RemoveImageOptions>,
                None,
            )
            .await;

        let (a, b) = tokio::join!(
            executor.pull_shared(image, None),
            executor.pull_shared(image, None)
        );
        assert!(a.is_ok() && b.is_ok(), "{:?} {:?}", a, b);
        assert!(executor.docker.inspect_image(image).await.is_ok());
        assert_eq!(executor.pulls.len(), 0);

        let missing = "turboci-missing.invalid/none:1";
        let (a, b) = tokio::join!(
            executor.pull_shared(missing, None),
            executor.pull_shared(missing, None)
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
                        &std::collections::HashMap::new(),
                        executor.host_config(Vec::new(), Some(network.as_str())),
                    )
                    .await
                    .unwrap();
                executor
                    .docker
                    .start_container(
                        &id,
                        None::<bollard::container::StartContainerOptions<String>>,
                    )
                    .await
                    .unwrap();
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
                &mut trace,
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
            .wait_in_container(job_id, &network, &[&check], &mut trace, &limits)
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
        let files = executor.untracked_files(&j).await.unwrap();

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

        for name in ["pre_get_sources_script", "post_get_sources_script"] {
            assert_eq!(
                sources_user(&hooked(name), workspace).await,
                None,
                "{}",
                name
            );
        }
        let meta = std::fs::metadata(workspace).unwrap();
        if meta.uid() != 0 {
            assert_eq!(
                sources_user(&plain, workspace).await,
                Some(format!("{}:{}", meta.uid(), meta.gid()))
            );
            return;
        }
        // As root: a root-owned workspace keeps root
        assert_eq!(sources_user(&plain, workspace).await, None);
        for entry in walkdir::WalkDir::new(workspace) {
            std::os::unix::fs::chown(entry.unwrap().path(), Some(1000), Some(1000)).unwrap();
        }
        assert_eq!(
            sources_user(&plain, workspace).await,
            Some("1000:1000".to_string())
        );
        // A leftover of someone else's that only root may remove
        std::fs::write(workspace.join("project/.git/index.lock"), "").unwrap();
        assert_eq!(sources_user(&plain, workspace).await, None);
        assert_eq!(sources_user(&plain, &workspace.join("missing")).await, None);
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
    fn offline(config: DockerConfig) -> DockerExecutor {
        let docker =
            Docker::connect_with_http("http://127.0.0.1:9", 1, bollard::API_DEFAULT_VERSION)
                .unwrap();
        DockerExecutor {
            docker: Arc::new(docker),
            config,
            ca_pem: None,
            owner: "turboci".to_string(),
            pulls: Arc::default(),
        }
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
        let login = |user: &str, password: &str| bollard::auth::DockerCredentials {
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

        let inspected = |gateway: &str| bollard::models::ContainerInspectResponse {
            network_settings: Some(bollard::models::NetworkSettings {
                networks: Some(std::collections::HashMap::from([(
                    "turboci-job-1".to_string(),
                    bollard::models::EndpointSettings {
                        gateway: Some(gateway.to_string()),
                        ip_address: Some("127.0.0.2".to_string()),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
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
            service_timeout_warning(&check),
            "WARNING: service turboci-job-1-svc-0 (port 2375, 2376) did not respond within 30s"
        );
    }

    #[test]
    fn service_ports_follow_gitlab_runner() {
        let config = |env: Vec<&str>, ports: &[&str]| bollard::models::ContainerConfig {
            env: Some(env.into_iter().map(str::to_string).collect()),
            exposed_ports: Some(
                ports
                    .iter()
                    .map(|p| (p.to_string(), std::collections::HashMap::new()))
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
        let bare = bollard::models::SystemInfo::default();
        let with_proxy = bollard::models::SystemInfo {
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
    fn with_insecure_cidrs(cidrs: &[&str]) -> bollard::models::SystemInfo {
        bollard::models::SystemInfo {
            registry_config: Some(bollard::models::RegistryServiceConfig {
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
        use bollard::models::IndexInfo;

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
                    ..Default::default()
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
        use bollard::models::{IndexInfo, RegistryServiceConfig, SystemInfo};

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
                        name: Some("insecure.ok".to_string()),
                        secure: Some(false),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
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
        assert_eq!(mounts[0].typ, Some(bollard::models::MountTypeEnum::BIND));
    }
}
