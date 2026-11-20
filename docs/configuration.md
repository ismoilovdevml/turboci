# Configuration

The runner reads one TOML file, `/etc/turboci-runner.toml` when installed with
the installer. Every key is optional except `runner_token`. Unknown keys are
logged as warnings at startup; invalid values stop the runner with an error
instead of being silently ignored.

Apply changes with `sudo systemctl restart turboci`. Changes to `concurrent`,
`check_interval` and the [admission limits](#admission-control) also apply
with `sudo systemctl reload turboci`, without stopping running jobs (see
[reloading the config](operations.md#reloading-the-config)).

## Example

```toml
concurrent = 4
check_interval = 3
runner_token = "glrt-XXXX"
gitlab_url = "https://gitlab.example.com"
# tls_ca_file = "/etc/turboci-ca.pem"

cache_enabled = true
cache_dir = "/var/lib/turboci/cache"
cache_max_age_days = 14
# cache_max_size = "50g"
state_dir = "/var/lib/turboci"

[executor]
executor_type = "docker"

[executor.docker]
default_image = "alpine:latest"
pull_policy = "always"
allowed_pull_policies = []
helper_image = "alpine/git:latest"
privileged = false
volumes = []
network_mode = "bridge"
# memory = "2g"
# cpus = 2.0

[executor.shell]
work_dir = "/var/lib/turboci/builds"
```

## Runner

| Key | Default | Description |
|---|---|---|
| `runner_token` | – | Runner authentication token (`glrt-...`). Required |
| `gitlab_url` | `https://gitlab.com` | GitLab URL (`http://` or `https://`) |
| `concurrent` | `4` | Jobs run at the same time. The runner only asks GitLab for a job when a slot is free |
| `check_interval` | `3` | Seconds between job requests while idle |
| `tls_ca_file` | – | PEM CA for a GitLab with a self-signed or internal certificate; also given to jobs as `CI_SERVER_TLS_CA_FILE` and `GIT_SSL_CAINFO` |
| `state_dir` | `/var/lib/turboci` | Where the runner keeps state it writes: its system ID and a [rotated token](operations.md#runner-token-rotation) |
| `metrics_listen` | unset | `IP:port` of the [Prometheus endpoint](operations.md#metrics), e.g. `"127.0.0.1:9252"`. Unset: no endpoint |
| `environment` | `[]` | `KEY=value` variables added to every job; they override the job's own |
| `pre_get_sources_script` | – | Script run before the checkout of every job (before the job's `hooks:pre_get_sources_script`) |
| `post_get_sources_script` | – | Script run after the checkout |
| `pre_build_script` | – | Script run before `before_script`, in the same shell as the job's script |
| `post_build_script` | – | Script run after `script`, in the same shell |

These work as the settings of the same names in gitlab-runner's
`[[runners]]`, so a gitlab-runner config can be carried over:

```toml
environment = ["ANDROID_HOME=/home/ci/Android/Sdk", "GIT_STRATEGY=clone"]
pre_get_sources_script = "export PATH=$PATH:/opt/flutter/bin"
```

## Admission control

The runner can hold off asking GitLab for jobs while the host is short of
memory, disk space or CPU. All three limits are off by default.

```toml
min_free_memory = "2g"
min_free_disk = "20g"
max_load_avg = 16.0
```

| Key | Default | Description |
|---|---|---|
| `min_free_memory` | unset | No job is requested while the memory available (`MemAvailable` in `/proc/meminfo`, page cache that can be dropped included) is below this size (`"512m"`, `"2g"`, bytes). Unset or `"0"`: off. Linux only |
| `min_free_disk` | unset | No job is requested while the file system of the builds directory or of `cache_dir` has less space free than this. Unset or `"0"`: off |
| `max_load_avg` | unset | No job is requested while the 1-minute load average is above this number. It is not divided by the CPU count: for a 16-core host, `16.0` means "all cores busy". Linux only |

Before each job request, once a job slot is free, the runner checks the
limits. When one is not met it sends no request, waits `check_interval` and
checks again; jobs already running are never touched. The log shows each
change, not every check:

```text
⏸️  Not taking new jobs: free disk space for /var/lib/turboci/builds 4.2 GB is below min_free_disk 20.0 GB
▶️  Taking jobs again: the host is within the admission limits
```

A value that cannot be read (for example `/proc` not mounted) does not block
jobs. On macOS only `min_free_disk` is checked; `min_free_memory` and
`max_load_avg` are ignored, with a warning at start.

## Proxy

```toml
proxy = "http://proxy.corp:3128"
no_proxy = "localhost,.corp.local,10.0.0.0/8"
```

| Key | Default | Description |
|---|---|---|
| `proxy` | – | Proxy for HTTP and HTTPS (`http://` or `https://`, may hold `user:pass@`) |
| `no_proxy` | – | Comma-separated hosts, domains (`.corp.local`), IPs and CIDRs reached directly. Needs `proxy` |

With `proxy` set:

- The runner's own requests (GitLab API, job logs, artifacts, the S3 cache)
  go through it. Without it the runner reads `HTTP_PROXY`, `HTTPS_PROXY` and
  `NO_PROXY` from its environment.
- Jobs, the source checkout and services get `HTTP_PROXY`, `HTTPS_PROXY`,
  `NO_PROXY` and their lowercase forms. A job that sets one of them itself
  keeps its own value; the runner's `environment` overrides both. Each of the
  six is overridden on its own: a job that sets only `HTTP_PROXY` still gets
  the runner's `http_proxy`, which curl and git read first, and a job-level
  `NO_PROXY` replaces the runner's list, including the loopback and service
  aliases below.
- `NO_PROXY` also lists `localhost`, `127.0.0.1`, `::1` and the job's service
  aliases, so `tcp://docker:2375` and `postgres:5432` never go to the proxy.
- A password in the proxy URL is masked in job logs. Jobs still read the full
  URL, password included, from their proxy variables.

Image pulls are done by dockerd, which has its own proxy setting. At start the
runner logs a warning with the drop-in to add when dockerd has no proxy:

```ini
# /etc/systemd/system/docker.service.d/proxy.conf
[Service]
Environment="HTTP_PROXY=http://proxy.corp:3128" "HTTPS_PROXY=http://proxy.corp:3128" "NO_PROXY=localhost,.corp.local"
```

Then `systemctl daemon-reload && systemctl restart docker`. The installer
writes the runner's side with `--proxy URL --no-proxy LIST`.

## Cache

| Key | Default | Description |
|---|---|---|
| `cache_enabled` | `true` | Enables `cache:` in jobs |
| `cache_dir` | `/var/lib/turboci/cache` | Local cache, one directory per project |
| `cache_max_age_days` | `14` | Archives not used for this many days are deleted (`0` keeps them forever) |
| `cache_max_size` | unset | Total size of the archives of all projects (`"512m"`, `"50g"`, bytes); after each save or download the least recently used archives are deleted until the cache fits, whatever their project. The archive just written is kept even if it is larger on its own. Unset or `"0"`: no limit |

Without `cache_s3` the cache is local to the runner host, like gitlab-runner
without a distributed cache. Archives are written to a temporary file and
renamed into place, so concurrent jobs never read a partial archive.

Cache archives are ZIP files with zstd-compressed entries (artifacts stay
deflate, which GitLab reads). On a 262 MB `node_modules` of 15,450 files
saving took 0.76 s instead of 2.0 s, at the same size. Caches written by
older versions still restore; older versions cannot read the new ones and
treat them as a cache miss with a warning, so upgrade every runner that
shares an S3 cache together.

### Shared cache in S3

Runners on several hosts share one cache through S3-compatible storage (AWS
S3, MinIO, Ceph RGW, SeaweedFS, Cloudflare R2, Google Cloud Storage with HMAC
keys):

```toml
[cache_s3]
url = "https://minio.corp:9000/turboci-cache"
access_key = "..."
secret_key = "..."
# region = "eu-central-1"
```

| Key | Default | Description |
|---|---|---|
| `url` | – | `https://host[:port]/bucket[/prefix]` (path-style); `http://` for storage without TLS |
| `access_key`, `secret_key` | – | Credentials. The runner transfers the archives, so docker jobs never see them; shell jobs run as the service user and can read the config file, as they can the runner token |
| `region` | from `s3.<region>.amazonaws.com`, else `us-east-1` | Signing region (`auto` for R2) |

The credentials need `s3:GetObject`, `s3:PutObject` and
`s3:AbortMultipartUpload` on the archives, and `s3:ListBucket` on the bucket
(it may be limited to the prefix). Without
`s3:ListBucket` AWS answers 403 instead of 404 for an archive that does not
exist yet, so every cache miss logs a warning, and the start-up check reports
access denied. A grant limited with an `s3:prefix` condition may not allow
that check (HEAD on the bucket), which then logs access denied although the
archives work.

`cache_dir` stays the first layer. To restore, the runner asks S3 whether the
archive changed (`If-None-Match`): an unchanged one is taken from the local
disk, a changed one is downloaded. Saved archives are written locally and
uploaded; archives above 64 MiB go as a multipart upload, in parts of 16 MiB
(larger above 156 GiB, to stay within S3's 10,000 parts), four at a time, each signed with its own hash. A
failed or canceled multipart upload is aborted, so S3 keeps no parts of it.
An archive is the same for the same files (entries in path order, fixed
timestamps), so a `pull-push` cache the job did not change is not uploaded
again: the runner stores the archive's SHA-256 with the object
(`x-amz-meta-sha256`) and, before uploading, asks with a HEAD request
whether S3 holds that content already, whichever host uploaded it. For
objects without that metadata (uploaded by older versions) it compares the
ETag with the one it last downloaded or uploaded for that hash
(`<archive>.sha256`). The job log shows which happened:

```
Successfully restored cache main (S3: not modified, local copy)
Successfully restored cache main (downloaded 120.0 MB from S3)
Uploaded cache main to S3 (45.2 MB)
Cache main unchanged, S3 holds it already: not uploaded
```

When S3 cannot be reached the job uses the local copy and the log shows a
warning; a cache never fails a job. The runner then stops calling S3 for 60
seconds, so the jobs in that time go straight to the local copy instead of
each waiting through the same timeouts; after that one call tries S3 again
while the others keep skipping it. The pause starts on the first attempt
that cannot connect or stalls (no data for 60 seconds); 5xx answers and
dropped connections are tried three times first. `proxy`, `no_proxy` and
`tls_ca_file` apply to S3 too. At start the runner checks the bucket in
the background, without delaying jobs, and logs what is wrong (not found,
access denied, unreachable).

The runner never deletes objects. Expire old archives with a bucket lifecycle
rule, e.g. `mc ilm rule add --expire-days 14 minio/turboci-cache` or an S3
lifecycle rule with `Expiration: {Days: 14}`. Add
`AbortIncompleteMultipartUpload: {DaysAfterInitiation: 1}` to the rule as
well: a runner that is killed during an upload, or loses S3 in the middle,
cannot abort it, and S3 keeps (and bills) the parts until the rule removes
them. Archives are stored as
`<prefix>/project-<id>/<encoded-key>.zip`, the cache key percent-encoded
(`feature/x` is `feature%2Fx.zip`), and a key whose encoded name is longer
than 200 characters as `h-<blake3 hash>.zip`; every runner with the
credentials can write every project's cache, so use one bucket or prefix per
group of runners that trust each other. Archives above 5 TiB, S3's largest
object, are kept only locally.

## Executor

`[executor] executor_type` is `docker` (default) or `shell`.

### Docker

Each job runs in its own container. Sources are checked out by a short-lived
helper container, so job images need neither `git` nor any other tool. The
helper runs as the owner of the workspace directory, so the checkout belongs
to the runner's user; it runs as root when `pre_get_sources_script` or
`post_get_sources_script` hooks are set (they may need root), when the
workspace belongs to root, or when an earlier job left files in it that belong
to another user. Jobs
with `services` get their own network where each service is reachable by its
aliases. The checkout runs while the job image is pulled and the services
start; the log shows the services' progress after the `get_sources` section.
If either fails, or the job times out or is canceled, the other is stopped
and every container of the job is removed. Concurrent jobs that need the same image with the same registry
login wait for one pull of it; a job never shares the pull of a job that
logged in differently.

| Key | Default | Description |
|---|---|---|
| `builds_dir` | `<state_dir>/builds` | Host directory of the job workspaces, mounted as `/builds` |
| `default_image` | `alpine:latest` | Image for jobs that do not set `image:` |
| `pull_policy` | `always` | `always`, `if-not-present` or `never` |
| `allowed_pull_policies` | `[]` | Policies a job may request with `image:pull_policy`. Empty: only `pull_policy`. This stops a project from using `if-not-present` to run another project's cached private image |
| `helper_image` | `alpine/git:latest` | Image (with `git` and `sh`) that checks out sources and cleans up workspaces |
| `privileged` | `false` | Privileged job and service containers (needed for Docker-in-Docker) |
| `volumes` | `[]` | Extra binds for job containers, e.g. `"/srv/cache:/cache:rw"` |
| `network_mode` | `bridge` | Network for jobs without services |
| `memory` | – | Memory limit per container, e.g. `"2g"`, `"512m"` |
| `memory_swap` | – | Memory plus swap per container, at least `memory`; `"-1"` for unlimited swap |
| `memory_reservation` | – | Soft memory limit per container |
| `cpus` | – | CPU limit per container, e.g. `1.5` |
| `shm_size` | – | Size of `/dev/shm`, e.g. `"1g"`. Docker's 64 MB is too small for browsers and some test runners |
| `oom_score_adjust` | – | OOM killer preference of containers (-1000 to 1000) |
| `pids_limit` | – | Most processes per container (at least 1), so a fork bomb cannot exhaust the host, e.g. `4096` |
| `cpu_shares` | – | Relative CPU weight of job and service containers (2 to 262144; Docker's default is 1024). Unlike `cpus` it caps nothing: a container may use idle CPU, it only gets a smaller share while the CPUs are busy |
| `storage_opt` | `{}` | Storage driver options of job and service containers, e.g. `size = "20G"` (overlay2 needs xfs mounted with `pquota` for `size`; dockerd rejects the container otherwise) |
| `tmpfs` | `{}` | tmpfs mounts of job containers (not services): container path → mount options, e.g. `"/tmp" = "rw,noexec,size=1g"`. Paths at or below `/builds` are rejected: they would hide the workspace |
| `auth_config_file` | `~/.docker/config.json` of the service user | Docker client config whose `auths` are used to pull private images |
| `insecure_registries` | `[]` | Registries (`host[:port]`) reached over HTTP or without certificate checks; docker:dind services get `--insecure-registry` for each |
| `registry_ca` | `{}` | Registry → PEM CA file; mounted into docker:dind services as `/etc/docker/certs.d/<registry>/ca.crt` |
| `services` | `[]` | Services started for every job (see below) |

`tmpfs` and `storage_opt` are tables:

```toml
[executor.docker.tmpfs]
"/tmp" = "rw,noexec,size=1g"

[executor.docker.storage_opt]
size = "20G"
```

`volumes` takes `host:container[:mode]` binds and bare container paths. A
bare path such as `"/cache"` becomes a Docker volume that keeps its content
between the jobs of a project (one per project and concurrent slot), like
gitlab-runner's cache volumes. The same volume is mounted in the job's
services, so `"/certs/client"` shares Docker-in-Docker TLS certificates.

### Services for every job

Services listed here start before the job's own `services:`, for example a
Docker daemon for jobs that build images:

```toml
[executor.docker]
privileged = true

[[executor.docker.services]]
name = "docker:27-dind"
alias = "docker"
command = ["--host=tcp://0.0.0.0:2375", "--tls=false"]
```

Jobs then reach it as `tcp://docker:2375`. They must also set
`DOCKER_TLS_CERTDIR` to empty, or the docker:dind image turns on TLS, on port
2375 too. Variables from `.gitlab-ci.yml` reach the services listed here as
well:

```yaml
variables:
  DOCKER_HOST: tcp://docker:2375
  DOCKER_TLS_CERTDIR: ""
```

### Private registries

An image is pulled with the first login found, in gitlab-runner's order:

1. the job's `DOCKER_AUTH_CONFIG` variable (a Docker `config.json`, usually a
   masked project or group variable);
2. `auth_config_file`, by default the service user's `~/.docker/config.json`
   (`/var/lib/turboci/.docker/config.json` for an installed runner);
3. the credentials GitLab sends for its own container registry.

Only `auths` entries are read; credential helpers (`credsStore`,
`credHelpers`) are not run.

A registry with an internal CA or without TLS:

```toml
[executor.docker]
insecure_registries = ["harbor.old.local"]

[executor.docker.registry_ca]
"harbor.corp.local" = "/etc/turboci-registry-ca/harbor.corp.local.pem"
```

Quote the registry names in `registry_ca`: an unquoted
`harbor.corp.local = ...` is a nested TOML table, not a registry.

Use `registry_ca` when the registry's certificate is signed by an internal
CA: certificates stay verified. Use `insecure_registries` for a registry
without TLS or without a CA file.

Both apply to docker:dind services (any service image with `dind` in its
name), so `docker build` and `docker push` in jobs work without changing the
service's `command`. Arguments are appended after the service's own
`command`, so that command must be dockerd flags (the usual case).
`registry_ca` works for rootful docker:dind, which reads
`/etc/docker/certs.d`. A rootless image (`docker:dind-rootless`) reads its
CAs from `~/.config/docker/certs.d`, so use `insecure_registries` for it.

Job images are pulled by the host dockerd, which reads its own settings. At
start the runner checks them and logs the exact fix:

- `insecure_registries`: the registry must be in `insecure-registries` of
  `/etc/docker/daemon.json` (dockerd restart needed).
- `registry_ca`: the CA must be in `/etc/docker/certs.d/<registry>/ca.crt`
  (read on every connection, no restart).

The installer does the second for you: `--registry-ca HOST=FILE` installs the
CA for dockerd and writes `registry_ca`. `--insecure-registry HOST` writes
`insecure_registries`; `daemon.json` is left to you.

Files that jobs create in the workspace belong to root in the container; the
runner hands them back to its own user after every job.

!!! warning "The docker group is root-equivalent"
    The `turboci` user is in the `docker` group, which can start privileged
    containers. Treat the runner host accordingly.

### Workspaces

Like gitlab-runner, the runner keeps a project's workspace for its next job:
with `GIT_STRATEGY: fetch` (the default when the project's *Git strategy*
setting is *git fetch*), that job updates the existing checkout with
`git fetch` instead of cloning the repository again, then removes untracked
and ignored files (`GIT_CLEAN_FLAGS`, default `-ffdx`).

| Key | Default | Description |
|---|---|---|
| `[executor] workspace_max_age_days` | `7` | Workspaces no job used for this many days are deleted. `0` deletes every workspace when its job ends, so every job clones |
| `[executor] workspace_max_size` | unset | Total size of the kept workspaces of all projects (`"512m"`, `"50g"`, bytes). Beyond it the least recently used workspaces are deleted, never one a job is using. Unset or `"0"`: no limit |

Both limits are applied when the runner starts and then every hour, for every
project, whether or not it still has jobs. A workspace that a job is using is
never deleted, even when the workspaces in use alone exceed
`workspace_max_size` (the runner then logs a warning). Workspaces are
measured without blocking jobs; a deleted workspace is first renamed, so a
job that starts meanwhile gets a fresh one and clones.

- Each project has one workspace per job running at the same time
  (`CI_CONCURRENT_PROJECT_ID`), and protected branches and tags never share
  a workspace with unprotected ones. They live in
  `<builds directory>/project-<id>-<slot>[-protected]`.
- An earlier job may have changed the repository, so its git config and
  hooks are replaced before the checkout. Repositories with submodules
  (`GIT_SUBMODULE_STRATEGY`) are always cloned, and a checkout that cannot be
  updated (for example after a job was killed mid-fetch) is cloned again.
- When GitLab does not say whether the ref is protected, the job gets a
  workspace of its own that is deleted when it ends.

### Shell

!!! danger "No isolation"
    Job scripts run directly on the host as the `turboci` user. Any project
    that can run jobs on this runner can read what that user can read. Use it
    only when every project on the runner is trusted.

| Key | Default | Description |
|---|---|---|
| `work_dir` | `/var/lib/turboci/builds` | Where job workspaces are created |

Jobs run with `bash` when it is installed, otherwise `sh`, as the service
user. To run them as a user that already has the tools (SDKs in its home, for
example), install the runner with `--user`; see
[a shell runner next to a docker runner](installation.md#a-shell-runner-next-to-a-docker-runner).
