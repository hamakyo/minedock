//! Safe local stopped-world backup engine.
//!
//! A backup is a verified ZIP archive plus a sidecar manifest. The archive is
//! published only after every selected file has been copied, the completed
//! archive has been hashed, and the manifest has been durably written. Server
//! JARs, caches, runtime leases, raw logs, temporary files, and previous
//! backups are deliberately outside the selection.

use atomic_write_file::AtomicWriteFile;
use chrono::Utc;
use minedock_core::{
    BackupFileEntry, BackupId, BackupManifest, BackupReason, BackupRecord, MineDockError, Result,
    World, validate_backup_source_status, validate_relative_file,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const BACKUPS_DIR: &str = "backups";
const INDEX_FILE: &str = "index.json";
const LEGACY_MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_EXTENSION: &str = "manifest.json";
const RECOVERY_FILE: &str = "recovery.json";
const TEMP_SUFFIX: &str = ".part";
const ARTIFACT_EXTENSION: &str = "zip";
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
        let temporary = backup_root.join(format!("{backup_id}.{ARTIFACT_EXTENSION}{TEMP_SUFFIX}"));
        let published = backup_root.join(format!("{backup_id}.{ARTIFACT_EXTENSION}"));
        let manifest_temporary =
            backup_root.join(format!("{backup_id}.{MANIFEST_EXTENSION}{TEMP_SUFFIX}"));
        let manifest_published = backup_root.join(format!("{backup_id}.{MANIFEST_EXTENSION}"));
        for path in [
            &temporary,
            &published,
            &manifest_temporary,
            &manifest_published,
        ] {
            validate_owned_path(&backup_root, path)?;
            if path.exists() {
                return Err(MineDockError::Backup(
                    "backup destination already exists".into(),
                ));
            }
        }

        let result = self.create_into(
            world,
            reason,
            backup_id,
            &source_root,
            &temporary,
            &published,
            &manifest_temporary,
            &manifest_published,
            &backup_root,
        );
        if result.is_err() {
            for path in [&temporary, &manifest_temporary] {
                let _ = remove_if_exists(path);
            }
            if !published.exists() {
                let _ = remove_if_exists(&manifest_published);
            }
        }
        result
    }

    /// Return fully verified backup records. This is intentionally a worker
    /// operation: the integrity digest reads every archived file.
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

    #[allow(dead_code)]
    pub fn latest(&self, world_id: minedock_core::WorldId) -> Result<Option<BackupRecord>> {
        Ok(self.list(world_id)?.into_iter().next())
    }

    /// Remove MineDock-owned partials and rebuild indexes using only archive
    /// structure, manifest fields, and entry sizes. No archive payload is
    /// hashed here so startup cannot synchronously read large worlds.
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
                        self.reconcile_index_structural(world_id)?;
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
            self.remove_artifact(&old)?;
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
        manifest_temporary: &Path,
        manifest_published: &Path,
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

        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)
            .map_err(io_backup)?;
        let mut archive = ZipWriter::new(output);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        let mut entries = Vec::with_capacity(selected.len());
        for (relative, source) in selected {
            require_regular_file(&source)?;
            let mut input = File::open(&source).map_err(io_backup)?;
            archive.start_file(&relative, options).map_err(zip_backup)?;
            let (size, sha256) = copy_into_archive(&mut input, &mut archive)?;
            entries.push(BackupFileEntry {
                path: relative,
                size,
                sha256,
            });
        }
        let mut output = archive.finish().map_err(zip_backup)?;
        output.flush().map_err(io_backup)?;
        output.sync_all().map_err(io_backup)?;
        drop(output);

        let (archive_bytes, archive_sha256) = hash_file(temporary)?;
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
            minecraft_version: world.server.version.clone(),
            files: entries,
            total_bytes,
            archive_bytes,
            archive_sha256,
        };
        manifest.validate()?;

        // The sidecar is published before the archive. A crash between the
        // two leaves an ignored sidecar, never a valid backup record.
        write_json_atomic(manifest_temporary, &manifest)?;
        fs::rename(manifest_temporary, manifest_published).map_err(io_backup)?;
        sync_directory(backup_root)?;
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
            .join(format!("{backup_id}.{ARTIFACT_EXTENSION}"))
    }

    fn recovery_path(&self, world_id: minedock_core::WorldId) -> PathBuf {
        self.root
            .join(BACKUPS_DIR)
            .join(world_id.to_string())
            .join(RECOVERY_FILE)
    }

    fn manifest_path(&self, artifact: &Path) -> PathBuf {
        artifact.with_extension(MANIFEST_EXTENSION)
    }

    fn remove_artifact(&self, record: &BackupRecord) -> Result<()> {
        let artifact = self.root.join(&record.artifact_path);
        validate_owned_path(&self.root, &artifact)?;
        remove_owned_path(&artifact)?;
        let manifest = self.manifest_path(&artifact);
        validate_owned_path(&self.root, &manifest)?;
        remove_if_exists(&manifest)
    }

    /// Rebuild the index from the published artifacts. Full verification is
    /// used for normal reads; startup uses the structural variant below.
    fn reconcile_index(&self, world_id: minedock_core::WorldId) -> Result<()> {
        self.reconcile_index_with_verification(world_id, true)
    }

    fn reconcile_index_structural(&self, world_id: minedock_core::WorldId) -> Result<()> {
        self.reconcile_index_with_verification(world_id, false)
    }

    fn reconcile_index_with_verification(
        &self,
        world_id: minedock_core::WorldId,
        verify_content: bool,
    ) -> Result<()> {
        let backup_root = self.root.join(BACKUPS_DIR).join(world_id.to_string());
        if !backup_root.exists() {
            return Ok(());
        }
        validate_owned_path(&self.root, &backup_root)?;
        let records = self.scan_published_artifacts(world_id, &backup_root, verify_content)?;
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
        verify_content: bool,
    ) -> Result<Vec<BackupRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(backup_root).map_err(io_backup)? {
            let entry = entry.map_err(io_backup)?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == INDEX_FILE
                || name == RECOVERY_FILE
                || name.ends_with(TEMP_SUFFIX)
                || name.ends_with(MANIFEST_EXTENSION)
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(io_backup)?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(MineDockError::Backup(
                    "backup catalog contains a symlink or reparse point".into(),
                ));
            }
            if metadata.is_dir() {
                // Preserve directory snapshots written by the pre-archive
                // development build, but do not expose them as current
                // verified backups. A future migration/restore flow can
                // handle them explicitly without making startup destructive.
                let legacy_manifest = path.join(LEGACY_MANIFEST_FILE);
                if fs::symlink_metadata(&legacy_manifest).is_ok_and(|metadata| metadata.is_file()) {
                    continue;
                }
            }
            if !metadata.is_file() || !name.ends_with(&format!(".{ARTIFACT_EXTENSION}")) {
                return Err(MineDockError::Backup(
                    "backup catalog contains an unsupported entry".into(),
                ));
            }
            let manifest_path = self.manifest_path(&path);
            validate_owned_path(&self.root, &manifest_path)?;
            let bytes = fs::read(&manifest_path).map_err(io_backup)?;
            let manifest: BackupManifest = serde_json::from_slice(&bytes).map_err(|error| {
                MineDockError::Backup(format!("backup manifest is corrupt: {error}"))
            })?;
            manifest.validate()?;
            if manifest.world_id != world_id
                || name != format!("{}.{ARTIFACT_EXTENSION}", manifest.backup_id)
            {
                return Err(MineDockError::Backup(
                    "backup artifact identity does not match its manifest".into(),
                ));
            }
            let mut archive =
                ZipArchive::new(File::open(&path).map_err(io_backup)?).map_err(zip_backup)?;
            validate_archive_structure(&mut archive, &manifest)?;
            if verify_content {
                verify_archive_content(&path, &mut archive, &manifest)?;
            }
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

fn copy_into_archive<R: Read, W: Write>(input: &mut R, output: &mut W) -> Result<(u64, String)> {
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
    Ok((size, format_digest(digest.finalize().as_slice())))
}

fn validate_archive_structure(
    archive: &mut ZipArchive<File>,
    manifest: &BackupManifest,
) -> Result<()> {
    let expected: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    let mut found = BTreeSet::new();
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(zip_backup)?;
        let name = file.name().to_owned();
        validate_relative_file(&name)?;
        if file.is_dir() {
            return Err(MineDockError::Backup(
                "backup archive contains a directory entry".into(),
            ));
        }
        let Some(entry) = manifest.files.iter().find(|entry| entry.path == name) else {
            return Err(MineDockError::Backup(
                "backup archive contains an unlisted file".into(),
            ));
        };
        if file.size() != entry.size {
            return Err(MineDockError::Backup(
                "backup archive entry size does not match its manifest".into(),
            ));
        }
        found.insert(name);
    }
    if found.len() != expected.len() || found != expected {
        return Err(MineDockError::Backup(
            "backup archive entries do not match its manifest".into(),
        ));
    }
    Ok(())
}

fn verify_archive_content(
    artifact: &Path,
    archive: &mut ZipArchive<File>,
    manifest: &BackupManifest,
) -> Result<()> {
    let (archive_bytes, archive_sha256) = hash_file(artifact)?;
    if archive_bytes != manifest.archive_bytes || archive_sha256 != manifest.archive_sha256 {
        return Err(MineDockError::Backup(
            "backup archive does not match its manifest digest".into(),
        ));
    }
    for entry in &manifest.files {
        let mut file = archive.by_name(&entry.path).map_err(zip_backup)?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(io_backup)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            size = size
                .checked_add(count as u64)
                .ok_or_else(|| MineDockError::Backup("backup file size overflowed".into()))?;
        }
        if size != entry.size || format_digest(digest.finalize().as_slice()) != entry.sha256 {
            return Err(MineDockError::Backup(
                "backup archive file does not match its manifest".into(),
            ));
        }
    }
    Ok(())
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
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| MineDockError::Backup("backup file size overflowed".into()))?;
    }
    Ok((size, format_digest(digest.finalize().as_slice())))
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

fn remove_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        remove_owned_path(path)?;
    }
    Ok(())
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

fn zip_backup(error: zip::result::ZipError) -> MineDockError {
    MineDockError::Backup(format!("backup archive error: {error}"))
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
    fn stopped_world_archive_excludes_jars_logs_and_previous_backups() {
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
        assert_eq!(record.manifest.minecraft_version, value.server.version);
        assert!(record.manifest.archive_bytes > 0);
        assert_eq!(record.manifest.archive_sha256.len(), 64);
        assert!(root.path().join(&record.artifact_path).is_file());
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
        let artifact = root.path().join(&record.artifact_path);
        let index = artifact.with_file_name(INDEX_FILE);
        fs::write(&index, b"not-json").expect("corrupt index");

        assert_eq!(
            engine.latest(value.id).expect("reconcile").as_ref(),
            Some(&record)
        );

        fs::remove_file(&artifact).expect("remove artifact");
        fs::remove_file(artifact.with_extension(MANIFEST_EXTENSION)).expect("remove manifest");
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

    #[test]
    fn startup_reconcile_checks_structure_without_hashing_archive_payload() {
        let root = TempDir::new().expect("root");
        let mut value = world();
        value.status = WorldStatus::Stopped;
        let source = root.path().join(&value.data_path).join("world");
        fs::create_dir_all(&source).expect("world");
        fs::write(source.join("level.dat"), b"save").expect("save");
        let engine = SafeBackupEngine::new(root.path());
        let record = engine.create(&value, BackupReason::Manual).expect("backup");

        let artifact = root.path().join(&record.artifact_path);
        let output = File::create(&artifact).expect("tampered archive");
        let mut archive = ZipWriter::new(output);
        archive
            .start_file(
                "world/level.dat",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .expect("archive entry");
        archive.write_all(b"evil").expect("tampered payload");
        let mut output = archive.finish().expect("finish archive");
        output.flush().expect("flush archive");

        engine
            .recover_stale_partials()
            .expect("structural recovery");
        assert!(engine.list(value.id).is_err());
    }
}
