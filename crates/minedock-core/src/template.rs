use crate::{
    BackupPolicy, Difficulty, GameMode, MineDockError, MinecraftEdition, MinecraftVersionSelector,
    Result, ServerDistribution, ServerProfile, ServerProfileId, TemplateId, World, WorldId,
    WorldSettings, normalize_world_name,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldTemplate {
    pub schema_version: u16,
    pub id: TemplateId,
    pub name: String,
    pub description: String,
    pub minecraft: TemplateMinecraft,
    pub server: TemplateServer,
    pub world: TemplateWorld,
    pub backup: BackupPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateMinecraft {
    pub edition: MinecraftEdition,
    pub distribution: ServerDistribution,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateServer {
    pub max_players: u16,
    pub online_mode: bool,
    pub whitelist: bool,
    pub port: u16,
    pub memory_min_mb: u32,
    pub memory_max_mb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateWorld {
    pub gamemode: GameMode,
    pub difficulty: Difficulty,
    pub hardcore: bool,
    pub seed: Option<String>,
    pub generate_structures: bool,
    pub pvp: bool,
}

impl WorldTemplate {
    pub fn parse_yaml(input: &str) -> Result<Self> {
        let template: Self = serde_yaml::from_str(input)?;
        template.validate()?;
        Ok(template)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(MineDockError::InvalidConfiguration(format!(
                "unsupported template schema version {}",
                self.schema_version
            )));
        }
        if self.name.trim().is_empty() {
            return Err(MineDockError::InvalidConfiguration(
                "template name must not be empty".into(),
            ));
        }
        MinecraftVersionSelector::parse(&self.minecraft.version)?;
        if self.server.max_players == 0 {
            return Err(MineDockError::InvalidConfiguration(
                "max_players must be greater than zero".into(),
            ));
        }
        if self.server.port == 0 {
            return Err(MineDockError::InvalidConfiguration(
                "port must be greater than zero".into(),
            ));
        }
        if self.server.memory_min_mb == 0 || self.server.memory_max_mb < self.server.memory_min_mb {
            return Err(MineDockError::InvalidConfiguration(
                "memory range is invalid".into(),
            ));
        }
        if !self.server.online_mode {
            return Err(MineDockError::InvalidConfiguration(
                "online_mode must remain enabled for safe defaults".into(),
            ));
        }
        self.backup.validate()?;
        Ok(())
    }

    pub fn server_profile(&self) -> ServerProfile {
        ServerProfile {
            id: ServerProfileId::new(),
            edition: self.minecraft.edition,
            distribution: self.minecraft.distribution,
            version: self.minecraft.version.clone(),
            max_players: self.server.max_players,
            online_mode: self.server.online_mode,
            whitelist: self.server.whitelist,
            port: self.server.port,
            memory_min_mb: self.server.memory_min_mb,
            memory_max_mb: self.server.memory_max_mb,
        }
    }

    pub fn world_settings(&self) -> WorldSettings {
        WorldSettings {
            gamemode: self.world.gamemode,
            difficulty: self.world.difficulty,
            hardcore: self.world.hardcore,
            seed: self.world.seed.clone(),
            generate_structures: self.world.generate_structures,
            pvp: self.world.pvp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateCatalog {
    templates: Vec<WorldTemplate>,
}

impl TemplateCatalog {
    pub fn new(templates: impl IntoIterator<Item = WorldTemplate>) -> Result<Self> {
        let templates: Vec<_> = templates.into_iter().collect();
        let mut ids = std::collections::HashSet::with_capacity(templates.len());
        for template in &templates {
            template.validate()?;
            if !ids.insert(template.id.clone()) {
                return Err(MineDockError::InvalidConfiguration(format!(
                    "duplicate template id {}",
                    template.id
                )));
            }
        }
        Ok(Self { templates })
    }

    /// Built-ins are embedded at compile time and do not depend on the
    /// process working directory.
    pub fn built_in() -> Result<Self> {
        Self::new([
            WorldTemplate::parse_yaml(include_str!("../../../templates/vanilla-survival.yml"))?,
            WorldTemplate::parse_yaml(include_str!("../../../templates/hardcore.yml"))?,
            WorldTemplate::parse_yaml(include_str!("../../../templates/creative.yml"))?,
        ])
    }

    pub fn templates(&self) -> &[WorldTemplate] {
        &self.templates
    }

    pub fn iter(&self) -> impl Iterator<Item = &WorldTemplate> {
        self.templates.iter()
    }

    pub fn get(&self, id: impl AsRef<str>) -> Option<&WorldTemplate> {
        self.templates
            .iter()
            .find(|template| template.id.as_str() == id.as_ref())
    }

    pub fn get_by_id(&self, id: impl AsRef<str>) -> Option<&WorldTemplate> {
        self.get(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateWorldRequest {
    pub name: String,
    pub template_id: TemplateId,
    pub seed: Option<String>,
}

impl CreateWorldRequest {
    pub fn new(name: impl Into<String>, template_id: impl AsRef<str>) -> Result<Self> {
        Ok(Self {
            name: name.into(),
            template_id: TemplateId::parse(template_id)?,
            seed: None,
        })
    }

    pub fn try_new(
        name: impl Into<String>,
        template_id: impl TryInto<TemplateId, Error = MineDockError>,
    ) -> Result<Self> {
        Ok(Self {
            name: name.into(),
            template_id: template_id.try_into()?,
            seed: None,
        })
    }

    pub fn build_world(&self, catalog: &TemplateCatalog) -> Result<World> {
        let name = normalize_world_name(&self.name)?;
        let template = catalog.get(&self.template_id).ok_or_else(|| {
            MineDockError::InvalidConfiguration(format!(
                "template {} is not available",
                self.template_id
            ))
        })?;
        let world_id = WorldId::new();
        let profile_id = ServerProfileId::new();
        let mut settings = template.world_settings();
        if let Some(seed) = &self.seed {
            let seed = seed.trim();
            if seed.chars().count() > 64 || seed.chars().any(char::is_control) {
                return Err(MineDockError::InvalidConfiguration(
                    "world seed must be 64 characters or fewer and contain no control characters"
                        .into(),
                ));
            }
            settings.seed = (!seed.is_empty()).then(|| seed.to_owned());
        }
        let server = template.server_profile();
        Ok(World::with_ids(
            world_id,
            profile_id,
            name,
            template.id.clone(),
            PathBuf::from("worlds").join(world_id.to_string()),
            server,
            settings,
            template.backup.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_have_exact_safe_defaults() {
        let catalog = TemplateCatalog::built_in().expect("built-ins should parse");
        assert_eq!(catalog.templates().len(), 3);
        let survival = catalog.get_by_id("vanilla-survival").expect("survival");
        assert_eq!(survival.world.gamemode, GameMode::Survival);
        assert_eq!(survival.world.difficulty, Difficulty::Normal);
        assert!(!survival.world.hardcore);
        assert_eq!(survival.server.max_players, 4);
        assert!(survival.server.online_mode);
        assert!(survival.server.whitelist);

        let hardcore = catalog.get_by_id("hardcore").expect("hardcore");
        assert_eq!(hardcore.world.gamemode, GameMode::Survival);
        assert_eq!(hardcore.world.difficulty, Difficulty::Hard);
        assert!(hardcore.world.hardcore);
        assert!(hardcore.server.online_mode);
        assert!(hardcore.server.whitelist);

        let creative = catalog.get_by_id("creative").expect("creative");
        assert_eq!(creative.world.gamemode, GameMode::Creative);
        assert_eq!(creative.world.difficulty, Difficulty::Peaceful);
        assert!(!creative.world.hardcore);
        assert!(creative.server.online_mode);
        assert!(creative.server.whitelist);
    }

    #[test]
    fn executable_like_fields_are_rejected() {
        let input = include_str!("../../../templates/hardcore.yml").replace(
            "description: One-life survival for a small private group.",
            "description: One-life survival for a small private group.\nexecutable: java",
        );
        assert!(WorldTemplate::parse_yaml(&input).is_err());

        let input = include_str!("../../../templates/hardcore.yml").replace(
            "  version: current-release",
            "  version: current-release\n  download_url: https://example.invalid/server.jar",
        );
        assert!(WorldTemplate::parse_yaml(&input).is_err());
    }

    #[test]
    fn malformed_template_values_fail_closed() {
        let source = include_str!("../../../templates/hardcore.yml");
        for (needle, replacement) in [
            ("schema_version: 1", "schema_version: 2"),
            ("  max_players: 4", "  max_players: 0"),
            ("  port: 25565", "  port: 0"),
            (
                "  memory_min_mb: 1024",
                "  memory_min_mb: 4096\n  memory_max_mb: 1024",
            ),
            ("  version: current-release", "  version: ''"),
        ] {
            let input = source.replacen(needle, replacement, 1);
            assert!(
                WorldTemplate::parse_yaml(&input).is_err(),
                "replacement {replacement:?} should fail"
            );
        }

        let invalid_id = source.replace("id: hardcore", "id: ../hardcore");
        assert!(WorldTemplate::parse_yaml(&invalid_id).is_err());
        let offline = source.replace("online_mode: true", "online_mode: false");
        assert!(WorldTemplate::parse_yaml(&offline).is_err());
    }

    #[test]
    fn invalid_template_id_cannot_enter_world_construction() {
        assert!(TemplateId::parse("../unsafe").is_err());
        assert!(TemplateId::parse("UpperCase").is_err());
    }

    #[test]
    fn duplicate_template_ids_are_rejected() {
        let hardcore = WorldTemplate::parse_yaml(include_str!("../../../templates/hardcore.yml"))
            .expect("hardcore");
        let mut duplicate = hardcore.clone();
        duplicate.name = "Duplicate".into();
        assert!(TemplateCatalog::new([hardcore, duplicate]).is_err());
    }

    #[test]
    fn creation_uses_stopped_state_and_relative_path() {
        let catalog = TemplateCatalog::built_in().expect("built-ins should parse");
        let request = CreateWorldRequest::new("  Sunday  ", "hardcore").expect("request");
        let world = request.build_world(&catalog).expect("world");
        assert_eq!(world.name, "Sunday");
        assert_eq!(world.status, crate::WorldStatus::Stopped);
        assert!(world.data_path.is_relative());
        assert!(world.data_path.starts_with("worlds"));
        assert_eq!(world.template_id.as_str(), "hardcore");
    }
}
