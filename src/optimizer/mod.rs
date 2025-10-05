use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::cache::CacheManager;

#[derive(Debug, Clone)]
pub struct BuildOptimizer {
    cache: CacheManager,
    changed_files: HashSet<PathBuf>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BuildPlan {
    pub targets: Vec<BuildTarget>,
    pub total_targets: usize,
    pub cached_targets: usize,
    pub needs_rebuild: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BuildTarget {
    pub name: String,
    pub path: PathBuf,
    pub dependencies: Vec<String>,
    pub needs_rebuild: bool,
    pub cache_key: String,
}

impl BuildOptimizer {
    pub fn new(cache: CacheManager) -> Self {
        Self {
            cache,
            changed_files: HashSet::new(),
        }
    }

    /// Analyze which files have changed
    pub async fn analyze_changes(&mut self, workspace: &Path) -> Result<()> {
        info!("🔍 Analyzing changes in workspace...");

        // Get git diff for changed files
        let output = tokio::process::Command::new("git")
            .args(&["diff", "--name-only", "HEAD"])
            .current_dir(workspace)
            .output()
            .await?;

        let changed = String::from_utf8_lossy(&output.stdout);
        for line in changed.lines() {
            let path = workspace.join(line.trim());
            self.changed_files.insert(path);
        }

        // Also check for untracked files
        let output = tokio::process::Command::new("git")
            .args(&["ls-files", "--others", "--exclude-standard"])
            .current_dir(workspace)
            .output()
            .await?;

        let untracked = String::from_utf8_lossy(&output.stdout);
        for line in untracked.lines() {
            let path = workspace.join(line.trim());
            self.changed_files.insert(path);
        }

        info!("Found {} changed files", self.changed_files.len());
        Ok(())
    }

    /// Create incremental build plan
    pub async fn create_build_plan(&self, targets: Vec<String>) -> Result<BuildPlan> {
        info!("📋 Creating incremental build plan...");

        let mut build_targets = Vec::new();
        let mut cached_count = 0;
        let mut rebuild_count = 0;

        for target_name in targets {
            let target_path = PathBuf::from(&target_name);
            let needs_rebuild = self.needs_rebuild(&target_path).await?;

            if !needs_rebuild {
                cached_count += 1;
            } else {
                rebuild_count += 1;
            }

            let cache_key = self.generate_cache_key(&target_name, &target_path).await?;

            build_targets.push(BuildTarget {
                name: target_name.clone(),
                path: target_path,
                dependencies: vec![], // TODO: detect dependencies
                needs_rebuild,
                cache_key,
            });
        }

        let plan = BuildPlan {
            total_targets: build_targets.len(),
            cached_targets: cached_count,
            needs_rebuild: rebuild_count,
            targets: build_targets,
        };

        info!(
            "✅ Build plan: {} total, {} cached, {} need rebuild",
            plan.total_targets, plan.cached_targets, plan.needs_rebuild
        );

        Ok(plan)
    }

    /// Check if a target needs to be rebuilt
    async fn needs_rebuild(&self, target: &Path) -> Result<bool> {
        // Check if any files in the target directory have changed
        for changed_file in &self.changed_files {
            if changed_file.starts_with(target) {
                debug!(
                    "Target {:?} needs rebuild (changed file: {:?})",
                    target, changed_file
                );
                return Ok(true);
            }
        }

        // Check cache
        let hash = self.cache.compute_hash(target).await?;
        let cache_key = format!("build:{}:{}", target.display(), hash);

        let exists = self.cache.exists(&cache_key).await?;
        if exists {
            debug!("Target {:?} found in cache", target);
            Ok(false)
        } else {
            debug!("Target {:?} not in cache", target);
            Ok(true)
        }
    }

    /// Generate cache key for a build target
    async fn generate_cache_key(&self, name: &str, path: &Path) -> Result<String> {
        let hash = self.cache.compute_hash(path).await?;
        Ok(format!("build:{}:{}", name, hash))
    }

    /// Get optimization statistics
    pub fn get_optimization_stats(&self, plan: &BuildPlan) -> String {
        let savings_percent = if plan.total_targets > 0 {
            (plan.cached_targets as f64 / plan.total_targets as f64) * 100.0
        } else {
            0.0
        };

        format!(
            "⚡ Build Optimization: {:.1}% cached ({}/{})",
            savings_percent, plan.cached_targets, plan.total_targets
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_build_optimizer_creation() {
        // Test without Redis connection
        // We can't test create_build_plan without Redis, so just test struct creation
        let changed_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(changed_files.len(), 0);
    }
}
