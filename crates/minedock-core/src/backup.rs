//! Backup metadata and policy-independent validation.

use crate::{MineDockError, Result, WorldId, WorldStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const BACKUP_SCHEMA_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackupId(Uuid);

impl BackupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BackupId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BackupId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackupReason {
    Shutdown,
    Manual,
    PreUpgrade,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupFileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl BackupFileEntry {
    pub fn validate(&self) -> Result<()> {
        validate_relative_file(&self.path)?;
        if self.sha256.len() != 64 || !self.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MineDockError::Backup(
                "backup manifest contains an invalid SHA-256 digest".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub schema_version: u16,
    pub backup_id: BackupId,
    pub world_id: WorldId,
    pub created_at: DateTime<Utc>,
    pub reason: BackupReason,
    pub minecraft_version: String,
    pub files: Vec<BackupFileEntry>,
    pub total_bytes: u64,
    pub archive_bytes: u64,
    pub archive_sha256: String,
}

impl BackupManifest {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != BACKUP_SCHEMA_VERSION {
            return Err(MineDockError::Backup(
                "unsupported backup manifest schema version".into(),
            ));
        }
        if self.minecraft_version.trim().is_empty() {
            return Err(MineDockError::Backup(
                "backup manifest is missing the Minecraft version".into(),
            ));
        }
        let mut total = 0_u64;
        for file in &self.files {
            file.validate()?;
            total = total.saturating_add(file.size);
        }
        if total != self.total_bytes {
            return Err(MineDockError::Backup(
                "backup manifest total does not match its files".into(),
            ));
        }
        if self.archive_bytes == 0 {
            return Err(MineDockError::Backup(
                "backup manifest archive size must be greater than zero".into(),
            ));
        }
        if self.archive_sha256.len() != 64
            || !self.archive_sha256.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(MineDockError::Backup(
                "backup manifest contains an invalid archive SHA-256 digest".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupRecord {
    pub schema_version: u16,
    pub backup_id: BackupId,
    pub world_id: WorldId,
    pub created_at: DateTime<Utc>,
    pub reason: BackupReason,
    pub artifact_path: PathBuf,
    pub manifest: BackupManifest,
}

impl BackupRecord {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != BACKUP_SCHEMA_VERSION || self.manifest.backup_id != self.backup_id
        {
            return Err(MineDockError::Backup(
                "backup record identity or schema is invalid".into(),
            ));
        }
        if self.manifest.world_id != self.world_id
            || self.manifest.created_at != self.created_at
            || self.manifest.reason != self.reason
        {
            return Err(MineDockError::Backup(
                "backup record does not match its manifest".into(),
            ));
        }
        validate_relative_file(&self.artifact_path.to_string_lossy())?;
        self.manifest.validate()
    }
}

pub fn validate_backup_source_status(status: WorldStatus) -> Result<()> {
    if status != WorldStatus::Stopped {
        return Err(MineDockError::InvalidState(
            "a backup requires an authoritative Stopped world".into(),
        ));
    }
    Ok(())
}

pub fn validate_relative_file(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(MineDockError::Backup(
            "backup paths must be non-empty and relative".into(),
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(MineDockError::Backup(
            "backup paths must not contain traversal or root components".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups_require_stopped_state_and_consistent_manifest() {
        assert!(validate_backup_source_status(WorldStatus::Running).is_err());
        let backup_id = BackupId::new();
        let entry = BackupFileEntry {
            path: "world/level.dat".into(),
            size: 3,
            sha256: "0".repeat(64),
        };
        let manifest = BackupManifest {
            schema_version: BACKUP_SCHEMA_VERSION,
            backup_id,
            world_id: WorldId::new(),
            created_at: Utc::now(),
            reason: BackupReason::Manual,
            minecraft_version: "1.21.5".into(),
            files: vec![entry],
            total_bytes: 3,
            archive_bytes: 1,
            archive_sha256: "0".repeat(64),
        };
        assert!(manifest.validate().is_ok());
        assert!(validate_relative_file("../escape").is_err());
    }
}
