# Benchmarks

Every number on this page was measured, and each section says where, when and
with which versions. They come from one production host and one workload, so
treat them as an example rather than a promise: run the same jobs on your own
runners (see [measuring your own runners](#measuring-your-own-runners)).

## Footprint

| | TurboCI 0.13.4 | gitlab-runner 18.5.0 |
|---|---|---|
| Binary, Linux x86_64 | **8.6 MB**, static (musl) | 92.0 MB |
| Memory when idle (RSS) | **about 8 MB**, 5 threads | 88 MB |

Binary sizes are the release downloads (`turboci-x86_64-unknown-linux-musl`,
`gitlab-runner-linux-amd64`). Idle memory (TurboCI 0.13) was measured on
the same 16-core x86_64 host, with both runners registered and waiting for
jobs. The other release binaries are 7.2 MB (Linux aarch64) and 6.9 MB (macOS
on Apple Silicon).

## Side by side on one host

![Median job time: pipeline 13.5 s vs 6.0 s, 50 MB upload 6.7 s vs 3.0 s, cache 4.9 s vs 2.6 s, artifact download 4.5 s vs 2.1 s, job with a service 4.4 s vs 2.4 s, empty job 3.8 s vs 1.7 s](assets/benchmarks/benchmark-light.svg#only-light)
![Median job time: pipeline 13.5 s vs 6.0 s, 50 MB upload 6.7 s vs 3.0 s, cache 4.9 s vs 2.6 s, artifact download 4.5 s vs 2.1 s, job with a service 4.4 s vs 2.4 s, empty job 3.8 s vs 1.7 s](assets/benchmarks/benchmark-dark.svg#only-dark)

| Median | gitlab-runner 18.5.0 | TurboCI 0.13.4 | |
|---|---|---|---|
| Pipeline of 5 jobs, wall time | 13.5 s | **6.0 s** | −56% |
| Job uploading a 50 MB artifact | 6.7 s | **3.0 s** | −55% |
| Job saving a cache of 2000 files | 4.9 s | **2.6 s** | −47% |
| Job downloading the artifact | 4.5 s | **2.1 s** | −53% |
| Job with a Redis service | 4.4 s | **2.4 s** | −45% |
| Empty job | 3.8 s | **1.7 s** | −55% |

**Setup.** 2026-11-24, one VM with Rocky Linux 9.8, 32 vCPU, 31 GB RAM and
Docker 29.5.3 (overlay2, cgroup v2). Both runners ran on that host against the
same self-hosted GitLab, with the same Docker executor settings: privileged,
a runner-level `docker:dind` service for every job, 3 GB memory and 3 CPUs per
job, 5 concurrent jobs. The run was during working hours, with the team's own
jobs on the same host, so both runners had a few slow outliers; medians are
shown for that reason.

**Workload.** One pipeline of five jobs: an empty job, a job uploading a
50 MB artifact, a job downloading it, a job saving a cache of 2000 small files,
and a job with a Redis service. Pipelines ran one at a time, alternating
between the two runners, five rounds each after a warm-up round that is not
counted. All 50 jobs succeeded. The time of a job is GitLab's `started_at` to
`finished_at`.

The day before, with TurboCI 0.13.0 and no other jobs on the host, the same
pipeline took 7.3 s against 13.0 s (−44%). The difference since then comes
from the parallel cache and artifact restore in 0.13.3 and 0.13.4.

Where the time goes: the checkout, the image pull and the services start at the
same time instead of one after another, the cache and artifacts are restored
while the containers start, artifacts are uploaded while they are written, and
the result is reported to GitLab before the containers are removed.

## Production jobs

The same host also runs the team's real pipelines, on TurboCI since
2026-11-22. The table shows the runner's own time per job: everything except
the job's script (checkout, containers, cache, artifacts, reporting). Each row
is the last 40 successful jobs of that period; gitlab-runner is the same
projects' jobs before the switch.

| Docker executor | Median | Slowest 10% (p90) |
|---|---|---|
| gitlab-runner 18.5.0 | 4.9 s | 9.7 s |
| TurboCI 0.12 | 7.5 s | 12.5 s |
| TurboCI 0.13.0 and 0.13.2 | 2.9 s | 4.2 s |
| **TurboCI 0.13.4** | **2.4 s** | **3.5 s** |

| Shell executor | Median | p90 | Waiting in the queue |
|---|---|---|---|
| gitlab-runner 18.5.0 | 2.9 s | 5.1 s | 7.5 s |
| TurboCI 0.12.1 | 1.1 s | 3.1 s | 2.2 s |
| **TurboCI 0.13.4** | **1.1 s** | **2.7 s** | **2.2 s** |

For long jobs the script itself takes most of the time (a three-minute
`docker build` is still three minutes), so the gain is per job, not
proportional: a pipeline of five Docker jobs run one after another finishes
about 12 s sooner. Deploy jobs, which are mostly runner work, gain the most:
in the first days one service's production deploy went from 41 s to 8 s.

On that host every Docker job starts a `docker:dind` service, which takes
1.4–2.3 s to accept connections and is a large part of the Docker number for
both runners. Since 0.13.4, jobs that do not use Docker can skip that wait with
[`wait_for_services_timeout = -1`](configuration.md#docker), or the service can
be declared only on the jobs that need it.

## Changes in 0.13.4

Measured on a test host, the same job before and after the change:

| Job | Before | 0.13.4 |
|---|---|---|
| Restore a cache and 3 dependency artifacts, with a service | 1.06 s | **0.73 s** |
| Download 3 dependency artifacts | 0.48 s | **0.17 s** |

The cache and artifacts are now restored while the containers start, and up
to four dependency artifacts download at the same time.

## Measuring your own runners

Each job writes one line with the time of every phase to the runner's log:

```bash
sudo journalctl -u turboci | grep '⏱️'
```

The same phases are exported as `turboci_job_phase_duration_seconds` on the
[metrics endpoint](operations.md#metrics). To compare with gitlab-runner, run
the same pipeline on both runners one at a time and compare the jobs'
durations in GitLab (`GET /projects/:id/pipelines/:pipeline_id/jobs`).
