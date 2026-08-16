use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{
    CreateWorldRequest, MineDockError, Result, TemplateCatalog, World, WorldId,
    validate_world_data_path,
};

pub const APP_METADATA_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppMetadata {
    pub schema_version: u16,
    pub worlds: Vec<World>,
}

impl Default for AppMetadata {
    fn default() -> Self {
        Self {
            schema_version: APP_METADATA_SCHEMA_VERSION,
            worlds: Vec::new(),
        }
    }
}

impl AppMetadata {
    pub fn new(worlds: Vec<World>) -> Self {
        Self {
            schema_version: APP_METADATA_SCHEMA_VERSION,
            worlds,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != APP_METADATA_SCHEMA_VERSION {
            return Err(MineDockError::Persistence(format!(
                "unsupported MineDock metadata schema version {} (expected {})",
                self.schema_version, APP_METADATA_SCHEMA_VERSION
            )));
        }

        let mut ids = std::collections::HashSet::with_capacity(self.worlds.len());
        let mut profile_ids = std::collections::HashSet::with_capacity(self.worlds.len());
        let mut data_paths = std::collections::HashSet::with_capacity(self.worlds.len());
        for world in &self.worlds {
            world.validate()?;
            if !ids.insert(world.id) {
                return Err(MineDockError::Persistence(format!(
                    "metadata contains duplicate world id {}",
                    world.id
                )));
            }
            if !profile_ids.insert(world.server_profile_id) {
                return Err(MineDockError::Persistence(format!(
                    "metadata contains duplicate server profile id {}",
                    world.server_profile_id
                )));
            }
            if !data_paths.insert(world.data_path.clone()) {
                return Err(MineDockError::Persistence(format!(
                    "metadata contains duplicate world data path {}",
                    world.data_path.display()
                )));
            }
        }
        Ok(())
    }
}

/// Persistence boundary for world metadata.
pub trait WorldRepository {
    fn load(&self) -> Result<AppMetadata>;
    fn save(&self, metadata: &AppMetadata) -> Result<()>;

    fn load_worlds(&self) -> Result<Vec<World>> {
        Ok(self.load()?.worlds)
    }

    fn save_worlds(&self, worlds: &[World]) -> Result<()> {
        self.save(&AppMetadata {
            schema_version: APP_METADATA_SCHEMA_VERSION,
            worlds: worlds.to_vec(),
        })
    }
}

/// JSON metadata repository rooted at the app-data directory.
#[derive(Debug, Clone)]
pub struct JsonWorldRepository {
    root: PathBuf,
    metadata_path: PathBuf,
}

impl JsonWorldRepository {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            metadata_path: root.join("minedock.json"),
            root,
        }
    }

    pub fn with_metadata_path(root: impl Into<PathBuf>, metadata_path: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let metadata_path = metadata_path.into();
        let metadata_path = if metadata_path.is_absolute() {
            metadata_path
        } else {
            root.join(metadata_path)
        };
        Self {
            root,
            metadata_path,
        }
    }

    pub fn from_metadata_path(metadata_path: impl Into<PathBuf>) -> Self {
        let metadata_path = metadata_path.into();
        let root = metadata_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Self {
            root,
            metadata_path,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    pub fn load_metadata(&self) -> Result<AppMetadata> {
        self.load()
    }

    pub fn save_metadata(&self, metadata: &AppMetadata) -> Result<()> {
        self.save(metadata)
    }

    fn validate_rooted_paths(&self, metadata: &AppMetadata) -> Result<()> {
        metadata.validate()?;
        for world in &metadata.worlds {
            validate_world_data_path(&world.data_path, world.id)?;
            // Keep this check explicit at the persistence boundary: paths in
            // metadata are relative, and are never allowed to escape root.
            let joined = self.root.join(&world.data_path);
            if !joined.starts_with(&self.root) {
                return Err(MineDockError::Persistence(format!(
                    "world {} data path escapes app-data root",
                    world.id
                )));
            }
        }
        Ok(())
    }
}

pub type WorldLibraryService<R> = WorldLibrary<R>;

impl WorldRepository for JsonWorldRepository {
    fn load(&self) -> Result<AppMetadata> {
        let bytes = match fs::read(&self.metadata_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppMetadata::default());
            }
            Err(error) => {
                return Err(MineDockError::Persistence(format!(
                    "could not read metadata {}: {error}",
                    self.metadata_path.display()
                )));
            }
        };

        let metadata: AppMetadata = serde_json::from_slice(&bytes).map_err(|error| {
            MineDockError::Persistence(format!(
                "metadata {} is corrupt JSON: {error}",
                self.metadata_path.display()
            ))
        })?;
        self.validate_rooted_paths(&metadata)?;
        Ok(metadata)
    }

    fn save(&self, metadata: &AppMetadata) -> Result<()> {
        self.validate_rooted_paths(metadata)?;
        fs::create_dir_all(&self.root).map_err(|error| {
            MineDockError::Persistence(format!(
                "could not create app-data root {}: {error}",
                self.root.display()
            ))
        })?;
        if let Some(parent) = self.metadata_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                MineDockError::Persistence(format!(
                    "could not create metadata directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        let bytes = serde_json::to_vec_pretty(metadata).map_err(|error| {
            MineDockError::Persistence(format!("could not serialize metadata: {error}"))
        })?;
        let mut file = AtomicWriteFile::open(&self.metadata_path).map_err(|error| {
            MineDockError::Persistence(format!(
                "could not open atomic metadata writer {}: {error}",
                self.metadata_path.display()
            ))
        })?;
        file.write_all(&bytes).map_err(|error| {
            MineDockError::Persistence(format!(
                "could not write metadata {}: {error}",
                self.metadata_path.display()
            ))
        })?;
        file.flush().map_err(|error| {
            MineDockError::Persistence(format!(
                "could not flush metadata {}: {error}",
                self.metadata_path.display()
            ))
        })?;
        file.commit().map_err(|error| {
            MineDockError::Persistence(format!(
                "could not atomically replace metadata {}: {error}",
                self.metadata_path.display()
            ))
        })?;
        Ok(())
    }
}

/// In-memory projection of persisted worlds. A candidate is saved before this
/// projection changes, so a failed write cannot create phantom worlds.
#[derive(Debug)]
pub struct WorldLibrary<R> {
    repository: R,
    worlds: Vec<World>,
}

impl<R: WorldRepository> WorldLibrary<R> {
    pub fn new(repository: R) -> Self {
        Self {
            repository,
            worlds: Vec::new(),
        }
    }

    pub fn repository(&self) -> &R {
        &self.repository
    }

    pub fn load(&mut self) -> Result<&[World]> {
        let metadata = self.repository.load()?;
        // Assignment happens only after the repository has loaded and
        // validated the complete candidate.
        self.worlds = metadata.worlds;
        Ok(&self.worlds)
    }

    pub fn reload(&mut self) -> Result<&[World]> {
        self.load()
    }

    pub fn list(&self) -> &[World] {
        &self.worlds
    }

    pub fn worlds(&self) -> &[World] {
        &self.worlds
    }

    pub fn get(&self, id: WorldId) -> Option<&World> {
        self.worlds.iter().find(|world| world.id == id)
    }

    pub fn create(&mut self, world: World) -> Result<World> {
        world.validate()?;
        if self.worlds.iter().any(|existing| existing.id == world.id) {
            return Err(MineDockError::Persistence(format!(
                "world id {} already exists",
                world.id
            )));
        }
        let mut candidate = self.worlds.clone();
        candidate.push(world.clone());
        self.repository.save_worlds(&candidate)?;
        self.worlds = candidate;
        Ok(world)
    }

    pub fn create_from_template(
        &mut self,
        request: &CreateWorldRequest,
        catalog: &TemplateCatalog,
    ) -> Result<World> {
        let world = request.build_world(catalog)?;
        self.create(world)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateWorldRequest, TemplateCatalog, WorldStatus};
    use std::cell::Cell;
    use tempfile::TempDir;

    fn sample_world(name: &str) -> World {
        let catalog = TemplateCatalog::built_in().expect("built-ins");
        let request = CreateWorldRequest::new(name, "hardcore").expect("request");
        request.build_world(&catalog).expect("world")
    }

    #[test]
    fn missing_metadata_loads_empty() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        assert_eq!(
            repository.load().expect("missing metadata").worlds,
            Vec::new()
        );
    }

    #[test]
    fn metadata_round_trip_is_exact() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        let metadata = AppMetadata {
            schema_version: APP_METADATA_SCHEMA_VERSION,
            worlds: vec![sample_world("Round Trip")],
        };
        repository.save(&metadata).expect("save");
        assert_eq!(repository.load().expect("load"), metadata);
    }

    #[test]
    fn second_save_replaces_first_and_leaves_valid_json() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        repository
            .save(&AppMetadata {
                schema_version: APP_METADATA_SCHEMA_VERSION,
                worlds: vec![sample_world("First")],
            })
            .expect("first save");
        let first_bytes = std::fs::read(repository.metadata_path()).expect("first bytes");
        repository
            .save(&AppMetadata {
                schema_version: APP_METADATA_SCHEMA_VERSION,
                worlds: vec![sample_world("Second")],
            })
            .expect("second save");
        let second_bytes = std::fs::read(repository.metadata_path()).expect("second bytes");
        assert_ne!(first_bytes, second_bytes);
        assert_eq!(repository.load().expect("load").worlds[0].name, "Second");
    }

    #[test]
    fn corrupt_and_unsupported_metadata_are_errors_without_overwrite() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        let corrupt = br#"{"schema_version":1,"worlds":["not-a-world"]}"#;
        std::fs::write(repository.metadata_path(), corrupt).expect("write corrupt");
        assert!(repository.load().is_err());
        assert_eq!(
            std::fs::read(repository.metadata_path()).expect("corrupt bytes"),
            corrupt
        );

        let unsupported = br#"{"schema_version":99,"worlds":[]}"#;
        std::fs::write(repository.metadata_path(), unsupported).expect("write unsupported");
        assert!(repository.load().is_err());
        assert_eq!(
            std::fs::read(repository.metadata_path()).expect("unsupported bytes"),
            unsupported
        );
    }

    #[test]
    fn absolute_and_parent_data_paths_are_rejected() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        let mut world = sample_world("Path Check");
        world.data_path = PathBuf::from("../outside");
        let metadata = AppMetadata {
            schema_version: APP_METADATA_SCHEMA_VERSION,
            worlds: vec![world],
        };
        assert!(repository.save(&metadata).is_err());
    }

    #[test]
    fn metadata_rejects_duplicate_profiles_and_data_paths() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        let first = sample_world("First");

        let mut duplicate_profile = sample_world("Second");
        duplicate_profile.server_profile_id = first.server_profile_id;
        duplicate_profile.server.id = first.server.id;
        assert!(
            repository
                .save(&AppMetadata::new(vec![first.clone(), duplicate_profile]))
                .is_err()
        );

        let mut duplicate_path = sample_world("Third");
        duplicate_path.data_path = first.data_path.clone();
        assert!(
            repository
                .save(&AppMetadata::new(vec![first, duplicate_path]))
                .is_err()
        );
    }

    #[test]
    fn metadata_rejects_root_extra_and_mismatched_world_paths() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());

        let valid_world = sample_world("Malformed dot path");
        let valid_id = valid_world.id;
        let mut dot_path_world = valid_world;
        dot_path_world.data_path = PathBuf::from(format!("worlds/./{valid_id}"));
        assert!(
            repository
                .save(&AppMetadata::new(vec![dot_path_world]))
                .is_err()
        );

        for path in [
            PathBuf::from("."),
            PathBuf::from("worlds"),
            PathBuf::from("worlds/extra/components"),
            PathBuf::from("worlds/not-the-world-id"),
        ] {
            let mut world = sample_world("Malformed path");
            world.data_path = path;
            assert!(
                repository.save(&AppMetadata::new(vec![world])).is_err(),
                "path should be rejected"
            );
        }
    }

    struct FailingRepository {
        saves: Cell<u32>,
    }

    impl WorldRepository for FailingRepository {
        fn load(&self) -> Result<AppMetadata> {
            Ok(AppMetadata::default())
        }

        fn save(&self, _: &AppMetadata) -> Result<()> {
            self.saves.set(self.saves.get() + 1);
            Err(MineDockError::Persistence("simulated disk failure".into()))
        }
    }

    #[test]
    fn save_failure_does_not_mutate_library_memory() {
        let repository = FailingRepository {
            saves: Cell::new(0),
        };
        let mut library = WorldLibrary::new(repository);
        library.load().expect("empty load");
        let result = library.create(sample_world("Should Not Persist"));
        assert!(result.is_err());
        assert!(library.list().is_empty());
        assert_eq!(library.repository().saves.get(), 1);
    }

    #[test]
    fn created_world_survives_reopen_and_stays_stopped() {
        let root = TempDir::new().expect("temp root");
        let repository = JsonWorldRepository::new(root.path());
        let mut library = WorldLibrary::new(repository.clone());
        library.load().expect("empty load");
        let catalog = TemplateCatalog::built_in().expect("built-ins");
        let request = CreateWorldRequest::new("Persistent", "creative").expect("request");
        let world = library
            .create_from_template(&request, &catalog)
            .expect("create");
        assert_eq!(world.status, WorldStatus::Stopped);
        assert!(world.data_path.is_relative());

        let mut reopened = WorldLibrary::new(repository);
        reopened.load().expect("reopen");
        assert_eq!(reopened.list(), &[world]);
    }
}
