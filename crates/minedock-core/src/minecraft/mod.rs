//! Minecraft-version, Vanilla-provider, and deterministic configuration
//! boundaries.  None of these APIs perform network I/O unless a caller
//! explicitly supplies a transport and invokes a provider operation.

pub mod properties;
pub mod provider;

pub mod vanilla {
    pub use super::provider::*;
}

pub use properties::*;
pub use provider::*;

pub mod version {
    pub use crate::domain::{MinecraftVersionId, MinecraftVersionSelector};
    pub use crate::runtime::{
        JavaCompatibility, JavaMajor, JavaRequirement, JavaRuntime, JavaUnavailableReason,
        JavaVersion,
    };
}
