use crate::minecraft::generate_server_properties;
use crate::{
    MineDockError, MinecraftEdition, MinecraftVersionId, MinecraftVersionSelector, Result,
    ServerDistribution, World,
};
use atomic_write_file::AtomicWriteFile;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
pub const OFFICIAL_EULA_URL: &str = "https://www.minecraft.net/en-us/eula";
pub const MOJANG_VERSION_MANIFEST_URL: &str = VERSION_MANIFEST_URL;
pub const MINECRAFT_EULA_URL: &str = OFFICIAL_EULA_URL;
pub const METADATA_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_REDIRECT_HOPS: usize = 8;
pub const EULA_RECORD_SCHEMA_VERSION: u16 = 1;
pub const PROVISION_RECORD_SCHEMA_VERSION: u16 = 1;
const INCOMPLETE_PROVISION_MARKER: &str = ".minedock-provision.incomplete";

const METADATA_AUTHORITIES: &[&str] = &["piston-meta.mojang.com", "launchermeta.mojang.com"];
const ARTIFACT_AUTHORITIES: &[&str] = &["piston-data.mojang.com", "launcher.mojang.com"];

/// Filesystem safety is an app adapter concern because Windows reparse-point
/// inspection requires OS APIs. Core supplies a portable symlink/type
/// baseline and accepts a stricter adapter for production paths.
pub trait PathSafety: Clone + Send + Sync + 'static {
    fn validate_existing_ancestors(&self, path: &Path) -> Result<()>;
    fn reject_path_type(&self, path: &Path, directory: bool) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PortablePathSafety;

impl PathSafety for PortablePathSafety {
    fn validate_existing_ancestors(&self, path: &Path) -> Result<()> {
        portable_validate_existing_ancestors(path)
    }

    fn reject_path_type(&self, path: &Path, directory: bool) -> Result<()> {
        portable_reject_path_type(path, directory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponse {
    pub status: u16,
    pub requested_url: String,
    pub final_url: String,
    pub redirects: Vec<String>,
    pub body: Vec<u8>,
}

impl TransportResponse {
    pub fn new(status: u16, url: impl Into<String>, body: Vec<u8>) -> Self {
        let url = url.into();
        Self {
            status,
            requested_url: url.clone(),
            final_url: url,
            redirects: Vec::new(),
            body,
        }
    }
}

/// Network is deliberately behind this narrow boundary.  Production can
/// provide a bounded HTTPS implementation; tests use a keyed fake transport
/// and therefore never contact Mojang.
pub trait AuthoritativeTransport {
    fn get(&self, url: &str, max_bytes: usize) -> Result<TransportResponse>;
}

pub use AuthoritativeTransport as HttpTransport;

/// Distribution-provider boundary.  Implementations must resolve only
/// authoritative metadata and return a verified artifact descriptor; callers
/// cannot provide an arbitrary base URL through a template.
pub trait ServerDistributionProvider {
    fn resolve_vanilla_version(
        &self,
        selector: &MinecraftVersionSelector,
    ) -> Result<ResolvedVanillaVersion>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoNetworkTransport;

impl AuthoritativeTransport for NoNetworkTransport {
    fn get(&self, _: &str, _: usize) -> Result<TransportResponse> {
        Err(MineDockError::Download(
            "no production HTTPS transport is configured".into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

impl ArtifactDescriptor {
    pub fn new(url: impl Into<String>, sha1: impl Into<String>, size: u64) -> Result<Self> {
        let descriptor = Self {
            url: url.into(),
            sha1: sha1.into(),
            size,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<()> {
        validate_authoritative_url(&self.url, ARTIFACT_AUTHORITIES)?;
        validate_sha1(&self.sha1)?;
        if self.size == 0 {
            return Err(MineDockError::Verification(
                "server artifact size must be nonzero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedVanillaVersion {
    pub id: MinecraftVersionId,
    #[serde(rename = "type")]
    pub version_type: String,
    pub java_requirement: crate::JavaRequirement,
    pub artifact: ArtifactDescriptor,
}

impl ResolvedVanillaVersion {
    pub fn validate(&self) -> Result<()> {
        if self.version_type != "release" {
            return Err(MineDockError::Download(format!(
                "unsupported Minecraft version type {}",
                self.version_type
            )));
        }
        self.artifact.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EulaNotice {
    pub official_url: String,
    pub edition: MinecraftEdition,
}

impl Default for EulaNotice {
    fn default() -> Self {
        Self {
            official_url: OFFICIAL_EULA_URL.into(),
            edition: MinecraftEdition::Java,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EulaAcceptance {
    pub schema_version: u16,
    pub accepted_at: DateTime<Utc>,
    pub official_url: String,
    pub edition: MinecraftEdition,
}

impl EulaAcceptance {
    pub fn new(now: DateTime<Utc>) -> Result<Self> {
        let acceptance = Self {
            schema_version: EULA_RECORD_SCHEMA_VERSION,
            accepted_at: now,
            official_url: OFFICIAL_EULA_URL.into(),
            edition: MinecraftEdition::Java,
        };
        acceptance.validate()?;
        Ok(acceptance)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != EULA_RECORD_SCHEMA_VERSION
            || self.official_url != OFFICIAL_EULA_URL
            || self.edition != MinecraftEdition::Java
        {
            return Err(MineDockError::Persistence(
                "invalid MineDock EULA acceptance record".into(),
            ));
        }
        Ok(())
    }
}

pub trait EulaAcceptanceRepository {
    fn load(&self) -> Result<Option<EulaAcceptance>>;
    fn save(&self, acceptance: &EulaAcceptance) -> Result<()>;

    fn is_accepted(&self) -> Result<bool> {
        Ok(self.load()?.is_some())
    }

    /// A false/cancelled response intentionally has no persistence side
    /// effect.  Callers must pass true only after an explicit affirmative UI
    /// action.
    fn record_explicit_acceptance(&self, accepted: bool) -> Result<Option<EulaAcceptance>> {
        if !accepted {
            return Ok(None);
        }
        let acceptance = EulaAcceptance::new(Utc::now())?;
        self.save(&acceptance)?;
        Ok(Some(acceptance))
    }
}

pub use EulaAcceptanceRepository as EulaRepository;

#[derive(Debug, Clone)]
pub struct FileEulaAcceptanceRepository {
    path: PathBuf,
}

pub type FileEulaRepository = FileEulaAcceptanceRepository;

impl FileEulaAcceptanceRepository {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl EulaAcceptanceRepository for FileEulaAcceptanceRepository {
    fn load(&self) -> Result<Option<EulaAcceptance>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(MineDockError::Persistence(format!(
                    "could not read EULA record: {error}"
                )));
            }
        };
        let acceptance: EulaAcceptance = serde_json::from_slice(&bytes).map_err(|error| {
            MineDockError::Persistence(format!("EULA record is corrupt: {error}"))
        })?;
        acceptance.validate()?;
        Ok(Some(acceptance))
    }

    fn save(&self, acceptance: &EulaAcceptance) -> Result<()> {
        acceptance.validate()?;
        if let Some(parent) = self.path.parent() {
            validate_existing_ancestors(parent)?;
            fs::create_dir_all(parent).map_err(|error| {
                MineDockError::Persistence(format!("could not create EULA directory: {error}"))
            })?;
            validate_existing_ancestors(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(acceptance).map_err(|error| {
            MineDockError::Persistence(format!("could not serialize EULA record: {error}"))
        })?;
        atomic_replace(&self.path, &bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRecord {
    pub schema_version: u16,
    /// Optional for reading early schema-v1 records. New records always pin
    /// the world identity so a copied provision file cannot launch another
    /// world's artifact.
    #[serde(default)]
    pub world_id: Option<crate::WorldId>,
    pub resolved_version: MinecraftVersionId,
    pub java_major: crate::JavaMajor,
    pub artifact_sha1: String,
    pub artifact_size: u64,
    pub server_jar: String,
    pub world_directory: String,
    pub logs_directory: String,
    pub server_properties: String,
    /// SHA-1 attestation of the deterministic allowlisted properties file.
    /// `None` is accepted only for reading old schema-v1 records; launch
    /// validation requires a present, matching attestation.
    #[serde(default)]
    pub server_properties_sha1: Option<String>,
    pub eula_file: String,
    /// Path to the explicit affirmative MineDock acceptance copied into the
    /// world at provision time. It is intentionally distinct from eula.txt.
    #[serde(default)]
    pub eula_acceptance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IncompleteProvision {
    schema_version: u16,
    world_id: crate::WorldId,
    record: ProvisionRecord,
}

impl ProvisionRecord {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROVISION_RECORD_SCHEMA_VERSION {
            return Err(MineDockError::Persistence(
                "unsupported provision record schema".into(),
            ));
        }
        validate_sha1(&self.artifact_sha1)?;
        if self.artifact_size == 0 {
            return Err(MineDockError::Persistence(
                "provision artifact size must be nonzero".into(),
            ));
        }
        for path in [
            &self.server_jar,
            &self.world_directory,
            &self.logs_directory,
            &self.server_properties,
            &self.eula_file,
        ] {
            validate_relative_path(Path::new(path))?;
        }
        if let Some(path) = &self.eula_acceptance {
            validate_relative_path(Path::new(path))?;
        }
        if let Some(hash) = &self.server_properties_sha1 {
            validate_sha1(hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedServer {
    world_root: PathBuf,
    server_jar: PathBuf,
    world_directory: PathBuf,
    logs_directory: PathBuf,
    server_properties: PathBuf,
    eula_file: PathBuf,
    record: ProvisionRecord,
}

impl ProvisionedServer {
    pub fn world_root(&self) -> &Path {
        &self.world_root
    }

    pub fn server_jar(&self) -> &Path {
        &self.server_jar
    }

    pub fn world_directory(&self) -> &Path {
        &self.world_directory
    }

    pub fn logs_directory(&self) -> &Path {
        &self.logs_directory
    }

    pub fn server_properties(&self) -> &Path {
        &self.server_properties
    }

    pub fn eula_file(&self) -> &Path {
        &self.eula_file
    }

    pub fn record(&self) -> &ProvisionRecord {
        &self.record
    }
}

/// Launch-critical state loaded from the persisted provision transaction.
/// The fields are private so callers cannot construct a launchable state from
/// a path and a pre-existing eula.txt alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedProvision {
    world_root: PathBuf,
    server_jar: PathBuf,
    record: ProvisionRecord,
    world_id: crate::WorldId,
}

impl ValidatedProvision {
    pub fn world_root(&self) -> &Path {
        &self.world_root
    }

    pub fn server_jar(&self) -> &Path {
        &self.server_jar
    }

    pub fn world_id(&self) -> crate::WorldId {
        self.world_id
    }

    pub fn record(&self) -> &ProvisionRecord {
        &self.record
    }

    pub fn from_provisioned(
        provisioned: &ProvisionedServer,
        expected_world_id: crate::WorldId,
        runtime_major: crate::JavaMajor,
    ) -> Result<Self> {
        validate_launch_root(&provisioned.world_root)?;
        let provision_path = provisioned.world_root.join("server").join("provision.json");
        let record = read_provision_record(&provision_path)?;
        validate_persisted_provision(
            &provisioned.world_root,
            &record,
            expected_world_id,
            runtime_major,
        )?;
        let server_jar = provisioned.world_root.join(&record.server_jar);
        if server_jar != provisioned.server_jar {
            return Err(MineDockError::ProcessStart(
                "provisioned server path does not match persisted record".into(),
            ));
        }
        Ok(Self {
            world_root: provisioned.world_root.clone(),
            server_jar,
            record,
            world_id: expected_world_id,
        })
    }

    pub fn load_for_launch(
        world_root: impl Into<PathBuf>,
        expected_world_id: crate::WorldId,
        runtime_major: crate::JavaMajor,
    ) -> Result<Self> {
        let world_root = world_root.into();
        validate_launch_root(&world_root)?;
        let provision_path = world_root.join("server").join("provision.json");
        let record = read_provision_record(&provision_path)?;
        validate_persisted_provision(&world_root, &record, expected_world_id, runtime_major)?;
        let server_jar = world_root.join(&record.server_jar);
        Ok(Self {
            world_root,
            server_jar,
            record,
            world_id: expected_world_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct VanillaArtifactCache<S = PortablePathSafety> {
    root: PathBuf,
    max_bytes: u64,
    path_safety: S,
}

impl VanillaArtifactCache<PortablePathSafety> {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: DEFAULT_ARTIFACT_MAX_BYTES,
            path_safety: PortablePathSafety,
        }
    }
}

impl<S: PathSafety> VanillaArtifactCache<S> {
    pub fn with_path_safety<S2: PathSafety>(self, path_safety: S2) -> VanillaArtifactCache<S2> {
        VanillaArtifactCache {
            root: self.root,
            max_bytes: self.max_bytes,
            path_safety,
        }
    }

    pub fn path_safety(&self) -> &S {
        &self.path_safety
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn path(&self, version: &MinecraftVersionId, sha1: &str) -> Result<PathBuf> {
        validate_sha1(sha1)?;
        Ok(self
            .root
            .join("vanilla")
            .join(version.as_str())
            .join(sha1)
            .join("server.jar"))
    }

    pub fn verify(&self, path: &Path, descriptor: &ArtifactDescriptor) -> Result<bool> {
        descriptor.validate()?;
        if let Some(parent) = path.parent() {
            self.path_safety
                .validate_existing_ancestors(parent)
                .map_err(|error| {
                    MineDockError::Verification(format!("cache path is unsafe: {error}"))
                })?;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(MineDockError::Download(format!(
                    "could not inspect cache: {error}"
                )));
            }
        };
        if !metadata.file_type().is_file() {
            return Err(MineDockError::Verification(
                "cached artifact is not a regular file".into(),
            ));
        }
        if metadata.len() != descriptor.size {
            return Ok(false);
        }
        Ok(sha1_file(path)? == descriptor.sha1)
    }

    pub fn acquire<T: AuthoritativeTransport>(
        &self,
        version: &MinecraftVersionId,
        descriptor: &ArtifactDescriptor,
        transport: &T,
    ) -> Result<PathBuf> {
        descriptor.validate()?;
        if descriptor.size > self.max_bytes {
            return Err(MineDockError::Download(
                "server artifact exceeds configured byte ceiling".into(),
            ));
        }
        self.path_safety
            .validate_existing_ancestors(&self.root)
            .map_err(|error| MineDockError::Download(format!("cache root is unsafe: {error}")))?;
        fs::create_dir_all(&self.root).map_err(|error| {
            MineDockError::Download(format!("could not create artifact cache root: {error}"))
        })?;
        self.path_safety
            .validate_existing_ancestors(&self.root)
            .map_err(|error| MineDockError::Download(format!("cache root is unsafe: {error}")))?;
        let destination = self.path(version, &descriptor.sha1)?;
        match self.verify(&destination, descriptor) {
            Ok(true) => return Ok(destination),
            Ok(false) => {
                if let Ok(metadata) = fs::symlink_metadata(&destination) {
                    if !metadata.file_type().is_file() {
                        return Err(MineDockError::Verification(
                            "refusing to replace non-file cache entry".into(),
                        ));
                    }
                    fs::remove_file(&destination).map_err(|error| {
                        MineDockError::Download(format!("could not remove corrupt cache: {error}"))
                    })?;
                }
            }
            Err(error) => return Err(error),
        }
        let response = transport.get(
            &descriptor.url,
            usize::try_from(self.max_bytes).unwrap_or(usize::MAX),
        )?;
        validate_response_url(&response, &descriptor.url, ARTIFACT_AUTHORITIES)?;
        if response.status != 200 {
            return Err(MineDockError::Download(format!(
                "artifact request returned HTTP {}",
                response.status
            )));
        }
        if response.body.len() as u64 > self.max_bytes
            || response.body.len() as u64 != descriptor.size
        {
            return Err(MineDockError::Verification(
                "downloaded artifact size did not match authoritative metadata".into(),
            ));
        }
        if sha1_bytes(&response.body) != descriptor.sha1 {
            return Err(MineDockError::Verification(
                "downloaded artifact SHA-1 did not match authoritative metadata".into(),
            ));
        }
        if let Some(parent) = destination.parent() {
            self.path_safety
                .validate_existing_ancestors(parent)
                .map_err(|error| {
                    MineDockError::Download(format!("cache destination is unsafe: {error}"))
                })?;
            fs::create_dir_all(parent).map_err(|error| {
                MineDockError::Download(format!("could not create artifact cache: {error}"))
            })?;
        }
        let temporary = unique_temp_path(&destination);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) => {
                return Err(MineDockError::Download(format!(
                    "could not create artifact temporary file: {error}"
                )));
            }
        };
        let write_result = (|| -> Result<()> {
            file.write_all(&response.body).map_err(|error| {
                MineDockError::Download(format!("could not write artifact: {error}"))
            })?;
            file.flush().map_err(|error| {
                MineDockError::Download(format!("could not flush artifact: {error}"))
            })?;
            file.sync_all().map_err(|error| {
                MineDockError::Download(format!("could not sync artifact: {error}"))
            })?;
            drop(file);
            if sha1_file(&temporary)? != descriptor.sha1
                || fs::metadata(&temporary)
                    .map_err(|error| MineDockError::Download(error.to_string()))?
                    .len()
                    != descriptor.size
            {
                return Err(MineDockError::Verification(
                    "temporary artifact verification failed".into(),
                ));
            }
            if destination.exists() {
                if self.verify(&destination, descriptor)? {
                    fs::remove_file(&temporary).ok();
                    return Ok(());
                }
                return Err(MineDockError::Verification(
                    "refusing to overwrite a mismatched cache artifact".into(),
                ));
            }
            fs::rename(&temporary, &destination).map_err(|error| {
                MineDockError::Download(format!("could not promote artifact atomically: {error}"))
            })?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result?;
        Ok(destination)
    }
}

#[derive(Debug, Clone)]
pub struct VanillaProvider<T, S = PortablePathSafety> {
    transport: T,
    cache: VanillaArtifactCache<S>,
    metadata_limit: usize,
}

impl<T> VanillaProvider<T, PortablePathSafety> {
    pub fn new(transport: T, downloads_root: impl Into<PathBuf>) -> Self {
        Self {
            transport,
            cache: VanillaArtifactCache::new(downloads_root),
            metadata_limit: METADATA_MAX_BYTES,
        }
    }
}

impl<T, S: PathSafety> VanillaProvider<T, S> {
    pub fn with_path_safety<S2: PathSafety>(self, path_safety: S2) -> VanillaProvider<T, S2> {
        VanillaProvider {
            transport: self.transport,
            cache: self.cache.with_path_safety(path_safety),
            metadata_limit: self.metadata_limit,
        }
    }

    pub fn with_limits(mut self, metadata_limit: usize, artifact_limit: u64) -> Self {
        self.metadata_limit = metadata_limit;
        self.cache = self.cache.with_max_bytes(artifact_limit);
        self
    }

    pub fn cache(&self) -> &VanillaArtifactCache<S> {
        &self.cache
    }
}

impl<T: AuthoritativeTransport, S: PathSafety> VanillaProvider<T, S> {
    pub fn resolve_version(
        &self,
        selector: &MinecraftVersionSelector,
    ) -> Result<ResolvedVanillaVersion> {
        let manifest = self.get_metadata(VERSION_MANIFEST_URL)?;
        let manifest_json: serde_json::Value =
            serde_json::from_slice(&manifest.body).map_err(|error| {
                MineDockError::Download(format!("version manifest is invalid JSON: {error}"))
            })?;
        let versions = manifest_json
            .get("versions")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                MineDockError::Download("version manifest has no versions array".into())
            })?;
        let selected_id = match selector {
            MinecraftVersionSelector::CurrentRelease => manifest_json
                .get("latest")
                .and_then(|latest| latest.get("release"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    MineDockError::Download("version manifest has no latest release".into())
                })?
                .to_owned(),
            MinecraftVersionSelector::Exact(id) => id.as_str().to_owned(),
        };
        let entry = versions
            .iter()
            .find(|entry| {
                entry.get("id").and_then(serde_json::Value::as_str) == Some(selected_id.as_str())
            })
            .ok_or_else(|| {
                MineDockError::Download(format!(
                    "Minecraft version {selected_id} is not in the authoritative manifest"
                ))
            })?;
        let id = MinecraftVersionId::parse(
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )?;
        let version_type = entry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| MineDockError::Download("version manifest entry has no type".into()))?;
        if version_type != "release" {
            return Err(MineDockError::Download(
                "only release Minecraft versions are supported".into(),
            ));
        }
        let metadata_url = entry
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                MineDockError::Download("version manifest entry has no metadata URL".into())
            })?;
        validate_authoritative_url(metadata_url, METADATA_AUTHORITIES)?;
        let expected_hash = entry
            .get("sha1")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                MineDockError::Verification(
                    "version manifest entry has no authoritative SHA-1".into(),
                )
            })?;
        validate_sha1(expected_hash)?;
        let expected_size = entry.get("size").and_then(serde_json::Value::as_u64);
        if expected_size.is_some_and(|size| size == 0) {
            return Err(MineDockError::Verification(
                "version metadata size must be nonzero".into(),
            ));
        }
        let version_response = self.get_metadata(metadata_url)?;
        if let Some(expected_size) = expected_size {
            if version_response.body.len() as u64 != expected_size {
                return Err(MineDockError::Verification(
                    "version metadata size mismatch".into(),
                ));
            }
        }
        if sha1_bytes(&version_response.body) != expected_hash {
            return Err(MineDockError::Verification(
                "version metadata SHA-1 mismatch".into(),
            ));
        }
        parse_version_json(&version_response.body, &id, version_type)
    }

    pub fn acquire_server<E: EulaAcceptanceRepository>(
        &self,
        resolved: &ResolvedVanillaVersion,
        eula: &E,
    ) -> Result<PathBuf> {
        resolved.validate()?;
        // This check is deliberately immediately before the artifact request.
        if !eula.is_accepted()? {
            return Err(MineDockError::EulaRequired);
        }
        self.cache
            .acquire(&resolved.id, &resolved.artifact, &self.transport)
    }

    pub fn provision_server<E: EulaAcceptanceRepository>(
        &self,
        world: &World,
        resolved: &ResolvedVanillaVersion,
        artifact_path: &Path,
        eula: &E,
    ) -> Result<ProvisionedServer> {
        self.provision_server_at(Path::new("."), world, resolved, artifact_path, eula)
    }

    /// Provision under an explicit MineDock app-data root. The convenience
    /// method above is retained for callers whose current directory is already
    /// the app-data root.
    pub fn provision_server_at<E: EulaAcceptanceRepository>(
        &self,
        app_data_root: &Path,
        world: &World,
        resolved: &ResolvedVanillaVersion,
        artifact_path: &Path,
        eula: &E,
    ) -> Result<ProvisionedServer> {
        if world.server.distribution != ServerDistribution::Vanilla {
            return Err(MineDockError::InvalidConfiguration(
                "only Vanilla distribution is supported".into(),
            ));
        }
        if world.server.edition != MinecraftEdition::Java {
            return Err(MineDockError::InvalidConfiguration(
                "only Minecraft Java Edition is supported".into(),
            ));
        }
        resolved.validate()?;
        // Gate again immediately before writing launchable files.
        if !eula.is_accepted()? {
            return Err(MineDockError::EulaRequired);
        }
        if !self.cache.verify(artifact_path, &resolved.artifact)? {
            return Err(MineDockError::Verification(
                "server artifact is not a verified cache entry".into(),
            ));
        }
        if app_data_root.as_os_str().is_empty() {
            return Err(MineDockError::InvalidConfiguration(
                "app-data root must not be empty".into(),
            ));
        }
        let world_root = app_data_root.join(&world.data_path);
        validate_world_root(&world.data_path, world.id)?;
        let server_dir = world_root.join("server");
        let world_dir = world_root.join("world");
        let logs_dir = world_root.join("logs");
        let server_jar = server_dir.join("server.jar");
        let server_properties = world_root.join("server.properties");
        let eula_file = world_root.join("eula.txt");
        let provision_path = server_dir.join("provision.json");
        let incomplete_marker = world_root.join(INCOMPLETE_PROVISION_MARKER);
        self.cache
            .path_safety
            .validate_existing_ancestors(app_data_root)?;
        fs::create_dir_all(app_data_root).map_err(|error| {
            MineDockError::Persistence(format!("could not create app-data root: {error}"))
        })?;
        self.cache
            .path_safety
            .validate_existing_ancestors(app_data_root)?;
        self.cache
            .path_safety
            .validate_existing_ancestors(&world_root)?;
        self.cache.path_safety.reject_path_type(&world_root, true)?;
        for directory in [&server_dir, &world_dir, &logs_dir] {
            self.cache
                .path_safety
                .validate_existing_ancestors(directory)?;
            self.cache.path_safety.reject_path_type(directory, true)?;
        }
        for file in [&server_jar, &server_properties, &eula_file, &provision_path] {
            self.cache.path_safety.reject_path_type(file, false)?;
        }
        let eula_contents =
            b"# MineDock recorded explicit acceptance of the official Minecraft EULA\neula=true\n";
        if eula_file.exists() {
            let existing = fs::read(&eula_file).map_err(|error| {
                MineDockError::Persistence(format!("could not read existing eula.txt: {error}"))
            })?;
            if !is_valid_eula_contents(&existing) {
                return Err(MineDockError::EulaRequired);
            }
        }
        let build_record = |server_properties_sha1| ProvisionRecord {
            schema_version: PROVISION_RECORD_SCHEMA_VERSION,
            world_id: Some(world.id),
            resolved_version: resolved.id.clone(),
            java_major: resolved.java_requirement.major,
            artifact_sha1: resolved.artifact.sha1.clone(),
            artifact_size: resolved.artifact.size,
            server_jar: "server/server.jar".into(),
            world_directory: "world".into(),
            logs_directory: "logs".into(),
            server_properties: "server.properties".into(),
            server_properties_sha1,
            eula_file: "eula.txt".into(),
            eula_acceptance: Some("server/eula-acceptance.json".into()),
        };
        let properties = match generate_server_properties(world) {
            Ok(properties) => properties,
            Err(error) => {
                // Preserve an owned transaction marker even when validation
                // fails before any launchable files are written. A later
                // retry may complete the deterministic properties attestation.
                if !provision_path.exists() && !incomplete_marker.exists() {
                    reject_unknown_world_content(&world_root)?;
                    let marker = IncompleteProvision {
                        schema_version: PROVISION_RECORD_SCHEMA_VERSION,
                        world_id: world.id,
                        record: build_record(None),
                    };
                    atomic_replace(
                        &incomplete_marker,
                        &serde_json::to_vec_pretty(&marker).map_err(|serialize_error| {
                            MineDockError::Persistence(serialize_error.to_string())
                        })?,
                    )?;
                }
                return Err(error);
            }
        };
        let record = build_record(Some(sha1_bytes(properties.as_bytes())));
        record.validate()?;
        if provision_path.exists() {
            let existing = read_provision_record(&provision_path)?;
            if !provision_records_match_for_retry(&existing, &record) {
                return Err(MineDockError::Persistence(
                    "existing provision record does not match authoritative state".into(),
                ));
            }
            if incomplete_marker.exists() {
                remove_owned_incomplete_marker(&incomplete_marker)?;
            }
        } else if incomplete_marker.exists() {
            repair_incomplete_provision(&incomplete_marker, &world_root, &record)?;
            reject_unknown_world_content(&world_root)?;
        } else {
            reject_unknown_world_content(&world_root)?;
        }
        if !provision_path.exists() {
            let marker = IncompleteProvision {
                schema_version: PROVISION_RECORD_SCHEMA_VERSION,
                world_id: world.id,
                record: record.clone(),
            };
            atomic_replace(
                &incomplete_marker,
                &serde_json::to_vec_pretty(&marker)
                    .map_err(|error| MineDockError::Persistence(error.to_string()))?,
            )?;
        }
        fs::create_dir_all(&server_dir).map_err(|error| {
            MineDockError::Persistence(format!("could not create server directory: {error}"))
        })?;
        fs::create_dir_all(&world_dir).map_err(|error| {
            MineDockError::Persistence(format!("could not create world directory: {error}"))
        })?;
        fs::create_dir_all(&logs_dir).map_err(|error| {
            MineDockError::Persistence(format!("could not create logs directory: {error}"))
        })?;
        if !server_jar.exists() {
            copy_verified_artifact(artifact_path, &server_jar, &resolved.artifact)?;
        } else if !self.cache.verify(&server_jar, &resolved.artifact)? {
            return Err(MineDockError::Verification(
                "existing server.jar does not match provision record".into(),
            ));
        }
        let acceptance_path =
            world_root.join(record.eula_acceptance.as_deref().ok_or_else(|| {
                MineDockError::Persistence("provision record has no EULA acceptance path".into())
            })?);
        let acceptance = eula.load()?.ok_or(MineDockError::EulaRequired)?;
        acceptance.validate()?;
        let acceptance_bytes = serde_json::to_vec_pretty(&acceptance)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        // The affirmative acceptance record is durably copied before the
        // server's eula.txt is created. It is never inferred from eula.txt.
        atomic_replace(&acceptance_path, &acceptance_bytes)?;
        atomic_replace(&server_properties, properties.as_bytes())?;
        // This file is created only after the MineDock acceptance record has
        // been durably read.  The acceptance record itself is never inferred
        // from an existing eula.txt.
        if !eula_file.exists() {
            atomic_replace(&eula_file, eula_contents)?;
        }
        // The record is the final write and makes all preceding paths owned by
        // MineDock for retry/repair purposes.
        atomic_replace(
            &provision_path,
            &serde_json::to_vec_pretty(&record)
                .map_err(|error| MineDockError::Persistence(error.to_string()))?,
        )?;
        if incomplete_marker.exists() {
            remove_owned_incomplete_marker(&incomplete_marker)?;
        }
        Ok(ProvisionedServer {
            world_root,
            server_jar,
            world_directory: world_dir,
            logs_directory: logs_dir,
            server_properties,
            eula_file,
            record,
        })
    }

    fn get_metadata(&self, url: &str) -> Result<TransportResponse> {
        validate_authoritative_url(url, METADATA_AUTHORITIES)?;
        let response = self.transport.get(url, self.metadata_limit)?;
        validate_response_url(&response, url, METADATA_AUTHORITIES)?;
        if response.status != 200 {
            return Err(MineDockError::Download(format!(
                "metadata request returned HTTP {}",
                response.status
            )));
        }
        if response.body.len() > self.metadata_limit {
            return Err(MineDockError::Download(
                "metadata response exceeds byte ceiling".into(),
            ));
        }
        Ok(response)
    }
}

impl<T: AuthoritativeTransport, S: PathSafety> ServerDistributionProvider
    for VanillaProvider<T, S>
{
    fn resolve_vanilla_version(
        &self,
        selector: &MinecraftVersionSelector,
    ) -> Result<ResolvedVanillaVersion> {
        self.resolve_version(selector)
    }
}

fn parse_version_json(
    body: &[u8],
    expected_id: &MinecraftVersionId,
    expected_type: &str,
) -> Result<ResolvedVanillaVersion> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
        MineDockError::Download(format!("version metadata is invalid JSON: {error}"))
    })?;
    let actual_id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MineDockError::Download("version metadata has no id".into()))?;
    if actual_id != expected_id.as_str() {
        return Err(MineDockError::Download(
            "version metadata id does not match manifest entry".into(),
        ));
    }
    let actual_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MineDockError::Download("version metadata has no type".into()))?;
    if actual_type != expected_type {
        return Err(MineDockError::Download(
            "version metadata type does not match manifest entry".into(),
        ));
    }
    let java_major = value
        .get("javaVersion")
        .and_then(|java| java.get("majorVersion"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            MineDockError::Download("version metadata has no Java major version".into())
        })?;
    let java_requirement = crate::JavaRequirement::new(
        u16::try_from(java_major)
            .map_err(|_| MineDockError::Download("Java major version is out of range".into()))?,
    )?;
    let artifact = value
        .get("downloads")
        .and_then(|downloads| downloads.get("server"))
        .ok_or_else(|| MineDockError::Download("version metadata has no server artifact".into()))?;
    let url = artifact
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MineDockError::Download("server artifact has no URL".into()))?;
    let sha1 = artifact
        .get("sha1")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MineDockError::Download("server artifact has no SHA-1".into()))?;
    let size = artifact
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| MineDockError::Download("server artifact has no size".into()))?;
    Ok(ResolvedVanillaVersion {
        id: expected_id.clone(),
        version_type: actual_type.into(),
        java_requirement,
        artifact: ArtifactDescriptor::new(url, sha1, size)?,
    })
}

pub fn parse_version_metadata(
    body: &[u8],
    expected_id: &MinecraftVersionId,
    expected_type: &str,
) -> Result<ResolvedVanillaVersion> {
    parse_version_json(body, expected_id, expected_type)
}

pub fn validate_sha1(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(MineDockError::Verification(
            "SHA-1 must be exactly 40 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

pub fn validate_authoritative_url(url: &str, authorities: &[&str]) -> Result<()> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(MineDockError::Download(
            "authoritative URL must use HTTPS".into(),
        ));
    };
    if scheme != "https" || rest.is_empty() || rest.contains(['?', '#', '@']) {
        return Err(MineDockError::Download(
            "authoritative URL must be a plain HTTPS URL".into(),
        ));
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains(':') || !authorities.contains(&authority) {
        return Err(MineDockError::Download(
            "URL authority is not an allowed Mojang host".into(),
        ));
    }
    if !rest.contains('/') {
        return Err(MineDockError::Download(
            "authoritative URL must include a path".into(),
        ));
    }
    Ok(())
}

fn validate_response_url(
    response: &TransportResponse,
    requested: &str,
    authorities: &[&str],
) -> Result<()> {
    validate_authoritative_url(requested, authorities)?;
    if response.requested_url != requested {
        return Err(MineDockError::Download(
            "transport response does not identify the requested authoritative URL".into(),
        ));
    }
    if response.redirects.len() > MAX_REDIRECT_HOPS {
        return Err(MineDockError::Download(
            "authoritative redirect chain exceeds the hop limit".into(),
        ));
    }
    for hop in &response.redirects {
        validate_authoritative_url(hop, authorities)?;
    }
    validate_authoritative_url(&response.final_url, authorities)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(MineDockError::Persistence(
            "provision record contains an unsafe relative path".into(),
        ));
    }
    Ok(())
}

fn validate_world_root(path: &Path, id: crate::WorldId) -> Result<()> {
    crate::validate_world_data_path(path, id)
}

fn validate_launch_root(root: &Path) -> Result<()> {
    if !root.is_absolute() {
        return Err(MineDockError::ProcessStart(
            "world launch root must be absolute".into(),
        ));
    }
    validate_existing_ancestors(root)?;
    reject_reparse_or_wrong_type(root, true)?;
    Ok(())
}

/// Walk every existing component below the trusted root. Checking only the
/// final path is insufficient when a parent is a junction/reparse point.
fn portable_validate_existing_ancestors(path: &Path) -> Result<()> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(MineDockError::ProcessStart(
                    "launch path contains a symlink or reparse point".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(MineDockError::ProcessStart(format!(
                    "could not inspect launch path: {error}"
                )));
            }
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

fn validate_persisted_provision(
    world_root: &Path,
    record: &ProvisionRecord,
    expected_world_id: crate::WorldId,
    runtime_major: crate::JavaMajor,
) -> Result<()> {
    record.validate()?;
    if record.world_id != Some(expected_world_id) {
        return Err(MineDockError::ProcessStart(
            "provision record world identity does not match launch request".into(),
        ));
    }
    if record.java_major != runtime_major {
        return Err(MineDockError::JavaUnavailable(
            "selected Java major does not match the persisted provision".into(),
        ));
    }
    let acceptance_rel = record
        .eula_acceptance
        .as_deref()
        .ok_or(MineDockError::EulaRequired)?;
    let acceptance_path = rooted_record_path(world_root, acceptance_rel)?;
    validate_existing_ancestors(&acceptance_path)?;
    let acceptance_bytes = fs::read(&acceptance_path).map_err(|_| MineDockError::EulaRequired)?;
    let acceptance: EulaAcceptance =
        serde_json::from_slice(&acceptance_bytes).map_err(|_| MineDockError::EulaRequired)?;
    acceptance.validate()?;
    let eula_path = rooted_record_path(world_root, &record.eula_file)?;
    validate_existing_ancestors(&eula_path)?;
    let eula = fs::read(&eula_path).map_err(|_| MineDockError::EulaRequired)?;
    if !is_valid_eula_contents(&eula) {
        return Err(MineDockError::EulaRequired);
    }
    let properties_path = rooted_record_path(world_root, &record.server_properties)?;
    validate_existing_ancestors(&properties_path)?;
    let properties = fs::read(&properties_path).map_err(|_| {
        MineDockError::ProcessStart("persisted server.properties is missing".into())
    })?;
    let expected_properties_sha1 = record.server_properties_sha1.as_deref().ok_or_else(|| {
        MineDockError::ProcessStart(
            "provision record has no server.properties attestation; reprovision required".into(),
        )
    })?;
    if sha1_bytes(&properties) != expected_properties_sha1
        || !has_mandatory_online_mode(&properties)
    {
        return Err(MineDockError::Verification(
            "server.properties is missing, tampered, or disables online-mode".into(),
        ));
    }
    let jar_path = rooted_record_path(world_root, &record.server_jar)?;
    validate_existing_ancestors(&jar_path)?;
    let metadata = fs::symlink_metadata(&jar_path)
        .map_err(|_| MineDockError::ProcessStart("persisted server.jar is missing".into()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MineDockError::ProcessStart(
            "persisted server.jar is not a regular file".into(),
        ));
    }
    if metadata.len() != record.artifact_size || sha1_file(&jar_path)? != record.artifact_sha1 {
        return Err(MineDockError::Verification(
            "persisted server.jar hash or size does not match provision record".into(),
        ));
    }
    for relative in [
        &record.server_jar,
        &record.world_directory,
        &record.logs_directory,
        &record.server_properties,
        &record.eula_file,
        &acceptance_rel.to_owned(),
    ] {
        let path = rooted_record_path(world_root, relative)?;
        validate_existing_ancestors(&path)?;
    }
    Ok(())
}

fn rooted_record_path(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_path(Path::new(relative))?;
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(MineDockError::ProcessStart(
            "persisted provision path escapes world root".into(),
        ));
    }
    Ok(path)
}

fn reject_unknown_world_content(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| MineDockError::Persistence(error.to_string()))?;
    if !metadata.is_dir()
        || fs::read_dir(root)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name() != INCOMPLETE_PROVISION_MARKER)
    {
        return Err(MineDockError::Persistence(
            "world directory contains unknown user content without a MineDock provision record"
                .into(),
        ));
    }
    Ok(())
}

fn portable_reject_path_type(path: &Path, directory: bool) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(MineDockError::Persistence(error.to_string())),
    };
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(MineDockError::Persistence(format!(
            "refusing to use a reparse point or unexpected path type: {}",
            path.display()
        )));
    }
    Ok(())
}

fn provision_records_match_for_retry(
    existing: &ProvisionRecord,
    expected: &ProvisionRecord,
) -> bool {
    if existing == expected {
        return true;
    }
    // Older schema-v1 records have no launch attestation fields. They are
    // upgradeable only when every other pinned value matches exactly.
    let mut upgraded = existing.clone();
    if upgraded.server_properties_sha1.is_none() {
        upgraded.server_properties_sha1 = expected.server_properties_sha1.clone();
    }
    if upgraded.eula_acceptance.is_none() {
        upgraded.eula_acceptance = expected.eula_acceptance.clone();
    }
    upgraded == *expected
}

fn validate_existing_ancestors(path: &Path) -> Result<()> {
    PortablePathSafety.validate_existing_ancestors(path)
}

fn reject_reparse_or_wrong_type(path: &Path, directory: bool) -> Result<()> {
    PortablePathSafety.reject_path_type(path, directory)
}

fn remove_owned_incomplete_marker(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| MineDockError::Persistence(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MineDockError::Persistence(
            "incomplete provision marker is not a regular owned file".into(),
        ));
    }
    fs::remove_file(path)
        .map_err(|error| MineDockError::Persistence(format!("could not remove marker: {error}")))
}

fn repair_incomplete_provision(
    marker_path: &Path,
    world_root: &Path,
    expected: &ProvisionRecord,
) -> Result<()> {
    let marker_bytes = fs::read(marker_path).map_err(|error| {
        MineDockError::Persistence(format!("could not read provision marker: {error}"))
    })?;
    let marker: IncompleteProvision = serde_json::from_slice(&marker_bytes).map_err(|error| {
        MineDockError::Persistence(format!("provision marker is corrupt: {error}"))
    })?;
    let mut marker_record = marker.record;
    // An early transaction marker may have been written before deterministic
    // properties generation rejected an invalid profile. The retry computes
    // the attestation again and may safely upgrade this owned marker.
    if marker_record.server_properties_sha1.is_none() {
        marker_record.server_properties_sha1 = expected.server_properties_sha1.clone();
    }
    if marker.schema_version != PROVISION_RECORD_SCHEMA_VERSION || marker_record != *expected {
        return Err(MineDockError::Persistence(
            "incomplete provision marker does not match authoritative state".into(),
        ));
    }
    for relative in [
        expected.eula_acceptance.as_deref().unwrap_or(""),
        &expected.server_properties,
        &expected.eula_file,
        &expected.server_jar,
    ] {
        if relative.is_empty() {
            continue;
        }
        let path = rooted_record_path(world_root, relative)?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.file_type().is_symlink()
            {
                return Err(MineDockError::Persistence(
                    "partial provision contains an unsafe owned file".into(),
                ));
            }
            fs::remove_file(&path).map_err(|error| {
                MineDockError::Persistence(format!("could not repair partial provision: {error}"))
            })?;
        }
    }
    for relative in [
        expected.logs_directory.as_str(),
        expected.world_directory.as_str(),
    ] {
        let path = rooted_record_path(world_root, relative)?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.file_type().is_symlink()
            {
                return Err(MineDockError::Persistence(
                    "partial provision contains an unsafe owned directory".into(),
                ));
            }
            if fs::read_dir(&path)
                .map_err(|error| MineDockError::Persistence(error.to_string()))?
                .next()
                .is_some()
            {
                return Err(MineDockError::Persistence(
                    "partial provision directory contains unknown user content".into(),
                ));
            }
            fs::remove_dir(&path).map_err(|error| {
                MineDockError::Persistence(format!("could not repair partial provision: {error}"))
            })?;
        }
    }
    let server_dir = world_root.join("server");
    if let Ok(metadata) = fs::symlink_metadata(&server_dir) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(MineDockError::Persistence(
                "partial server directory is unsafe".into(),
            ));
        }
        if fs::read_dir(&server_dir)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?
            .next()
            .is_none()
        {
            fs::remove_dir(&server_dir).map_err(|error| {
                MineDockError::Persistence(format!("could not repair partial provision: {error}"))
            })?;
        } else {
            return Err(MineDockError::Persistence(
                "partial server directory contains unknown user content".into(),
            ));
        }
    }
    remove_owned_incomplete_marker(marker_path)
}

pub fn is_valid_eula_contents(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.lines().any(|line| line.trim() == "eula=true")
        && !text.lines().any(|line| line.trim() == "eula=false")
        && !text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn has_mandatory_online_mode(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut found_true = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "online-mode=false" {
            return false;
        }
        if trimmed == "online-mode=true" {
            found_true = true;
        }
    }
    found_true
}

fn read_provision_record(path: &Path) -> Result<ProvisionRecord> {
    let bytes = fs::read(path).map_err(|error| {
        MineDockError::Persistence(format!("could not read provision record: {error}"))
    })?;
    let record: ProvisionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        MineDockError::Persistence(format!("provision record is corrupt: {error}"))
    })?;
    record.validate()?;
    Ok(record)
}

fn copy_verified_artifact(
    source: &Path,
    destination: &Path,
    descriptor: &ArtifactDescriptor,
) -> Result<()> {
    descriptor.validate()?;
    let bytes = fs::read(source).map_err(|error| {
        MineDockError::Download(format!("could not read verified artifact: {error}"))
    })?;
    if bytes.len() as u64 != descriptor.size || sha1_bytes(&bytes) != descriptor.sha1 {
        return Err(MineDockError::Verification(
            "source artifact changed after verification".into(),
        ));
    }
    if let Some(parent) = destination.parent() {
        validate_existing_ancestors(parent).map_err(|error| {
            MineDockError::Persistence(format!("artifact destination is unsafe: {error}"))
        })?;
    }
    let temporary = unique_temp_path(destination);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        file.write_all(&bytes)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        file.sync_all()
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        fs::rename(&temporary, destination).map_err(|error| {
            MineDockError::Persistence(format!("could not atomically promote artifact: {error}"))
        })?;
        Ok(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        validate_existing_ancestors(parent)?;
        fs::create_dir_all(parent)
            .map_err(|error| MineDockError::Persistence(error.to_string()))?;
        validate_existing_ancestors(parent)?;
    }
    let mut file = AtomicWriteFile::open(path)
        .map_err(|error| MineDockError::Persistence(error.to_string()))?;
    file.write_all(bytes)
        .map_err(|error| MineDockError::Persistence(error.to_string()))?;
    file.flush()
        .map_err(|error| MineDockError::Persistence(error.to_string()))?;
    file.commit()
        .map_err(|error| MineDockError::Persistence(error.to_string()))
}

fn unique_temp_path(destination: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    destination.with_extension(format!("tmp-{}-{}", std::process::id(), stamp))
}

pub fn sha1_bytes(input: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(input);
    hash.finish()
}

fn sha1_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).map_err(|error| MineDockError::Verification(error.to_string()))?;
    let mut hash = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| MineDockError::Verification(error.to_string()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finish())
}

struct Sha1 {
    state: [u32; 5],
    length: u64,
    buffer: Vec<u8>,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            length: 0,
            buffer: Vec::with_capacity(64),
        }
    }
    fn update(&mut self, bytes: &[u8]) {
        self.length = self.length.saturating_add(bytes.len() as u64);
        let mut input = bytes;
        if !self.buffer.is_empty() {
            let needed = 64 - self.buffer.len();
            let take = needed.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() == 64 {
                let block = self.buffer.clone();
                self.process(&block);
                self.buffer.clear();
            }
        }
        while input.len() >= 64 {
            self.process(&input[..64]);
            input = &input[64..];
        }
        self.buffer.extend_from_slice(input);
    }
    fn process(&mut self, block: &[u8]) {
        let mut words = [0_u32; 80];
        for (index, chunk) in block.chunks_exact(4).take(16).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
        );
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
    fn finish(mut self) -> String {
        let bit_length = self.length.saturating_mul(8);
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_length.to_be_bytes());
        for block in self.buffer.clone().chunks_exact(64) {
            self.process(block);
        }
        self.state
            .iter()
            .map(|value| format!("{value:08x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CreateWorldRequest, JavaCandidateSource, JavaCompatibility, JavaMajor, JavaRequirement,
        JavaRuntime, JavaVersion, LaunchSpec, TemplateCatalog,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Clone)]
    struct FakeTransport {
        responses: Arc<Mutex<HashMap<String, TransportResponse>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }
    impl AuthoritativeTransport for FakeTransport {
        fn get(&self, url: &str, _: usize) -> Result<TransportResponse> {
            self.requests.lock().expect("requests").push(url.into());
            self.responses
                .lock()
                .expect("responses")
                .get(url)
                .cloned()
                .ok_or_else(|| MineDockError::Download("missing fake URL".into()))
        }
    }

    fn descriptor_body() -> (String, Vec<u8>) {
        let body = b"fixture jar".to_vec();
        (sha1_bytes(&body), body)
    }

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(sha1_bytes(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_bytes(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn artifact_urls_hashes_sizes_and_redirects_are_validated() {
        assert!(
            ArtifactDescriptor::new(
                "https://piston-data.mojang.com/x",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                1
            )
            .is_ok()
        );
        assert!(
            ArtifactDescriptor::new(
                "http://piston-data.mojang.com/x",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                1
            )
            .is_err()
        );
        assert!(
            ArtifactDescriptor::new(
                "https://evil.example/x",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                1
            )
            .is_err()
        );
        assert!(
            ArtifactDescriptor::new(
                "https://piston-data.mojang.com/x",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn cache_reuses_verified_artifact_and_retries_corruption() {
        let root = TempDir::new().expect("root");
        let (sha1, body) = descriptor_body();
        let descriptor = ArtifactDescriptor::new(
            "https://piston-data.mojang.com/server.jar",
            sha1,
            body.len() as u64,
        )
        .expect("descriptor");
        let id = MinecraftVersionId::parse("1.20.6").expect("id");
        let transport = FakeTransport {
            responses: Arc::new(Mutex::new(HashMap::from([(
                descriptor.url.clone(),
                TransportResponse::new(200, descriptor.url.clone(), body.clone()),
            )]))),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let cache = VanillaArtifactCache::new(root.path());
        let first = cache
            .acquire(&id, &descriptor, &transport)
            .expect("download");
        assert!(cache.verify(&first, &descriptor).expect("verify"));
        fs::write(&first, b"bad").expect("corrupt");
        let second = cache.acquire(&id, &descriptor, &transport).expect("retry");
        assert_eq!(first, second);
        assert_eq!(transport.requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn manifest_entry_sha1_is_required_and_version_body_is_verified() {
        let root = TempDir::new().expect("root");
        let version_id = MinecraftVersionId::parse("1.20.6").expect("id");
        let version_body = br#"{"id":"1.20.6","type":"release","javaVersion":{"majorVersion":17},"downloads":{"server":{"url":"https://piston-data.mojang.com/server.jar","sha1":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1}}}"#.to_vec();
        let manifest_without_hash = br#"{"latest":{"release":"1.20.6"},"versions":[{"id":"1.20.6","type":"release","url":"https://piston-meta.mojang.com/1.20.6.json"}]}"#;
        let missing_hash = FakeTransport {
            responses: Arc::new(Mutex::new(HashMap::from([(
                VERSION_MANIFEST_URL.into(),
                TransportResponse::new(200, VERSION_MANIFEST_URL, manifest_without_hash.to_vec()),
            )]))),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let provider = VanillaProvider::new(missing_hash, root.path());
        assert!(
            provider
                .resolve_version(&MinecraftVersionSelector::Exact(version_id.clone()))
                .is_err()
        );

        let manifest_hash = sha1_bytes(&version_body);
        let manifest = format!(
            "{{\"latest\":{{\"release\":\"1.20.6\"}},\"versions\":[{{\"id\":\"1.20.6\",\"type\":\"release\",\"url\":\"https://piston-meta.mojang.com/1.20.6.json\",\"sha1\":\"{manifest_hash}\",\"size\":{}}}]}}",
            version_body.len()
        );
        let transport = FakeTransport {
            responses: Arc::new(Mutex::new(HashMap::from([
                (
                    VERSION_MANIFEST_URL.into(),
                    TransportResponse::new(200, VERSION_MANIFEST_URL, manifest.into_bytes()),
                ),
                (
                    "https://piston-meta.mojang.com/1.20.6.json".into(),
                    TransportResponse::new(
                        200,
                        "https://piston-meta.mojang.com/1.20.6.json",
                        version_body,
                    ),
                ),
            ]))),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let resolved = VanillaProvider::new(transport, root.path())
            .resolve_version(&MinecraftVersionSelector::Exact(version_id))
            .expect("verified metadata");
        assert_eq!(resolved.java_requirement.major.get(), 17);
    }

    #[test]
    fn eula_gate_precedes_any_artifact_request() {
        let root = TempDir::new().expect("root");
        let (sha1, body) = descriptor_body();
        let descriptor = ArtifactDescriptor::new(
            "https://piston-data.mojang.com/server.jar",
            sha1,
            body.len() as u64,
        )
        .expect("descriptor");
        let transport = FakeTransport {
            responses: Arc::new(Mutex::new(HashMap::from([(
                descriptor.url.clone(),
                TransportResponse::new(200, descriptor.url.clone(), body),
            )]))),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let requests = transport.requests.clone();
        let provider = VanillaProvider::new(transport, root.path());
        let resolved = ResolvedVanillaVersion {
            id: MinecraftVersionId::parse("1.20.6").expect("id"),
            version_type: "release".into(),
            java_requirement: JavaRequirement::new(17).expect("requirement"),
            artifact: descriptor,
        };
        let eula = FileEulaAcceptanceRepository::new(root.path().join("acceptance.json"));
        assert!(matches!(
            provider.acquire_server(&resolved, &eula),
            Err(MineDockError::EulaRequired)
        ));
        assert!(requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn provision_and_launch_reload_acceptance_and_hashes() {
        let root = TempDir::new().expect("root");
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let request = CreateWorldRequest::new("Provision", "creative").expect("request");
        let world = request.build_world(&catalog).expect("world");
        let body = b"verified fixture jar".to_vec();
        let sha1 = sha1_bytes(&body);
        let provider = VanillaProvider::new(NoNetworkTransport, root.path().join("downloads"));
        let descriptor = ArtifactDescriptor::new(
            "https://piston-data.mojang.com/server.jar",
            sha1,
            body.len() as u64,
        )
        .expect("descriptor");
        let resolved = ResolvedVanillaVersion {
            id: MinecraftVersionId::parse("1.20.6").expect("id"),
            version_type: "release".into(),
            java_requirement: JavaRequirement::new(17).expect("requirement"),
            artifact: descriptor.clone(),
        };
        let artifact = provider
            .cache()
            .path(&resolved.id, &descriptor.sha1)
            .expect("cache path");
        fs::create_dir_all(artifact.parent().expect("parent")).expect("cache");
        fs::write(&artifact, body).expect("artifact");
        let eula = FileEulaAcceptanceRepository::new(root.path().join("acceptance.json"));
        eula.record_explicit_acceptance(true)
            .expect("explicit acceptance");
        let provisioned = provider
            .provision_server_at(root.path(), &world, &resolved, &artifact, &eula)
            .expect("provision");
        let runtime = JavaRuntime {
            executable: std::env::current_exe().expect("test exe"),
            version: JavaVersion {
                raw: "17.0.1".into(),
                major: JavaMajor::new(17).expect("major"),
                components: vec![17, 0, 1],
            },
            compatibility: JavaCompatibility::Exact,
            source: JavaCandidateSource::Configured,
        };
        let spec = LaunchSpec::new(&runtime, &provisioned, &world.server, world.id)
            .expect("validated launch");
        assert_eq!(spec.argument_strings()[2], "-jar");
        fs::write(provisioned.server_jar(), b"tampered").expect("tamper");
        assert!(LaunchSpec::new(&runtime, &provisioned, &world.server, world.id).is_err());
        fs::write(provisioned.server_jar(), b"verified fixture jar").expect("restore jar");
        fs::write(provisioned.server_properties(), b"online-mode=false\n")
            .expect("tamper properties");
        assert!(LaunchSpec::new(&runtime, &provisioned, &world.server, world.id).is_err());
    }

    #[test]
    fn incomplete_provision_repairs_only_owned_paths_and_rejects_unknown_content() {
        let root = TempDir::new().expect("root");
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let request = CreateWorldRequest::new("Retry", "creative").expect("request");
        let mut world = request.build_world(&catalog).expect("world");
        let body = b"retry fixture jar".to_vec();
        let descriptor = ArtifactDescriptor::new(
            "https://piston-data.mojang.com/retry.jar",
            sha1_bytes(&body),
            body.len() as u64,
        )
        .expect("descriptor");
        let provider = VanillaProvider::new(NoNetworkTransport, root.path().join("downloads"));
        let artifact = provider
            .cache()
            .path(
                &MinecraftVersionId::parse("1.20.6").expect("id"),
                &descriptor.sha1,
            )
            .expect("cache path");
        fs::create_dir_all(artifact.parent().expect("parent")).expect("cache");
        fs::write(&artifact, body).expect("artifact");
        let resolved = ResolvedVanillaVersion {
            id: MinecraftVersionId::parse("1.20.6").expect("id"),
            version_type: "release".into(),
            java_requirement: JavaRequirement::new(17).expect("requirement"),
            artifact: descriptor,
        };
        let eula = FileEulaAcceptanceRepository::new(root.path().join("acceptance.json"));
        eula.record_explicit_acceptance(true)
            .expect("explicit acceptance");
        world.server.online_mode = false;
        assert!(
            provider
                .provision_server_at(root.path(), &world, &resolved, &artifact, &eula)
                .is_err()
        );
        let world_root = root.path().join(&world.data_path);
        assert!(world_root.join(INCOMPLETE_PROVISION_MARKER).exists());
        fs::write(world_root.join("user-owned.txt"), b"keep").expect("unknown content");
        world.server.online_mode = true;
        assert!(
            provider
                .provision_server_at(root.path(), &world, &resolved, &artifact, &eula)
                .is_err()
        );
        assert!(world_root.join("user-owned.txt").exists());
    }
}
