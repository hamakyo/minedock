//! Schema-versioned server session and raw-log records.
//!
//! The process adapter remains the only owner of stdout/stderr readers.  The
//! app persists the events it receives from that adapter through these small,
//! process-independent records.  Lifecycle state is intentionally not derived
//! from the raw log file.

use crate::{LogStream, MineDockError, RawLogLine, Result, SessionId, WorldId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SERVER_SESSION_SCHEMA_VERSION: u16 = 1;
pub const RAW_LOG_SCHEMA_VERSION: u16 = 1;
pub const MAX_PERSISTED_LOG_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_SESSION_ID_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionExitReason {
    Graceful { code: Option<i32> },
    GracefulAfterTimeout { code: Option<i32> },
    ForceStopped { code: Option<i32> },
    Unexpected { code: Option<i32>, success: bool },
    Interrupted,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSessionRecord {
    pub schema_version: u16,
    pub world_id: WorldId,
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub pid: Option<u32>,
    pub exit_reason: Option<SessionExitReason>,
}

impl ServerSessionRecord {
    pub fn started(world_id: WorldId, session_id: SessionId, pid: Option<u32>) -> Self {
        Self {
            schema_version: SERVER_SESSION_SCHEMA_VERSION,
            world_id,
            session_id: session_id.to_string(),
            started_at: Utc::now(),
            ended_at: None,
            pid,
            exit_reason: None,
        }
    }

    pub fn finalize(&mut self, ended_at: DateTime<Utc>, reason: SessionExitReason) -> Result<()> {
        if ended_at < self.started_at {
            return Err(MineDockError::Persistence(
                "session end time precedes session start time".into(),
            ));
        }
        if self.ended_at.is_some() || self.exit_reason.is_some() {
            return Err(MineDockError::Persistence(
                "server session has already been finalized".into(),
            ));
        }
        self.ended_at = Some(ended_at);
        self.exit_reason = Some(reason);
        self.validate()
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SERVER_SESSION_SCHEMA_VERSION {
            return Err(MineDockError::Persistence(
                "unsupported server session schema version".into(),
            ));
        }
        if self.session_id.is_empty() || self.session_id.len() > MAX_SESSION_ID_BYTES {
            return Err(MineDockError::Persistence(
                "server session id is outside the allowed bound".into(),
            ));
        }
        SessionId::parse(&self.session_id)?;
        if self.ended_at.is_some() != self.exit_reason.is_some() {
            return Err(MineDockError::Persistence(
                "server session end fields must be present together".into(),
            ));
        }
        if let Some(ended_at) = self.ended_at {
            if ended_at < self.started_at {
                return Err(MineDockError::Persistence(
                    "session end time precedes session start time".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLogRecord {
    pub schema_version: u16,
    pub timestamp: DateTime<Utc>,
    pub world_id: WorldId,
    pub session_id: String,
    pub stream: LogStream,
    pub text: String,
    pub truncated: bool,
}

impl RawLogRecord {
    pub fn from_event(timestamp: DateTime<Utc>, event: &RawLogLine) -> Result<Self> {
        let text = sanitize_log_text(&event.line, event.truncated)?;
        let record = Self {
            schema_version: RAW_LOG_SCHEMA_VERSION,
            timestamp,
            world_id: event.world_id,
            session_id: event.session_id.to_string(),
            stream: event.stream,
            text,
            truncated: event.truncated,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RAW_LOG_SCHEMA_VERSION {
            return Err(MineDockError::Persistence(
                "unsupported raw log schema version".into(),
            ));
        }
        if self.session_id.is_empty() || self.session_id.len() > MAX_SESSION_ID_BYTES {
            return Err(MineDockError::Persistence(
                "raw log session id is outside the allowed bound".into(),
            ));
        }
        SessionId::parse(&self.session_id)?;
        if self.text.len() > MAX_PERSISTED_LOG_TEXT_BYTES {
            return Err(MineDockError::Persistence(
                "raw log text exceeds the allowed bound".into(),
            ));
        }
        if self.text.chars().any(char::is_control) {
            return Err(MineDockError::Persistence(
                "raw log text contains a control character".into(),
            ));
        }
        Ok(())
    }
}

pub fn sanitize_log_text(value: &str, truncated: bool) -> Result<String> {
    let mut text = value.replace('\0', "�");
    for key in [
        "access_token=",
        "refresh_token=",
        "authorization=",
        "password=",
        "token=",
        "secret=",
    ] {
        redact_key_value(&mut text, key);
    }
    if text.len() > MAX_PERSISTED_LOG_TEXT_BYTES {
        text.truncate(MAX_PERSISTED_LOG_TEXT_BYTES);
        if !truncated {
            return Err(MineDockError::Persistence(
                "raw log text exceeded its declared bound".into(),
            ));
        }
    }
    if text.chars().any(|character| character.is_control()) {
        return Err(MineDockError::Persistence(
            "raw log text contains a control character".into(),
        ));
    }
    Ok(text)
}

fn redact_key_value(text: &mut String, key: &str) {
    let mut search_from = 0;
    while let Some(relative) = text[search_from..].find(key) {
        let start = search_from + relative + key.len();
        let end = text[start..]
            .find(|character: char| {
                character.is_whitespace() || character == ',' || character == ';'
            })
            .map_or(text.len(), |offset| start + offset);
        text.replace_range(start..end, "[REDACTED]");
        search_from = start + "[REDACTED]".len();
        if search_from >= text.len() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RawLogLine;

    #[test]
    fn session_record_requires_a_paired_end_reason() {
        let world_id = WorldId::new();
        let mut record = ServerSessionRecord::started(world_id, SessionId::new(), Some(42));
        assert!(record.validate().is_ok());
        record.ended_at = Some(Utc::now());
        assert!(record.validate().is_err());
    }

    #[test]
    fn raw_logs_are_bounded_and_credentials_are_redacted() {
        let raw = RawLogLine {
            world_id: WorldId::new(),
            session_id: SessionId::new(),
            stream: LogStream::Stdout,
            line: "hello access_token=secret world".into(),
            truncated: false,
        };
        let record = RawLogRecord::from_event(Utc::now(), &raw).expect("record");
        assert!(record.text.contains("access_token=[REDACTED]"));
        assert!(!record.text.contains("secret"));
    }
}
