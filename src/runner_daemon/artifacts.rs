use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use tracing::{info, warn};
use zip::{ZipArchive, ZipWriter};

/// Download and extract artifacts for a job
pub async fn download_and_extract_artifacts(
    gitlab_url: &str,
    job_id: u64,
    token: &str,
    workspace_path: &str,
    artifact_names: &[String],
) -> Result<()> {
    let client = reqwest::Client::new();

    for artifact_name in artifact_names {
        let url = format!(
            "{}/api/v4/jobs/{}/artifacts/{}",
            gitlab_url, job_id, artifact_name
        );

        info!("📥 Downloading artifact: {}", artifact_name);

        let response = client
            .get(&url)
            .header("JOB-TOKEN", token)
            .send()
            .await
            .context("Failed to download artifact")?;

        if !response.status().is_success() {
            warn!("Artifact {} not found or failed to download", artifact_name);
            continue;
        }

        let artifact_data = response
            .bytes()
            .await
            .context("Failed to read artifact data")?;

        // Extract ZIP to workspace
        extract_zip_to_workspace(&artifact_data, workspace_path)?;

        info!(
            "✅ Extracted artifact {} ({} bytes)",
            artifact_name,
            artifact_data.len()
        );
    }

    Ok(())
}

/// Download and extract cache for a job
pub async fn download_and_extract_cache(
    gitlab_url: &str,
    job_id: u64,
    token: &str,
    workspace_path: &str,
    cache_key: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/v4/jobs/{}/cache/{}", gitlab_url, job_id, cache_key);

    info!("📥 Downloading cache: {}", cache_key);

    let response = client
        .get(&url)
        .header("JOB-TOKEN", token)
        .send()
        .await
        .context("Failed to download cache")?;

    if !response.status().is_success() {
        info!("📭 Cache miss: {}", cache_key);
        return Ok(());
    }

    let cache_data = response
        .bytes()
        .await
        .context("Failed to read cache data")?;

    // Extract ZIP to workspace
    extract_zip_to_workspace(&cache_data, workspace_path)?;

    info!(
        "✅ Extracted cache {} ({} bytes)",
        cache_key,
        cache_data.len()
    );

    Ok(())
}

/// Extract ZIP archive to workspace directory
fn extract_zip_to_workspace(zip_data: &[u8], workspace_path: &str) -> Result<()> {
    let cursor = std::io::Cursor::new(zip_data);
    let mut archive = ZipArchive::new(cursor).context("Failed to read ZIP archive")?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let file_path = file.name().to_string();

        // Skip directories
        if file.is_dir() {
            continue;
        }

        // Create full path
        let out_path = Path::new(workspace_path).join(&file_path);

        // Create parent directories
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Extract file
        let mut out_file = std::fs::File::create(&out_path)?;
        std::io::copy(&mut file, &mut out_file)?;

        // Set permissions (Unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }

    Ok(())
}

/// Create ZIP archive from paths (used for artifacts and cache upload)
pub async fn create_zip_from_paths(workspace_path: &str, paths: &[String]) -> Result<Vec<u8>> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let mut zip_buffer = Vec::new();
    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));

    for path_pattern in paths {
        let full_pattern = format!("{}/{}", workspace_path, path_pattern);

        // Use glob to find matching files
        for entry in glob::glob(&full_pattern)? {
            let file_path = entry?;

            // Handle both files and directories
            if file_path.is_file() {
                add_file_to_zip(&mut zip, &file_path, workspace_path).await?;
            } else if file_path.is_dir() {
                add_directory_to_zip(&mut zip, &file_path, workspace_path).await?;
            }
        }
    }

    zip.finish()?;
    drop(zip);

    Ok(zip_buffer)
}

/// Add a single file to ZIP archive with best compression
async fn add_file_to_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    file_path: &Path,
    workspace_path: &str,
) -> Result<()> {
    use zip::write::FileOptions;
    use zip::CompressionMethod;

    // Get relative path for ZIP entry
    let relative_path = file_path
        .strip_prefix(workspace_path)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    // Read file content
    let file_data = tokio::fs::read(file_path).await?;

    // Get file permissions and set high compression
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(file_path)?;
        FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(9)) // Best compression (0-9)
            .unix_permissions(metadata.permissions().mode())
    };

    #[cfg(not(unix))]
    let options = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9)); // Best compression

    // Add to ZIP
    zip.start_file(relative_path, options)?;
    zip.write_all(&file_data)?;

    Ok(())
}

/// Add all files in a directory to ZIP archive (recursive)
async fn add_directory_to_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    dir_path: &Path,
    workspace_path: &str,
) -> Result<()> {
    use walkdir::WalkDir;

    for entry in WalkDir::new(dir_path).follow_links(false) {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            add_file_to_zip(zip, path, workspace_path).await?;
        }
    }

    Ok(())
}
