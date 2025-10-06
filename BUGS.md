# TurboCI Known Bugs & Issues

Last updated: 2025-10-07

---

## 🔴 Critical - Docker Executor

### BUG-001: Trace Streaming Returns 416 Range Not Satisfiable
**Severity:** HIGH
**File:** `src/gitlab/mod.rs:155`
**Status:** ❌ Not Fixed

**Symptom:**
```
WARN turboci::gitlab: Trace streaming failed: 416 Range Not Satisfiable
```

**Root Cause:**
```rust
.header("Content-Range", format!("0-{}", end_offset))  // Always 0-N
```

GitLab expects cumulative offset:
- Request 1: `Content-Range: 0-100`
- Request 2: `Content-Range: 100-250` ← Should continue from 100, not 0!

**Fix:**
```rust
.header("Content-Range", format!("{}-{}", offset, end_offset))
```

**Impact:** Job logs don't show in GitLab UI

---

### BUG-002: Artifacts Upload Empty Data
**Severity:** CRITICAL
**File:** `src/runner_daemon/mod.rs:226-228`
**Status:** ❌ Not Implemented

**Symptom:**
```
WARN turboci::runner_daemon: Failed to upload artifact artifact:
     Artifact upload failed: {"error":"file is missing"}
```

**Root Cause:**
```rust
let artifact_data = Vec::new();  // TODO: Collect artifacts from job workspace
```

Literally uploading 0 bytes!

**What's Missing:**
1. Copy artifact files from Docker container to host
2. Create ZIP archive
3. Upload ZIP

**Impact:** `node_modules` and other artifacts lost between jobs

---

### BUG-003: Cache Upload Empty Data
**Severity:** HIGH
**File:** `src/runner_daemon/mod.rs:248-249`
**Status:** ❌ Not Implemented

**Symptom:**
```
WARN turboci::gitlab: Cache upload failed: 404 Not Found
```

**Root Cause:**
```rust
let cache_data = Vec::new();  // TODO: Create zip from cache paths
```

Same as BUG-002, uploading 0 bytes!

**Impact:** Cache doesn't work, full npm install every time (slow!)

---

### BUG-004: Docker Container Logs Not Streamed to GitLab
**Severity:** HIGH
**File:** `src/runner_daemon/executor.rs:239-240`
**Status:** ❌ Not Fixed

**Symptom:**
GitLab UI shows only:
```
Preparing execution environment...
```

Then job completes (success or failure) with no visible output.

**Root Cause:**
```rust
let text = msg.to_string();
print!("{}", text);           // ❌ Only prints to server console
output.push_str(&text);       // ✅ Collected, but sent only at END
```

Output is collected and returned, but not streamed during execution.

**Fix:**
Stream to GitLab API inside the loop:
```rust
while let Some(chunk) = stream.next().await {
    let text = msg.to_string();
    print!("{}", text);

    // NEW: Stream in real-time
    self.stream_trace(job_id, &text, &mut offset).await?;
}
```

**Impact:** No real-time feedback during job execution

---

### BUG-005: Docker Containers Have No Volume Mounts
**Severity:** CRITICAL
**File:** `src/runner_daemon/executor.rs:180-185`
**Status:** ❌ Not Implemented

**Symptom:**
Artifacts created inside container are lost when container is removed.

**Root Cause:**
```rust
let config = ContainerCreateBody {
    image: Some(image.to_string()),
    working_dir: Some("/builds".to_string()),
    cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
    ..Default::default()  // ❌ No host_config, no binds!
};
```

Container filesystem is ephemeral - destroyed on cleanup.

**Fix:**
```rust
use bollard::models::HostConfig;

let config = ContainerCreateBody {
    host_config: Some(HostConfig {
        binds: Some(vec![
            format!("{}:/builds", host_workspace),
            format!("{}:/cache", host_cache),
        ]),
        ..Default::default()
    }),
    ..Default::default()
};
```

**Impact:** Can't collect artifacts from container after job completes

---

### BUG-006: Downloaded Artifacts/Cache Not Extracted
**Severity:** HIGH
**File:** `src/runner_daemon/mod.rs:149`
**Status:** ❌ Not Implemented

**Symptom:**
Jobs with `dependencies:` or `needs:` fail because previous job artifacts are missing.

**Root Cause:**
Code downloads artifacts but never extracts them:
```rust
// Download happens (maybe), but then... nothing!
info!("📥 Stage: Downloading artifacts");
```

**What's Missing:**
1. Download artifact ZIP from GitLab
2. Extract ZIP into `/builds/project/`
3. Set permissions

**Impact:** Multi-job pipelines completely broken

---

## 🟡 Medium - Shell Executor

### BUG-007: Shell Executor Artifact Upload Fails Too
**Severity:** MEDIUM
**File:** Same as BUG-002
**Status:** ❌ Not Fixed

**Symptom:**
Even Shell executor can't upload artifacts!

**Root Cause:**
Artifact collection code is shared - same empty `Vec::new()`.

**Impact:** Artifacts don't work for ANY executor

---

## 🟢 Low - General

### BUG-008: No Graceful Shutdown
**Severity:** LOW
**File:** N/A
**Status:** ❌ Not Implemented

**Symptom:**
`systemctl stop turboci` immediately kills running jobs.

**Impact:** Job failures on runner restart/update

---

### BUG-009: No Retry Logic for Network Failures
**Severity:** LOW
**File:** `src/gitlab/mod.rs` (all upload functions)
**Status:** ❌ Not Implemented

**Symptom:**
Transient network errors cause job failures.

**Impact:** Flaky jobs on poor network

---

## 📋 Bug Summary Table

| ID | Severity | Component | Impact | Est. Fix Time |
|----|----------|-----------|--------|---------------|
| BUG-001 | HIGH | Trace stream | No logs in UI | 15 min |
| BUG-002 | CRITICAL | Artifacts | Jobs fail | 4 hours |
| BUG-003 | HIGH | Cache | Slow builds | 2 hours |
| BUG-004 | HIGH | Logs | No real-time | 2 hours |
| BUG-005 | CRITICAL | Docker | Can't collect files | 1 hour |
| BUG-006 | HIGH | Artifacts | Multi-job fail | 3 hours |
| BUG-007 | MEDIUM | Shell | No artifacts | Same as BUG-002 |
| BUG-008 | LOW | Daemon | Job kills | 2 hours |
| BUG-009 | LOW | Network | Flaky | 1 hour |

**Total Estimated Fix Time:** ~16 hours of focused work

---

## 🔥 Hotfix Priority (Fix Today)

1. **BUG-001** - Trace header (15 min) - Quick win!
2. **BUG-005** - Volume mounts (1 hour) - Enables artifact collection
3. **BUG-002** - Artifact ZIP (4 hours) - Core functionality

With these 3 fixed, Docker executor would be minimally functional.

---

## 🧪 How to Test Fixes

### Test BUG-001 (Trace Streaming):
```bash
# Before: Only shows "Preparing..."
# After: Should show full job output in GitLab UI
```

### Test BUG-002 (Artifacts):
```yaml
job1:
  script:
    - echo "test" > artifact.txt
  artifacts:
    paths:
      - artifact.txt

job2:
  dependencies:
    - job1
  script:
    - cat artifact.txt  # Should print "test"
```

### Test BUG-003 (Cache):
```yaml
job1:
  script:
    - npm install  # Should cache node_modules
  cache:
    paths:
      - node_modules/

job2:  # Run later
  script:
    - npm install  # Should be instant (cache hit)
  cache:
    paths:
      - node_modules/
```

---

## 📚 References

- GitLab API Trace Docs: https://docs.gitlab.com/ee/api/jobs.html#upload-a-trace
- GitLab Artifacts API: https://docs.gitlab.com/ee/api/job_artifacts.html
- Bollard Docker Lib: https://docs.rs/bollard/latest/bollard/
- GitLab Runner Source: https://gitlab.com/gitlab-org/gitlab-runner

---

## 💡 Debug Tips

### Enable verbose logging:
```bash
RUST_LOG=debug turboci runner-start -c /etc/turboci-runner.toml
```

### Check Docker container state:
```bash
docker ps -a | grep turboci
docker logs <container_id>
```

### Check artifact upload request:
```bash
# Add this before upload in mod.rs:
eprintln!("Uploading {} bytes", artifact_data.len());
```

### Monitor GitLab API calls:
```bash
journalctl -u turboci -f | grep -E "(Trace|Artifact|Cache)"
```
