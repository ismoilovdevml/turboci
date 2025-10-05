use anyhow::Result;
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Content-based cache key generator
#[derive(Debug, Clone)]
pub struct ContentHasher {
    /// Files to include in hash
    #[allow(dead_code)]
    include_patterns: Vec<String>,
    /// Files to exclude from hash
    exclude_patterns: Vec<String>,
}

#[allow(dead_code)]
impl ContentHasher {
    pub fn new() -> Self {
        Self {
            include_patterns: vec!["**/*".to_string()],
            exclude_patterns: vec![
                "**/node_modules/**".to_string(),
                "**/target/**".to_string(),
                "**/.git/**".to_string(),
                "**/dist/**".to_string(),
                "**/build/**".to_string(),
            ],
        }
    }

    /// Compute content hash for a directory
    pub fn hash_directory(&self, path: &Path) -> Result<ContentHash> {
        let mut hasher = Hasher::new();
        let mut file_map = BTreeMap::new();

        // Walk directory and collect files
        for entry in WalkDir::new(path).follow_links(false).sort_by_file_name() {
            let entry = entry?;
            let entry_path = entry.path();

            // Skip excluded files
            if self.should_exclude(entry_path) {
                continue;
            }

            if entry_path.is_file() {
                let relative_path = entry_path
                    .strip_prefix(path)
                    .unwrap_or(entry_path)
                    .to_string_lossy()
                    .to_string();

                let content = fs::read(entry_path)?;
                let file_hash = blake3::hash(&content);

                file_map.insert(relative_path.clone(), file_hash.to_hex().to_string());

                // Add to overall hash
                hasher.update(relative_path.as_bytes());
                hasher.update(&content);
            }
        }

        let overall_hash = hasher.finalize().to_hex().to_string();

        Ok(ContentHash {
            hash: overall_hash,
            file_hashes: file_map,
        })
    }

    /// Compute hash for specific files
    pub fn hash_files(&self, files: &[PathBuf]) -> Result<String> {
        let mut hasher = Hasher::new();

        for file in files {
            if file.is_file() {
                let content = fs::read(file)?;
                hasher.update(file.to_string_lossy().as_bytes());
                hasher.update(&content);
            }
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Compute hash for lockfiles (package-lock.json, Cargo.lock, etc.)
    pub fn hash_dependencies(&self, base_path: &Path) -> Result<String> {
        let lockfiles = [
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "Cargo.lock",
            "go.sum",
            "Gemfile.lock",
            "poetry.lock",
            "requirements.txt",
        ];

        let mut hasher = Hasher::new();
        let mut found_any = false;

        for lockfile in &lockfiles {
            let path = base_path.join(lockfile);
            if path.exists() {
                let content = fs::read(&path)?;
                hasher.update(lockfile.as_bytes());
                hasher.update(&content);
                found_any = true;
            }
        }

        if !found_any {
            // No lockfiles found, hash package files
            let package_files = [
                "package.json",
                "Cargo.toml",
                "go.mod",
                "Gemfile",
                "pyproject.toml",
            ];

            for pkg_file in &package_files {
                let path = base_path.join(pkg_file);
                if path.exists() {
                    let content = fs::read(&path)?;
                    hasher.update(pkg_file.as_bytes());
                    hasher.update(&content);
                }
            }
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    fn should_exclude(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.exclude_patterns
            .iter()
            .any(|pattern| glob_match::glob_match(pattern, &path_str))
    }
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentHash {
    /// Overall hash of directory
    pub hash: String,
    /// Map of file path to file hash
    pub file_hashes: BTreeMap<String, String>,
}

#[allow(dead_code)]
impl ContentHash {
    /// Check if any files changed compared to another hash
    pub fn changed_files(&self, other: &ContentHash) -> Vec<String> {
        let mut changed = Vec::new();

        for (path, hash) in &self.file_hashes {
            if let Some(other_hash) = other.file_hashes.get(path) {
                if hash != other_hash {
                    changed.push(path.clone());
                }
            } else {
                // New file
                changed.push(path.clone());
            }
        }

        // Check for deleted files
        for path in other.file_hashes.keys() {
            if !self.file_hashes.contains_key(path) {
                changed.push(path.clone());
            }
        }

        changed
    }

    /// Get subset of files matching pattern
    pub fn filter_files(&self, pattern: &str) -> BTreeMap<String, String> {
        self.file_hashes
            .iter()
            .filter(|(path, _)| glob_match::glob_match(pattern, path))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_content_hash_directory() {
        let dir = tempdir().unwrap();
        let file1 = dir.path().join("test1.txt");
        let file2 = dir.path().join("test2.txt");

        fs::write(&file1, b"content1").unwrap();
        fs::write(&file2, b"content2").unwrap();

        let hasher = ContentHasher::new();
        let hash = hasher.hash_directory(dir.path()).unwrap();

        assert!(!hash.hash.is_empty());
        assert_eq!(hash.file_hashes.len(), 2);
    }

    #[test]
    fn test_changed_files() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();

        fs::write(dir1.path().join("file1.txt"), b"v1").unwrap();
        fs::write(dir2.path().join("file1.txt"), b"v2").unwrap();

        let hasher = ContentHasher::new();
        let hash1 = hasher.hash_directory(dir1.path()).unwrap();
        let hash2 = hasher.hash_directory(dir2.path()).unwrap();

        let changed = hash2.changed_files(&hash1);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0], "file1.txt");
    }
}

// Simple glob matching (placeholder - can use glob crate for production)
mod glob_match {
    pub fn glob_match(pattern: &str, text: &str) -> bool {
        if pattern == "**/*" {
            return true;
        }

        let pattern = pattern.replace("**", ".*").replace("*", "[^/]*");
        if let Ok(re) = regex::Regex::new(&pattern) {
            re.is_match(text)
        } else {
            false
        }
    }
}
