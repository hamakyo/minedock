//! Telemetry-free, sanitized diagnostics and owned-temp recovery.

use atomic_write_file::AtomicWriteFile;
use chrono::Utc;
use minedock_core::{JsonWorldRepository, Result, WorldRepository, WorldStatus};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticBundle {
    schema_version: u16,
    app_version: &'static str,
    generated_at: String,
    platform: &'static str,
    metadata_available: bool,
    world_count: usize,
    status_counts: BTreeMap<String, usize>,
    server_versions: Vec<String>,
}

/// Remove only temporary files whose names are produced by the Vanilla
/// provider (`*.tmp-<pid>-<timestamp>`). Unknown files are never touched.
pub fn cleanup_owned_download_temps(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    validate_safe_path(root)?;
    let mut removed = 0;
    cleanup_download_tree(root, &mut removed)?;
    Ok(removed)
}

pub fn write_diagnostic_bundle(
    app_data_root: &Path,
    destination: impl Into<PathBuf>,
) -> Result<PathBuf> {
    let destination = destination.into();
    validate_safe_path(&destination)?;
    let repository = JsonWorldRepository::new(app_data_root.to_path_buf());
    let metadata = repository.load().ok();
    let mut status_counts = BTreeMap::new();
    let mut server_versions = Vec::new();
    let (world_count, metadata_available) = if let Some(metadata) = metadata {
        for world in &metadata.worlds {
            *status_counts
                .entry(status_name(world.status).to_owned())
                .or_insert(0) += 1;
            server_versions.push(world.server.version.clone());
        }
        server_versions.sort();
        server_versions.dedup();
        (metadata.worlds.len(), true)
    } else {
        (0, false)
    };
    let bundle = DiagnosticBundle {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        app_version: env!("CARGO_PKG_VERSION"),
        generated_at: Utc::now().to_rfc3339(),
        platform: std::env::consts::OS,
        metadata_available,
        world_count,
        status_counts,
        server_versions,
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_persistence)?;
        validate_safe_path(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&bundle).map_err(|error| {
        minedock_core::MineDockError::Persistence(format!(
            "could not serialize diagnostics: {error}"
        ))
    })?;
    let mut file = AtomicWriteFile::open(&destination).map_err(io_persistence)?;
    file.write_all(&bytes).map_err(io_persistence)?;
    file.flush().map_err(io_persistence)?;
    file.commit().map_err(io_persistence)?;
    Ok(destination)
}

fn cleanup_download_tree(path: &Path, removed: &mut usize) -> Result<()> {
    for entry in fs::read_dir(path).map_err(io_persistence)? {
        let entry = entry.map_err(io_persistence)?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child).map_err(io_persistence)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(minedock_core::MineDockError::Persistence(
                "download cache contains a symlink or reparse point".into(),
            ));
        }
        if metadata.is_dir() {
            cleanup_download_tree(&child, removed)?;
        } else if metadata.is_file()
            && child
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().starts_with("tmp-"))
        {
            fs::remove_file(&child).map_err(io_persistence)?;
            *removed += 1;
        }
    }
    Ok(())
}

fn status_name(status: WorldStatus) -> &'static str {
    match status {
        WorldStatus::Stopped => "stopped",
        WorldStatus::Preparing => "preparing",
        WorldStatus::Starting => "starting",
        WorldStatus::Running => "running",
        WorldStatus::Stopping => "stopping",
        WorldStatus::BackingUp => "backing-up",
        WorldStatus::Failed => "failed",
    }
}

fn io_persistence(error: std::io::Error) -> minedock_core::MineDockError {
    minedock_core::MineDockError::Persistence(error.to_string())
}

fn validate_safe_path(path: &Path) -> Result<()> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                return Err(minedock_core::MineDockError::Persistence(
                    "diagnostic path contains a symlink or reparse point".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_persistence(error)),
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use minedock_core::{AppMetadata, CreateWorldRequest, TemplateCatalog, WorldRepository};
    use tempfile::TempDir;

    #[test]
    fn diagnostics_omit_paths_and_owned_temp_cleanup_is_narrow() {
        let root = TempDir::new().expect("root");
        let repository = JsonWorldRepository::new(root.path());
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let world = CreateWorldRequest::new("Private Name", "creative")
            .expect("request")
            .build_world(&catalog)
            .expect("world");
        repository
            .save(&AppMetadata::new(vec![world]))
            .expect("metadata");
        let downloads = root.path().join("downloads");
        fs::create_dir_all(&downloads).expect("downloads");
        fs::write(downloads.join("server.tmp-1-2"), b"partial").expect("temp");
        fs::write(downloads.join("keep.txt"), b"keep").expect("keep");
        assert_eq!(
            cleanup_owned_download_temps(&downloads).expect("cleanup"),
            1
        );
        let output = root.path().join("diagnostics.json");
        write_diagnostic_bundle(root.path(), &output).expect("diagnostics");
        let text = fs::read_to_string(output).expect("read");
        assert!(!text.contains("Private Name"));
        assert!(!text.contains(root.path().to_string_lossy().as_ref()));
    }
}
