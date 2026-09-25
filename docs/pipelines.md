# Pipeline support

Most of `.gitlab-ci.yml` is handled by GitLab itself (rules, `needs` ordering,
`retry`, `parallel`, `trigger`, environments, coverage parsing). This page
lists what a runner has to do, and how TurboCI does it.

## Jobs and scripts

| Feature | Notes |
|---|---|
| `before_script`, `script`, `after_script` | Each step runs as one shell script with `set -e` (and `pipefail` where the shell has it), so a failing command fails the step |
| `bash` / `sh` | `bash` when the image has it, otherwise `sh` |
| `when`, `allow_failure` | `after_script` sees the job result in `CI_JOB_STATUS` (`success`, `failed`, `canceled`) |
| Job timeout | From the project or runner setting; the job fails as *job execution timeout* |
| `after_script` timeout | 5 minutes, or `RUNNER_AFTER_SCRIPT_TIMEOUT` |
| `RUNNER_SCRIPT_TIMEOUT` | Caps the script steps inside the job timeout (Go duration: `10m`, `1h30m`) |
| Cancel from the UI | The script is stopped, `after_script` still runs, then the job is cleaned up |
| `hooks:pre_get_sources_script` | Runs before the checkout, after the runner's `pre_get_sources_script` |
| `CI_DEBUG_TRACE: "true"` | Every command is echoed (`set -x`); masking still applies |
| Collapsible log sections | `get_sources`, `restore_cache`, `download_artifacts`, `step_*`, `after_script`, `archive_cache`, `upload_artifacts_*` |

## Images and services (docker executor)

| Feature | Notes |
|---|---|
| `image:name`, `entrypoint` | The job image needs no `git`: sources are checked out by a helper container |
| `image:pull_policy` | Honoured when listed in [`allowed_pull_policies`](configuration.md#docker) |
| `services` | Own network per job; reachable by name-derived aliases and `alias`; `variables`, `command`, `entrypoint` per service. The script starts once service ports accept connections (up to 30 s) |
| Private images | `DOCKER_AUTH_CONFIG`, the runner's Docker `config.json`, or GitLab's registry credentials; see [private registries](configuration.md#private-registries) |
| Runner services | Services from the runner config start for every job, e.g. Docker-in-Docker; see [services for every job](configuration.md#services-for-every-job) |

## Sources

| Variable | Notes |
|---|---|
| `GIT_STRATEGY` | `fetch`, `clone`, `none`, `empty`. Every job starts from a fresh workspace, so `fetch` and `clone` behave the same |
| `GIT_DEPTH` | Shallow fetch depth; the project setting is the default |
| `GIT_CHECKOUT` | `false` fetches without checking out |
| `GIT_FETCH_EXTRA_FLAGS` | Extra `git fetch` flags, e.g. `--filter=blob:none` |
| `GIT_CLONE_PATH` | Checks out into `$CI_BUILDS_DIR/<path>` (e.g. `$CI_BUILDS_DIR/$CI_PROJECT_PATH`); a path outside the builds directory fails the job |
| `GIT_SUBMODULE_STRATEGY` | `none`, `normal`, `recursive` |
| `GIT_SUBMODULE_DEPTH`, `GIT_SUBMODULE_PATHS`, `GIT_SUBMODULE_UPDATE_FLAGS` | As in gitlab-runner |
| Git LFS | LFS objects are pulled after checkout when `git-lfs` is available; `GIT_LFS_SKIP_SMUDGE=1` skips them |
| `GET_SOURCES_ATTEMPTS` | Retries of the checkout (1–10) |

The pipeline's exact commit is checked out through the refspecs GitLab sends,
so merge request and tag pipelines get the right code.

## Artifacts

| Feature | Notes |
|---|---|
| `paths`, `exclude`, `untracked` | Globs with `**`; paths may use variables |
| `name`, `expire_in`, `when` | `when: on_success` (default), `on_failure`, `always` |
| `reports` | Uploaded in the format GitLab asks for (`zip`, `gzip`, `raw`) |
| Dependencies | `needs` and `dependencies` artifacts are downloaded and extracted before the script |
| `ARTIFACT_DOWNLOAD_ATTEMPTS` | Retries of dependency downloads (1–10) |

Archives are streamed to and from disk, never held in memory. Extraction
rejects entries that would escape the workspace (`../`, absolute paths,
symlinks pointing outside).

## Cache

| Feature | Notes |
|---|---|
| `key` | May use variables; `key:files` is computed by GitLab |
| `paths`, `untracked` | |
| `policy` | `pull-push` (default), `pull`, `push` |
| `when` | `on_success` (default), `on_failure`, `always` |
| `fallback_keys`, `CACHE_FALLBACK_KEY` | Tried in order when the key has no archive |
| `RESTORE_CACHE_ATTEMPTS` | Retries of a cache restore (1–10) |

## Variables

All CI/CD variables GitLab sends, including file-type variables (written to a
file whose path is the variable's value) and `$VAR` / `${VAR}` expansion.
Masked variables, the job token and dependency tokens are replaced by
`[MASKED]` in the job log, also when a value is split across output chunks.

TurboCI adds the variables gitlab-runner adds:

| Variable | Value |
|---|---|
| `CI_SERVER` | `yes` |
| `CI_BUILDS_DIR`, `CI_PROJECT_DIR` | `/builds`, `/builds/project` in containers |
| `CI_JOB_IMAGE` | The job image |
| `CI_JOB_TIMEOUT` | Job timeout in seconds |
| `CI_JOB_STATUS` | In `after_script`: `success`, `failed` or `canceled` |
| `CI_CONCURRENT_ID`, `CI_CONCURRENT_PROJECT_ID` | Slot of the job on this runner / among the project's jobs |
| `CI_DISPOSABLE_ENVIRONMENT`, `CI_SHARED_ENVIRONMENT` | Docker / shell executor |
| `CI_RUNNER_VERSION`, `CI_RUNNER_REVISION`, `CI_RUNNER_EXECUTABLE_ARCH`, `CI_RUNNER_SHORT_TOKEN` | Runner details |
| `CI_SERVER_TLS_CA_FILE`, `GIT_SSL_CAINFO` | When [`tls_ca_file`](configuration.md#runner) is set |

## Not supported

- Executors other than `docker` and `shell` (Kubernetes, docker-autoscaler, ...)
- Native GCS and Azure caches (GCS works through its S3 interoperability with HMAC keys, see [S3 cache](configuration.md#shared-cache-in-s3))
- Interactive web terminal and the session server
- External secrets (`secrets:` with Vault, Azure Key Vault, ...)
- Docker credential helpers (`credsStore`, `credHelpers`)
- `GIT_SUBMODULE_FORCE_HTTPS`, `GIT_CLEAN_FLAGS`
- Feature flags (`FF_*`)

The runner tells GitLab which features it has, so jobs that need an
unsupported executor feature are not sent to it.
