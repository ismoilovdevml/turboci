# TurboCI Docker Executor Roadmap

**Goal:** Make TurboCI a truly "turbo" fast, fully GitLab-compatible CI/CD runner with production-ready Docker executor support.

**Current Status:** Shell executor works great (7x faster!), but Docker executor is 70% incomplete.

---

## 🚨 P0: Critical Bugs (Must Fix for Basic Functionality)

### 1. Trace Streaming - 416 Range Not Satisfiable ❌
**File:** `src/gitlab/mod.rs:155`

**Problem:**
```rust
.header("Content-Range", format!("0-{}", end_offset))  // ❌ Always starts at 0
```

**Fix:**
```rust
.header("Content-Range", format!("{}-{}", offset, end_offset))  // ✅ Cumulative offset
```

**Impact:** Job logs don't show in GitLab UI - only "Preparing execution environment..."

---

### 2. Artifact Upload - Empty Data ❌
**File:** `src/runner_daemon/mod.rs:226-228`

**Problem:**
```rust
// TODO: Collect artifacts from job workspace
// For now, create empty zip as placeholder
let artifact_data = Vec::new();  // ❌ Empty!
```

**What's Needed:**
1. Copy artifact files from Docker container to host
2. Create ZIP archive from artifact paths
3. Upload ZIP to GitLab

**Impact:** Artifacts between jobs don't work - `node_modules` lost after each job

---

### 3. Cache Upload - Empty Data ❌
**File:** `src/runner_daemon/mod.rs:248-249`

**Problem:**
```rust
// TODO: Create zip from cache paths
let cache_data = Vec::new();  // ❌ Empty!
```

**Impact:** Cache doesn't persist between pipelines

---

### 4. Docker Output Not Streamed ❌
**File:** `src/runner_daemon/executor.rs:239-240`

**Problem:**
```rust
let text = msg.to_string();
print!("{}", text);  // ❌ Only prints to console, not sent to GitLab
output.push_str(&text);
```

**Fix:** Stream output to GitLab in real-time via trace API during execution, not just at the end

**Impact:** No real-time logs visible in GitLab UI during job execution

---

### 5. Docker Containers - No Volume Mounts ❌
**File:** `src/runner_daemon/executor.rs:180-185`

**Problem:**
```rust
let config = ContainerCreateBody {
    image: Some(image.to_string()),
    working_dir: Some("/builds".to_string()),
    cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
    ..Default::default()  // ❌ No volume mounts!
};
```

**What's Needed:**
```rust
host_config: Some(HostConfig {
    binds: Some(vec![
        format!("{}:/builds", host_workspace_path),  // Mount workspace
        format!("{}:/cache", host_cache_path),       // Mount cache
    ]),
    ..Default::default()
}),
```

**Impact:** Artifacts in container are lost when container is removed

---

### 6. Artifact/Cache Download - Not Extracted ❌
**File:** `src/runner_daemon/mod.rs:149`

**Problem:** Downloaded artifacts and cache are fetched but never extracted into the Docker container workspace

**What's Needed:**
1. Download artifact ZIP from GitLab
2. Extract ZIP into Docker container before job starts
3. Same for cache

**Impact:** Jobs depending on previous job artifacts fail

---

## ⚡ P1: Core Features (Needed for Full GitLab Compatibility)

### 7. Docker Services Support
**Example use case:** PostgreSQL, Redis, Elasticsearch for integration tests

**What's Needed:**
- Parse `services:` from `.gitlab-ci.yml`
- Start service containers
- Link service containers to job container
- Network configuration

**Impact:** Can't run integration tests with databases

---

### 8. Dependency Downloads
**Problem:** Job variables like `$CI_PROJECT_DIR`, `$CI_COMMIT_SHA` not available

**What's Needed:**
- Inject all GitLab CI environment variables
- Support for `dependencies:` keyword
- Support for `needs:` keyword

---

### 9. Git Submodules & LFS
**What's Needed:**
- `git submodule update --init --recursive`
- Git LFS support

---

## 🚀 P2: Performance Optimizations (Make it "Turbo"!)

### 10. BuildKit Integration
**Goal:** Earthly/Dagger-level caching

**What's Needed:**
```rust
use bollard::image::BuildImageOptions;

BuildImageOptions {
    buildargs: HashMap::from([
        ("BUILDKIT_INLINE_CACHE", "1"),
    ]),
    cachefrom: vec!["node:20"],  // Layer cache
    ..Default::default()
}
```

**Impact:** 5-60s saved per job from Docker image layer caching

---

### 11. Container Pooling
**Goal:** Pre-warm containers

**Strategy:**
1. Keep pool of warm containers (1-3 per image)
2. Reuse instead of create/destroy
3. Reset filesystem between jobs

**Impact:** Save 2-3s container startup time per job

---

### 12. Parallel Artifact/Cache Uploads
**Current:** Sequential uploads (slow!)

**Fix:**
```rust
let upload_futures: Vec<_> = artifacts.iter()
    .map(|a| async { upload_artifact(a).await })
    .collect();

tokio::try_join_all(upload_futures).await?;
```

**Impact:** 3-5x faster artifact uploads

---

### 13. Batched Trace Streaming
**Current:** Send trace every 1-2 seconds

**Optimization:** Buffer 10KB or 1s intervals, send once

**Impact:** 10x fewer HTTP requests, less overhead

---

### 14. Artifact Compression
**Current:** No compression

**Add:** Gzip compression before upload

**Impact:** 80% bandwidth reduction for text files

---

## 🔧 P3: Production Readiness

### 15. Graceful Shutdown
**What's Needed:**
- Handle SIGTERM/SIGINT
- Finish running jobs before exit
- Clean up containers properly

---

### 16. Retry Logic
**What's Needed:**
- Retry failed artifact uploads (3x)
- Retry failed trace streams (3x)
- Exponential backoff

---

### 17. Monitoring & Metrics
**What's Needed:**
- Prometheus metrics endpoint
- Job duration histograms
- Cache hit rate tracking
- Container pool stats

---

## 📊 Estimated Timeline

| Priority | Work Days | What's Included |
|----------|-----------|-----------------|
| **P0** | 5 days | All critical bugs fixed - Docker executor functional |
| **P1** | 5 days | Full GitLab compatibility |
| **P2** | 10 days | Performance optimizations - true "turbo" speed |
| **P3** | 5 days | Production hardening |
| **Total** | ~25 days | Full production-ready Docker executor |

---

## 🎯 Quick Wins (Fix First)

1. **Trace streaming header** (15 min) - Line 155 in `gitlab/mod.rs`
2. **Real-time log streaming** (2 hours) - Buffer and send during execution
3. **Volume mounts** (1 hour) - Mount workspace directory
4. **Artifact ZIP creation** (4 hours) - Collect files, create archive, upload

These 4 fixes would make Docker executor minimally functional!

---

## 🏁 Success Criteria

When TurboCI Docker executor is "done":

✅ All job logs visible in GitLab UI in real-time
✅ Artifacts pass between jobs correctly
✅ Cache persists between pipelines
✅ Docker services work (databases, etc)
✅ 2-3x faster than GitLab Runner (via caching)
✅ No job failures due to TurboCI bugs
✅ Production-ready (retries, monitoring, graceful shutdown)

---

## 📝 Notes

**Why Shell executor works well:**
- Direct filesystem access (no container overhead)
- Artifacts are just file copies
- Cache is just directory copies
- Trace is printed directly to console

**Why Docker executor is complex:**
- Need to copy files IN/OUT of containers
- Volume mounts must be set up correctly
- Container lifecycle management
- Network configuration for services
- But: Worth it for isolation and reproducibility!

**Reference implementations:**
- GitLab Runner: https://gitlab.com/gitlab-org/gitlab-runner
- Earthly: https://github.com/earthly/earthly
- Dagger: https://github.com/dagger/dagger
- BuildKit: https://github.com/moby/buildkit
