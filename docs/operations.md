# Operations

## Service

```bash
sudo systemctl status turboci
sudo journalctl -u turboci -f       # runner and job events
sudo systemctl restart turboci      # after a config change
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

`systemctl stop` and `restart` send `SIGTERM`; the unit allows 180 seconds for
the cleanup. To drain the runner before maintenance:

```bash
sudo systemctl kill -s SIGQUIT turboci
```

## Cleanup

Every job's containers, network and workspace are removed when it ends, also
when it fails, times out or is canceled. Containers and networks carry the
label `turboci.runner=<system id>`; on start the runner removes any it left
behind (for example after a power loss). Containers of other runners on the
same Docker daemon are never touched.

The local cache removes archives not used for `cache_max_age_days` (default
14). To see or clear it:

```bash
turboci runner-stats -c /etc/turboci-runner.toml
sudo rm -rf /var/lib/turboci/cache/*
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
`/etc/machine-id` (`s_...`), so it stays the same across restarts and
reinstalls on the same machine.

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

Versions that ran as root left root-owned workspaces. Remove them once:

```bash
sudo rm -rf /tmp/turboci-builds /tmp/turboci
sudo systemd-tmpfiles --create /etc/tmpfiles.d/turboci.conf
```
