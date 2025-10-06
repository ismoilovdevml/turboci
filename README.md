# ⚡ TurboCI - Lightning-Fast CI/CD Runner

**TurboCI** is a high-performance GitLab CI/CD runner with distributed caching and intelligent job execution. Built in Rust for maximum speed and efficiency.

## 🚀 Features

### ⚡ **Blazing Fast Execution**
- **Shell Executor**: Direct host execution - **10x faster** than Docker containers  
- **Docker Executor**: Isolated container execution for security
- **Redis Caching**: In-memory cache for **sub-second** artifact retrieval
- **Hybrid Storage**: Automatically route small files to Redis, large files to S3

### 🎯 **Smart Caching**
- **Content-based hashing**: Automatic cache invalidation on code changes
- **Multi-tier storage**: Redis (hot) + S3 (cold) for optimal performance
- **Cache hit rates**: Track and optimize your build performance
- **Dependency caching**: Hash-based dependency cache management

### 🔧 **Flexible Configuration**
- **Multiple executors**: Shell (fast) or Docker (isolated)
- **Optional S3**: Use Redis-only mode for maximum speed
- **Concurrent jobs**: Run multiple jobs in parallel
- **GitLab integration**: Full GitLab CI/CD protocol support

## 📦 Installation

### Quick Install (Recommended)

```bash
curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh | bash
```

**Supported Platforms:**
- ✅ Linux x86_64 (musl static)
- ✅ macOS Intel  
- ✅ macOS Apple Silicon (M1/M2/M3/M4)

### Manual Installation

#### 1. Install Dependencies

**Linux (Ubuntu/Debian):**
```bash
sudo apt-get update
sudo apt-get install -y redis-server git
sudo systemctl enable --now redis-server
```

**macOS:**
```bash
brew install redis git
brew services start redis
```

#### 2. Install TurboCI

```bash
# Download latest release
curl -L https://github.com/ismoilovdevml/turboci/releases/latest/download/turboci-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m) -o turboci
chmod +x turboci
sudo mv turboci /usr/local/bin/
```

#### 3. Configure

```bash
sudo turboci init-runner -o /etc/turboci-runner.toml
sudo nano /etc/turboci-runner.toml
```

**Minimal Config (Shell + Redis-only):**
```toml
concurrent = 4
runner_token = "glrt-YOUR_TOKEN_HERE"
gitlab_url = "https://gitlab.com"
redis_url = "redis://127.0.0.1:6379"

[executor]
executor_type = "shell"

[executor.shell]
work_dir = "/tmp/turboci-builds"
```

#### 4. Start

```bash
turboci runner-start -c /etc/turboci-runner.toml
```

## 🎯 Usage

### GitLab CI Configuration

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
```

### Performance Stats

```bash
turboci runner-stats -c /etc/turboci-runner.toml
```

## ⚙️ Configuration

### Executors

**Shell (Fast):**
```toml
[executor]
executor_type = "shell"
[executor.shell]
work_dir = "/tmp/turboci-builds"
```

**Docker (Secure):**
```toml
[executor]
executor_type = "docker"
[executor.docker]
default_image = "alpine:latest"
```

### Storage

**Redis-only (Fastest):**
```toml
redis_url = "redis://127.0.0.1:6379"
```

**Hybrid (Redis + S3):**
```toml
redis_url = "redis://127.0.0.1:6379"
s3_bucket = "turboci"
s3_endpoint = "http://localhost:9000"
storage_threshold = 10485760
```

## 📊 Performance

| Metric | TurboCI Shell | GitLab Runner |
|--------|---------------|---------------|
| Job Startup | 50ms | 10s |
| Cache Access | 10ms | 100ms |
| Build (cached) | 2s | 15s |

## 🏗️ Build from Source

```bash
git clone https://github.com/ismoilovdevml/turboci.git
cd turboci
cargo build --release --features runner
```

## 📝 License

MIT License - see [LICENSE](LICENSE)

---

⚡ **TurboCI - Because every second counts in CI/CD!**
