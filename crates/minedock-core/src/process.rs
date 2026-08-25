//! Process-independent launch and lifecycle contracts.
//!
//! Native process, pipe, job-object, and file-lock mechanics live in the app
//! adapter.  This module contains only validated values, events, and a
//! generic supervisor over injected process/persistence seams.

use crate::{
    JavaRuntime, MineDockError, ProvisionedServer, Result, ServerProfile, WorldId, WorldStatus,
    can_transition,
};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

pub const MAX_SERVER_COMMAND_BYTES: usize = 8 * 1024;
pub const MAX_RAW_LOG_LINE_BYTES: usize = 16 * 1024;
pub const RAW_EVENT_CHANNEL_CAPACITY: usize = 256;

pub mod events {
    pub use super::{
        LogStream, RawLogLine, ReliableLifecycleEvent, ServerEvent, StopEscalationToken,
        StopOutcome,
    };
}

/// A native-independent exit projection.  The app adapter translates the
/// platform's exit status into this value before returning it to the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: PathBuf,
    pub world_id: WorldId,
}

impl LaunchSpec {
    pub fn new(
        runtime: &JavaRuntime,
        provisioned: &ProvisionedServer,
        profile: &ServerProfile,
        world_id: WorldId,
    ) -> Result<Self> {
        if !runtime.is_usable() {
            return Err(MineDockError::JavaUnavailable(
                "selected Java runtime is not compatible with the server requirement".into(),
            ));
        }
        profile.validate()?;
        // This constructor deliberately reloads and validates the persisted
        // provision record and acceptance record.  Public convenience fields
        // on ProvisionedServer are never sufficient to authorize a launch.
        let validated = crate::ValidatedProvision::from_provisioned(
            provisioned,
            world_id,
            runtime.version.major,
        )?;
        Self::from_validated(runtime, &validated, profile, world_id)
    }

    pub fn from_validated(
        runtime: &JavaRuntime,
        provisioned: &crate::ValidatedProvision,
        profile: &ServerProfile,
        world_id: WorldId,
    ) -> Result<Self> {
        if !runtime.is_usable() {
            return Err(MineDockError::JavaUnavailable(
                "selected Java runtime is not compatible with the server requirement".into(),
            ));
        }
        profile.validate()?;
        if provisioned.world_id() != world_id {
            return Err(MineDockError::ProcessStart(
                "validated provision belongs to a different world".into(),
            ));
        }
        let min = format!("-Xms{}M", profile.memory_min_mb);
        let max = format!("-Xmx{}M", profile.memory_max_mb);
        let args = vec![
            OsString::from(min),
            OsString::from(max),
            OsString::from("-jar"),
            provisioned.server_jar().as_os_str().to_owned(),
            OsString::from("nogui"),
        ];
        let spec = Self {
            executable: runtime.executable.clone(),
            args,
            current_dir: provisioned.world_root().to_path_buf(),
            world_id,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.args
    }

    pub fn argument_strings(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.executable.clone().into_os_string());
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Validate only the platform-neutral shape of a launch specification.
    /// Existence, regular-file, reparse, and executable checks belong to the
    /// native app adapter immediately before spawning.
    pub fn validate(&self) -> Result<()> {
        if self.executable.as_os_str().is_empty() || !self.executable.is_absolute() {
            return Err(MineDockError::ProcessStart(
                "Java executable must be an absolute path".into(),
            ));
        }
        if self.current_dir.as_os_str().is_empty() || !self.current_dir.is_absolute() {
            return Err(MineDockError::ProcessStart(
                "world current directory must be absolute".into(),
            ));
        }
        if self
            .args
            .iter()
            .any(|argument| argument.to_string_lossy().contains(['\0', '\r', '\n']))
        {
            return Err(MineDockError::ProcessStart(
                "launch arguments contain an unsafe control character".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedServerCommand {
    Stop,
    Text(String),
}

pub type ServerCommand = ValidatedServerCommand;

/// Native process adapters own stdin, wait handles, and output readers.
pub trait ServerProcess: Send {
    fn world_id(&self) -> WorldId;
    fn session_id(&self) -> SessionId;
    fn pid(&self) -> u32;
    fn send_command(&mut self, command: &ValidatedServerCommand) -> Result<()>;
    fn graceful_stop(&mut self, timeout: Duration) -> Result<GracefulStopResult>;
    fn force_terminate(&mut self, token: StopEscalationToken) -> Result<ProcessExit>;
    /// Emergency cleanup for a process whose launch could not be committed
    /// to persistence. This is deliberately separate from normal stop and
    /// may use the adapter's crash-safety job termination. The supervisor
    /// calls it only while the session is still uncommitted.
    fn force_cleanup_after_start_failure(&mut self) -> Result<ProcessExit>;
    fn try_wait(&mut self) -> Result<Option<ProcessExit>>;
    fn drain_events(&self, max_raw: usize) -> Vec<ServerEvent>;
}

pub trait ProcessFactory: Send {
    type Process: ServerProcess;

    fn spawn(&mut self, spec: &LaunchSpec, session_id: SessionId) -> Result<Self::Process>;
}

pub trait LifecycleLeaseProvider: Send {
    type Lease: Send;

    fn acquire(&mut self, app_data_root: &Path) -> Result<Self::Lease>;
}

impl ValidatedServerCommand {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        Self::parse(value)
    }

    pub const fn stop() -> Self {
        Self::Stop
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > MAX_SERVER_COMMAND_BYTES {
            return Err(MineDockError::InvalidInput(
                "server command must be nonempty and within the byte limit".into(),
            ));
        }
        if value.contains(['\0', '\r', '\n']) {
            return Err(MineDockError::InvalidInput(
                "server command must not contain NUL, CR, or LF".into(),
            ));
        }
        if value.trim().eq_ignore_ascii_case("stop") {
            Ok(Self::Stop)
        } else {
            Ok(Self::Text(value.to_owned()))
        }
    }

    pub fn wire_bytes(&self) -> Vec<u8> {
        match self {
            Self::Stop => b"stop\n".to_vec(),
            Self::Text(text) => {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(b'\n');
                bytes
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        Uuid::parse_str(value.as_ref())
            .map(Self)
            .map_err(|error| MineDockError::Persistence(format!("invalid session id: {error}")))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLogLine {
    pub world_id: WorldId,
    pub session_id: SessionId,
    pub stream: LogStream,
    pub line: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReliableLifecycleEvent {
    Spawned { pid: u32 },
    StopRequested,
    StopTimedOut,
    Exited { code: Option<i32>, success: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    Reliable(ReliableLifecycleEvent),
    Raw(RawLogLine),
    RawEventsDropped { count: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GracefulStopResult {
    Exited { exit: ProcessExit },
    TimedOut,
}

/// Result returned by the supervisor after a graceful stop. The timeout
/// token is issued by core only after the injected process reports a real
/// bounded timeout; adapters cannot construct one through their standalone
/// APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Exited { exit: ProcessExit },
    TimedOut(StopEscalationToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopEscalationToken {
    world_id: WorldId,
    session_id: SessionId,
}

impl StopEscalationToken {
    /// Core is the sole issuer. Process adapters report `GracefulStopResult::TimedOut`
    /// without receiving a forgeable constructor.
    fn new_after_timeout(world_id: WorldId, session_id: SessionId) -> Self {
        Self {
            world_id,
            session_id,
        }
    }

    pub fn world_id(self) -> WorldId {
        self.world_id
    }

    pub fn session_id(self) -> SessionId {
        self.session_id
    }
}

fn token_matches(token: StopEscalationToken, world_id: WorldId, session_id: SessionId) -> bool {
    token.world_id == world_id && token.session_id == session_id
}

#[derive(Debug, Clone, Default)]
pub struct WorldReservations {
    worlds: Arc<Mutex<HashSet<WorldId>>>,
}

impl WorldReservations {
    pub fn reserve(&self, world_id: WorldId) -> Result<WorldReservation> {
        let mut worlds = self
            .worlds
            .lock()
            .map_err(|_| MineDockError::InvalidState("world reservation lock poisoned".into()))?;
        if !worlds.insert(world_id) {
            return Err(MineDockError::InvalidState(
                "world already has an active lifecycle operation".into(),
            ));
        }
        Ok(WorldReservation {
            world_id,
            worlds: self.worlds.clone(),
            released: false,
        })
    }
}

#[derive(Debug)]
pub struct WorldReservation {
    world_id: WorldId,
    worlds: Arc<Mutex<HashSet<WorldId>>>,
    released: bool,
}

impl WorldReservation {
    pub fn world_id(&self) -> WorldId {
        self.world_id
    }

    /// Release only after process exit and persistence work are authoritative.
    pub fn release(mut self) {
        self.released = true;
        if let Ok(mut worlds) = self.worlds.lock() {
            worlds.remove(&self.world_id);
        }
    }
}

impl Drop for WorldReservation {
    fn drop(&mut self) {
        if !self.released {
            if let Ok(mut worlds) = self.worlds.lock() {
                worlds.remove(&self.world_id);
            }
        }
    }
}

pub fn stale_status_recovery(status: WorldStatus) -> Option<WorldStatus> {
    match status {
        WorldStatus::Preparing
        | WorldStatus::Starting
        | WorldStatus::Running
        | WorldStatus::Stopping
        | WorldStatus::BackingUp => Some(WorldStatus::Failed),
        WorldStatus::Stopped | WorldStatus::Failed => None,
    }
}

pub trait LifecyclePersistence {
    fn persist_status(&mut self, world_id: WorldId, status: WorldStatus) -> Result<()>;

    /// Persist status, PID, and session identity as one adapter-defined
    /// record. The default preserves compatibility with simple test seams.
    fn persist_session(
        &mut self,
        world_id: WorldId,
        status: WorldStatus,
        pid: Option<u32>,
        _session_id: Option<SessionId>,
    ) -> Result<()> {
        self.persist_status(world_id, status)?;
        if let Some(pid) = pid {
            self.persist_process_id(world_id, pid)
        } else {
            self.clear_process_id(world_id)
        }
    }

    fn persist_process_id(&mut self, _world_id: WorldId, _pid: u32) -> Result<()> {
        Ok(())
    }

    fn clear_process_id(&mut self, _world_id: WorldId) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct InMemoryLifecyclePersistence {
    statuses: std::collections::HashMap<WorldId, WorldStatus>,
    process_ids: std::collections::HashMap<WorldId, u32>,
}

impl InMemoryLifecyclePersistence {
    pub fn status(&self, world_id: WorldId) -> Option<WorldStatus> {
        self.statuses.get(&world_id).copied()
    }

    pub fn process_id(&self, world_id: WorldId) -> Option<u32> {
        self.process_ids.get(&world_id).copied()
    }
}

impl LifecyclePersistence for InMemoryLifecyclePersistence {
    fn persist_status(&mut self, world_id: WorldId, status: WorldStatus) -> Result<()> {
        self.statuses.insert(world_id, status);
        Ok(())
    }

    fn persist_process_id(&mut self, world_id: WorldId, pid: u32) -> Result<()> {
        self.process_ids.insert(world_id, pid);
        Ok(())
    }

    fn clear_process_id(&mut self, world_id: WorldId) -> Result<()> {
        self.process_ids.remove(&world_id);
        Ok(())
    }

    fn persist_session(
        &mut self,
        world_id: WorldId,
        status: WorldStatus,
        pid: Option<u32>,
        _session_id: Option<SessionId>,
    ) -> Result<()> {
        self.statuses.insert(world_id, status);
        if let Some(pid) = pid {
            self.process_ids.insert(world_id, pid);
        } else {
            self.process_ids.remove(&world_id);
        }
        Ok(())
    }
}

struct ManagedSession<Process, Lease> {
    _lease: Lease,
    _reservation: WorldReservation,
    process: Process,
    status: WorldStatus,
    stop_token: Option<StopEscalationToken>,
}

pub struct LifecycleSupervisor<P, F, L>
where
    F: ProcessFactory,
    L: LifecycleLeaseProvider,
{
    persistence: P,
    factory: F,
    lease_provider: L,
    reservations: WorldReservations,
    sessions: std::collections::HashMap<WorldId, ManagedSession<F::Process, L::Lease>>,
}

impl<P, F, L> std::fmt::Debug for LifecycleSupervisor<P, F, L>
where
    P: std::fmt::Debug,
    F: ProcessFactory + std::fmt::Debug,
    L: LifecycleLeaseProvider + std::fmt::Debug,
    F::Process: std::fmt::Debug,
    L::Lease: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LifecycleSupervisor")
            .field("persistence", &self.persistence)
            .field("factory", &self.factory)
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl<P, F, L> LifecycleSupervisor<P, F, L>
where
    P: LifecyclePersistence,
    F: ProcessFactory,
    L: LifecycleLeaseProvider,
{
    pub fn new(persistence: P, factory: F, lease_provider: L) -> Self {
        Self {
            persistence,
            factory,
            lease_provider,
            reservations: WorldReservations::default(),
            sessions: std::collections::HashMap::new(),
        }
    }

    pub fn persistence(&self) -> &P {
        &self.persistence
    }

    pub fn persistence_mut(&mut self) -> &mut P {
        &mut self.persistence
    }

    pub fn factory(&self) -> &F {
        &self.factory
    }

    pub fn start<Resolve>(
        &mut self,
        app_data_root: &Path,
        world_id: WorldId,
        resolve_launch: Resolve,
    ) -> Result<SessionId>
    where
        Resolve: FnOnce() -> Result<LaunchSpec>,
    {
        self.start_checked(
            app_data_root,
            world_id,
            WorldStatus::Stopped,
            resolve_launch,
        )
    }

    pub fn start_checked<Resolve>(
        &mut self,
        app_data_root: &Path,
        world_id: WorldId,
        current_status: WorldStatus,
        resolve_launch: Resolve,
    ) -> Result<SessionId>
    where
        Resolve: FnOnce() -> Result<LaunchSpec>,
    {
        if current_status != WorldStatus::Stopped {
            return Err(MineDockError::InvalidState(format!(
                "start requires Stopped, found {current_status:?}"
            )));
        }
        if self.sessions.contains_key(&world_id) {
            return Err(MineDockError::InvalidState(
                "world already has an active session".into(),
            ));
        }
        // Both guards are acquired before any slow resolution work.
        let lease = self.lease_provider.acquire(app_data_root)?;
        let reservation = self.reservations.reserve(world_id)?;
        self.persistence
            .persist_session(world_id, WorldStatus::Preparing, None, None)?;
        let spec = match resolve_launch() {
            Ok(spec) => spec,
            Err(error) => {
                let _ = self
                    .persistence
                    .persist_session(world_id, WorldStatus::Failed, None, None);
                return Err(error);
            }
        };
        self.persistence
            .persist_session(world_id, WorldStatus::Starting, None, None)?;
        let session_id = SessionId::new();
        let process = match self.factory.spawn(&spec, session_id) {
            Ok(process) => process,
            Err(error) => {
                let _ = self
                    .persistence
                    .persist_session(world_id, WorldStatus::Failed, None, None);
                return Err(error);
            }
        };
        let pid = process.pid();
        self.sessions.insert(
            world_id,
            ManagedSession {
                _lease: lease,
                _reservation: reservation,
                process,
                status: WorldStatus::Starting,
                stop_token: None,
            },
        );
        if let Err(error) = self.persistence.persist_session(
            world_id,
            WorldStatus::Starting,
            Some(pid),
            Some(session_id),
        ) {
            self.rollback_uncommitted_session(world_id);
            return Err(error);
        }
        if let Err(error) = self.persistence.persist_session(
            world_id,
            WorldStatus::Running,
            Some(pid),
            Some(session_id),
        ) {
            self.rollback_uncommitted_session(world_id);
            return Err(error);
        }
        if let Some(session) = self.sessions.get_mut(&world_id) {
            session.status = WorldStatus::Running;
        }
        Ok(session_id)
    }

    /// Roll back a child whose PID/status could not be committed. A graceful
    /// stop cannot make this failure safe because the failed persistence
    /// boundary may also prevent a Stopping record and a timeout token from
    /// being delivered. Use the adapter's explicit launch-rollback cleanup;
    /// if that fails, retain ownership in Failed with a public retry method.
    fn rollback_uncommitted_session(&mut self, world_id: WorldId) {
        let cleanup = self
            .sessions
            .get_mut(&world_id)
            .map(|session| session.process.force_cleanup_after_start_failure());
        match cleanup {
            Some(Ok(_exit)) => {
                let _ = self
                    .persistence
                    .persist_session(world_id, WorldStatus::Failed, None, None);
                self.sessions.remove(&world_id);
            }
            Some(Err(_cleanup_error)) => {
                let (pid, session_id) = self
                    .sessions
                    .get_mut(&world_id)
                    .map(|session| {
                        session.status = WorldStatus::Failed;
                        (
                            Some(session.process.pid()),
                            Some(session.process.session_id()),
                        )
                    })
                    .unwrap_or((None, None));
                let _ = self.persistence.persist_session(
                    world_id,
                    WorldStatus::Failed,
                    pid,
                    session_id,
                );
            }
            None => {}
        }
    }

    /// Retry explicit cleanup when the bounded launch rollback itself could
    /// not complete. This is never called by normal stop and does not accept
    /// a timeout token.
    pub fn force_cleanup_after_start_failure(&mut self, world_id: WorldId) -> Result<ProcessExit> {
        let session = self.sessions.get_mut(&world_id).ok_or_else(|| {
            MineDockError::InvalidState("world has no pending launch cleanup".into())
        })?;
        if session.status != WorldStatus::Failed {
            return Err(MineDockError::InvalidState(
                "launch cleanup is not pending for this world".into(),
            ));
        }
        let exit = session.process.force_cleanup_after_start_failure()?;
        let persistence_result =
            self.persistence
                .persist_session(world_id, WorldStatus::Failed, None, None);
        // The child has authoritatively exited even when the final Failed
        // projection cannot be written. Do not retain a dead process and a
        // reservation that callers can no longer manage.
        self.sessions.remove(&world_id);
        persistence_result?;
        Ok(exit)
    }

    /// Returns `Ok(None)` when no active session exists, making repeated stop
    /// requests for a persisted Stopped world idempotent.
    pub fn stop(&mut self, world_id: WorldId, timeout: Duration) -> Result<Option<StopOutcome>> {
        self.stop_inner(world_id, timeout, None)
            .map(|(outcome, _events)| outcome)
    }

    /// Stop a world and collect the final bounded process events before a
    /// normally exited session is removed from the supervisor. This keeps the
    /// UI's current-session log projection from losing output emitted during
    /// graceful shutdown.
    pub fn stop_with_events(
        &mut self,
        world_id: WorldId,
        timeout: Duration,
        max_raw: usize,
    ) -> Result<(Option<StopOutcome>, Vec<ServerEvent>)> {
        self.stop_inner(world_id, timeout, Some(max_raw))
    }

    fn stop_inner(
        &mut self,
        world_id: WorldId,
        timeout: Duration,
        max_raw: Option<usize>,
    ) -> Result<(Option<StopOutcome>, Vec<ServerEvent>)> {
        let Some(session) = self.sessions.get(&world_id) else {
            return Ok((None, Vec::new()));
        };
        if session.status != WorldStatus::Running {
            return Err(MineDockError::InvalidState(format!(
                "normal stop requires Running, found {:?}",
                session.status
            )));
        }
        self.persistence
            .persist_session(world_id, WorldStatus::Stopping, None, None)?;
        if let Some(session) = self.sessions.get_mut(&world_id) {
            session.status = WorldStatus::Stopping;
        }
        let result = match self
            .sessions
            .get_mut(&world_id)
            .ok_or_else(|| MineDockError::InvalidState("world session disappeared".into()))?
            .process
            .graceful_stop(timeout)
        {
            Ok(result) => result,
            Err(error) => {
                self.restore_running_after_stop_failure(world_id);
                return Err(error);
            }
        };
        match result {
            GracefulStopResult::TimedOut => {
                let session_id = self
                    .sessions
                    .get(&world_id)
                    .map(|session| session.process.session_id())
                    .ok_or_else(|| {
                        MineDockError::InvalidState("world session disappeared".into())
                    })?;
                let token = StopEscalationToken::new_after_timeout(world_id, session_id);
                if let Some(session) = self.sessions.get_mut(&world_id) {
                    session.stop_token = Some(token);
                }
                let events = max_raw.map_or_else(Vec::new, |limit| {
                    self.sessions
                        .get(&world_id)
                        .map_or_else(Vec::new, |session| session.process.drain_events(limit))
                });
                Ok((Some(StopOutcome::TimedOut(token)), events))
            }
            GracefulStopResult::Exited { exit } => {
                let target = if exit.success {
                    WorldStatus::Stopped
                } else {
                    WorldStatus::Failed
                };
                let events = max_raw.map_or_else(Vec::new, |limit| {
                    self.sessions
                        .get(&world_id)
                        .map_or_else(Vec::new, |session| {
                            drain_all_process_events(&session.process, limit)
                        })
                });
                self.persistence
                    .persist_session(world_id, target, None, None)?;
                self.persistence.clear_process_id(world_id)?;
                self.sessions.remove(&world_id);
                Ok((Some(StopOutcome::Exited { exit }), events))
            }
        }
    }

    fn restore_running_after_stop_failure(&mut self, world_id: WorldId) {
        let (pid, session_id) = self
            .sessions
            .get(&world_id)
            .map(|session| {
                (
                    Some(session.process.pid()),
                    Some(session.process.session_id()),
                )
            })
            .unwrap_or((None, None));
        if let Some(session) = self.sessions.get_mut(&world_id) {
            session.status = WorldStatus::Running;
        }
        let _ = self
            .persistence
            .persist_session(world_id, WorldStatus::Running, pid, session_id);
    }

    pub fn force_terminate(
        &mut self,
        world_id: WorldId,
        token: StopEscalationToken,
    ) -> Result<ProcessExit> {
        self.force_terminate_with_events(world_id, token, 0)
            .map(|(exit, _events)| exit)
    }

    pub fn force_terminate_with_events(
        &mut self,
        world_id: WorldId,
        token: StopEscalationToken,
        max_raw: usize,
    ) -> Result<(ProcessExit, Vec<ServerEvent>)> {
        let session = self
            .sessions
            .get_mut(&world_id)
            .ok_or_else(|| MineDockError::InvalidState("world has no active session".into()))?;
        if session.status != WorldStatus::Stopping {
            return Err(MineDockError::InvalidState(
                "force termination requires Stopping".into(),
            ));
        }
        if session.stop_token != Some(token)
            || !token_matches(token, world_id, session.process.session_id())
        {
            return Err(MineDockError::ProcessControl(
                "stale or mismatched force-termination token".into(),
            ));
        }
        let exit = session.process.force_terminate(token)?;
        let events = if max_raw == 0 {
            Vec::new()
        } else {
            drain_all_process_events(&session.process, max_raw)
        };
        self.persistence
            .persist_session(world_id, WorldStatus::Failed, None, None)?;
        self.sessions.remove(&world_id);
        Ok((exit, events))
    }

    pub fn poll_exit(&mut self, world_id: WorldId) -> Result<Option<ProcessExit>> {
        self.poll_exit_inner(world_id, None)
            .map(|(exit, _events)| exit)
    }

    /// Poll a world and collect process events before removing an exited
    /// session. Native adapters synchronize their output readers from
    /// `try_wait`, so events emitted immediately before an unexpected exit
    /// remain available to the caller.
    pub fn poll_exit_with_events(
        &mut self,
        world_id: WorldId,
        max_raw: usize,
    ) -> Result<(Option<ProcessExit>, Vec<ServerEvent>)> {
        self.poll_exit_inner(world_id, Some(max_raw))
    }

    fn poll_exit_inner(
        &mut self,
        world_id: WorldId,
        max_raw: Option<usize>,
    ) -> Result<(Option<ProcessExit>, Vec<ServerEvent>)> {
        let (exit, stopping, events) = {
            let session = self
                .sessions
                .get_mut(&world_id)
                .ok_or_else(|| MineDockError::InvalidState("world has no active session".into()))?;
            let exit = session.process.try_wait()?;
            let events = max_raw.map_or_else(Vec::new, |limit| {
                if exit.is_some() {
                    drain_all_process_events(&session.process, limit)
                } else {
                    session.process.drain_events(limit)
                }
            });
            (exit, session.status == WorldStatus::Stopping, events)
        };
        let Some(exit) = exit else {
            return Ok((None, events));
        };
        let target = if stopping && exit.success {
            WorldStatus::Stopped
        } else {
            WorldStatus::Failed
        };
        self.persistence
            .persist_session(world_id, target, None, None)?;
        self.sessions.remove(&world_id);
        Ok((Some(exit), events))
    }

    pub fn session_id(&self, world_id: WorldId) -> Option<SessionId> {
        self.sessions
            .get(&world_id)
            .map(|session| session.process.session_id())
    }

    pub fn drain_events(&self, world_id: WorldId, max_raw: usize) -> Result<Vec<ServerEvent>> {
        self.sessions
            .get(&world_id)
            .map(|session| session.process.drain_events(max_raw))
            .ok_or_else(|| MineDockError::InvalidState("world has no active session".into()))
    }
}

fn drain_all_process_events<P: ServerProcess>(process: &P, max_raw: usize) -> Vec<ServerEvent> {
    if max_raw == 0 {
        return process.drain_events(0);
    }
    let mut events = Vec::new();
    loop {
        let batch = process.drain_events(max_raw);
        if batch.is_empty() {
            break;
        }
        events.extend(batch);
    }
    events
}

pub fn validate_process_transition(from: WorldStatus, to: WorldStatus) -> Result<()> {
    if can_transition(from, to) {
        Ok(())
    } else {
        Err(MineDockError::InvalidLifecycleTransition { from, to })
    }
}

pub fn recover_persisted_active_states<P: LifecyclePersistence>(
    persistence: &mut P,
    states: impl IntoIterator<Item = (WorldId, WorldStatus)>,
) -> Result<Vec<WorldId>> {
    let mut recovered = Vec::new();
    for (world_id, status) in states {
        if stale_status_recovery(status).is_some() {
            persistence.persist_session(world_id, WorldStatus::Failed, None, None)?;
            recovered.push(world_id);
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;

    #[derive(Debug, Default)]
    struct TestLease;

    #[derive(Debug, Default)]
    struct TestLeaseProvider;

    impl LifecycleLeaseProvider for TestLeaseProvider {
        type Lease = TestLease;

        fn acquire(&mut self, _: &Path) -> Result<Self::Lease> {
            Ok(TestLease)
        }
    }

    #[derive(Debug)]
    struct TestProcess {
        world_id: WorldId,
        session_id: SessionId,
        pid: u32,
        timeout: bool,
        command_fails: bool,
        exit_on_wait: bool,
    }

    impl ServerProcess for TestProcess {
        fn world_id(&self) -> WorldId {
            self.world_id
        }

        fn session_id(&self) -> SessionId {
            self.session_id
        }

        fn pid(&self) -> u32 {
            self.pid
        }

        fn send_command(&mut self, _: &ValidatedServerCommand) -> Result<()> {
            if self.command_fails {
                Err(MineDockError::ProcessControl("fixture write failed".into()))
            } else {
                Ok(())
            }
        }

        fn graceful_stop(&mut self, _: Duration) -> Result<GracefulStopResult> {
            if self.command_fails {
                return Err(MineDockError::ProcessControl("fixture write failed".into()));
            }
            if self.timeout {
                Ok(GracefulStopResult::TimedOut)
            } else {
                Ok(GracefulStopResult::Exited {
                    exit: ProcessExit {
                        code: Some(0),
                        success: true,
                    },
                })
            }
        }

        fn force_terminate(&mut self, token: StopEscalationToken) -> Result<ProcessExit> {
            if token.world_id() != self.world_id || token.session_id() != self.session_id {
                return Err(MineDockError::ProcessControl("stale token".into()));
            }
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn force_cleanup_after_start_failure(&mut self) -> Result<ProcessExit> {
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
            if self.exit_on_wait {
                Ok(Some(ProcessExit {
                    code: Some(1),
                    success: false,
                }))
            } else {
                Ok(None)
            }
        }

        fn drain_events(&self, _: usize) -> Vec<ServerEvent> {
            Vec::new()
        }
    }

    #[derive(Debug, Default)]
    struct TestFactory {
        timeout: bool,
        command_fails: bool,
        exit_on_wait: bool,
    }

    impl ProcessFactory for TestFactory {
        type Process = TestProcess;

        fn spawn(&mut self, spec: &LaunchSpec, session_id: SessionId) -> Result<Self::Process> {
            Ok(TestProcess {
                world_id: spec.world_id,
                session_id,
                pid: 1234,
                timeout: self.timeout,
                command_fails: self.command_fails,
                exit_on_wait: self.exit_on_wait,
            })
        }
    }

    #[derive(Debug, Default)]
    struct FinalEventFactory {
        unexpected_exit: bool,
    }

    #[derive(Debug)]
    struct FinalEventProcess {
        world_id: WorldId,
        session_id: SessionId,
        unexpected_exit: bool,
        pending_events: RefCell<Vec<ServerEvent>>,
    }

    impl ServerProcess for FinalEventProcess {
        fn world_id(&self) -> WorldId {
            self.world_id
        }

        fn session_id(&self) -> SessionId {
            self.session_id
        }

        fn pid(&self) -> u32 {
            4321
        }

        fn send_command(&mut self, _: &ValidatedServerCommand) -> Result<()> {
            Ok(())
        }

        fn graceful_stop(&mut self, _: Duration) -> Result<GracefulStopResult> {
            Ok(GracefulStopResult::Exited {
                exit: ProcessExit {
                    code: Some(0),
                    success: true,
                },
            })
        }

        fn force_terminate(&mut self, _: StopEscalationToken) -> Result<ProcessExit> {
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn force_cleanup_after_start_failure(&mut self) -> Result<ProcessExit> {
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
            if self.unexpected_exit {
                Ok(Some(ProcessExit {
                    code: Some(1),
                    success: false,
                }))
            } else {
                Ok(None)
            }
        }

        fn drain_events(&self, max_raw: usize) -> Vec<ServerEvent> {
            let mut pending = self.pending_events.borrow_mut();
            let count = max_raw.min(pending.len());
            pending.drain(..count).collect()
        }
    }

    impl ProcessFactory for FinalEventFactory {
        type Process = FinalEventProcess;

        fn spawn(&mut self, spec: &LaunchSpec, session_id: SessionId) -> Result<Self::Process> {
            let pending_events = if self.unexpected_exit {
                let mut events = (0..70)
                    .map(|index| {
                        ServerEvent::Raw(RawLogLine {
                            world_id: spec.world_id,
                            session_id,
                            stream: LogStream::Stdout,
                            line: format!("unexpected log {index}"),
                            truncated: false,
                        })
                    })
                    .collect::<Vec<_>>();
                events.push(ServerEvent::Raw(RawLogLine {
                    world_id: spec.world_id,
                    session_id,
                    stream: LogStream::Stdout,
                    line: "unexpected final stdout".into(),
                    truncated: false,
                }));
                events.push(ServerEvent::Raw(RawLogLine {
                    world_id: spec.world_id,
                    session_id,
                    stream: LogStream::Stderr,
                    line: "unexpected final stderr".into(),
                    truncated: false,
                }));
                events
            } else {
                vec![ServerEvent::Raw(RawLogLine {
                    world_id: spec.world_id,
                    session_id,
                    stream: LogStream::Stdout,
                    line: "server stopped cleanly".into(),
                    truncated: false,
                })]
            };
            Ok(FinalEventProcess {
                world_id: spec.world_id,
                session_id,
                unexpected_exit: self.unexpected_exit,
                pending_events: RefCell::new(pending_events),
            })
        }
    }

    #[derive(Debug, Default)]
    struct FailOnRunningPersistence {
        inner: InMemoryLifecyclePersistence,
    }

    impl LifecyclePersistence for FailOnRunningPersistence {
        fn persist_status(&mut self, world_id: WorldId, status: WorldStatus) -> Result<()> {
            self.inner.persist_status(world_id, status)
        }

        fn persist_session(
            &mut self,
            world_id: WorldId,
            status: WorldStatus,
            pid: Option<u32>,
            session_id: Option<SessionId>,
        ) -> Result<()> {
            if status == WorldStatus::Running {
                return Err(MineDockError::Persistence(
                    "simulated post-spawn running save failure".into(),
                ));
            }
            self.inner
                .persist_session(world_id, status, pid, session_id)
        }
    }

    fn test_spec(world_id: WorldId) -> LaunchSpec {
        LaunchSpec {
            executable: PathBuf::from(if cfg!(windows) {
                r"C:\java.exe"
            } else {
                "/java"
            }),
            args: vec![
                "-Xms1M".into(),
                "-Xmx2M".into(),
                "-jar".into(),
                "server.jar".into(),
                "nogui".into(),
            ],
            current_dir: PathBuf::from(if cfg!(windows) { r"C:\world" } else { "/world" }),
            world_id,
        }
    }

    #[test]
    fn command_validation_reserves_stop_and_rejects_control_input() {
        assert_eq!(
            ValidatedServerCommand::parse("stop").expect("stop"),
            ValidatedServerCommand::Stop
        );
        assert!(ValidatedServerCommand::parse("stop\n").is_err());
        assert!(ValidatedServerCommand::parse("say hi\0").is_err());
        assert_eq!(ValidatedServerCommand::stop().wire_bytes(), b"stop\n");
    }

    #[test]
    fn stop_with_events_collects_final_output_before_session_removal() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            FinalEventFactory::default(),
            TestLeaseProvider,
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");

        let (outcome, events) = supervisor
            .stop_with_events(world_id, Duration::from_millis(1), 64)
            .expect("stop with events");
        assert!(matches!(
            outcome,
            Some(StopOutcome::Exited { exit }) if exit.success
        ));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ServerEvent::Raw(RawLogLine { line, .. }) if line == "server stopped cleanly"
            )
        }));
        assert_eq!(supervisor.session_id(world_id), None);
        assert_eq!(
            supervisor.persistence().status(world_id),
            Some(WorldStatus::Stopped)
        );
    }

    #[test]
    fn stop_with_events_drains_all_final_output_before_session_removal() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            FinalEventFactory {
                unexpected_exit: true,
            },
            TestLeaseProvider,
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");

        let (outcome, events) = supervisor
            .stop_with_events(world_id, Duration::from_millis(1), 64)
            .expect("stop with events");
        assert!(matches!(
            outcome,
            Some(StopOutcome::Exited { exit }) if exit.success
        ));
        assert!(events.len() > 64);
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine { line, .. }) if line == "unexpected final stdout"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine { line, .. }) if line == "unexpected final stderr"
        )));
        assert_eq!(supervisor.session_id(world_id), None);
        assert_eq!(
            supervisor.persistence().status(world_id),
            Some(WorldStatus::Stopped)
        );
    }

    #[test]
    fn poll_exit_with_events_collects_unexpected_final_output_before_session_removal() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            FinalEventFactory {
                unexpected_exit: true,
            },
            TestLeaseProvider,
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");

        let (exit, events) = supervisor
            .poll_exit_with_events(world_id, 64)
            .expect("poll with events");
        assert!(matches!(exit, Some(ProcessExit { success: false, .. })));
        assert!(events.len() > 64);
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine { line, .. }) if line == "unexpected final stdout"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine { line, .. }) if line == "unexpected final stderr"
        )));
        assert_eq!(supervisor.session_id(world_id), None);
        assert_eq!(
            supervisor.persistence().status(world_id),
            Some(WorldStatus::Failed)
        );
    }

    #[test]
    fn stale_active_status_recovers_to_failed() {
        assert_eq!(
            stale_status_recovery(WorldStatus::Running),
            Some(WorldStatus::Failed)
        );
        assert_eq!(stale_status_recovery(WorldStatus::Stopped), None);
    }

    #[test]
    fn token_is_opaque_and_session_scoped() {
        let world = WorldId::new();
        let session = SessionId::new();
        let token = StopEscalationToken::new_after_timeout(world, session);
        assert_eq!(token.world_id(), world);
        assert_eq!(token.session_id(), session);
        assert!(!token_matches(token, WorldId::new(), session));
    }

    #[test]
    fn supervisor_stop_is_idempotent_and_timeout_token_is_session_scoped() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestFactory {
                timeout: true,
                command_fails: false,
                exit_on_wait: false,
            },
            TestLeaseProvider,
        );
        assert_eq!(
            supervisor
                .stop(world_id, Duration::from_millis(1))
                .expect("idempotent"),
            None
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");
        assert!(
            supervisor
                .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
                .is_err()
        );
        let result = supervisor
            .stop(world_id, Duration::from_millis(1))
            .expect("timeout");
        let StopOutcome::TimedOut(token) = result.expect("result") else {
            panic!("expected timeout");
        };
        let stale = StopEscalationToken::new_after_timeout(world_id, SessionId::new());
        assert!(supervisor.force_terminate(world_id, stale).is_err());
        assert!(supervisor.force_terminate(world_id, token).is_ok());
        assert_eq!(
            supervisor
                .stop(world_id, Duration::from_millis(1))
                .expect("stopped"),
            None
        );
    }

    #[test]
    fn command_failure_restores_running_without_force_kill() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestFactory {
                timeout: false,
                command_fails: true,
                exit_on_wait: false,
            },
            TestLeaseProvider,
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");
        assert!(supervisor.stop(world_id, Duration::from_millis(1)).is_err());
        assert_eq!(
            supervisor.persistence().status(world_id),
            Some(WorldStatus::Running)
        );
    }

    #[test]
    fn post_spawn_persistence_failure_uses_launch_rollback_cleanup() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            FailOnRunningPersistence::default(),
            TestFactory {
                timeout: false,
                command_fails: false,
                exit_on_wait: false,
            },
            TestLeaseProvider,
        );
        assert!(
            supervisor
                .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
                .is_err()
        );
        assert_eq!(supervisor.session_id(world_id), None);
        assert_eq!(
            supervisor.persistence().inner.status(world_id),
            Some(WorldStatus::Failed)
        );
    }

    #[test]
    fn unexpected_exit_persists_failed_and_releases_session() {
        let world_id = WorldId::new();
        let mut supervisor = LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestFactory {
                timeout: false,
                command_fails: false,
                exit_on_wait: true,
            },
            TestLeaseProvider,
        );
        supervisor
            .start(Path::new("."), world_id, || Ok(test_spec(world_id)))
            .expect("start");
        let exit = supervisor
            .poll_exit(world_id)
            .expect("poll")
            .expect("unexpected exit");
        assert!(!exit.success);
        assert_eq!(supervisor.session_id(world_id), None);
        assert_eq!(
            supervisor.persistence().status(world_id),
            Some(WorldStatus::Failed)
        );
    }
}
