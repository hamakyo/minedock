//! App-side lifecycle persistence and ownership adapters.

use atomic_write_file::AtomicWriteFile;
use minedock_core::{
    JsonWorldRepository, LifecycleLeaseProvider, LifecyclePersistence, MineDockError, Result,
    SessionId, WorldId, WorldRepository, WorldStatus,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A process-lifetime exclusive lease. On Windows the OS lock is held by the
/// File handle and is released automatically when the process crashes; the
/// marker file is never used as ownership state.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AppDataLease {
    path: PathBuf,
    file: Arc<File>,
}

#[allow(dead_code)]
impl AppDataLease {
    pub fn acquire(app_data_root: &Path) -> Result<Self> {
        // Validate the nearest existing ancestor before creating anything.
        // Otherwise a junction inserted above a missing root could redirect
        // `create_dir_all` outside MineDock's trusted app-data tree.
        validate_safe_path(app_data_root).map_err(|error| {
            MineDockError::Persistence(format!("app-data root is unsafe: {error}"))
        })?;
        fs::create_dir_all(app_data_root).map_err(|error| {
            MineDockError::Persistence(format!("could not create app-data root: {error}"))
        })?;
        validate_safe_path(app_data_root).map_err(|error| {
            MineDockError::Persistence(format!("app-data root is unsafe: {error}"))
        })?;
        let path = app_data_root.join("minedock.lifecycle.lease");
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(MineDockError::InvalidState(
                    "lifecycle lease path is a symlink or reparse point".into(),
                ));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                MineDockError::InvalidState(format!(
                    "could not open app-data lifecycle lease: {error}"
                ))
            })?;
        lock_file(&file).map_err(|error| {
            MineDockError::InvalidState(format!(
                "could not acquire app-data lifecycle lease: {error}"
            ))
        })?;
        Ok(Self {
            path,
            file: Arc::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_held(&self) -> bool {
        self.file.metadata().is_ok()
    }
}

#[derive(Debug, Default, Clone)]
pub struct AppDataLeaseProvider {
    held: Option<AppDataLease>,
}

impl AppDataLeaseProvider {
    pub fn with_lease(lease: AppDataLease) -> Self {
        Self { held: Some(lease) }
    }
}

impl LifecycleLeaseProvider for AppDataLeaseProvider {
    type Lease = AppDataLease;

    fn acquire(&mut self, app_data_root: &Path) -> Result<Self::Lease> {
        if let Some(lease) = &self.held {
            return Ok(lease.clone());
        }
        AppDataLease::acquire(app_data_root)
    }
}

#[cfg(windows)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped = OVERLAPPED::default();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, unix)))]
fn lock_file(_: &File) -> std::io::Result<()> {
    Ok(())
}

const SESSION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecord {
    schema_version: u16,
    world_id: WorldId,
    session_id: Option<String>,
    status: WorldStatus,
    pid: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct JsonLifecyclePersistence {
    repository: JsonWorldRepository,
}

impl JsonLifecyclePersistence {
    pub fn new(repository: JsonWorldRepository) -> Self {
        Self { repository }
    }

    fn session_path(&self, world_id: WorldId) -> PathBuf {
        self.repository
            .root()
            .join("worlds")
            .join(world_id.to_string())
            .join("server")
            .join("session.json")
    }

    fn write_record(
        &self,
        world_id: WorldId,
        status: WorldStatus,
        pid: Option<u32>,
        session_id: Option<SessionId>,
    ) -> Result<()> {
        let path = self.session_path(world_id);
        validate_safe_path(&path).map_err(|error| {
            MineDockError::Persistence(format!("session path is unsafe: {error}"))
        })?;
        if path.exists() {
            let existing = fs::read(&path).map_err(|error| {
                MineDockError::Persistence(format!(
                    "could not inspect existing session record: {error}"
                ))
            })?;
            let existing: SessionRecord = serde_json::from_slice(&existing).map_err(|error| {
                MineDockError::Persistence(format!(
                    "refusing to overwrite an unowned session record: {error}"
                ))
            })?;
            if existing.schema_version != SESSION_SCHEMA_VERSION || existing.world_id != world_id {
                return Err(MineDockError::Persistence(
                    "refusing to overwrite a session record owned by another schema/world".into(),
                ));
            }
        }
        // World.status is the UI-facing projection. Save it first only after
        // the session path is known to be MineDock-owned; if this later
        // session write is interrupted, startup recovery sees the active
        // world and converges both records to Failed.
        self.update_world_status(world_id, status)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                MineDockError::Persistence(format!("could not create session directory: {error}"))
            })?;
        }
        let record = SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            world_id,
            session_id: session_id.map(|value| value.to_string()),
            status,
            pid,
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        let mut file = AtomicWriteFile::open(&path)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        file.write_all(&bytes)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        file.flush()
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        file.commit()
            .map_err(|error| MineDockError::Persistence(error.to_string()))
    }

    fn update_world_status(&self, world_id: WorldId, status: WorldStatus) -> Result<()> {
        validate_safe_path(self.repository.root()).map_err(|error| {
            MineDockError::Persistence(format!("app-data root is unsafe: {error}"))
        })?;
        let mut metadata = self.repository.load()?;
        let world = metadata
            .worlds
            .iter_mut()
            .find(|world| world.id == world_id)
            .ok_or_else(|| {
                MineDockError::Persistence(format!(
                    "cannot persist lifecycle for unknown world {world_id}"
                ))
            })?;
        world.status = status;
        self.repository.save(&metadata)
    }

    fn read_record(&self, world_id: WorldId) -> Result<SessionRecord> {
        let path = self.session_path(world_id);
        validate_safe_path(&path).map_err(|error| {
            MineDockError::Persistence(format!("session path is unsafe: {error}"))
        })?;
        let bytes = fs::read(&path).map_err(|error| {
            MineDockError::Persistence(format!("could not read session record: {error}"))
        })?;
        let record: SessionRecord = serde_json::from_slice(&bytes).map_err(|error| {
            MineDockError::Persistence(format!("session record is corrupt: {error}"))
        })?;
        if record.schema_version != SESSION_SCHEMA_VERSION || record.world_id != world_id {
            return Err(MineDockError::Persistence(
                "session record identity or schema is invalid".into(),
            ));
        }
        Ok(record)
    }

    /// Under the app-data lease, active persisted states are stale after a
    /// crash and become Failed. The repository save is atomic and happens
    /// before the returned in-memory library is loaded.
    pub fn recover_startup(repository: &JsonWorldRepository) -> Result<Vec<WorldId>> {
        let mut metadata = repository.load()?;
        let mut recovered = Vec::new();
        for world in &mut metadata.worlds {
            if matches!(
                world.status,
                WorldStatus::Preparing
                    | WorldStatus::Starting
                    | WorldStatus::Running
                    | WorldStatus::Stopping
                    | WorldStatus::BackingUp
            ) {
                world.status = WorldStatus::Failed;
                recovered.push(world.id);
            }
            let session_path = repository
                .root()
                .join("worlds")
                .join(world.id.to_string())
                .join("server")
                .join("session.json");
            validate_safe_path(&session_path).map_err(|error| {
                MineDockError::Persistence(format!("startup session path is unsafe: {error}"))
            })?;
            if session_path.exists() {
                let bytes = fs::read(&session_path).map_err(|error| {
                    MineDockError::Persistence(format!(
                        "could not read startup session record: {error}"
                    ))
                })?;
                let session: SessionRecord = serde_json::from_slice(&bytes).map_err(|error| {
                    MineDockError::Persistence(format!(
                        "startup session record is corrupt: {error}"
                    ))
                })?;
                if session.schema_version != SESSION_SCHEMA_VERSION || session.world_id != world.id
                {
                    return Err(MineDockError::Persistence(
                        "startup session record identity or schema is invalid".into(),
                    ));
                }
                if matches!(
                    session.status,
                    WorldStatus::Preparing
                        | WorldStatus::Starting
                        | WorldStatus::Running
                        | WorldStatus::Stopping
                        | WorldStatus::BackingUp
                ) && !recovered.contains(&world.id)
                {
                    world.status = WorldStatus::Failed;
                    recovered.push(world.id);
                }
            }
        }
        if !recovered.is_empty() {
            repository.save(&metadata)?;
            let persistence = Self::new(repository.clone());
            for world_id in &recovered {
                persistence.write_record(*world_id, WorldStatus::Failed, None, None)?;
            }
        }
        Ok(recovered)
    }
}

fn validate_safe_path(path: &Path) -> std::io::Result<()> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path contains a symlink or reparse point",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
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

impl LifecyclePersistence for JsonLifecyclePersistence {
    fn persist_status(&mut self, world_id: WorldId, status: WorldStatus) -> Result<()> {
        self.write_record(world_id, status, None, None)
    }

    fn persist_session(
        &mut self,
        world_id: WorldId,
        status: WorldStatus,
        pid: Option<u32>,
        session_id: Option<SessionId>,
    ) -> Result<()> {
        self.write_record(world_id, status, pid, session_id)
    }

    fn persist_process_id(&mut self, world_id: WorldId, pid: u32) -> Result<()> {
        self.write_record(world_id, WorldStatus::Starting, Some(pid), None)
    }

    fn clear_process_id(&mut self, world_id: WorldId) -> Result<()> {
        let existing = self.read_record(world_id)?;
        self.write_record(world_id, existing.status, None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minedock_core::{CreateWorldRequest, TemplateCatalog, WorldRepository};
    use tempfile::TempDir;

    fn sample_world() -> minedock_core::World {
        let catalog = TemplateCatalog::built_in().expect("catalog");
        CreateWorldRequest::new("Lifecycle", "creative")
            .expect("request")
            .build_world(&catalog)
            .expect("world")
    }

    #[test]
    fn session_updates_world_projection_and_startup_reconciles_active_state() {
        let root = TempDir::new().expect("root");
        let repository = JsonWorldRepository::new(root.path());
        let world = sample_world();
        repository
            .save(&minedock_core::AppMetadata::new(vec![world.clone()]))
            .expect("metadata");
        let mut persistence = JsonLifecyclePersistence::new(repository.clone());
        let session_id = SessionId::new();
        persistence
            .persist_session(world.id, WorldStatus::Running, Some(42), Some(session_id))
            .expect("running");
        assert_eq!(
            repository.load().expect("load").worlds[0].status,
            WorldStatus::Running
        );
        let recovered = JsonLifecyclePersistence::recover_startup(&repository).expect("recovery");
        assert_eq!(recovered, vec![world.id]);
        assert_eq!(
            repository.load().expect("reloaded").worlds[0].status,
            WorldStatus::Failed
        );
        let session = persistence.read_record(world.id).expect("session");
        assert_eq!(session.status, WorldStatus::Failed);
        assert_eq!(session.pid, None);
    }
}
