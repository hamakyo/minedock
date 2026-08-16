use crate::{MineDockError, Result, World};
use std::collections::BTreeMap;

/// Generate the small, deterministic server.properties surface MineDock owns.
/// Unknown properties are never copied from templates or user input.
pub fn generate_server_properties(world: &World) -> Result<String> {
    if !world.server.online_mode {
        return Err(MineDockError::InvalidConfiguration(
            "offline server profiles are not supported".into(),
        ));
    }
    if world.server.max_players == 0 || world.server.port == 0 {
        return Err(MineDockError::InvalidConfiguration(
            "server max players and port must be nonzero".into(),
        ));
    }
    let mut properties = BTreeMap::new();
    properties.insert("difficulty", difficulty_value(world.settings.difficulty));
    properties.insert("gamemode", gamemode_value(world.settings.gamemode));
    properties.insert("hardcore", bool_value(world.settings.hardcore));
    properties.insert("max-players", world.server.max_players.to_string());
    properties.insert("white-list", bool_value(world.server.whitelist));
    properties.insert("server-port", world.server.port.to_string());
    properties.insert(
        "level-seed",
        world
            .settings
            .seed
            .as_deref()
            .map(validate_property_value)
            .transpose()?
            .unwrap_or_default(),
    );
    properties.insert(
        "generate-structures",
        bool_value(world.settings.generate_structures),
    );
    properties.insert("pvp", bool_value(world.settings.pvp));
    properties.insert("level-name", "world".into());
    properties.insert("enable-rcon", "false".into());
    properties.insert("online-mode", "true".into());

    let mut output = String::new();
    for (key, value) in properties {
        output.push_str(key);
        output.push('=');
        output.push_str(&value);
        output.push('\n');
    }
    Ok(output)
}

pub fn validate_property_value(value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.len() > 256
        || value
            .chars()
            .any(|c| c == '\0' || c == '\r' || c == '\n' || c == '=' || c == '#' || c == '\\')
    {
        return Err(MineDockError::InvalidConfiguration(
            "server property value contains an unsafe character or is too long".into(),
        ));
    }
    Ok(value.to_owned())
}

fn bool_value(value: bool) -> String {
    value.to_string()
}

fn gamemode_value(value: crate::GameMode) -> String {
    match value {
        crate::GameMode::Survival => "survival",
        crate::GameMode::Creative => "creative",
    }
    .into()
}

fn difficulty_value(value: crate::Difficulty) -> String {
    match value {
        crate::Difficulty::Peaceful => "peaceful",
        crate::Difficulty::Easy => "easy",
        crate::Difficulty::Normal => "normal",
        crate::Difficulty::Hard => "hard",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateWorldRequest, TemplateCatalog};

    #[test]
    fn properties_are_deterministic_and_safe() {
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let world = CreateWorldRequest::new("Props", "hardcore")
            .expect("request")
            .build_world(&catalog)
            .expect("world");
        let first = generate_server_properties(&world).expect("properties");
        let second = generate_server_properties(&world).expect("properties");
        assert_eq!(first, second);
        assert!(first.contains("online-mode=true\n"));
        assert!(first.contains("level-name=world\n"));
        assert!(first.contains("enable-rcon=false\n"));
    }

    #[test]
    fn property_injection_and_offline_profiles_are_rejected() {
        assert!(validate_property_value("ok\nattack=true").is_err());
        let catalog = TemplateCatalog::built_in().expect("catalog");
        let mut world = CreateWorldRequest::new("Props", "hardcore")
            .expect("request")
            .build_world(&catalog)
            .expect("world");
        world.server.online_mode = false;
        assert!(generate_server_properties(&world).is_err());
    }
}
