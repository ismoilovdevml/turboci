# Benchmarks

TurboCI and gitlab-runner ran the same pipeline on the same host, against the
same GitLab, with the same settings, taking turns. Every number below was
measured; the scripts to reproduce it are in
[`bench/`](https://github.com/ismoilovdevml/turboci/tree/main/bench).

## Results

16 pipelines per runner (after one warm-up round per series), medians unless
noted.

| | TurboCI 0.6.0 | gitlab-runner 18.5.0 | |
|---|---:|---:|---:|
| **Pipeline duration** (created → finished) | **9.6 s** | 11.9 s | 19% faster |
| Range (min – max) | 7.3 – 13.8 s | 10.1 – 15.3 s | |
| **Sum of the 5 job durations** | **13.5 s** | 18.7 s | 27% less |
| **Memory, idle** (RSS) | **10.8 MB** | 87.9 MB | 8× less |
| Memory, peak while running jobs | 13.5 MB | 88.8 MB | |
| **Binary size** | **11.5 MB** (static) | 92.0 MB | |
| Wait before a job starts (queued) | 1.5 s | 1.5 s | same |

### Per job

| Job | What it does | TurboCI | gitlab-runner |
|---|---|---:|---:|
| `noop` | `true` — pure runner overhead | 1.65 s | 2.70 s |
| `upload` | writes and uploads a 50 MB artifact | 4.47 s | 5.44 s |
| `cache-job` | restores and saves a cache of 2,000 files | 2.58 s | 3.68 s |
| `download` | downloads the 50 MB artifact from `upload` | 2.12 s | 3.34 s |
| `service` | starts `redis:7-alpine` as a service, connects to it | 2.64 s | 3.44 s |

Durations are GitLab's own `duration` and `queued_duration` for each job.

## What the numbers mean

- **Job overhead.** Most of the gain is fixed cost per job: TurboCI starts one
  small helper container for the checkout and runs the script in the job
  container, with no per-stage helper containers. On short jobs this is about
  a second per job; on a 20-minute build it disappears in the noise.
- **Queueing is the same.** Both runners poll GitLab every 3 seconds when idle
  (`check_interval`), so jobs wait the same time to be picked up.
- **Memory** is the runner process itself. It matters most on small VMs and
  when many runners share a host.

!!! note "CPU time is not comparable"
    TurboCI compresses and extracts artifacts and cache inside its own
    process, while gitlab-runner does that in helper containers. Measured per
    process, TurboCI used 3.2 s of CPU per pipeline and gitlab-runner 0.2 s,
    but gitlab-runner's share runs in containers that this measurement does
    not see. We therefore do not compare CPU.

## Setup

| | |
|---|---|
| Host | Rocky Linux 9.8, kernel 5.14, 32 vCPU, 31 GB RAM, Docker 29.5.3 |
| GitLab | Self-hosted GitLab 19.0.1 on the same network |
| Executor | `docker` for both, `concurrent = 4`, `check_interval = 3` |
| Images | `alpine:3.20` and `redis:7-alpine`, already on the host; `pull_policy = if-not-present` for both |
| Runners | TurboCI as its systemd service; gitlab-runner as a separate process with its own config and a project runner, next to the host's existing gitlab-runner |
| Order | The two runners took turns, and the order flipped every round |
| Date | 2026-09-24 |

The host also runs other CI jobs, which is why individual pipelines vary by a
few seconds. The comparison uses medians of 16 runs each.

## Pipeline

```yaml
--8<-- "bench/gitlab-ci.yml"
```

## Reproduce

1. Create a test project and push [`bench/gitlab-ci.yml`](https://github.com/ismoilovdevml/turboci/blob/main/bench/gitlab-ci.yml)
   as `.gitlab-ci.yml` on a branch named `bench`.
2. Register TurboCI with the tag `turboci` and a gitlab-runner with the tag
   `bench-glr` for that project, on the same host and with the same settings.
3. On the host, record memory and CPU:
   `bench/sample.sh <gitlab-runner unit> > samples.txt`
4. From anywhere:

    ```bash
    GITLAB_URL=https://gitlab.example.com GITLAB_TOKEN=glpat-... PROJECT_ID=42 \
      python3 bench/run.py 11 results.json
    python3 bench/analyze.py results.json samples.txt
    ```
