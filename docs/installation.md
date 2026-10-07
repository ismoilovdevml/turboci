# Installation

TurboCI runs on Linux (x86_64 and aarch64) and on macOS with Apple Silicon.
On Linux the installer supports Ubuntu, Debian, RHEL, Rocky Linux, AlmaLinux
and Fedora with systemd. On macOS it installs a shell runner as a launchd
agent: see [macOS](#macos).

## One command

First [create a runner in GitLab](gitlab.md#create-a-runner) and copy its token
(`glrt-...`). Then, on the machine that will run jobs:

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo bash -s -- --url https://gitlab.example.com --token glrt-XXXX
```

The installer:

1. downloads the latest release and verifies it against the release's
   `SHA256SUMS` (it refuses to install without a matching checksum);
2. installs Docker if it is missing (docker executor only);
3. creates an unprivileged `turboci` system user (in the `docker` group);
4. writes `/etc/turboci-runner.toml` (`root:turboci`, mode `0640`, it holds the
   token);
5. creates and starts the `turboci` systemd service;
6. waits until the runner has reached GitLab and prints the result.

```text
✓ Service running and polling https://gitlab.example.com for jobs
```

If GitLab rejects the token or cannot be reached, the installer says so and
shows the runner's last warnings instead.

The runner then shows as **online** in GitLab.

### Options

| Option | Default | Description |
|---|---|---|
| `--url URL` | `https://gitlab.com` | GitLab URL |
| `--token TOKEN` | – | Runner token. Without it the runner is installed but not started |
| `--executor docker\|shell` | `docker` | `shell` runs jobs on the host without isolation |
| `--concurrent N` | `4` | Jobs run in parallel |
| `--version vX.Y.Z` | latest | Release to install |
| `--binary PATH` | – | Install a local binary instead of downloading one |
| `--tls-ca-file PATH` | – | CA (PEM) of a GitLab with a self-signed or internal certificate |
| `--proxy URL` | – | HTTP(S) proxy for the runner, its jobs and services, and the installer's own downloads (`http://[user:pass@]host:port`) |
| `--no-proxy LIST` | – | Hosts, domains (`.corp.local`) and CIDRs reached directly. Needs `--proxy` |
| `--insecure-registry HOST` | – | Registry reached over HTTP or without certificate checks, for docker:dind services (repeatable). dockerd needs it in `/etc/docker/daemon.json` too |
| `--registry-ca HOST=FILE` | – | CA (PEM) of a registry, installed for dockerd in `/etc/docker/certs.d/HOST/ca.crt` and for docker:dind services (repeatable) |
| `--no-start` | – | Configure but do not start the service |
| `--name NAME` | `turboci` | Install another runner next to the default one: service `NAME`, config `/etc/NAME-runner.toml`, state `/var/lib/NAME` |
| `--user USER` | a new system user | Run the service as this existing user (it is not modified; not `root`) |

Most options can also be set as environment variables: `TURBOCI_URL`,
`TURBOCI_TOKEN`, `TURBOCI_EXECUTOR`, `TURBOCI_CONCURRENT`, `TURBOCI_VERSION`,
`TURBOCI_TLS_CA_FILE`, `TURBOCI_PROXY`, `TURBOCI_NO_PROXY`, `TURBOCI_NAME` and
`TURBOCI_USER`. Passing the token through the environment keeps it out of the
process list and shell history:

```bash
read -rs TURBOCI_TOKEN && export TURBOCI_TOKEN
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo -E bash -s -- --url https://gitlab.example.com
```

### Upgrading

Run the same command again. With a token the config is replaced and the old
one is kept as `/etc/turboci-runner.toml.bak.<date>`; without a token the
existing config is kept. A running service is restarted on the new binary.

Alternatively, on the host:

```bash
sudo turboci upgrade && sudo systemctl restart turboci
```

`turboci upgrade` also verifies the download against `SHA256SUMS`.

### A shell runner next to a docker runner

A host can run several runners, each with its own GitLab token, service and
config. A typical pair is a docker runner for most projects and a shell
runner for builds that need tools installed on the host (mobile SDKs, for
example), running as the user that owns them:

```bash
# docker runner: service turboci, config /etc/turboci-runner.toml
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo bash -s -- --url https://gitlab.example.com --token glrt-DOCKER

# shell runner as the existing user "ci": service turboci-shell,
# config /etc/turboci-shell-runner.toml
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo bash -s -- --url https://gitlab.example.com --token glrt-SHELL \
      --executor shell --name turboci-shell --user ci
```

A shell runner's service can read and write home directories (jobs use the
user's SDKs and package caches); `/usr`, `/boot` and `/etc` stay read-only.
Put the variables the tools need in `environment` in its config.

Each runner has its own system ID, so neither removes the other's containers.
Remove one with `uninstall.sh --name NAME`; a user given with `--user` is
never deleted.

### macOS

A Mac runs the shell executor, for builds that need Xcode, simulators or the
login keychain. Like gitlab-runner, the runner is a launchd agent of an
existing user and runs in that user's login session, so the user must be
logged in: on a CI Mac, turn on automatic login. Run the installer with
`sudo` as that user (or pass `--user`):

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo bash -s -- --url https://gitlab.example.com --token glrt-XXXX --executor shell
```

It runs next to an existing gitlab-runner: give each its own token and tag.
macOS then notifies that "turboci" can run in the background and lists it in
**System Settings → General → Login Items & Extensions**; switching it off
there keeps the runner from starting.

| Path | Purpose |
|---|---|
| `/usr/local/bin/turboci` | The binary |
| `/etc/turboci-runner.toml` | Configuration (owned by the user, mode `0600`) |
| `~/Library/LaunchAgents/io.github.ismoilovdevml.turboci.plist` | The launchd agent |
| `~/Library/TurboCI/turboci` | State: builds, local cache, `runner.log` |

Jobs get `PATH` with Homebrew (`/opt/homebrew/bin`) and `LANG=en_US.UTF-8`
from the agent. Scripts run in `bash -c`, not a login shell: what
`~/.zprofile` or `~/.bash_profile` sets up (rbenv, nvm, `ANDROID_HOME`) is not
loaded. Put such variables in `environment` in the config, or source the
profile in `before_script`.

```bash
tail -f ~/Library/TurboCI/turboci/runner.log                              # logs
sudo launchctl kickstart -k gui/$(id -u)/io.github.ismoilovdevml.turboci  # restart
```

`--name` installs more runners for the same or another user. The docker
executor and the registry options are Linux only.

## What gets installed

| Path | Purpose |
|---|---|
| `/usr/local/bin/turboci` | The binary |
| `/etc/turboci-runner.toml` | Configuration, including the runner token |
| `/etc/systemd/system/turboci.service` | Service unit (hardened: `ProtectSystem=strict`, `NoNewPrivileges`, ...) |
| `/var/lib/turboci` | State: local cache, job workspaces (`builds/`), rotated token |
| `/tmp/turboci-builds` | Docker job workspaces of versions up to 0.9 |
| `/etc/tmpfiles.d/turboci.conf` | Recreates `/tmp/turboci-builds` after a reboot (versions up to 0.9) |

## Manual installation

```bash
VERSION=$(curl -s https://api.github.com/repos/ismoilovdevml/turboci/releases/latest \
  | grep tag_name | cut -d'"' -f4)
BASE="https://github.com/ismoilovdevml/turboci/releases/download/${VERSION}"
# or turboci-aarch64-unknown-linux-musl, turboci-aarch64-apple-darwin
curl -fLO "$BASE/turboci-x86_64-unknown-linux-musl"
curl -fLO "$BASE/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
sudo install -m 0755 turboci-x86_64-unknown-linux-musl /usr/local/bin/turboci

sudo turboci init-runner -o /etc/turboci-runner.toml   # then set runner_token and gitlab_url
turboci runner-start -c /etc/turboci-runner.toml       # run in the foreground
```

For a service, see the unit the installer writes in
[`install.sh`](https://github.com/ismoilovdevml/turboci/blob/main/install.sh).

## Build from source

```bash
git clone https://github.com/ismoilovdevml/turboci.git
cd turboci
cargo build --release
sudo install -m 0755 target/release/turboci /usr/local/bin/
```

## Uninstall

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/uninstall.sh | sudo bash
# a runner installed with --name:
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/uninstall.sh | sudo bash -s -- --name turboci-shell
```

It stops the service and removes the config (a `0600` backup is kept), the
state directory, and the containers, networks and cache volumes the runner
created. The service user is removed only if the installer created it. The
binary, the shared workspace directory and `/etc/turboci-registry-ca` stay
while other TurboCI runners are installed. Registry CAs installed for dockerd
in `/etc/docker/certs.d` are never removed. On macOS it removes the launchd
agent, the binary (unless other runners use it), the config and the state
directory; the user stays. Delete the runner in GitLab afterwards.
