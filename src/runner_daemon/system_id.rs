//! Stable runner system ID, sent with every job request. GitLab tracks runner
//! managers by it, so it must not change between polls or restarts.
//! Mirrors gitlab-runner: a `.runner_system_id` file next to the config,
//! `s_` + 12 chars derived from the machine ID, or `r_` + 12 random chars.

use std::path::Path;
use tracing::{info, warn};

const ID_LENGTH: usize = 12;
const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

fn is_valid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == ID_LENGTH + 2
        && matches!(&bytes[..2], b"s_" | b"r_")
        && bytes[2..].iter().all(u8::is_ascii_alphanumeric)
}

/// Deterministic ID for a host, so it survives even when the state file can't be written
fn from_machine_id(machine_id: &str) -> String {
    let hash = blake3::derive_key("turboci runner system id", machine_id.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    format!("s_{}", &hex[..ID_LENGTH])
}

fn random_id() -> String {
    format!(
        "r_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..ID_LENGTH]
    )
}

fn generate() -> String {
    MACHINE_ID_PATHS
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|id| id.trim().to_string())
        .find(|id| !id.is_empty())
        .map(|id| from_machine_id(&id))
        .unwrap_or_else(random_id)
}

/// Read the system ID from `state_file`, creating it if missing or malformed
pub fn load_or_create(state_file: &Path) -> String {
    if let Ok(contents) = std::fs::read_to_string(state_file) {
        let id = contents.trim();
        if is_valid(id) {
            return id.to_string();
        }
    }

    let id = generate();
    match write_state(state_file, &id) {
        Ok(()) => info!("Created runner system ID {} in {:?}", id, state_file),
        // A machine-derived ID is the same after a restart, so not saving it is harmless
        Err(e) if id.starts_with("s_") => info!(
            "Using machine-derived runner system ID {} (not saved to {:?}: {})",
            id, state_file, e
        ),
        Err(e) => warn!(
            "Could not save runner system ID {} to {:?}: {}; it will change on restart",
            id, state_file, e
        ),
    }
    id
}

fn write_state(state_file: &Path, id: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(state_file)?.write_all(id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn machine_derived_id_is_stable_and_well_formed() {
        let a = from_machine_id("0123456789abcdef");
        assert_eq!(a, from_machine_id("0123456789abcdef"));
        assert_ne!(a, from_machine_id("fedcba9876543210"));
        assert!(is_valid(&a), "{}", a);
        assert!(is_valid(&random_id()));
    }

    #[test]
    fn creates_state_file_and_reuses_it() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".runner_system_id");

        let first = load_or_create(&file);
        let second = load_or_create(&file);

        assert!(is_valid(&first));
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first);
    }

    #[test]
    fn keeps_existing_valid_id_and_replaces_garbage() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".runner_system_id");

        std::fs::write(&file, "r_AbC123xyz789\n").unwrap();
        assert_eq!(load_or_create(&file), "r_AbC123xyz789");

        std::fs::write(&file, "not-an-id").unwrap();
        let replaced = load_or_create(&file);
        assert!(is_valid(&replaced));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), replaced);
    }

    #[test]
    fn unwritable_location_still_returns_valid_id() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("missing-dir").join(".runner_system_id");

        assert!(is_valid(&load_or_create(&file)));
    }
}
