use crate::{MineDockError, Result, WorldStatus};

pub fn can_transition(from: WorldStatus, to: WorldStatus) -> bool {
    use WorldStatus::*;

    matches!(
        (from, to),
        // Stopped is safe to request repeatedly; all other transitions must
        // represent a real lifecycle edge.
        (Stopped, Stopped)
            | (Stopped, Preparing)
            | (Preparing, Starting)
            | (Starting, Running)
            | (Running, Stopping)
            | (Stopping, BackingUp)
            | (Stopping, Stopped)
            | (BackingUp, Stopped)
            | (Failed, Stopped)
            | (Preparing, Failed)
            | (Starting, Failed)
            | (Running, Failed)
            | (Stopping, Failed)
            | (BackingUp, Failed)
    )
}

pub fn validate_transition(from: WorldStatus, to: WorldStatus) -> Result<()> {
    if can_transition(from, to) {
        Ok(())
    } else {
        Err(MineDockError::InvalidLifecycleTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_start_and_stop_path_is_valid() {
        let path = [
            WorldStatus::Stopped,
            WorldStatus::Preparing,
            WorldStatus::Starting,
            WorldStatus::Running,
            WorldStatus::Stopping,
            WorldStatus::BackingUp,
            WorldStatus::Stopped,
        ];

        for pair in path.windows(2) {
            assert!(validate_transition(pair[0], pair[1]).is_ok());
        }
    }

    #[test]
    fn cannot_jump_from_stopped_to_running() {
        assert!(validate_transition(WorldStatus::Stopped, WorldStatus::Running).is_err());
    }

    #[test]
    fn self_transitions_are_not_blanket_allowed() {
        assert!(validate_transition(WorldStatus::Stopped, WorldStatus::Stopped).is_ok());
        assert!(validate_transition(WorldStatus::Running, WorldStatus::Running).is_err());
        assert!(validate_transition(WorldStatus::Failed, WorldStatus::Failed).is_err());
    }

    #[test]
    fn invalid_transition_does_not_mutate_world() {
        let catalog = crate::TemplateCatalog::built_in().expect("built-ins");
        let request = crate::CreateWorldRequest::new("Lifecycle", "creative").expect("request");
        let mut world = request.build_world(&catalog).expect("world");
        assert_eq!(world.status, WorldStatus::Stopped);
        assert!(world.transition_to(WorldStatus::Running).is_err());
        assert_eq!(world.status, WorldStatus::Stopped);
        world
            .transition_to(WorldStatus::Preparing)
            .expect("preparing");
        assert!(world.transition_to(WorldStatus::Stopped).is_err());
        assert_eq!(world.status, WorldStatus::Preparing);
    }
}
