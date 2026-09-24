# TurboCI

TurboCI is a GitLab CI/CD runner written in Rust. It speaks GitLab's runner API
like [gitlab-runner](https://gitlab.com/gitlab-org/gitlab-runner) does, so it
runs your existing `.gitlab-ci.yml` jobs unchanged. It runs each job in a Docker
container (default) or directly on the host (shell executor).

It is one static binary (Linux x86_64, musl) with no external services: no
Redis, no database, no helper daemon.

<div class="grid cards" markdown>

- **Install with one command**

    Downloads a checksum-verified release, creates a systemd service and
    registers with GitLab. [Installation →](installation.md)

- **Drop-in for GitLab jobs**

    Artifacts, cache, services, `needs`, masked variables, cancellation and
    timeouts behave as on gitlab-runner. [Pipeline support →](pipelines.md)

- **Small footprint**

    11 MB of memory while idle against 88 MB for gitlab-runner, and 19%
    faster pipelines on the same host. [Benchmarks →](benchmarks.md)

</div>

![A pipeline run by TurboCI](assets/screenshots/pipeline.png)

## How it works

![How TurboCI runs a job: it polls GitLab, checks out sources in a helper container, restores the cache, runs the steps in the job container next to its services, streams the log and uploads artifacts](assets/diagrams/how-it-works.svg)

1. **Request.** Whenever it has a free slot (`concurrent`), the runner asks
   GitLab for a job.
2. **Payload.** GitLab answers with the script, variables, image, services,
   cache and artifact settings.
3. **Checkout.** A small helper container checks out the pipeline's exact
   commit, so job images need no `git`.
4. **Restore.** The cache and the artifacts of earlier jobs are put into the
   workspace.
5. **Run.** Each step runs in the job container; services are reachable by
   their aliases on the job's own network.
6. **Live log.** Output streams to GitLab while the job runs, with secrets
   masked.
7. **Finish.** Artifacts are uploaded, the cache is saved and the result is
   reported. Every container, network and workspace of the job is removed.

## Status

TurboCI runs in production on a self-hosted GitLab 19 instance, next to
gitlab-runner on the same host.
Things it does not do yet are listed under
[not supported](pipelines.md#not-supported); GitLab never sends such jobs to a
runner that does not advertise the feature.
