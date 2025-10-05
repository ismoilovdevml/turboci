# TurboCI Runner Architecture

## Overview
Transform TurboCI from a tool into a standalone intelligent runner that can replace GitLab Runner or GitHub Actions runner with built-in caching and optimization.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    TurboCI Runner                       │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  ┌──────────────────────────────────────────────────┐  │
│  │           Runner Daemon                          │  │
│  │  • Job Polling (GitLab/GitHub API)              │  │
│  │  • Job Queue Management                         │  │
│  │  • Concurrent Job Execution                     │  │
│  └──────────────────────────────────────────────────┘  │
│                         ↓                               │
│  ┌──────────────────────────────────────────────────┐  │
│  │           Smart Cache Engine                     │  │
│  │  • Redis-based distributed cache                │  │
│  │  • BLAKE3 hash-based keys                       │  │
│  │  • Automatic cache invalidation                 │  │
│  │  • Cross-project cache sharing                  │  │
│  └──────────────────────────────────────────────────┘  │
│                         ↓                               │
│  ┌──────────────────────────────────────────────────┐  │
│  │           Execution Engine                       │  │
│  │  • Docker/Podman container support              │  │
│  │  • Shell executor                               │  │
│  │  • Parallel step execution                      │  │
│  │  • Resource limiting (CPU/Memory)               │  │
│  └──────────────────────────────────────────────────┘  │
│                         ↓                               │
│  ┌──────────────────────────────────────────────────┐  │
│  │           Analytics & Monitoring                 │  │
│  │  • Cache hit rate tracking                      │  │
│  │  • Performance metrics                          │  │
│  │  • Cost savings calculation                     │  │
│  └──────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

## Installation & Setup

### 1. Install TurboCI Runner

```bash
# Download and install
curl -sSL https://turboci.dev/runner/install.sh | sh

# Or build from source
git clone https://github.com/turboci/turboci.git
cd turboci
cargo build --release --features runner
sudo mv target/release/turboci-runner /usr/local/bin/
```

### 2. Setup Redis (Required)

```bash
# Using Docker
docker run -d \
  --name turboci-redis \
  -p 6379:6379 \
  -v redis-data:/data \
  redis:alpine redis-server --appendonly yes

# Or install locally
# Ubuntu/Debian
sudo apt-get install redis-server
sudo systemctl start redis

# macOS
brew install redis
brew services start redis
```

### 3. Register Runner with GitLab

```bash
# Register runner
turboci-runner register \
  --url https://gitlab.com \
  --token glrt-YOUR_RUNNER_TOKEN \
  --name "TurboCI Smart Runner" \
  --executor docker \
  --redis-url redis://localhost:6379 \
  --cache-enabled true \
  --max-jobs 4

# This creates config: /etc/turboci-runner/config.toml
```

### 4. Start Runner Daemon

```bash
# Start as daemon
sudo turboci-runner start

# Or run in foreground (for debugging)
turboci-runner run

# Check status
turboci-runner status
```

## Configuration

### config.toml
```toml
concurrent = 4
check_interval = 3

[[runners]]
  name = "turboci-runner-1"
  url = "https://gitlab.com"
  token = "glrt-xxxxxxxxxxxx"
  executor = "docker"

  [runners.cache]
    enabled = true
    redis_url = "redis://localhost:6379"
    ttl_seconds = 86400
    compression = true

  [runners.docker]
    image = "alpine:latest"
    privileged = false
    volumes = ["/cache"]

  [runners.optimization]
    incremental_builds = true
    parallel_steps = true
    smart_caching = true
```

## GitLab Integration

### .gitlab-ci.yml
```yaml
# Use TurboCI runner with tags
build:
  tags:
    - turboci
    - docker
  script:
    - npm install
    - npm run build
  # TurboCI automatically caches:
  # - node_modules (by package-lock.json hash)
  # - build outputs (by source hash)

test:
  tags:
    - turboci
  parallel: 8  # TurboCI runs these truly parallel
  script:
    - npm run test
```

## GitHub Actions Integration

### Setup
```bash
# Register with GitHub
turboci-runner register \
  --platform github \
  --url https://github.com/your-org/your-repo \
  --token ghp_YOUR_TOKEN \
  --redis-url redis://localhost:6379
```

### Workflow
```yaml
# .github/workflows/ci.yml
name: CI

on: [push, pull_request]

jobs:
  build:
    runs-on: turboci  # Use TurboCI runner
    steps:
      - uses: actions/checkout@v3
      - run: npm install
      - run: npm run build
```

## Performance Comparison

### Standard GitLab Runner
```
Job 1: Install + Build + Test    → 10 min
Job 2: Install + Build + Test    → 10 min (no cache reuse)
Job 3: Install + Build + Test    → 10 min (no cache reuse)
────────────────────────────────────────
Total: 30 minutes
Cache Hit Rate: 0%
```

### TurboCI Runner (Smart Cache)
```
Job 1: Install + Build + Test    → 10 min (populate cache)
Job 2: Install + Build + Test    → 1.5 min (90% cache hit)
Job 3: Install + Build + Test    → 1.5 min (90% cache hit)
────────────────────────────────────────
Total: 13 minutes (2.3x faster!)
Cache Hit Rate: 85%
Cost Savings: ~57%
```

### TurboCI Runner (Smart Cache + Parallel)
```
Job 1,2,3: Parallel execution    → 10 min (with cache)
────────────────────────────────────────
Total: 10 minutes (3x faster!)
All jobs benefit from shared cache
```

## Key Features

### 1. Intelligent Caching
- **Automatic**: No manual cache configuration needed
- **Hash-based**: Uses BLAKE3 for fast, accurate cache keys
- **Distributed**: Redis enables sharing across runners
- **Granular**: Caches at file/directory level

### 2. Incremental Builds
- Git diff analysis
- Only rebuild changed components
- Dependency graph tracking

### 3. Parallel Execution
- Multi-core utilization
- Concurrent job execution
- Parallel test running

### 4. Cross-Project Cache
```bash
# Project A builds a library
Project A: Build library → Cache

# Project B depends on same library
Project B: Uses cached library → Instant!
```

## Monitoring

### CLI
```bash
# View runner status
turboci-runner status

# Cache statistics
turboci-runner cache-stats
# Output:
# Cache Hit Rate: 87.3%
# Total Cached: 15.2 GB
# Jobs Accelerated: 1,234
# Time Saved: 45.6 hours
# Cost Saved: $127.80

# Live metrics
turboci-runner metrics --follow
```

### Web Dashboard (Optional)
```bash
# Start metrics server
turboci-runner metrics-server --port 9090

# View at http://localhost:9090
# - Cache hit rates
# - Job execution times
# - Cost savings
# - Resource usage
```

## Use Cases

### 1. Large Monorepo
- **Problem**: 100+ microservices, each build takes 5 min
- **Solution**: TurboCI caches unchanged services
- **Result**: 500 min → 50 min (10x faster)

### 2. Frequent Commits
- **Problem**: Every commit rebuilds everything
- **Solution**: Incremental builds + smart cache
- **Result**: Only changed code rebuilds

### 3. Team Collaboration
- **Problem**: Each developer waits for CI
- **Solution**: Shared distributed cache
- **Result**: First build 10 min, rest 1-2 min

### 4. Multi-Branch Development
- **Problem**: Each branch rebuilds from scratch
- **Solution**: Cross-branch cache sharing
- **Result**: Instant builds for similar branches

## Security

### Cache Isolation
```toml
[runners.cache]
  # Project-level isolation
  namespace = "project-${CI_PROJECT_ID}"

  # Branch-level isolation (optional)
  namespace = "project-${CI_PROJECT_ID}-branch-${CI_COMMIT_REF_NAME}"
```

### Redis Authentication
```toml
[runners.cache]
  redis_url = "redis://:password@localhost:6379"
  tls_enabled = true
  tls_cert = "/path/to/cert.pem"
```

## Troubleshooting

### Cache Not Working
```bash
# Check Redis connection
redis-cli ping

# View cache keys
redis-cli keys "turboci:*"

# Clear cache if needed
turboci-runner cache-clear
```

### Performance Issues
```bash
# Check concurrent jobs
turboci-runner config get concurrent

# Monitor resource usage
turboci-runner metrics --resource-usage
```

## Roadmap

- [x] Core caching engine
- [x] Incremental build optimizer
- [x] Parallel execution
- [ ] **Full GitLab Runner replacement** (v2.0)
- [ ] **GitHub Actions runner** (v2.0)
- [ ] Web dashboard (v2.1)
- [ ] Kubernetes support (v2.2)
- [ ] Multi-node Redis cluster (v2.3)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

MIT License - see [LICENSE](LICENSE)
