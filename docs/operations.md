# Operations

## Service

```bash
sudo systemctl status turboci
sudo journalctl -u turboci -f       # runner and job events
sudo systemctl reload turboci       # apply concurrent, check_interval, admission limits
sudo systemctl restart turboci      # after any other config change
```

A healthy start ends with:

```text
✅ Connected to GitLab at https://gitlab.example.com, waiting for jobs
```

## Stopping

| Signal | What happens |
|---|---|
| `SIGQUIT` | Take no new jobs, let running jobs finish, then exit |
| `SIGTERM`, `SIGINT` | Take no new jobs, stop running jobs, clean them up and report them as failed (*runner system failure*), then exit |
| `SIGHUP` | [Reload the config](#reloading-the-config); the runner keeps running |

`systemctl stop` and `restart` send `SIGTERM`; the unit allows 180 seconds for
the cleanup. To drain the runner before maintenance:

```bash
sudo systemctl kill -s SIGQUIT turboci
```

## Reloading the config

`SIGHUP` makes the runner read its config file again, without stopping jobs:

```bash
sudo systemctl reload turboci                                     # Linux (sends SIGHUP)
sudo launchctl kill SIGHUP gui/$(id -u ci)/io.github.ismoilovdevml.turboci  # macOS, runner user "ci"
```

These settings take effect at once:

| Key | Effect |
|---|---|
| `concurrent` | More slots: new jobs are requested right away. Fewer: running jobs keep running, and new jobs are taken once fewer than the new number run |
| `check_interval` | From the next job request on |
| `min_free_memory`, `min_free_disk`, `max_load_avg` | From the next job request on |

```text
🔄 SIGHUP: reloading the config
🔄 Config reloaded: concurrent 4 → 8, check_interval 3s → 5s
Config changes to cache_dir, executor.docker.memory take effect after a restart
```

Any other change is listed as needing a restart (values are never logged,
only key names), and the runner keeps using the old value until then. A file
that does not parse or validate changes nothing, not even its valid
settings:

```text
❌ Config reload failed, the runner keeps its config: Invalid config file: "/etc/turboci-runner.toml": ...
```

`turboci_config_reloads_total{result="failure"}` in the [metrics](#metrics)
counts failed reloads. A reload during a shutdown (`SIGQUIT`, `SIGTERM`)
does not stop or restart it.

## Cleanup

Every job's containers and network are removed when it ends, also when it
fails, times out or is canceled. Its workspace is kept for the project's next
job and deleted after `workspace_max_age_days` without a job, or earlier when
the workspaces exceed `workspace_max_size` (see
[workspaces](configuration.md#workspaces)); file variables are always removed. Containers and networks carry the
label `turboci.runner=<system id>`; on start the runner removes any it left
behind (for example after a power loss). Containers of other runners on the
same Docker daemon are never touched.

The local cache removes archives not used for `cache_max_age_days` (default
14), and with `cache_max_size` the least recently used archives of any
project once all of them together exceed that size. Both are checked after
each save or download and, like the workspace limits, at start and every
hour for all projects, so the archives of a project without new jobs age out
too. To see or clear it:

```bash
turboci runner-stats -c /etc/turboci-runner.toml
sudo rm -rf /var/lib/turboci/cache/*
```

With `cache_s3` the next job downloads the deleted archives again from S3:
also delete the objects under `<prefix>/project-<id>/` in the bucket, or use
*Clear runner caches* on the project's **Build → Pipelines** page, which
gives the jobs new cache keys.

## Metrics

With `metrics_listen = "127.0.0.1:9252"` the runner serves Prometheus
metrics on `http://127.0.0.1:9252/metrics`; any other path or method gets
404. The endpoint has no authentication: keep it on loopback or a management
network. A port that is taken stops the runner at start with an error.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: turboci
    static_configs:
      - targets: ["ci-runner-01:9252"]
```

| Metric | Type | Meaning |
|---|---|---|
| `turboci_runner_info{version,revision,executor}` | gauge | Always 1; the labels say what runs |
| `turboci_jobs_running` | gauge | Jobs running now |
| `turboci_concurrent_jobs` | gauge | Job slots (`concurrent`) |
| `turboci_jobs_finished_total{result}` | counter | Jobs that ended: `success`, `failed`, `canceled` (in GitLab) |
| `turboci_jobs_failed_total{reason}` | counter | Failed jobs by failure reason: `script_failure`, `runner_system_failure`, `job_execution_timeout`, `image_pull_failure`, `job_canceled` |
| `turboci_job_requests_total{result}` | counter | Job requests to GitLab: `job`, `no_job`, `error`, `conflict` (409: another request took the job GitLab picked; retried after 0.25 s, doubling up to `check_interval`) |
| `turboci_admission_blocked{resource}` | gauge | 1 while [admission control](configuration.md#admission-control) holds off jobs for `memory`, `disk` or `load` |
| `turboci_cache_restores_total{result}` | counter | Cache entries restored: `hit`, `miss`, `error` |
| `turboci_cache_uploads_total{result}` | counter | Saved archives in S3: `sent`, `unchanged` (S3 had the same content), `failed` |
| `turboci_cache_s3_errors_total` | counter | S3 calls for the cache that failed; the jobs used the local cache |
| `turboci_config_reloads_total{result}` | counter | [Config reloads](#reloading-the-config): `success`, `failure` |
| `turboci_workspaces_evicted_total` | counter | Workspaces deleted to stay within `workspace_max_size` |
| `turboci_workspaces_size_bytes` | gauge | Size of the kept workspaces at the last hourly sweep (only with `workspace_max_size`) |
| `process_start_time_seconds` | gauge | Start of the runner process (Unix time) |
| `process_resident_memory_bytes` | gauge | Resident memory of the runner (Linux) |

Alerts worth having:

```yaml
- alert: TurboCIRequestErrors
  expr: rate(turboci_job_requests_total{result="error"}[10m]) > 0
  for: 15m
- alert: TurboCIAdmissionBlocked
  expr: max by (instance) (turboci_admission_blocked) == 1
  for: 30m
- alert: TurboCISystemFailures
  expr: increase(turboci_jobs_failed_total{reason="runner_system_failure"}[1h]) > 3
```

## Runner token rotation

When the GitLab instance gives runner tokens an expiry (*Admin → Settings →
CI/CD → Runner token expiration*), TurboCI renews its token after three
quarters of the token's lifetime, like gitlab-runner:

```text
Runner token expires at 2026-12-01T00:00:00+00:00, resetting it at 2026-11-08T06:00:00+00:00
🔑 Runner token rotated
```

The service cannot write its config in `/etc`, so the new token is stored in
`/var/lib/turboci/runner_token.json` (mode `0600`) and used from then on. It is
tied to the token in the config: putting a different `runner_token` in the
config makes the runner use that one again. The runner only resets the token
after checking that it can store the new one, and a reset in progress is
finished before the runner exits.

## Job logs

- Output is sent to GitLab every 3 seconds, or at the interval GitLab asks for
  (it asks for less frequent updates while nobody is watching the log).
- Logs are limited to 4 MiB, as in gitlab-runner; the job keeps running, and
  the log says that output was cut.
- Masked variables and tokens are replaced by `[MASKED]` before anything is
  sent, including in runner messages.

## System ID

GitLab lists each runner host under the runner as a separate *runner
manager*, identified by a system ID. TurboCI derives it from
`/etc/machine-id` (`s_...`) and keeps it in `state_dir/.runner_system_id`, so
it stays the same across restarts and reinstalls on the same machine. Runners
installed side by side with `--name` get different IDs.

## Troubleshooting

**The runner is not online in GitLab**

```bash
sudo journalctl -u turboci -n 50
```

- `Invalid runner token`: the token was deleted or mistyped; create a new
  runner and run the installer again with `--token`.
- TLS errors: the GitLab certificate is not trusted; see
  [self-signed certificates](gitlab.md#gitlab-with-a-self-signed-or-internal-certificate).

**Jobs stay pending**

The job's tags must all be on the runner, and the runner must be allowed to
run untagged jobs if the job has none. Check the runner's tags and the
*Run untagged jobs* setting in GitLab.

**`Permission denied` on job workspaces after upgrading from an old version**

Versions that ran as root left root-owned workspaces, and versions up to
0.9 kept them in `/tmp`, which the service no longer sees. Remove the old
ones once:

```bash
sudo rm -rf /tmp/turboci-builds /tmp/turboci
```
