# ⚡ TurboCI - Lightning-Fast GitLab Runner

High-performance GitLab CI/CD runner written in Rust. **5-10x faster** than standard GitLab Runner.

## 🎯 Why TurboCI?

### The Problem
GitLab Runner is slow:
- ✗ Job start: 10 seconds
- ✗ Cache access: 100ms+
- ✗ Cached build: 15+ seconds
- ✗ Memory: 300MB+

### TurboCI Solution
- ✅ Job start: **50ms** (200x faster!)
- ✅ Cache access: **5ms** (20x faster!)
- ✅ Cached build: **2s** (7x faster!)
- ✅ Memory: **50MB** (6x less!)

## 📊 Real-World Benchmark

**Test:** Rust project, 50 dependencies, 10K LOC

| Runner | First Build | Cached Build | Speedup |
|--------|-------------|--------------|---------|
| GitLab Runner | 45s | 14.8s | 1x |
| **TurboCI** | 45s | **2.1s** | **7x** ⚡ |

## 🚀 Installation

### Automated (Recommended)

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh | sudo bash
```

This script installs:
- ✅ Redis server
- ✅ TurboCI latest version
- ✅ Systemd service
- ✅ Auto-start on boot

### Manual Installation

#### 1. Install Redis

**Ubuntu/Debian:**
```bash
sudo apt-get update
sudo apt-get install -y redis-server
sudo systemctl enable --now redis-server
```

**RHEL/Rocky/AlmaLinux:**
```bash
sudo dnf install -y redis
sudo systemctl enable --now redis
```

#### 2. Install TurboCI

```bash
VERSION=$(curl -s https://api.github.com/repos/ismoilovdevml/turboci/releases/latest | grep tag_name | cut -d'"' -f4)
curl -L "https://github.com/ismoilovdevml/turboci/releases/download/${VERSION}/turboci-linux-x86_64" -o turboci
chmod +x turboci
sudo mv turboci /usr/local/bin/
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
redis_url = "redis://127.0.0.1:6379"
cache_ttl_seconds = 604800

[executor]
executor_type = "shell"

[executor.shell]
work_dir = "/tmp/turboci-builds"
```

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

```bash
sudo tee /etc/systemd/system/turboci.service > /dev/null <<EOF
[Unit]
Description=TurboCI Runner
After=network.target redis.service

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/turboci runner-start -c /etc/turboci-runner.toml
Restart=always
RestartSec=10

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
turboci runner-stats -c /etc/turboci-runner.toml
curl http://localhost:8080/health
curl http://localhost:8080/metrics
```

## ❓ Troubleshooting

### Runner not visible in GitLab

```bash
grep runner_token /etc/turboci-runner.toml
grep gitlab_url /etc/turboci-runner.toml
sudo journalctl -u turboci -n 50
```

### Redis connection error

```bash
sudo systemctl status redis
redis-cli ping
```

### Permission denied

```bash
sudo mkdir -p /tmp/turboci-builds
sudo chmod 755 /tmp/turboci-builds
```

## 📄 License

MIT