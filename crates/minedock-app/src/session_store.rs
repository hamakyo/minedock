//! Asynchronous-worker-facing persistence for server sessions.
//!
//! This module performs only filesystem work.  Callers must invoke it from a
//! GPUI background task, never while rendering or handling a click.

use atomic_write_file::AtomicWriteFile;
use chrono::Utc;
use minedock_core::{
    PlayerActivitySnapshot, PlayerActivityTracker, RawLogRecord, ServerEvent, ServerSessionRecord,
    SessionExitReason, SessionId, WorldId, parse_vanilla_log_line,
};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const SESSION_DIR_NAME: &str = "logs";
const RAW_LOG_FILE_NAME: &str = "server.jsonl";
const SESSION_FILE_NAME: &str = "session.json";
const ACTIVITY_FILE_NAME: &str = "activity.json";
const MAX_RECENT_PERSISTED_LOGS: usize = 80;

#[derive(Debug, Clone)]
pub struct SessionLogStore {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub record: ServerSessionRecord,
    pub logs: Vec<RawLogRecord>,
    pub activity: PlayerActivitySnapshot,
}

impl SessionLogStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn begin_session(
        &self,
        world_id: WorldId,
        session_id: SessionId,
        pid: Option<u32>,
    ) -> minedock_core::Result<()> {
        let directory = self.session_dir(world_id, session_id)?;
        self.prepare_directory(&directory)?;
        let session_path = directory.join(SESSION_FILE_NAME);
        if session_path.exists() {
            return Err(minedock_core::MineDockError::Persistence(
                "refusing to overwrite an existing server session record".into(),
            ));
        }
        write_json_atomic(
            &session_path,
            &ServerSessionRecord::started(world_id, session_id, pid),
        )?;
        let raw_path = directory.join(RAW_LOG_FILE_NAME);
        validate_safe_path(&raw_path)?;
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&raw_path)
            .map_err(|error| {
                minedock_core::MineDockError::Persistence(format!(
                    "could not create session raw log: {error}"
                ))
            })?;
        write_json_atomic(
            &directory.join(ACTIVITY_FILE_NAME),
            &PlayerActivitySnapshot::default(),
        )
    }

    pub fn append_events(
        &self,
        world_id: WorldId,
        session_id: SessionId,
        events: &[ServerEvent],
    ) -> minedock_core::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let directory = self.session_dir(world_id, session_id)?;
        let session_path = directory.join(SESSION_FILE_NAME);
        let mut session = read_json::<ServerSessionRecord>(&session_path)?;
        session.validate()?;
        if session.world_id != world_id || session.session_id != session_id.to_string() {
            return Err(minedock_core::MineDockError::Persistence(
                "session event identity does not match its record".into(),
            ));
        }
        if session.ended_at.is_some() {
            return Err(minedock_core::MineDockError::Persistence(
                "cannot append events to a finalized session".into(),
            ));
        }
        let raw_path = directory.join(RAW_LOG_FILE_NAME);
        validate_safe_path(&raw_path)?;
        let mut raw_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&raw_path)
            .map_err(|error| {
                minedock_core::MineDockError::Persistence(format!(
                    "could not open session raw log: {error}"
                ))
            })?;
        let activity_path = directory.join(ACTIVITY_FILE_NAME);
        let activity = if activity_path.exists() {
            read_json::<PlayerActivitySnapshot>(&activity_path)?
        } else {
            PlayerActivitySnapshot::default()
        };
        let mut tracker = PlayerActivityTracker::from_snapshot(activity).ok_or_else(|| {
            minedock_core::MineDockError::Persistence(
                "session activity snapshot is corrupt or unsupported".into(),
            )
        })?;
        let mut changed_activity = false;
        let mut changed_session = false;
        for event in events {
            let timestamp = Utc::now();
            match event {
                ServerEvent::Raw(raw) => {
                    if raw.world_id != world_id || raw.session_id != session_id {
                        return Err(minedock_core::MineDockError::Persistence(
                            "raw event identity does not match its session".into(),
                        ));
                    }
                    let record = RawLogRecord::from_event(timestamp, raw)?;
                    let line = serde_json::to_string(&record).map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not serialize raw log record: {error}"
                        ))
                    })?;
                    raw_file.write_all(line.as_bytes()).map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not append raw log record: {error}"
                        ))
                    })?;
                    raw_file.write_all(b"\n").map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not append raw log separator: {error}"
                        ))
                    })?;
                    if raw.stream == minedock_core::LogStream::Stdout {
                        if let Some(parsed) = parse_vanilla_log_line(&raw.line) {
                            changed_activity |= tracker.apply(timestamp, parsed);
                        }
                    }
                }
                ServerEvent::RawEventsDropped { count } => {
                    let record = RawLogRecord {
                        schema_version: minedock_core::RAW_LOG_SCHEMA_VERSION,
                        timestamp,
                        world_id,
                        session_id: session_id.to_string(),
                        stream: minedock_core::LogStream::Stderr,
                        text: format!("[MineDock] {count} raw log lines were dropped"),
                        truncated: false,
                    };
                    record.validate()?;
                    let line = serde_json::to_string(&record).map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not serialize dropped-log record: {error}"
                        ))
                    })?;
                    raw_file.write_all(line.as_bytes()).map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not append dropped-log record: {error}"
                        ))
                    })?;
                    raw_file.write_all(b"\n").map_err(|error| {
                        minedock_core::MineDockError::Persistence(format!(
                            "could not append dropped-log separator: {error}"
                        ))
                    })?;
                }
                ServerEvent::Reliable(reliable) => {
                    if let minedock_core::ReliableLifecycleEvent::Spawned { pid } = reliable {
                        if session.pid.is_none() {
                            session.pid = Some(*pid);
                            changed_session = true;
                        }
                    }
                }
            }
        }
        raw_file.flush().map_err(|error| {
            minedock_core::MineDockError::Persistence(format!(
                "could not flush session raw log: {error}"
            ))
        })?;
        raw_file.sync_all().map_err(|error| {
            minedock_core::MineDockError::Persistence(format!(
                "could not sync session raw log: {error}"
            ))
        })?;
        if changed_activity {
            write_json_atomic(&activity_path, &tracker.snapshot())?;
        }
        if changed_session {
            write_json_atomic(&session_path, &session)?;
        }
        Ok(())
    }

    pub fn finalize(
        &self,
        world_id: WorldId,
        session_id: SessionId,
        reason: SessionExitReason,
    ) -> minedock_core::Result<()> {
        let path = self
            .session_dir(world_id, session_id)?
            .join(SESSION_FILE_NAME);
        let mut record = read_json::<ServerSessionRecord>(&path)?;
        record.validate()?;
        if record.world_id != world_id || record.session_id != session_id.to_string() {
            return Err(minedock_core::MineDockError::Persistence(
                "cannot finalize a session with mismatched identity".into(),
            ));
        }
        let ended_at = Utc::now();
        let activity_path = path
            .parent()
            .ok_or_else(|| {
                minedock_core::MineDockError::Persistence(
                    "session record has no parent directory".into(),
                )
            })?
            .join(ACTIVITY_FILE_NAME);
        let activity = if activity_path.exists() {
            read_json::<PlayerActivitySnapshot>(&activity_path)?
        } else {
            PlayerActivitySnapshot::default()
        };
        let mut tracker = PlayerActivityTracker::from_snapshot(activity).ok_or_else(|| {
            minedock_core::MineDockError::Persistence(
                "session activity snapshot is corrupt or unsupported".into(),
            )
        })?;
        tracker.finalize(ended_at);
        // Persist the closed activity projection before marking the session
        // complete. If the process dies between these two atomic writes,
        // startup reconciliation can safely finalize the already-closed
        // projection again because there are no open intervals left.
        write_json_atomic(&activity_path, &tracker.snapshot())?;
        record.finalize(ended_at, reason)?;
        write_json_atomic(&path, &record)
    }

    pub fn reconcile_unfinished(&self) -> minedock_core::Result<usize> {
        let worlds_root = self.root.join("worlds");
        if !worlds_root.exists() {
            return Ok(0);
        }
        validate_safe_path(&worlds_root)?;
        let mut recovered = 0;
        for world_entry in fs::read_dir(&worlds_root).map_err(io_persistence)? {
            let world_entry = world_entry.map_err(io_persistence)?;
            let world_path = world_entry.path();
            let metadata = fs::symlink_metadata(&world_path).map_err(io_persistence)?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(minedock_core::MineDockError::Persistence(
                    "world data contains a symlink or reparse point".into(),
                ));
            }
            if !metadata.is_dir() {
                continue;
            }
            let Some(world_name) = world_path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            let Ok(world_id) = minedock_core::WorldId::parse(world_name) else {
                continue;
            };
            let logs_root = world_path.join(SESSION_DIR_NAME);
            if !logs_root.exists() {
                continue;
            }
            validate_safe_path(&logs_root)?;
            for session_entry in fs::read_dir(&logs_root).map_err(io_persistence)? {
                let session_entry = session_entry.map_err(io_persistence)?;
                let session_path = session_entry.path();
                let metadata = fs::symlink_metadata(&session_path).map_err(io_persistence)?;
                if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                    return Err(minedock_core::MineDockError::Persistence(
                        "session logs contain a symlink or reparse point".into(),
                    ));
                }
                if !metadata.is_dir() {
                    continue;
                }
                let record_path = session_path.join(SESSION_FILE_NAME);
                if !record_path.exists() {
                    continue;
                }
                let record = read_json::<ServerSessionRecord>(&record_path)?;
                record.validate()?;
                if record.world_id != world_id {
                    return Err(minedock_core::MineDockError::Persistence(
                        "session record world identity does not match its directory".into(),
                    ));
                }
                if record.ended_at.is_none() {
                    self.finalize(
                        record.world_id,
                        SessionId::parse(&record.session_id)?,
                        SessionExitReason::Interrupted,
                    )?;
                    recovered += 1;
                }
            }
        }
        Ok(recovered)
    }

    pub fn latest_snapshot(
        &self,
        world_id: WorldId,
    ) -> minedock_core::Result<Option<SessionSnapshot>> {
        let logs_root = self
            .root
            .join("worlds")
            .join(world_id.to_string())
            .join(SESSION_DIR_NAME);
        if !logs_root.exists() {
            return Ok(None);
        }
        validate_safe_path(&logs_root)?;
        let mut latest: Option<ServerSessionRecord> = None;
        for entry in fs::read_dir(&logs_root).map_err(io_persistence)? {
            let entry = entry.map_err(io_persistence)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_persistence)?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(minedock_core::MineDockError::Persistence(
                    "session logs contain a symlink or reparse point".into(),
                ));
            }
            if !metadata.is_dir() {
                continue;
            }
            let record_path = path.join(SESSION_FILE_NAME);
            if !record_path.exists() {
                continue;
            }
            let record = read_json::<ServerSessionRecord>(&record_path)?;
            record.validate()?;
            if record.world_id != world_id {
                return Err(minedock_core::MineDockError::Persistence(
                    "session record world identity does not match its directory".into(),
                ));
            }
            if latest
                .as_ref()
                .is_none_or(|current| record.started_at > current.started_at)
            {
                latest = Some(record);
            }
        }
        let Some(record) = latest else {
            return Ok(None);
        };
        let session_id = SessionId::parse(&record.session_id)?;
        let directory = self.session_dir(world_id, session_id)?;
        let logs = read_recent_logs(&directory.join(RAW_LOG_FILE_NAME))?;
        let activity_path = directory.join(ACTIVITY_FILE_NAME);
        let activity = if activity_path.exists() {
            read_json(&activity_path)?
        } else {
            PlayerActivitySnapshot::default()
        };
        Ok(Some(SessionSnapshot {
            record,
            logs,
            activity,
        }))
    }

    fn session_dir(
        &self,
        world_id: WorldId,
        session_id: SessionId,
    ) -> minedock_core::Result<PathBuf> {
        let path = self
            .root
            .join("worlds")
            .join(world_id.to_string())
            .join(SESSION_DIR_NAME)
            .join(session_id.to_string());
        validate_safe_path(&path)?;
        Ok(path)
    }

    fn prepare_directory(&self, path: &Path) -> minedock_core::Result<()> {
        validate_safe_path(path)?;
        fs::create_dir_all(path).map_err(io_persistence)?;
        validate_safe_path(path)
    }
}

fn read_recent_logs(path: &Path) -> minedock_core::Result<Vec<RawLogRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    validate_safe_path(path)?;
    let file = File::open(path).map_err(io_persistence)?;
    let reader = BufReader::new(file);
    let mut logs = VecDeque::new();
    for line in reader.lines() {
        let line = line.map_err(io_persistence)?;
        let record: RawLogRecord = serde_json::from_str(&line).map_err(|error| {
            minedock_core::MineDockError::Persistence(format!(
                "raw session log is corrupt: {error}"
            ))
        })?;
        record.validate()?;
        logs.push_back(record);
        while logs.len() > MAX_RECENT_PERSISTED_LOGS {
            let _ = logs.pop_front();
        }
    }
    Ok(logs.into_iter().collect())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> minedock_core::Result<()> {
    validate_safe_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_persistence)?;
        validate_safe_path(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        minedock_core::MineDockError::Persistence(format!(
            "could not serialize session record: {error}"
        ))
    })?;
    let mut file = AtomicWriteFile::open(path).map_err(io_persistence)?;
    file.write_all(&bytes).map_err(io_persistence)?;
    file.flush().map_err(io_persistence)?;
    file.commit().map_err(io_persistence)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> minedock_core::Result<T> {
    validate_safe_path(path)?;
    let bytes = fs::read(path).map_err(io_persistence)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        minedock_core::MineDockError::Persistence(format!("session record is corrupt: {error}"))
    })
}

fn io_persistence(error: std::io::Error) -> minedock_core::MineDockError {
    minedock_core::MineDockError::Persistence(error.to_string())
}

fn validate_safe_path(path: &Path) -> minedock_core::Result<()> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                return Err(minedock_core::MineDockError::Persistence(
                    "session path contains a symlink or reparse point".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_persistence(error)),
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

use std::collections::VecDeque;

#[cfg(test)]
mod tests {
    use super::*;
    use minedock_core::{LogStream, RawLogLine, ServerEvent};
    use tempfile::TempDir;

    #[test]
    fn session_events_round_trip_and_reconcile() {
        let root = TempDir::new().expect("root");
        let store = SessionLogStore::new(root.path());
        let world_id = WorldId::new();
        let session_id = SessionId::new();
        store
            .begin_session(world_id, session_id, Some(42))
            .expect("begin");
        store
            .append_events(
                world_id,
                session_id,
                &[ServerEvent::Raw(RawLogLine {
                    world_id,
                    session_id,
                    stream: LogStream::Stdout,
                    line: "[Server thread/INFO]: Alex joined the game".into(),
                    truncated: false,
                })],
            )
            .expect("append");
        let snapshot = store
            .latest_snapshot(world_id)
            .expect("snapshot")
            .expect("one");
        assert_eq!(snapshot.logs.len(), 1);
        assert_eq!(snapshot.activity.current_players, vec!["Alex"]);
        assert_eq!(store.reconcile_unfinished().expect("reconcile"), 1);
        let snapshot = store
            .latest_snapshot(world_id)
            .expect("snapshot")
            .expect("one");
        assert!(snapshot.record.ended_at.is_some());
    }

    #[test]
    fn finalizing_a_session_closes_open_player_activity() {
        let root = TempDir::new().expect("root");
        let store = SessionLogStore::new(root.path());
        let world_id = WorldId::new();
        let session_id = SessionId::new();
        store
            .begin_session(world_id, session_id, Some(42))
            .expect("begin");
        store
            .append_events(
                world_id,
                session_id,
                &[ServerEvent::Raw(RawLogLine {
                    world_id,
                    session_id,
                    stream: LogStream::Stdout,
                    line: "[Server thread/INFO]: Alex joined the game".into(),
                    truncated: false,
                })],
            )
            .expect("join");
        store
            .finalize(
                world_id,
                session_id,
                SessionExitReason::Graceful { code: Some(0) },
            )
            .expect("finalize");
        let snapshot = store
            .latest_snapshot(world_id)
            .expect("snapshot")
            .expect("one");
        assert!(snapshot.activity.current_players.is_empty());
        assert!(snapshot.activity.current_player_joined_at.is_empty());
    }
}
