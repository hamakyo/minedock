use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

use crate::{MineDockError, Result};

/// A validated Minecraft release identifier (for example `1.20.6`).  The
/// persisted schema still stores the original selector as a string; this type
/// is used at provider boundaries so a value cannot become an arbitrary URL or
/// path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct MinecraftVersionId(String);

impl MinecraftVersionId {
    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if value.is_empty() || value.len() > 64 {
            return Err(MineDockError::InvalidConfiguration(
                "Minecraft version id must contain 1-64 characters".into(),
            ));
        }
        // Mojang release identifiers are deliberately treated as opaque but
        // restricted to a conservative, URL/path-safe alphabet.  In
        // particular, no slash, query, fragment, whitespace, or control byte
        // may enter an authoritative request.
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            || !value
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphanumeric())
            || !value
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_alphanumeric())
            || !value.chars().any(|character| character.is_ascii_digit())
        {
            return Err(MineDockError::InvalidConfiguration(
                "Minecraft version id contains an unsafe character".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MinecraftVersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MinecraftVersionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for MinecraftVersionId {
    type Error = MineDockError;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<String> for MinecraftVersionId {
    type Error = MineDockError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::str::FromStr for MinecraftVersionId {
    type Err = MineDockError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl serde::Serialize for MinecraftVersionId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for MinecraftVersionId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// The value used by the v1 templates and metadata remains a string for
/// backwards compatibility.  New callers can validate it into this selector
/// before doing runtime/provider work.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MinecraftVersionSelector {
    CurrentRelease,
    Exact(MinecraftVersionId),
}

impl serde::Serialize for MinecraftVersionSelector {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.persisted_value())
    }
}

impl<'de> serde::Deserialize<'de> for MinecraftVersionSelector {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl MinecraftVersionSelector {
    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if value.eq_ignore_ascii_case("current-release") {
            Ok(Self::CurrentRelease)
        } else {
            Ok(Self::Exact(MinecraftVersionId::parse(value)?))
        }
    }

    pub fn persisted_value(&self) -> &str {
        match self {
            Self::CurrentRelease => "current-release",
            Self::Exact(value) => value.as_str(),
        }
    }
}

impl std::str::FromStr for MinecraftVersionSelector {
    type Err = MineDockError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

/// Stable identifier for a world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorldId(pub Uuid);

impl WorldId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        Uuid::parse_str(value.as_ref())
            .map(Self)
            .map_err(|error| MineDockError::Persistence(format!("invalid world id: {error}")))
    }
}

impl Default for WorldId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorldId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for the server profile attached to a world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerProfileId(pub Uuid);

impl ServerProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ServerProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ServerProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A safe, stable identifier used by declarative templates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct TemplateId(String);

impl TemplateId {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        Self::parse(value)
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if value.is_empty()
            || value.len() > 64
            || !value.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            })
            || value.starts_with('-')
            || value.ends_with('-')
        {
            return Err(MineDockError::InvalidConfiguration(
                "template id must contain 1-64 lowercase ASCII letters, digits, or internal hyphens"
                    .into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::str::FromStr for TemplateId {
    type Err = MineDockError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl AsRef<str> for TemplateId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for TemplateId {
    type Error = MineDockError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for TemplateId {
    type Error = MineDockError;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<&String> for TemplateId {
    type Error = MineDockError;

    fn try_from(value: &String) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::fmt::Display for TemplateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for TemplateId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TemplateId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorldStatus {
    Stopped,
    Preparing,
    Starting,
    Running,
    Stopping,
    BackingUp,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MinecraftEdition {
    Java,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerDistribution {
    Vanilla,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GameMode {
    Survival,
    Creative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Difficulty {
    Peaceful,
    Easy,
    Normal,
    Hard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerProfile {
    pub id: ServerProfileId,
    pub edition: MinecraftEdition,
    pub distribution: ServerDistribution,
    pub version: String,
    pub max_players: u16,
    pub online_mode: bool,
    pub whitelist: bool,
    pub port: u16,
    pub memory_min_mb: u32,
    pub memory_max_mb: u32,
}

impl ServerProfile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        edition: MinecraftEdition,
        distribution: ServerDistribution,
        version: impl Into<String>,
        max_players: u16,
        online_mode: bool,
        whitelist: bool,
        port: u16,
        memory_min_mb: u32,
        memory_max_mb: u32,
    ) -> Self {
        Self {
            id: ServerProfileId::new(),
            edition,
            distribution,
            version: version.into(),
            max_players,
            online_mode,
            whitelist,
            port,
            memory_min_mb,
            memory_max_mb,
        }
    }

    pub fn validate(&self) -> Result<()> {
        MinecraftVersionSelector::parse(&self.version)?;
        if self.max_players == 0 {
            return Err(MineDockError::InvalidConfiguration(
                "max_players must be greater than zero".into(),
            ));
        }
        if self.port == 0 {
            return Err(MineDockError::InvalidConfiguration(
                "port must be greater than zero".into(),
            ));
        }
        if self.memory_min_mb == 0 || self.memory_max_mb < self.memory_min_mb {
            return Err(MineDockError::InvalidConfiguration(
                "memory range is invalid".into(),
            ));
        }
        Ok(())
    }

    pub fn version_selector(&self) -> Result<MinecraftVersionSelector> {
        MinecraftVersionSelector::parse(&self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldSettings {
    pub gamemode: GameMode,
    pub difficulty: Difficulty,
    pub hardcore: bool,
    pub seed: Option<String>,
    pub generate_structures: bool,
    pub pvp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupPolicy {
    pub enabled: bool,
    pub on_shutdown: bool,
    pub retain: u16,
}

impl BackupPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.enabled && self.retain == 0 {
            return Err(MineDockError::InvalidConfiguration(
                "enabled backup policy must retain at least one backup".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct World {
    pub id: WorldId,
    pub name: String,
    pub template_id: TemplateId,
    pub created_at: DateTime<Utc>,
    pub last_played_at: Option<DateTime<Utc>>,
    pub total_playtime_seconds: u64,
    pub status: WorldStatus,
    /// Relative path beneath the configured MineDock app-data root.
    pub data_path: PathBuf,
    pub server_profile_id: ServerProfileId,
    pub server: ServerProfile,
    pub settings: WorldSettings,
    pub backup: BackupPolicy,
}

impl World {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        template_id: TemplateId,
        data_path: PathBuf,
        server: ServerProfile,
        settings: WorldSettings,
        backup: BackupPolicy,
    ) -> Self {
        Self {
            id: WorldId::new(),
            name: name.into(),
            template_id,
            created_at: Utc::now(),
            last_played_at: None,
            total_playtime_seconds: 0,
            status: WorldStatus::Stopped,
            data_path,
            server_profile_id: server.id,
            server,
            settings,
            backup,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_ids(
        id: WorldId,
        profile_id: ServerProfileId,
        name: impl Into<String>,
        template_id: TemplateId,
        data_path: PathBuf,
        mut server: ServerProfile,
        settings: WorldSettings,
        backup: BackupPolicy,
    ) -> Self {
        server.id = profile_id;
        Self {
            id,
            name: name.into(),
            template_id,
            created_at: Utc::now(),
            last_played_at: None,
            total_playtime_seconds: 0,
            status: WorldStatus::Stopped,
            data_path,
            server_profile_id: profile_id,
            server,
            settings,
            backup,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(MineDockError::InvalidConfiguration(
                "world name must not be empty".into(),
            ));
        }
        validate_world_data_path(&self.data_path, self.id)?;
        if self.server_profile_id != self.server.id {
            return Err(MineDockError::InvalidConfiguration(
                "world server_profile_id does not match server profile id".into(),
            ));
        }
        self.server.validate()?;
        self.backup.validate()?;
        Ok(())
    }

    pub fn transition_to(&mut self, next: WorldStatus) -> Result<()> {
        crate::validate_transition(self.status, next)?;
        self.status = next;
        Ok(())
    }
}

/// Reject absolute paths and traversal components before a path is persisted.
pub fn validate_relative_data_path(path: &Path) -> Result<()> {
    let path_text = path.to_string_lossy();
    let has_windows_prefix = path_text.len() >= 3
        && path_text.as_bytes()[1] == b':'
        && matches!(path_text.as_bytes()[2], b'\\' | b'/');
    let is_unc_path = path_text.starts_with("\\\\") || path_text.starts_with("//");
    if path.as_os_str().is_empty() || path.is_absolute() || has_windows_prefix || is_unc_path {
        return Err(MineDockError::InvalidConfiguration(
            "world data path must be a non-empty relative path".into(),
        ));
    }
    if path_text
        .split(['/', '\\'])
        .any(|component| component.is_empty() || component == ".")
    {
        return Err(MineDockError::InvalidConfiguration(
            "world data path must not contain root or current-directory components".into(),
        ));
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(MineDockError::InvalidConfiguration(
                    "world data path must not contain absolute or parent traversal components"
                        .into(),
                ));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Validate the metadata-only world directory shape. A world record may point
/// only to its own `worlds/<world-id>` directory; accepting arbitrary relative
/// paths would allow records to share or target the app-data root later.
pub fn validate_world_data_path(path: &Path, world_id: WorldId) -> Result<()> {
    validate_relative_data_path(path)?;
    let components: Vec<_> = path.components().collect();
    if components.len() != 2
        || !matches!(components[0], Component::Normal(value) if value == "worlds")
    {
        return Err(MineDockError::InvalidConfiguration(
            "world data path must have exactly the shape worlds/<world-id>".into(),
        ));
    }
    let Component::Normal(id_component) = components[1] else {
        return Err(MineDockError::InvalidConfiguration(
            "world data path must have exactly the shape worlds/<world-id>".into(),
        ));
    };
    if id_component != world_id.to_string().as_str() {
        return Err(MineDockError::InvalidConfiguration(
            "world data path id does not match world id".into(),
        ));
    }
    Ok(())
}

pub fn normalize_world_name(value: impl AsRef<str>) -> Result<String> {
    let normalized = value.as_ref().trim();
    if normalized.is_empty() {
        return Err(MineDockError::InvalidConfiguration(
            "world name must not be empty".into(),
        ));
    }
    if normalized.chars().count() > 80 {
        return Err(MineDockError::InvalidConfiguration(
            "world name must be 80 characters or fewer".into(),
        ));
    }
    if normalized == "." || normalized == ".." || normalized.chars().any(char::is_control) {
        return Err(MineDockError::InvalidConfiguration(
            "world name contains an unsafe value".into(),
        ));
    }
    if normalized.contains(['/', '\\', ':']) {
        return Err(MineDockError::InvalidConfiguration(
            "world name must not contain path separators".into(),
        ));
    }
    Ok(normalized.to_owned())
}
