# ⚡ TurboCI

A GitLab CI/CD runner written in Rust. It talks to GitLab's runner API like
[gitlab-runner](https://gitlab.com/gitlab-org/gitlab-runner) and runs jobs in
Docker containers (default) or directly on the host (shell executor). It is a
single static binary with no external services: no Redis, no database.

**Documentation:** https://ismoilovdevml.github.io/turboci/

## ✅ What is supported

- **Executors:** `docker` (one container per job) and `shell`
- **Sources:** the pipeline's exact commit via GitLab refspecs; `GIT_STRATEGY`,
  `GIT_DEPTH`, `GIT_CHECKOUT`, `GIT_FETCH_EXTRA_FLAGS`, submodules, Git LFS,
  `hooks:pre_get_sources_script`
- **Scripts:** `before_script`/`script`/`after_script`, `when`,
  `allow_failure`, job, script and `after_script` timeouts, `CI_DEBUG_TRACE`,
  collapsible log sections
- **Variables:** CI/CD variables incl. file-type and `$VAR` expansion, the
  predefined runner variables; masked values and tokens are masked in the log
- **Artifacts:** `paths`, `exclude`, `untracked`, `when`, `expire_in`, reports
  (`zip`, `gzip`, `raw`); dependency artifacts (`needs`/`dependencies`)
- **Cache:** local cache on the runner host, optionally shared between hosts
  through S3, with `key`, `fallback_keys`, `CACHE_FALLBACK_KEY`, `policy`,
  `when`, `untracked`
- **Docker:** `services` with aliases on a per-job network, `pull_policy`,
  GitLab registry credentials, `privileged`, `volumes`, memory/CPU limits
- **Operations:** cancellation from the UI, SIGQUIT/SIGTERM shutdown,
  `concurrent`, self-signed GitLab CA, runner token rotation,
  checksum-verified install and `turboci upgrade`

## ⛔ Not supported (yet)

Kubernetes and other executors, interactive web terminals, external secrets
(Vault), Docker credential helpers. See
[pipeline support](https://ismoilovdevml.github.io/turboci/pipelines/) for the
full list.

## 🚀 Installation

### Automated (Recommended)

Create a runner in GitLab (project or group **Settings → CI/CD → Runners →
New runner**, add a tag such as `turboci`), then install, register and start it
with one command:

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
  | sudo bash -s -- --url https://gitlab.example.com --token glrt-XXXX
```

The installer verifies the binary against the release's `SHA256SUMS`, installs
Docker if it is missing (docker executor), creates an unprivileged `turboci`
user, writes `/etc/turboci-runner.toml`, starts the systemd service and waits
until the runner has reached GitLab.

| Option | Default | |
|---|---|---|
| `--url URL` | `https://gitlab.com` | GitLab URL |
| `--token TOKEN` | – | runner token; without it the runner is installed but not started |
| `--executor docker\|shell` | `docker` | `shell` runs jobs on the host without isolation |
| `--concurrent N` | `4` | jobs run in parallel |
| `--version vX.Y.Z` | latest | release to install |
| `--binary PATH` | – | install a local binary instead of downloading one |
| `--tls-ca-file PATH` | – | CA (PEM) of a GitLab with a self-signed or internal certificate |
| `--proxy URL` | – | HTTP(S) proxy for the runner, its jobs and services (`http://[user:pass@]host:port`) |
| `--no-proxy LIST` | – | hosts, domains (`.corp.local`) and CIDRs reached directly; needs `--proxy` |
| `--insecure-registry HOST` | – | registry reached over HTTP or without certificate checks, for docker:dind services (repeatable); dockerd needs it in `/etc/docker/daemon.json` too |
| `--registry-ca HOST=FILE` | – | CA (PEM) of a registry, installed for dockerd in `/etc/docker/certs.d/HOST/ca.crt` and for docker:dind services (repeatable) |
| `--no-start` | – | configure but do not start |

Most options can also be given as environment variables (`TURBOCI_URL`,
`TURBOCI_TOKEN`, `TURBOCI_PROXY`, ...). Running the command again upgrades
the runner and replaces the config, keeping a backup.

### Manual Installation

#### 1. Install TurboCI

```bash
VERSION=$(curl -s https://api.github.com/repos/ismoilovdevml/turboci/releases/latest | grep tag_name | cut -d'"' -f4)
BASE="https://github.com/ismoilovdevml/turboci/releases/download/${VERSION}"
curl -fLO "$BASE/turboci-x86_64-unknown-linux-musl"
curl -fLO "$BASE/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
sudo install -m 0755 turboci-x86_64-unknown-linux-musl /usr/local/bin/turboci
```

## ⚙️ Configuration

### 1. Create Config File

```bash
sudo turboci init-runner -o /etc/turboci-runner.toml
```

### 2. Edit Configuration

```bash
sudo nano /etc/turboci-runner.toml
```

**Minimal configuration:**
```toml
concurrent = 4
runner_token = "glrt-YOUR_RUNNER_TOKEN_HERE"
gitlab_url = "https://gitlab.com"

[executor]
executor_type = "docker"
```

The default executor is `docker`, which isolates each job in a container.
`executor_type = "shell"` runs job scripts directly on the host with the
runner's privileges and no isolation; only choose it when every project on the
runner is trusted (the installer accepts `TURBOCI_EXECUTOR=shell`).

### 3. Connect to GitLab

#### Get Runner Token from GitLab:

1. Open your GitLab project
2. Go to **Settings** → **CI/CD** → **Runners**
3. Click **New project runner**
4. Add tag: `turboci`
5. Click **Create runner**
6. Copy the token (starts with `glrt-`)

#### Set Token in Config:

```bash
sudo nano /etc/turboci-runner.toml
```

```toml
runner_token = "glrt-YOUR-TOKEN-HERE"
```

### 4. Start TurboCI

**Systemd service (Linux):**

The service runs as an unprivileged `turboci` system user. Membership in the
`docker` group is needed for the Docker executor (and is root-equivalent on
the host). The config holds the runner token, so only that group may read it.

```bash
sudo useradd --system --no-create-home --home-dir /var/lib/turboci --shell /usr/sbin/nologin turboci
sudo usermod -aG docker turboci
sudo install -d -o turboci -g turboci -m 0750 /var/lib/turboci
sudo chown root:turboci /etc/turboci-runner.toml
sudo chmod 0640 /etc/turboci-runner.toml

sudo tee /etc/systemd/system/turboci.service > /dev/null <<EOF
[Unit]
Description=TurboCI Runner
After=network.target docker.service

[Service]
Type=simple
User=turboci
Group=turboci
WorkingDirectory=/var/lib/turboci
ExecStart=/usr/local/bin/turboci runner-start -c /etc/turboci-runner.toml
Restart=always
RestartSec=10
NoNewPrivileges=true
# Must stay false: Docker bind-mounts job workspaces from /tmp/turboci-builds
PrivateTmp=false
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/turboci /tmp
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictRealtime=true

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable turboci
sudo systemctl start turboci
sudo systemctl status turboci
```

**Manual (for testing):**

```bash
turboci runner-start -c /etc/turboci-runner.toml
```

## 📝 GitLab CI Configuration

In your `.gitlab-ci.yml`:

```yaml
build:
  tags:
    - turboci
  script:
    - cargo build --release
  cache:
    key: ${CI_COMMIT_REF_SLUG}
    paths:
      - target/

test:
  tags:
    - turboci
  script:
    - cargo test
```

## 🔧 Management Commands

```bash
sudo systemctl status turboci
sudo journalctl -u turboci -f
sudo systemctl restart turboci
sudo systemctl stop turboci
sudo systemctl disable --now turboci
```

`systemctl stop`/`restart` send SIGTERM: running jobs are stopped, cleaned up
and reported as failed (runner system failure). To let running jobs finish
first, send SIGQUIT and wait for the service to exit:

```bash
sudo systemctl kill -s SIGQUIT turboci
```

## 🗑️ Uninstallation

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/uninstall.sh | sudo bash
```

Or manually:

```bash
sudo systemctl stop turboci
sudo systemctl disable turboci
sudo rm /etc/systemd/system/turboci.service
sudo rm /usr/local/bin/turboci
sudo rm /etc/turboci-runner.toml
```

## 🏗️ Build from Source

```bash
git clone https://github.com/ismoilovdevml/turboci.git
cd turboci
cargo build --release --features runner
sudo cp target/release/turboci /usr/local/bin/
```

## 📊 Monitoring

```bash
sudo journalctl -u turboci -f                    # job and runner logs
turboci runner-stats -c /etc/turboci-runner.toml  # local cache usage
```

## ❓ Troubleshooting

### Runner not visible in GitLab

```bash
grep runner_token /etc/turboci-runner.toml
grep gitlab_url /etc/turboci-runner.toml
sudo journalctl -u turboci -n 50
```

### Permission denied on job workspaces

The service runs as the `turboci` user. Workspaces left by an older version
that ran as root can not be cleaned up by it; remove them once:

```bash
sudo rm -rf /tmp/turboci-builds /tmp/turboci
sudo systemd-tmpfiles --create /etc/tmpfiles.d/turboci.conf
```

## 📄 License

MIT