//! Telemetry-free, sanitized diagnostics and owned-temp recovery.

use atomic_write_file::AtomicWriteFile;
use chrono::Utc;
use minedock_core::{
    JsonWorldRepository, Result, ServerSessionRecord, WorldRepository, WorldStatus,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const DIAGNOSTIC_SCHEMA_VERSION: u16 = 2;
const MAX_DIAGNOSTIC_LOG_LINES: usize = 40;
const MAX_DIAGNOSTIC_LOG_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticBundleReport {
    pub path: PathBuf,
    pub contents_summary: String,
}

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
    settings: DiagnosticSettings,
    worlds: Vec<DiagnosticWorld>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticSettings {
    available: bool,
    language: Option<String>,
    java_path_configured: bool,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticWorld {
    world_id: String,
    lifecycle_status: String,
    minecraft_version: String,
    server_settings: DiagnosticServerSettings,
    session: Option<ServerSessionRecord>,
    recent_log_tails: Vec<DiagnosticLogTail>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticServerSettings {
    game_mode: String,
    difficulty: String,
    hardcore: bool,
    generate_structures: bool,
    pvp: bool,
    max_players: u16,
    online_mode: bool,
    whitelist: bool,
    port: u16,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticLogTail {
    timestamp: String,
    stream: String,
    text: String,
    truncated: bool,
}

/// Remove only temporary files whose names are produced by the Vanilla
/// provider (*.tmp-<pid>-<timestamp>). Unknown files are never touched.
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
) -> Result<DiagnosticBundleReport> {
    let destination = destination.into();
    validate_safe_path(&destination)?;
    let repository = JsonWorldRepository::new(app_data_root.to_path_buf());
    let metadata = repository.load().ok();
    let mut status_counts = BTreeMap::new();
    let mut server_versions = Vec::new();
    let settings = match crate::localization::load_settings(app_data_root) {
        Ok(settings) => DiagnosticSettings {
            available: true,
            language: Some(format!("{:?}", settings.language).to_lowercase()),
            java_path_configured: settings.java_path.is_some(),
        },
        Err(_) => DiagnosticSettings {
            available: false,
            language: None,
            java_path_configured: false,
        },
    };
    let session_store = crate::session_store::SessionLogStore::new(app_data_root.to_path_buf());
    let mut worlds = Vec::new();
    let (world_count, metadata_available) = if let Some(metadata) = metadata {
        for world in &metadata.worlds {
            *status_counts
                .entry(status_name(world.status).to_owned())
                .or_insert(0) += 1;
            server_versions.push(world.server.version.clone());

            let snapshot = session_store.latest_snapshot(world.id).ok().flatten();
            let (session, recent_log_tails) = match snapshot {
                Some(snapshot) => (
                    Some(snapshot.record),
                    snapshot
                        .logs
                        .into_iter()
                        .rev()
                        .take(MAX_DIAGNOSTIC_LOG_LINES)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|log| DiagnosticLogTail {
                            timestamp: log.timestamp.to_rfc3339(),
                            stream: format!("{:?}", log.stream).to_lowercase(),
                            text: truncate_log_tail(&log.text),
                            truncated: log.truncated,
                        })
                        .collect(),
                ),
                None => (None, Vec::new()),
            };
            worlds.push(DiagnosticWorld {
                world_id: world.id.to_string(),
                lifecycle_status: status_name(world.status).to_owned(),
                minecraft_version: world.server.version.clone(),
                server_settings: DiagnosticServerSettings {
                    game_mode: format!("{:?}", world.settings.gamemode).to_lowercase(),
                    difficulty: format!("{:?}", world.settings.difficulty).to_lowercase(),
                    hardcore: world.settings.hardcore,
                    generate_structures: world.settings.generate_structures,
                    pvp: world.settings.pvp,
                    max_players: world.server.max_players,
                    online_mode: world.server.online_mode,
                    whitelist: world.server.whitelist,
                    port: world.server.port,
                },
                session,
                recent_log_tails,
            });
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
        settings,
        worlds,
    };
    let session_count = bundle
        .worlds
        .iter()
        .filter(|world| world.session.is_some())
        .count();
    let log_count: usize = bundle
        .worlds
        .iter()
        .map(|world| world.recent_log_tails.len())
        .sum();
    let contents_summary = format!(
        "{} world(s), {session_count} validated session record(s), and {log_count} bounded recent log line(s). World saves, credentials, and absolute paths are excluded.",
        bundle.world_count
    );
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
    Ok(DiagnosticBundleReport {
        path: destination,
        contents_summary,
    })
}

fn truncate_log_tail(value: &str) -> String {
    let mut text = value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    if text.chars().count() > MAX_DIAGNOSTIC_LOG_CHARS {
        text = text.chars().take(MAX_DIAGNOSTIC_LOG_CHARS).collect();
    }
    text
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
    use minedock_core::{
        AppMetadata, CreateWorldRequest, LogStream, RawLogLine, ServerEvent, SessionId,
        TemplateCatalog, WorldRepository,
    };
    use tempfile::TempDir;

    #[test]
    fn diagnostics_include_sanitized_settings_session_metadata_and_bounded_tails() {
        let root = TempDir::new().expect("root");
        let repository = JsonWorldRepository::new(root.path());
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let world = CreateWorldRequest::new("Private Name", "creative")
            .expect("request")
            .build_world(&catalog)
            .expect("world");
        let world_id = world.id;
        repository
            .save(&AppMetadata::new(vec![world]))
            .expect("metadata");
        let session_store = crate::session_store::SessionLogStore::new(root.path());
        let session_id = SessionId::new();
        session_store
            .begin_session(world_id, session_id, Some(42))
            .expect("begin session");
        session_store
            .append_events(
                world_id,
                session_id,
                &[ServerEvent::Raw(RawLogLine {
                    world_id,
                    session_id,
                    stream: LogStream::Stdout,
                    line: "[Server thread/INFO]: Alex joined the game".into(),
                    truncated: false,
                })],
            )
            .expect("append session log");
        let downloads = root.path().join("downloads");
        fs::create_dir_all(&downloads).expect("downloads");
        fs::write(downloads.join("server.tmp-1-2"), b"partial").expect("temp");
        fs::write(downloads.join("keep.txt"), b"keep").expect("keep");
        assert_eq!(
            cleanup_owned_download_temps(&downloads).expect("cleanup"),
            1
        );
        let output = root.path().join("diagnostics.json");
        let report = write_diagnostic_bundle(root.path(), &output).expect("diagnostics");
        assert!(report.contents_summary.contains("World saves"));
        let text = fs::read_to_string(output).expect("read");
        assert!(!text.contains("Private Name"));
        assert!(!text.contains(root.path().to_string_lossy().as_ref()));
        assert!(text.contains("server_settings"));
        assert!(text.contains("session_id"));
        assert!(text.contains("recent_log_tails"));
        assert!(text.contains("Alex joined the game"));
        assert!(
            report
                .contents_summary
                .contains("1 validated session record")
        );
    }
}
