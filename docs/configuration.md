# Configuration

The runner reads one TOML file, `/etc/turboci-runner.toml` when installed with
the installer. Every key is optional except `runner_token`. Unknown keys are
logged as warnings at startup; invalid values stop the runner with an error
instead of being silently ignored.

Apply changes with `sudo systemctl reload turboci`, without stopping running
jobs: `concurrent`, `check_interval` and the [admission
limits](#admission-control) change at once, settings that only matter to a
job (`environment`, hooks, Docker settings, cache and workspace limits, ...)
apply to the jobs that start afterwards. A few settings, such as
`runner_token`, `gitlab_url`, `proxy`, the directories and `executor_type`,
need `sudo systemctl restart turboci` (see [reloading the
config](operations.md#reloading-the-config)).

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
| `check_interval` | `3` | Seconds between the starts of job requests while idle. A request GitLab holds open (long polling, up to 50 s with Workhorse) is followed by the next one at once, as in gitlab-runner; after a 409 Conflict the runner asks again within 0.25 s, doubling up to `check_interval` |
| `tls_ca_file` | – | PEM CA for a GitLab with a self-signed or internal certificate; also given to jobs as `CI_SERVER_TLS_CA_FILE` and `GIT_SSL_CAINFO` |
| `state_dir` | `/var/lib/turboci` | Where the runner keeps state it writes: its system ID, a [rotated token](operations.md#runner-token-rotation), and the part of each [job log](operations.md#job-logs) GitLab has received |
| `output_limit` | `4096` | Most job log kept, in KiB, as gitlab-runner's `output_limit` (`64` to `1048576`, i.e. 1 GiB). Past it the log says it was cut and the job keeps running |
| `metrics_listen` | unset | `IP:port` of the [Prometheus endpoint](operations.md#metrics), e.g. `"127.0.0.1:9252"`. Unset: no endpoint |
| `environment` | `[]` | `KEY=value` variables added to every job, as defaults: the job's own variables override them (see [variable precedence](#variable-precedence)) |
| `pre_get_sources_script` | – | Script run before the checkout of every job (before the job's `hooks:pre_get_sources_script`) |
| `post_get_sources_script` | – | Script run after the checkout |
| `pre_build_script` | – | Script run before `before_script`, in the same shell as the job's script |
| `post_build_script` | – | Script run after `script`, in the same shell |
| `name` | `default` | Name of the runner in logs and in the `runner` label of the [metrics](operations.md#metrics); letters, digits, `.`, `-`, `_` |
| `limit` | `0` | Jobs this runner runs at the same time at most, within `concurrent`; `0`: only `concurrent` limits it. Mostly useful with [several runners](#several-runners) |

These work as the settings of the same names in gitlab-runner's
`[[runners]]`, so a gitlab-runner config can be carried over:

```toml
environment = ["ANDROID_HOME=/home/ci/Android/Sdk"]
pre_get_sources_script = "export PATH=$PATH:/opt/flutter/bin"
```

### Variable precedence

A job gets its variables in this order, and a later definition of a name
overrides an earlier one, as in gitlab-runner:

1. The proxy variables of `proxy` (see [proxy](#proxy)).
2. The runner's `environment`.
3. The variables the runner defines: `CI_CONCURRENT_ID`,
   `CI_CONCURRENT_PROJECT_ID`, `CI_RUNNER_VERSION`, `CI_RUNNER_REVISION`,
   `CI_RUNNER_EXECUTABLE_ARCH`, `CI_RUNNER_SHORT_TOKEN`.
4. The job's variables from GitLab: predefined ones (`CI_COMMIT_SHA`, ...),
   then instance, group and project variables, `.gitlab-ci.yml`, pipeline,
   trigger and manual variables, in GitLab's own precedence.

So `environment` holds defaults: `environment = ["GIT_DEPTH=10"]` applies to
jobs that set no `GIT_DEPTH`, and a job's `GIT_DEPTH: 0` wins. The runner
logs a warning at start (and on a reload) when `environment` sets
`GIT_STRATEGY`, `GIT_DEPTH` or `GIT_CLEAN_FLAGS`.

!!! warning "Changed after 0.12.1"
    Up to 0.12.1 `environment` came last and overrode the job's own
    variables. A config carried over from gitlab-runner, or written for an
    older TurboCI, with for example `GIT_STRATEGY=clone` in `environment` made
    every job clone. It is now only the default for jobs that set none.

Unlike gitlab-runner, `environment` also overrides `CI_BUILDS_DIR`,
`CI_PROJECT_DIR`, `CI_SERVER`, `CI_JOB_STATUS`, `CI_JOB_TIMEOUT` and
`CI_SHARED_ENVIRONMENT` / `CI_DISPOSABLE_ENVIRONMENT` when it sets them; do
not set those there. An entry is split at its first `=`. The value is kept
as written, as in gitlab-runner: `"KEY= v"` gives `" v"`. Spaces around the
name are dropped.

## Several runners

One TurboCI process can run several runners, like gitlab-runner's
`[[runners]]`: for different GitLab instances, projects or groups, or a Docker
and a shell runner on one host. A file without `[[runners]]` is one runner,
as before.

```toml
concurrent = 8              # jobs of all runners together
check_interval = 3
state_dir = "/var/lib/turboci"
# metrics_listen, min_free_memory, min_free_disk and max_load_avg also go here

[[runners]]
name = "docker"
runner_token = "glrt-AAAA"
gitlab_url = "https://gitlab.example.com"
limit = 6                   # at most 6 of the 8
[runners.executor.docker]
memory = "4g"

[[runners]]
name = "macos-builds"
runner_token = "glrt-BBBB"
gitlab_url = "https://gitlab.other.example"
environment = ["ANDROID_HOME=/opt/android"]
[runners.executor]
executor_type = "shell"
```

- The top of the file holds only the settings of the whole process:
  `concurrent`, `check_interval`, `metrics_listen`, the [admission
  limits](#admission-control) and `state_dir`. Any other key there is an
  error, as is one of these in a `[[runners]]` entry.
- Each entry takes every runner setting of a file without `[[runners]]`
  (written `[runners.executor.docker]` and so on), and needs a unique `name`.
- Runners never share state: a runner's `state_dir` defaults to
  `<state_dir>/runners/<name>`, its `cache_dir` to `<its state_dir>/cache`
  and its workspaces (Docker `builds_dir`, shell `work_dir`) to
  `<its state_dir>/builds`. Directories set by hand must not be shared or
  nested between runners. Each runner has its own system ID, so one never
  removes another's containers.
- A job takes a slot of its runner (`limit`) and one of `concurrent`. Each
  runner polls its GitLab every `check_interval`.
- Runners with the same `proxy`, `no_proxy` and `tls_ca_file` share one HTTP
  connection pool.
- A reload applies each runner's settings as for a single runner; adding or
  removing a runner needs a restart.

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
  `NO_PROXY` and their lowercase forms. The runner's `environment` overrides
  them, and a job that sets one of them itself keeps its own value. Each of the
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
| `cache_max_size` | unset | Total size of the archives of all projects, of both formats (`"512m"`, `"50g"`, bytes); after each save or download the least recently used archives are deleted until the cache fits, whatever their project. The archive just written is kept even if it is larger on its own. Unset or `"0"`: no limit |
| `cache_legacy_zip` | `true` | Restore the ZIPs of versions up to 0.10 (`<key>.zip`), from the local cache and from S3, for keys without a `.tar.zst` archive. Each cache miss then costs a second S3 request (`.tar.zst`, then `.zip`). Set `false` once every runner that shares the cache runs 0.11.0 or later |

Without `cache_s3` the cache is local to the runner host, like gitlab-runner
without a distributed cache. Archives are written to a temporary file and
renamed into place, so concurrent jobs never read a partial archive.

Cache archives are a tar of the cached files, symlinks and directories
(empty ones included) in one zstd stream, `<key>.tar.zst`; artifacts stay
deflate ZIPs, which GitLab reads. One stream compresses across files and is
extracted as it downloads. File modes are kept (without setuid, setgid and
sticky bits), timestamps and owners are not. On a synthetic `node_modules`
of 15,000 files (57.6 MB, a twentieth of them incompressible images) the
archive is 17.1 MB, saved in 0.37 s and extracted in 0.62 s, against
20.6 MB, 0.46 s and 0.70 s for the zstd ZIPs of versions up to 0.10. zstd
level 3 was both larger (17.9 MB) and slower there, so level 1 is used.
Caches whose files hold more than 64 MiB are compressed by up to four zstd
threads (one per CPU) besides the one reading the files: on a 500 MB mixed
tree (23,000 files, two fifths incompressible) the archive took 0.6 to 0.8 s
instead of 1.3 s, for 0.04% more bytes (253.2 MB against 253.1 MB). zstd
writes the same archive for any number of threads, so runners with
different CPU counts still find each other's archives unchanged.

A `pull-push` cache the job leaves unchanged is not archived again. Right
after restoring the job's own key (not a fallback key), the runner records
the state of the files the cache's `paths` match: their paths and, for
each file and symlink, size, mode, mtime and ctime to the nanosecond,
inode and symlink target (no content is read). When it saves the key and
that state is the same, the job log shows `Cache main unchanged since
restore: not saved`, and neither the archive is written nor S3 asked.
Writing to a file or replacing it always changes its ctime or inode, which
no tool can set back, so a change that keeps size and mtime (npm extracts
every package file with the same fixed mtime; `cp -p`, `rsync -t` and
`tar -x` restore mtimes) is still seen; the runner waits 20 ms after the
restore so that no write lands in the clock tick the state was taken in.
The cache is saved as usual when the paths match files the archive did not
hold (checked-in files, another cache under the same paths), when the
local copy was used because S3 failed or lacked the key, for `untracked`
caches, for a ZIP of version 0.10, and on file systems with whole-second
timestamps.

Versions up to 0.10 saved ZIPs, `<key>.zip`. They still restore, from the
local cache and from S3, when a key has no `.tar.zst` archive; the next save
writes the new format and deletes the local ZIP. The new format has its own
name, so in a fleet that mixes versions older runners never download an
archive they cannot read: they keep reading and writing the ZIPs, and newer
runners read those until they save the key themselves. While both run, a
cache saved by a newer runner is not seen by older ones, and once a newer
runner saved a key, a ZIP an older runner saves later is not seen by newer
ones; upgrade every runner that shares an S3 cache to end that. Once all
run 0.11.0 or later, set `cache_legacy_zip = false`: a miss then asks S3
for the `.tar.zst` archive only, and ZIPs are no longer read (keys that
only have one start empty and are saved in the new format).

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
disk, a changed one is extracted while it downloads, and kept as the new
local copy. A download that breaks off midway is a warning, and the local
copy is extracted over what arrived. Saved archives are written locally and
uploaded; archives above 64 MiB go as a multipart upload, in parts of 16 MiB
(larger above 156 GiB, to stay within S3's 10,000 parts), four at a time.
Each part is read from disk once. Over HTTPS it streams from the archive
with an unsigned payload (`UNSIGNED-PAYLOAD`, which AWS S3, MinIO, Ceph RGW
and R2 accept): TLS protects the body, the headers stay signed, and the
archive's SHA-256 stored with the object is checked by every download. Over
plain HTTP a part is read into memory, hashed, signed with its hash and sent
from there, which holds at most 64 MiB per upload (four parts of 16 MiB, or
one larger part at a time for archives above 156 GiB). A failed or canceled
multipart upload is aborted, so S3 keeps no parts of it; on SIGTERM or
SIGINT the runner waits up to 10 seconds for the aborts of the uploads its
canceled jobs leave before it exits, and logs those that did not finish.
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
well: a runner that is killed during an upload, or loses S3 in the middle
(or whose aborts take longer than those 10 seconds), cannot abort it, and S3
keeps (and bills) the parts until the rule removes them. Archives are stored as
`<prefix>/project-<id>/<encoded-key>.tar.zst` (`.zip` for versions up to
0.10), the cache key percent-encoded (`feature/x` is `feature%2Fx.tar.zst`),
and a key whose encoded name is longer than 200 characters as
`h-<blake3 hash>.tar.zst`; every runner with the
credentials can write every project's cache, so use one bucket or prefix per
group of runners that trust each other. Archives above 5 TiB, S3's largest
object, are kept only locally.

## Executor

`[executor] executor_type` is `docker` (default) or `shell`.

### Docker

Each job runs in its own container. Sources are checked out by a helper
container, so job images need neither `git` nor any other tool. The
helper runs as the owner of the workspace directory, so the checkout belongs
to the runner's user; it runs as root when `pre_get_sources_script` or
`post_get_sources_script` hooks are set (they may need root), when the
workspace belongs to root, or when an earlier job left files in it that belong
to another user. Checking that walks the workspace, except when the last job
in it ended with everything handed back to the runner's user (remembered
until the runner restarts). Jobs
with `services` get their own network where each service is reachable by its
aliases. The checkout runs while the job image is pulled and the services
start; the log shows the services' progress after the `get_sources` section,
in a `prepare_containers` section, as it happens once `get_sources` is closed.
If either fails, or the job times out or is canceled, the other is stopped
and every container of the job is removed.

Once the scripts have ended, the job container is stopped and the helper
gives the files the job created (as root, usually) to the runner's user, so
the runner can read them for the cache and artifacts. The job's containers,
its network and the job container's volumes (a `docker:dind` service's
images) are removed after the job has been reported to GitLab; until then the
job keeps its slot and its workspace, so no other job starts in it early. Concurrent jobs that need the same image with the same registry
login wait for one pull of it; a job never shares the pull of a job that
logged in differently.

The runner talks to dockerd over `/var/run/docker.sock`, or over the
`unix://` socket that the service's `DOCKER_HOST` names. A `DOCKER_HOST`
that is not a unix socket (`tcp://`, `ssh://`) stops the runner at startup
instead of quietly using the default socket.

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
| `auth_config_file` | `~/.docker/config.json` of the service user | Docker client config whose logins (`auths`, `credsStore`, `credHelpers`) are used to pull private images |
| `allowed_docker_credential_helpers` | `{}` | Credential helpers a job's `DOCKER_AUTH_CONFIG` may run: helper → registry patterns (see [Private registries](#private-registries)) |
| `insecure_registries` | `[]` | Registries (`host[:port]`) reached over HTTP or without certificate checks; docker:dind services get `--insecure-registry` for each |
| `registry_ca` | `{}` | Registry → PEM CA file; mounted into docker:dind services as `/etc/docker/certs.d/<registry>/ca.crt` |
| `services` | `[]` | Services started for every job (see below) |
| `extra_hosts` | `[]` | `host:ip` entries of `/etc/hosts` in job, service and checkout containers; `host-gateway` is the Docker host |
| `dns` | `[]` | DNS servers (IP addresses) of job, service and checkout containers |
| `dns_search` | `[]` | DNS search domains of job, service and checkout containers |
| `cap_add` | `[]` | Linux capabilities added to job containers, e.g. `["SYS_PTRACE"]` |
| `cap_drop` | `[]` | Linux capabilities dropped from job containers, e.g. `["NET_RAW"]` |
| `security_opt` | `[]` | Security options of job containers, e.g. `["no-new-privileges"]`; `seccomp=<file>` sends the file's JSON profile (read at start) |
| `devices` | `[]` | Host devices of job containers: `/dev/host[:/dev/container[:rwm]]` |
| `gpus` | – | GPUs of job containers, as `docker run --gpus`: `"all"`, a count, or e.g. `"\"device=0,1\""` (needs the NVIDIA container toolkit on the host) |
| `sysctls` | `{}` | Kernel parameters of job containers, e.g. `"net.ipv4.ip_unprivileged_port_start" = "0"` |
| `ulimit` | `{}` | Resource limits of job containers: name → `"soft:hard"` or one value for both, e.g. `nofile = "1024:4096"` (`-1`: unlimited) |
| `runtime` | – | OCI runtime of job and service containers, e.g. `"nvidia"` (must be in dockerd's `runtimes`) |
| `allowed_images` | `[]` | Glob patterns of the images jobs may name in `image:`; empty: any (see [Allowed images](#allowed-images-and-services)) |
| `allowed_services` | `[]` | Glob patterns of the images jobs may name in `services:`; empty: any |
| `image_max_age_days` | – | Images no container uses and no job used for this many days are removed (0: every unused image); see [Image cleanup](#image-cleanup) |
| `image_max_size` | – | Total size of the unused images, e.g. `"50g"`; the least recently used are removed beyond it |

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

### Container options

The container options of gitlab-runner's `[runners.docker]` work the same
way. As in gitlab-runner, services only get `extra_hosts`, `dns`,
`dns_search` and `runtime`; capabilities, security options, devices, GPUs,
sysctls and ulimits apply to the job container only (gitlab-runner's separate
`services_*` variants are not supported). The container that checks out
sources gets `extra_hosts`, `dns` and `dns_search`, so a GitLab that only
`/etc/hosts` knows is reachable for the checkout too.

```toml
[executor.docker]
extra_hosts = ["gitlab.corp.local:10.0.0.5"]
dns = ["10.0.0.53"]
dns_search = ["corp.local"]
cap_drop = ["NET_RAW"]
security_opt = ["no-new-privileges"]
# GPU jobs (NVIDIA container toolkit on the host)
# gpus = "all"
# runtime = "nvidia"

[executor.docker.sysctls]
"net.ipv4.ip_unprivileged_port_start" = "0"

[executor.docker.ulimit]
nofile = "1024:4096"
```

Every value is checked when the runner starts (an IP for `dns`, `host:ip` for
`extra_hosts`, known `ulimit` names, a readable JSON seccomp profile, ...).
Whether the kernel or dockerd accepts a sysctl, a device or a runtime is only
known when the job container is created: a wrong one fails each job with
dockerd's error.

### Allowed images and services

With `allowed_images` or `allowed_services` set, a job whose `image:` or
`services:` matches none of the patterns fails before anything is pulled,
with the list of allowed patterns in its log (gitlab-runner's
`allowed_images` and `allowed_services`):

```toml
[executor.docker]
allowed_images = ["ruby:*", "registry.corp.local/**"]
allowed_services = ["postgres:*", "redis:*"]
```

Patterns are globs as in gitlab-runner: `*` and `?` match within one path
segment, `**` as a whole segment matches any number of segments
(`registry.corp.local/**` allows every image of that registry), `[abc]`
matches one character; `{a,b}` is not supported. The name is matched as the
job wrote it, variables expanded and not normalized: `ruby:*` allows
`ruby:3.3` but not `ruby` (no tag) or `docker.io/library/ruby:3.3`.
`default_image` (for jobs without `image:`, or naming it) and the services in
the runner's config are always allowed; `helper_image` is the runner's own.

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

Within one config, the login for a registry comes from, in the docker CLI's
and gitlab-runner's order:

1. `credHelpers["<registry>"]`, a credential helper for that registry;
2. `credsStore`, the config's credential store, when it holds a login for the
   registry (asked with `list`, then `get`);
3. the static `auths` entry.

A helper `<name>` is the program `docker-credential-<name>` on the runner's
`PATH` (for an installed runner, the service's `PATH`), run on the runner's
host as the runner's user with the docker-credential-helpers protocol
(`get` with the registry on stdin). A `Username` of `<token>` makes the secret
an identity token. A helper may take 30 seconds; secrets never reach a log.

When a helper is missing, fails or times out, that config gives no login for
the registry and the next one in the list above is tried (eventually an
anonymous pull), as in gitlab-runner. The runner's log gets a warning with
the helper's error output, and if the pull then fails, the job log shows the
warning after the pull error. A helper with no login for the registry
(`credentials not found`) also leaves it to the next config.

Helpers of `auth_config_file` belong to the operator and always run. A job's
`DOCKER_AUTH_CONFIG` (including one set in the runner's `environment`) may
only run the helpers `allowed_docker_credential_helpers` lists for the
registry, as gitlab-runner's setting of that name does: otherwise any project
could have the runner host log in with the operator's credentials (a cloud
role, a desktop keychain). Others are skipped with a warning, and the
config's static `auths` still apply:

```toml
[executor.docker.allowed_docker_credential_helpers]
ecr-login = ["*.dkr.ecr.*.amazonaws.com"]
```

With that, a job can pull from Amazon ECR with

```json
{"credHelpers": {"123456789012.dkr.ecr.eu-west-1.amazonaws.com": "ecr-login"}}
```

in `DOCKER_AUTH_CONFIG`, using the AWS credentials the runner's service has.

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

### Image cleanup

Pulled images pile up on the Docker host. With `image_max_age_days` or
`image_max_size` set, the housekeeping that runs at start and every hour
removes images no container uses:

```toml
[executor.docker]
image_max_age_days = 14
image_max_size = "100g"
```

- `image_max_age_days`: images no job used for that many days go (0: every
  unused image, at every sweep).
- `image_max_size`: while the unused images take more than this, the least
  recently used go. Sizes are those of `docker images`, so layers shared
  between images count once per image and the space freed can be less.

Kept are images any container uses (running or stopped), images a running
job of this runner uses (also while it is pulling them), `helper_image`,
`default_image` and the services of the runner's config. An image is removed
by untagging its tags, never forced: dockerd keeps an image a container
started using meanwhile, or one that is the parent of another image. Every
sweep logs the removed images and the space freed.

dockerd does not record when an image was last used, and its build time
(`Created`) says nothing about it: a fresh pull of `postgres:16` may bring an
image built months ago. The runner therefore counts an image's age from the
last job of this runner that used it, or else from when the cleanup first saw
it. That record lives in memory: after a restart every image starts again
from zero, so a restart can delay a removal but never brings one forward.
Images used the same time ago go in the order they were built.

!!! warning
    The cleanup covers every image on the Docker host, not only the ones the
    runner pulled. Enable it on hosts whose Docker is dedicated to the
    runner.

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
| `[executor] share_workspace_across_protection` | `false` | `true`: protected and unprotected refs of a project share its workspaces, as in gitlab-runner. `false`: protected refs get workspaces of their own |

Both limits are applied when the runner starts and then every hour, for every
project, whether or not it still has jobs. A workspace that a job is using is
never deleted, even when the workspaces in use alone exceed
`workspace_max_size` (the runner then logs a warning). Workspaces are
measured and deleted without blocking jobs: a workspace to delete is first
renamed, so a job that starts meanwhile gets a fresh one and clones.

- Each project has one workspace per job running at the same time
  (`CI_CONCURRENT_PROJECT_ID`). They live in
  `<builds directory>/project-<id>-<slot>[-protected]`.
- By default protected branches and tags never share a workspace with
  unprotected ones: a job can leave files, git config or hooks in its
  workspace, and a job of an unprotected branch (which anyone who can push
  a branch can write) must not be able to plant them for a job of a
  protected ref, which may hold deploy secrets. gitlab-runner keeps one
  workspace per project and slot for every ref; set
  `share_workspace_across_protection = true` to do the same and fetch less
  when a project alternates between protected and unprotected refs.
- An earlier job may have changed the repository, so its git config and
  hooks are replaced before the checkout. Repositories with submodules
  (`GIT_SUBMODULE_STRATEGY`) are always cloned, and a checkout that cannot be
  updated (for example after a job was killed mid-fetch) is cloned again.
- When GitLab does not say whether the ref is protected (older GitLab
  versions), the job uses the unprotected workspace, as gitlab-runner does:
  the runner cannot tell it from an unprotected job, and the protected
  workspace stays reserved for jobs GitLab marks protected. Up to 0.12.1
  such a job got a workspace of its own and always cloned.
- A Docker job with `GIT_STRATEGY: none` gets an empty workspace of its own
  that is deleted when it ends: nothing checks out or cleans it, so it would
  see what earlier jobs left. A shell job with `GIT_STRATEGY: none` uses the
  project's workspace, as in gitlab-runner.

### Shell

!!! danger "No isolation"
    Job scripts run directly on the host as the `turboci` user. Any project
    that can run jobs on this runner can read what that user can read. Use it
    only when every project on the runner is trusted.

| Key | Default | Description |
|---|---|---|
| `work_dir` | `<state_dir>/builds` | Where job workspaces are created. Up to 0.12.1 the default was `/tmp/turboci-builds`, which the service's private `/tmp` loses on every restart; a config that sets it keeps its value |

Jobs run with `bash` when it is installed, otherwise `sh`, as the service
user. To run them as a user that already has the tools (SDKs in its home, for
example), install the runner with `--user`; see
[a shell runner next to a docker runner](installation.md#a-shell-runner-next-to-a-docker-runner).
