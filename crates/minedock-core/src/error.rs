use thiserror::Error;

#[derive(Debug, Error)]
pub enum MineDockError {
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("invalid lifecycle transition: {from:?} -> {to:?}")]
    InvalidLifecycleTransition {
        from: crate::WorldStatus,
        to: crate::WorldStatus,
    },

    #[error("template parse failed: {0}")]
    TemplateParse(#[from] serde_yaml::Error),

    #[error("persistence failed: {0}")]
    Persistence(String),

    #[error("Java runtime is unavailable: {0}")]
    JavaUnavailable(String),

    #[error("Minecraft download failed: {0}")]
    Download(String),

    #[error("artifact verification failed: {0}")]
    Verification(String),

    #[error("Minecraft EULA acceptance is required before this operation")]
    EulaRequired,

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("operation timed out: {0}")]
    Timeout(String),

    #[error("invalid process state: {0}")]
    InvalidState(String),

    #[error("process start failed: {0}")]
    ProcessStart(String),

    #[error("process control failed: {0}")]
    ProcessControl(String),

    #[error("server process failed: {0}")]
    Process(String),

    #[error("backup failed: {0}")]
    Backup(String),
}

pub type Result<T> = std::result::Result<T, MineDockError>;
