## 🚀 TurboCI Runner Deployment Guide

Complete guide to deploy TurboCI as a GitLab Runner replacement with intelligent caching.

## 📋 Prerequisites

### Required
- Linux server (Ubuntu 20.04+ recommended)
- Docker installed and running
- Redis 6.0+ (for hot cache)
- 4GB+ RAM, 2+ CPU cores
- Internet access

### Optional (Recommended)
- MinIO/S3 (for cold storage of large artifacts)
- 20GB+ disk space for cache
- SSD storage for better performance

---

## 🔧 Installation

### 1. Install TurboCI Runner

```bash
# Download latest release
curl -sSL https://github.com/turboci/turboci/releases/latest/download/turboci-runner-linux-x86_64 \
  -o /usr/local/bin/turboci-runner

# Make executable
chmod +x /usr/local/bin/turboci-runner

# Verify installation
turboci-runner --version
```

### 2. Setup Redis (Hot Cache)

```bash
# Using Docker
docker run -d \
  --name turboci-redis \
  --restart unless-stopped \
  -p 6379:6379 \
  -v /var/lib/turboci/redis:/data \
  redis:alpine redis-server --appendonly yes

# Verify
docker ps | grep turboci-redis
redis-cli ping  # Should return PONG
```

### 3. Setup MinIO (Cold Storage - Optional)

```bash
# Create MinIO directories
mkdir -p /var/lib/turboci/minio

# Run MinIO
docker run -d \
  --name turboci-minio \
  --restart unless-stopped \
  -p 9000:9000 \
  -p 9001:9001 \
  -e "MINIO_ROOT_USER=turboci" \
  -e "MINIO_ROOT_PASSWORD=turboci-secret-password" \
  -v /var/lib/turboci/minio:/data \
  minio/minio server /data --console-address ":9001"

# Create bucket
docker exec turboci-minio \
  mc alias set local http://localhost:9000 turboci turboci-secret-password

docker exec turboci-minio \
  mc mb local/turboci-cache
```

Access MinIO Console: http://YOUR_SERVER:9001

---

## ⚙️ Configuration

### 1. Get GitLab Runner Token

#### For Shared Runners:
1. Go to GitLab → Admin Area → CI/CD → Runners
2. Click "New instance runner"
3. Copy the registration token

#### For Project Runners:
1. Go to Your Project → Settings → CI/CD → Runners
2. Expand "Runners" section
3. Click "New project runner"
4. Copy the registration token

### 2. Create Configuration File

```bash
# Create config directory
mkdir -p /etc/turboci-runner

# Copy example config
cat > /etc/turboci-runner/config.toml << 'EOF'
concurrent = 4
check_interval = 3
runner_token = "glrt-YOUR_TOKEN_HERE"
gitlab_url = "https://gitlab.com"

cache_enabled = true
redis_url = "redis://127.0.0.1:6379"
s3_bucket = "turboci-cache"
s3_endpoint = "http://localhost:9000"
s3_prefix = "turboci"
storage_threshold = 10485760

[executor]
executor_type = "docker"

[executor.docker]
default_image = "alpine:latest"
privileged = false
volumes = ["/cache:/cache:rw"]
network_mode = "bridge"
EOF

# Replace YOUR_TOKEN_HERE with actual token
vim /etc/turboci-runner/config.toml
```

### 3. Configure S3 Credentials

```bash
# Set environment variables
cat > /etc/turboci-runner/env << 'EOF'
AWS_ACCESS_KEY_ID=turboci
AWS_SECRET_ACCESS_KEY=turboci-secret-password
AWS_ENDPOINT_URL=http://localhost:9000
EOF
```

---

## 🏃 Running the Runner

### Option 1: Manual Start (Testing)

```bash
# Load environment
source /etc/turboci-runner/env

# Start runner
turboci-runner run --config /etc/turboci-runner/config.toml
```

### Option 2: Systemd Service (Production)

```bash
# Create systemd service
cat > /etc/systemd/system/turboci-runner.service << 'EOF'
[Unit]
Description=TurboCI Runner
After=docker.service redis.service
Requires=docker.service

[Service]
Type=simple
User=root
EnvironmentFile=/etc/turboci-runner/env
ExecStart=/usr/local/bin/turboci-runner run --config /etc/turboci-runner/config.toml
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
EOF

# Reload systemd
systemctl daemon-reload

# Enable and start service
systemctl enable turboci-runner
systemctl start turboci-runner

# Check status
systemctl status turboci-runner

# View logs
journalctl -u turboci-runner -f
```

---

## 📊 Monitoring

### Check Runner Status

```bash
# Runner status
systemctl status turboci-runner

# Live logs
journalctl -u turboci-runner -f

# Cache statistics
turboci-runner cache-stats
```

### Expected Output:
```
📊 Cache Statistics:
  Hit Rate:    87.3%
  Total Size:  15.2 GB
  Cached Items: 1,234
  Time Saved:  45.6 hours
  Cost Saved:  $127.80
```

### Metrics API (Optional)

```bash
# Start metrics server
turboci-runner metrics-server --port 9090

# Access metrics
curl http://localhost:9090/metrics
```

---

## 🔒 Security Best Practices

### 1. Runner Isolation

```toml
# config.toml
[executor.docker]
privileged = false  # Never use privileged unless absolutely needed
network_mode = "bridge"  # Isolated network
```

### 2. Cache Namespace

```toml
# Isolate cache by project
cache_prefix = "project-${CI_PROJECT_ID}"
```

### 3. Redis Security

```bash
# Enable Redis authentication
docker run -d \
  --name turboci-redis \
  -p 6379:6379 \
  redis:alpine redis-server --requirepass YOUR_STRONG_PASSWORD

# Update config
redis_url = "redis://:YOUR_STRONG_PASSWORD@127.0.0.1:6379"
```

### 4. MinIO Security

```bash
# Use strong credentials
MINIO_ROOT_USER=admin
MINIO_ROOT_PASSWORD=$(openssl rand -base64 32)

# Enable HTTPS
# Configure TLS certificates in MinIO
```

---

## 🎯 GitLab Integration

### 1. Register Runner

The runner auto-registers when it starts. Verify in GitLab:

1. Go to Settings → CI/CD → Runners
2. You should see "turboci-runner" listed
3. Tag the runner (optional): `turboci`, `docker`, `fast`

### 2. Use in `.gitlab-ci.yml`

```yaml
# Use TurboCI runner with tags
build:
  tags:
    - turboci
  script:
    - npm install
    - npm run build
  # TurboCI automatically caches based on:
  # - Script content
  # - Git commit SHA
  # - Environment variables

test:
  tags:
    - turboci
  parallel: 8
  script:
    - npm run test
```

### 3. Performance Comparison

**Before (Standard GitLab Runner):**
```
Job 1: npm install + build  → 8 min
Job 2: npm install + build  → 8 min
Job 3: npm install + build  → 8 min
Total: 24 minutes
```

**After (TurboCI Runner):**
```
Job 1: npm install + build  → 8 min (cache miss)
Job 2: npm install + build  → 30 sec (cache hit!)
Job 3: npm install + build  → 30 sec (cache hit!)
Total: 9 minutes (2.6x faster!)
```

---

## 🐛 Troubleshooting

### Runner Not Picking Up Jobs

```bash
# Check runner token
grep runner_token /etc/turboci-runner/config.toml

# Check GitLab connection
curl -I https://gitlab.com

# View logs
journalctl -u turboci-runner -n 100
```

### Docker Issues

```bash
# Check Docker is running
systemctl status docker

# Test Docker
docker run hello-world

# Check permissions
usermod -aG docker $USER
```

### Redis Connection Failed

```bash
# Test Redis
redis-cli ping

# Check Redis logs
docker logs turboci-redis

# Restart Redis
docker restart turboci-redis
```

### Cache Not Working

```bash
# Check Redis keys
redis-cli keys "turboci:*"

# View cache stats
turboci-runner cache-stats

# Clear cache
redis-cli FLUSHDB
```

### High Memory Usage

```bash
# Limit Redis memory
docker run -d \
  --name turboci-redis \
  -p 6379:6379 \
  redis:alpine redis-server --maxmemory 2gb --maxmemory-policy allkeys-lru

# Reduce concurrent jobs
# Edit config.toml: concurrent = 2
```

---

## 📈 Scaling

### Multiple Runners

```bash
# Server 1
turboci-runner run --config /etc/turboci-runner/config-1.toml

# Server 2
turboci-runner run --config /etc/turboci-runner/config-2.toml

# All servers share same Redis/MinIO for cache
```

### Redis Cluster (High Availability)

```bash
# Setup Redis Sentinel/Cluster
# Update config:
redis_url = "redis://redis-cluster:6379"
```

### Load Balancing

```yaml
# GitLab
# Multiple runners with same tags automatically load balance
```

---

## 💰 Cost Optimization

### Storage Tiers

```toml
# Hot cache (Redis): Fast, expensive
# - Metadata: ~100KB
# - Small files: < 10MB

# Cold storage (S3/MinIO): Slow, cheap
# - Large artifacts: > 10MB
# - Docker images
# - Build outputs

storage_threshold = 10485760  # 10MB
```

### Cache TTL

```toml
# Reduce Redis memory with TTL
[cache]
ttl_seconds = 86400  # 24 hours
```

### Compression

```toml
# Enable compression for S3
[cache]
compression = true
compression_level = 6  # 1-9
```

---

## 📚 Advanced Configuration

### Custom Docker Images

```toml
[executor.docker]
default_image = "my-registry.com/custom-build-image:latest"
```

### Docker-in-Docker

```toml
[executor.docker]
privileged = true
volumes = [
    "/var/run/docker.sock:/var/run/docker.sock:rw"
]
```

### Multiple Executors

```toml
[[runners]]
name = "docker-runner"
executor = "docker"

[[runners]]
name = "shell-runner"
executor = "shell"
```

---

## 🔄 Maintenance

### Backup

```bash
# Backup Redis
docker exec turboci-redis redis-cli BGSAVE

# Backup MinIO
mc mirror local/turboci-cache /backup/turboci-cache
```

### Updates

```bash
# Stop runner
systemctl stop turboci-runner

# Download new version
curl -sSL https://github.com/turboci/turboci/releases/latest/download/turboci-runner-linux-x86_64 \
  -o /usr/local/bin/turboci-runner

# Start runner
systemctl start turboci-runner
```

### Cleanup

```bash
# Remove old containers
docker system prune -af

# Clear old cache
redis-cli --scan --pattern "turboci:*" | xargs redis-cli DEL
```

---

## 📞 Support

- GitHub Issues: https://github.com/turboci/turboci/issues
- Documentation: https://docs.turboci.dev
- Discord: https://discord.gg/turboci

---

**⚡ Enjoy 5-10x faster CI/CD with TurboCI!**
