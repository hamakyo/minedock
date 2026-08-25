//! Safe local stopped-world backup engine.
//!
//! A backup is a versioned directory snapshot.  The snapshot is published
//! only after every selected file has been copied and re-hashed.  Server JARs,
//! caches, runtime leases, raw logs, temporary files, and previous backups are
//! deliberately outside the selection.

use atomic_write_file::AtomicWriteFile;
use chrono::Utc;
use minedock_core::{
    BackupFileEntry, BackupId, BackupManifest, BackupReason, BackupRecord, MineDockError, Result,
    World, validate_backup_source_status,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const BACKUPS_DIR: &str = "backups";
const INDEX_FILE: &str = "index.json";
const MANIFEST_FILE: &str = "manifest.json";
const RECOVERY_FILE: &str = "recovery.json";
const TEMP_SUFFIX: &str = ".part";
const RECOVERY_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone)]
pub struct SafeBackupEngine {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupIndex {
    schema_version: u16,
    records: Vec<BackupRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupRecoveryMarker {
    schema_version: u16,
    world_id: minedock_core::WorldId,
}

impl SafeBackupEngine {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn create(&self, world: &World, reason: BackupReason) -> Result<BackupRecord> {
        validate_backup_source_status(world.status)?;
        world.validate()?;
        let source_root = self.root.join(&world.data_path);
        validate_owned_path(&self.root, &source_root)?;
        let world_save = source_root.join("world");
        require_directory(&world_save)?;

        let backup_root = self.root.join(BACKUPS_DIR).join(world.id.to_string());
        prepare_directory(&self.root, &backup_root)?;
        let _ = cleanup_owned_partials(&backup_root)?;

        let backup_id = BackupId::new();
        let temporary = backup_root.join(format!("{backup_id}{TEMP_SUFFIX}"));
        let published = backup_root.join(backup_id.to_string());
        validate_owned_path(&backup_root, &temporary)?;
        validate_owned_path(&backup_root, &published)?;
        if published.exists() || temporary.exists() {
            return Err(MineDockError::Backup(
                "backup destination already exists".into(),
            ));
        }
        fs::create_dir(&temporary).map_err(io_backup)?;

        let result = self.create_into(
            world,
            reason,
            backup_id,
            &source_root,
            &temporary,
            &published,
            &backup_root,
        );
        if result.is_err() {
            let _ = remove_owned_path(&temporary);
        }
        result
    }

    pub fn list(&self, world_id: minedock_core::WorldId) -> Result<Vec<BackupRecord>> {
        self.reconcile_index(world_id)?;
        let index_path = self.index_path(world_id);
        if !index_path.exists() {
            return Ok(Vec::new());
        }
        validate_owned_path(&self.root, &index_path)?;
        let bytes = fs::read(&index_path).map_err(io_backup)?;
        let index: BackupIndex = serde_json::from_slice(&bytes)
            .map_err(|error| MineDockError::Backup(format!("backup index is corrupt: {error}")))?;
        if index.schema_version != minedock_core::BACKUP_SCHEMA_VERSION {
            return Err(MineDockError::Backup(
                "unsupported backup index schema version".into(),
            ));
        }
        let mut records = index.records;
        for record in &records {
            record.validate()?;
            if record.world_id != world_id
                || record.artifact_path != self.artifact_relative(world_id, record.backup_id)
            {
                return Err(MineDockError::Backup(
                    "backup index contains an out-of-scope artifact path".into(),
                ));
            }
        }
        records.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(records)
    }

    pub fn latest(&self, world_id: minedock_core::WorldId) -> Result<Option<BackupRecord>> {
        Ok(self.list(world_id)?.into_iter().next())
    }

    pub fn recover_stale_partials(&self) -> Result<usize> {
        let backups_root = self.root.join(BACKUPS_DIR);
        if !backups_root.exists() {
            return Ok(0);
        }
        validate_owned_path(&self.root, &backups_root)?;
        let mut removed = 0;
        for entry in fs::read_dir(&backups_root).map_err(io_backup)? {
            let entry = entry.map_err(io_backup)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_backup)?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(MineDockError::Backup(
                    "backup root contains a symlink or reparse point".into(),
                ));
            }
            if metadata.is_dir() {
                removed += cleanup_owned_partials(&path)?;
                if let Some(world_name) = path.file_name().and_then(|value| value.to_str()) {
                    if let Ok(world_id) = minedock_core::WorldId::parse(world_name) {
                        self.reconcile_index(world_id)?;
                    }
                }
            }
        }
        Ok(removed)
    }

    pub fn mark_recovery_required(&self, world_id: minedock_core::WorldId) -> Result<()> {
        let path = self.recovery_path(world_id);
        let parent = path
            .parent()
            .ok_or_else(|| MineDockError::Backup("backup recovery path has no parent".into()))?;
        prepare_directory(&self.root, parent)?;
        write_json_atomic(
            &path,
            &BackupRecoveryMarker {
                schema_version: RECOVERY_SCHEMA_VERSION,
                world_id,
            },
        )
    }

    pub fn clear_recovery_required(&self, world_id: minedock_core::WorldId) -> Result<()> {
        let path = self.recovery_path(world_id);
        validate_owned_path(&self.root, &path)?;
        if !path.exists() {
            return Ok(());
        }
        remove_owned_path(&path)
    }

    pub fn recovery_required(&self, world_id: minedock_core::WorldId) -> Result<bool> {
        let path = self.recovery_path(world_id);
        validate_owned_path(&self.root, &path)?;
        if !path.exists() {
            return Ok(false);
        }
        let bytes = fs::read(&path).map_err(io_backup)?;
        let marker: BackupRecoveryMarker = serde_json::from_slice(&bytes).map_err(|error| {
            MineDockError::Backup(format!("backup recovery marker is corrupt: {error}"))
        })?;
        if marker.schema_version != RECOVERY_SCHEMA_VERSION || marker.world_id != world_id {
            return Err(MineDockError::Backup(
                "backup recovery marker identity or schema is invalid".into(),
            ));
        }
        Ok(true)
    }

    pub fn retain(
        &self,
        world_id: minedock_core::WorldId,
        retain: u16,
    ) -> Result<Vec<BackupRecord>> {
        let mut records = self.list(world_id)?;
        if retain == 0 {
            return Err(MineDockError::Backup(
                "backup retention must retain at least one record".into(),
            ));
        }
        while records.len() > usize::from(retain) {
            let old = records.pop().ok_or_else(|| {
                MineDockError::Backup("backup retention list unexpectedly became empty".into())
            })?;
            let path = self.root.join(&old.artifact_path);
            validate_owned_path(&self.root, &path)?;
            remove_owned_path(&path)?;
        }
        self.write_index(world_id, &records)?;
        Ok(records)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_into(
        &self,
        world: &World,
        reason: BackupReason,
        backup_id: BackupId,
        source_root: &Path,
        temporary: &Path,
        published: &Path,
        backup_root: &Path,
    ) -> Result<BackupRecord> {
        let mut selected = BTreeMap::<String, PathBuf>::new();
        collect_files(
            &world_source_root(source_root),
            Path::new("world"),
            &mut selected,
        )?;
        for relative in [
            "server.properties",
            "eula.txt",
            "whitelist.json",
            "ops.json",
            "banned-players.json",
            "banned-ips.json",
            "server/provision.json",
        ] {
            let path = source_root.join(relative);
            if path.exists() {
                require_regular_file(&path)?;
                selected.insert(relative.replace('\\', "/"), path);
            }
        }
        if selected.is_empty() {
            return Err(MineDockError::Backup(
                "the stopped world has no save or server configuration to back up".into(),
            ));
        }

        let required_bytes = selected
            .values()
            .map(|path| fs::metadata(path).map(|metadata| metadata.len()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(io_backup)?
            .into_iter()
            .try_fold(256 * 1024_u64, |total, size| total.checked_add(size))
            .ok_or_else(|| MineDockError::Backup("backup size overflowed".into()))?;
        crate::native_safety::ensure_free_space(&self.root, required_bytes)
            .map_err(|error| MineDockError::Backup(error.to_string()))?;

        let mut entries = Vec::with_capacity(selected.len());
        for (relative, source) in selected {
            let destination = temporary.join(&relative);
            let (size, sha256) = copy_and_hash(&source, &destination)?;
            entries.push(BackupFileEntry {
                path: relative,
                size,
                sha256,
            });
        }
        let total_bytes = entries
            .iter()
            .map(|entry| entry.size)
            .try_fold(0_u64, |total, size| total.checked_add(size))
            .ok_or_else(|| MineDockError::Backup("backup size overflowed".into()))?;
        let created_at = Utc::now();
        let manifest = BackupManifest {
            schema_version: minedock_core::BACKUP_SCHEMA_VERSION,
            backup_id,
            world_id: world.id,
            created_at,
            reason,
            files: entries,
            total_bytes,
        };
        manifest.validate()?;
        write_json_atomic(&temporary.join(MANIFEST_FILE), &manifest)?;
        sync_directory(temporary)?;
        fs::rename(temporary, published).map_err(io_backup)?;
        sync_directory(backup_root)?;
        let record = BackupRecord {
            schema_version: minedock_core::BACKUP_SCHEMA_VERSION,
            backup_id,
            world_id: world.id,
            created_at,
            reason,
            artifact_path: self.artifact_relative(world.id, backup_id),
            manifest,
        };
        record.validate()?;
        let mut records = self.list(world.id)?;
        if !records
            .iter()
            .any(|existing| existing.backup_id == record.backup_id)
        {
            records.push(record.clone());
        }
        records.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        self.write_index(world.id, &records)?;
        Ok(record)
    }

    fn index_path(&self, world_id: minedock_core::WorldId) -> PathBuf {
        self.root
            .join(BACKUPS_DIR)
            .join(world_id.to_string())
            .join(INDEX_FILE)
    }

    fn artifact_relative(&self, world_id: minedock_core::WorldId, backup_id: BackupId) -> PathBuf {
        PathBuf::from(BACKUPS_DIR)
            .join(world_id.to_string())
            .join(backup_id.to_string())
    }

    fn recovery_path(&self, world_id: minedock_core::WorldId) -> PathBuf {
        self.root
            .join(BACKUPS_DIR)
            .join(world_id.to_string())
            .join(RECOVERY_FILE)
    }

    /// Rebuild the index from the published artifacts. This is deliberately
    /// idempotent: a crash after artifact publication but before index commit
    /// recovers the artifact, while a crash after retention deletion removes
    /// stale index entries on the next read/startup.
    fn reconcile_index(&self, world_id: minedock_core::WorldId) -> Result<()> {
        let backup_root = self.root.join(BACKUPS_DIR).join(world_id.to_string());
        if !backup_root.exists() {
            return Ok(());
        }
        validate_owned_path(&self.root, &backup_root)?;
        let records = self.scan_published_artifacts(world_id, &backup_root)?;
        let index_path = self.index_path(world_id);
        let current = if index_path.exists() {
            validate_owned_path(&self.root, &index_path)?;
            match fs::read(&index_path) {
                Ok(bytes) => serde_json::from_slice::<BackupIndex>(&bytes)
                    .ok()
                    .and_then(|index| {
                        (index.schema_version == minedock_core::BACKUP_SCHEMA_VERSION
                            && index.records.iter().all(|record| {
                                record.validate().is_ok()
                                    && record.world_id == world_id
                                    && record.artifact_path
                                        == self.artifact_relative(world_id, record.backup_id)
                            }))
                        .then_some(index.records)
                    }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(io_backup(error)),
            }
        } else {
            None
        };
        if current.as_ref() != Some(&records) {
            self.write_index(world_id, &records)?;
        }
        Ok(())
    }

    fn scan_published_artifacts(
        &self,
        world_id: minedock_core::WorldId,
        backup_root: &Path,
    ) -> Result<Vec<BackupRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(backup_root).map_err(io_backup)? {
            let entry = entry.map_err(io_backup)?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == INDEX_FILE || name == RECOVERY_FILE || name.ends_with(TEMP_SUFFIX) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(io_backup)?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(MineDockError::Backup(
                    "backup catalog contains a symlink or reparse point".into(),
                ));
            }
            if !metadata.is_dir() {
                return Err(MineDockError::Backup(
                    "backup catalog contains an unsupported entry".into(),
                ));
            }
            let manifest_path = path.join(MANIFEST_FILE);
            validate_owned_path(&self.root, &manifest_path)?;
            let bytes = fs::read(&manifest_path).map_err(io_backup)?;
            let manifest: BackupManifest = serde_json::from_slice(&bytes).map_err(|error| {
                MineDockError::Backup(format!("backup manifest is corrupt: {error}"))
            })?;
            manifest.validate()?;
            if manifest.world_id != world_id || name != manifest.backup_id.to_string() {
                return Err(MineDockError::Backup(
                    "backup artifact identity does not match its directory".into(),
                ));
            }
            verify_artifact(&path, &manifest)?;
            let record = BackupRecord {
                schema_version: minedock_core::BACKUP_SCHEMA_VERSION,
                backup_id: manifest.backup_id,
                world_id,
                created_at: manifest.created_at,
                reason: manifest.reason,
                artifact_path: self.artifact_relative(world_id, manifest.backup_id),
                manifest,
            };
            record.validate()?;
            records.push(record);
        }
        records.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(records)
    }

    fn write_index(
        &self,
        world_id: minedock_core::WorldId,
        records: &[BackupRecord],
    ) -> Result<()> {
        let path = self.index_path(world_id);
        prepare_directory(
            &self.root,
            path.parent().ok_or_else(|| {
                MineDockError::Backup("backup index has no parent directory".into())
            })?,
        )?;
        write_json_atomic(
            &path,
            &BackupIndex {
                schema_version: minedock_core::BACKUP_SCHEMA_VERSION,
                records: records.to_vec(),
            },
        )
    }
}

fn world_source_root(source_root: &Path) -> PathBuf {
    source_root.join("world")
}

fn collect_files(
    directory: &Path,
    relative: &Path,
    selected: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    require_directory(directory)?;
    for entry in fs::read_dir(directory).map_err(io_backup)? {
        let entry = entry.map_err(io_backup)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_backup)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(MineDockError::Backup(
                "world save contains a symlink or reparse point".into(),
            ));
        }
        let name = entry.file_name();
        let child_relative = relative.join(name);
        if metadata.is_dir() {
            collect_files(&path, &child_relative, selected)?;
        } else if metadata.is_file() {
            selected.insert(normalize_relative(&child_relative), path);
        } else {
            return Err(MineDockError::Backup(
                "world save contains an unsupported filesystem entry".into(),
            ));
        }
    }
    Ok(())
}

fn copy_and_hash(source: &Path, destination: &Path) -> Result<(u64, String)> {
    require_regular_file(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_backup)?;
    }
    let mut input = File::open(source).map_err(io_backup)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(io_backup)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let count = input.read(&mut buffer).map_err(io_backup)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count]).map_err(io_backup)?;
        digest.update(&buffer[..count]);
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| MineDockError::Backup("backup file size overflowed".into()))?;
    }
    output.flush().map_err(io_backup)?;
    output.sync_all().map_err(io_backup)?;
    let sha256 = format_digest(digest.finalize().as_slice());
    let (_, copied_digest) = hash_file(destination)?;
    if copied_digest != sha256 {
        return Err(MineDockError::Backup(
            "backup verification digest does not match the source".into(),
        ));
    }
    Ok((size, sha256))
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).map_err(io_backup)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(io_backup)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size = size.saturating_add(count as u64);
    }
    Ok((size, format_digest(digest.finalize().as_slice())))
}

fn verify_artifact(artifact: &Path, manifest: &BackupManifest) -> Result<()> {
    for entry in &manifest.files {
        let path = artifact.join(&entry.path);
        validate_owned_path(artifact, &path)?;
        require_regular_file(&path)?;
        let (size, sha256) = hash_file(&path)?;
        if size != entry.size || sha256 != entry.sha256 {
            return Err(MineDockError::Backup(
                "backup artifact does not match its manifest".into(),
            ));
        }
    }
    Ok(())
}

fn format_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_relative(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn prepare_directory(root: &Path, directory: &Path) -> Result<()> {
    validate_owned_path(root, directory)?;
    fs::create_dir_all(directory).map_err(io_backup)?;
    validate_owned_path(root, directory)
}

fn cleanup_owned_partials(directory: &Path) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(directory).map_err(io_backup)? {
        let entry = entry.map_err(io_backup)?;
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(TEMP_SUFFIX))
        {
            remove_owned_path(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn remove_owned_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_backup)?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(MineDockError::Backup(
            "refusing to remove a symlink or reparse point".into(),
        ));
    }
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(io_backup)
    } else {
        fs::remove_file(path).map_err(io_backup)
    }
}

fn require_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_backup)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(MineDockError::Backup(format!(
            "backup source is not a safe directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_backup)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(MineDockError::Backup(format!(
            "backup source is not a safe file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_owned_path(root: &Path, path: &Path) -> Result<()> {
    if path.is_absolute() && !path.starts_with(root) {
        return Err(MineDockError::Backup(
            "backup path escapes the app-data root".into(),
        ));
    }
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                return Err(MineDockError::Backup(
                    "backup path contains a symlink or reparse point".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_backup(error)),
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_backup)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        MineDockError::Backup(format!("could not serialize backup metadata: {error}"))
    })?;
    let mut file = AtomicWriteFile::open(path).map_err(io_backup)?;
    file.write_all(&bytes).map_err(io_backup)?;
    file.flush().map_err(io_backup)?;
    file.commit().map_err(io_backup)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .map_err(io_backup)?
            .sync_all()
            .map_err(io_backup)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn io_backup(error: std::io::Error) -> MineDockError {
    MineDockError::Backup(error.to_string())
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
    use minedock_core::{BackupPolicy, CreateWorldRequest, TemplateCatalog, WorldStatus};
    use tempfile::TempDir;

    fn world() -> World {
        let catalog = TemplateCatalog::built_in().expect("catalog");
        CreateWorldRequest::new("Backup", "creative")
            .expect("request")
            .build_world(&catalog)
            .expect("world")
    }

    #[test]
    fn stopped_world_snapshot_excludes_jars_logs_and_previous_backups() {
        let root = TempDir::new().expect("root");
        let mut value = world();
        value.status = WorldStatus::Stopped;
        value.backup = BackupPolicy {
            enabled: true,
            on_shutdown: true,
            retain: 2,
        };
        let source = root.path().join(&value.data_path);
        fs::create_dir_all(source.join("world/region")).expect("world");
        fs::write(source.join("world/level.dat"), b"save").expect("save");
        fs::write(source.join("server.properties"), b"server-port=25565").expect("properties");
        fs::create_dir_all(source.join("server")).expect("server");
        fs::write(source.join("server/server.jar"), b"jar").expect("jar");
        fs::create_dir_all(source.join("logs")).expect("logs");
        fs::write(source.join("logs/server.log"), b"log").expect("log");

        let engine = SafeBackupEngine::new(root.path());
        let record = engine.create(&value, BackupReason::Manual).expect("backup");
        assert_eq!(record.manifest.files.len(), 2);
        assert!(
            record
                .manifest
                .files
                .iter()
                .any(|entry| entry.path == "world/level.dat")
        );
        assert!(
            record
                .manifest
                .files
                .iter()
                .any(|entry| entry.path == "server.properties")
        );
        assert!(
            !record
                .manifest
                .files
                .iter()
                .any(|entry| entry.path.contains("jar"))
        );
        assert!(engine.latest(value.id).expect("latest").is_some());
    }

    #[test]
    fn backup_rejects_running_world_and_cleans_owned_partials() {
        let root = TempDir::new().expect("root");
        let mut value = world();
        value.status = WorldStatus::Running;
        let engine = SafeBackupEngine::new(root.path());
        assert!(engine.create(&value, BackupReason::Shutdown).is_err());
        let backup_dir = root.path().join("backups").join(value.id.to_string());
        fs::create_dir_all(&backup_dir).expect("backup dir");
        fs::create_dir(backup_dir.join("owned.part")).expect("partial");
        value.status = WorldStatus::Stopped;
        let source = root.path().join(&value.data_path).join("world");
        fs::create_dir_all(&source).expect("world");
        fs::write(source.join("level.dat"), b"save").expect("save");
        let _ = engine.create(&value, BackupReason::Manual).expect("backup");
        assert!(!backup_dir.join("owned.part").exists());
    }

    #[test]
    fn backup_index_reconciles_published_and_deleted_artifacts() {
        let root = TempDir::new().expect("root");
        let mut value = world();
        value.status = WorldStatus::Stopped;
        let source = root.path().join(&value.data_path).join("world");
        fs::create_dir_all(&source).expect("world");
        fs::write(source.join("level.dat"), b"save").expect("save");

        let engine = SafeBackupEngine::new(root.path());
        let record = engine.create(&value, BackupReason::Manual).expect("backup");
        let index = root
            .path()
            .join(&record.artifact_path)
            .with_file_name(INDEX_FILE);
        fs::write(&index, b"not-json").expect("corrupt index");

        // A published artifact is authoritative enough to rebuild an index
        // after a crash between artifact rename and index commit.
        assert_eq!(
            engine.latest(value.id).expect("reconcile").as_ref(),
            Some(&record)
        );

        let artifact = root.path().join(&record.artifact_path);
        fs::remove_dir_all(&artifact).expect("remove artifact");
        // A retention/index crash can leave a record pointing at a deleted
        // artifact; the next read removes the stale reference.
        assert!(
            engine
                .latest(value.id)
                .expect("repair missing artifact")
                .is_none()
        );
        let repaired: BackupIndex =
            serde_json::from_slice(&fs::read(&index).expect("repaired index")).expect("index");
        assert!(repaired.records.is_empty());
    }
}
