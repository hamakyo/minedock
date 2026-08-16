//! Windows/native server process adapter.
//!
//! The core supervisor receives this adapter through ProcessFactory. It owns
//! the Child, stdin, wait state, bounded output readers, and crash-safety job
//! handle; no shell command is constructed.

use crate::native_safety::KillOnDropJob;
use minedock_core::{
    GracefulStopResult, LaunchSpec, LogStream, MAX_RAW_LOG_LINE_BYTES, MineDockError, ProcessExit,
    ProcessFactory, RAW_EVENT_CHANNEL_CAPACITY, RawLogLine, ReliableLifecycleEvent, Result,
    ServerEvent, ServerProcess, SessionId, StopEscalationToken, ValidatedServerCommand, WorldId,
};
use std::fs;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy)]
pub struct NativeProcessFactory;

impl ProcessFactory for NativeProcessFactory {
    type Process = NativeServerProcess;

    fn spawn(&mut self, spec: &LaunchSpec, session_id: SessionId) -> Result<Self::Process> {
        NativeServerProcess::spawn(spec, session_id)
    }
}

#[derive(Debug)]
pub struct NativeServerProcess {
    child: Child,
    stdin: ChildStdin,
    world_id: WorldId,
    session_id: SessionId,
    job: KillOnDropJob,
    raw_rx: Receiver<RawLogLine>,
    reliable_rx: Receiver<ReliableLifecycleEvent>,
    reliable_tx: Sender<ReliableLifecycleEvent>,
    raw_drop_count: Arc<Mutex<u64>>,
    finished: bool,
}

impl NativeServerProcess {
    pub fn spawn(spec: &LaunchSpec, session_id: SessionId) -> Result<Self> {
        Self::spawn_inner(spec, session_id, true)
    }

    #[cfg(test)]
    fn spawn_without_job_for_test(spec: &LaunchSpec, session_id: SessionId) -> Result<Self> {
        Self::spawn_inner(spec, session_id, false)
    }

    fn spawn_inner(spec: &LaunchSpec, session_id: SessionId, attach_job: bool) -> Result<Self> {
        validate_native_spec(spec)?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Windows creates the process suspended. The job is attached before
        // the primary thread is resumed, closing the crash window in which a
        // child could spawn descendants outside the kill-on-close job.
        if attach_job {
            KillOnDropJob::configure_suspended(&mut command);
        }
        let mut child = command.spawn().map_err(|error| {
            MineDockError::ProcessStart(format!(
                "could not spawn Java {} in {}: {error}",
                spec.executable.display(),
                spec.current_dir.display()
            ))
        })?;
        // Assign before exposing the session as started. A failed assignment
        // immediately closes/terminates the child rather than leaking it.
        let job = if attach_job {
            match KillOnDropJob::attach(&child) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        } else {
            KillOnDropJob::disabled_for_test()
        };
        if let Err(error) = job.resume(&child) {
            let _ = job.terminate(&mut child);
            let _ = child.wait();
            return Err(error);
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| MineDockError::ProcessStart("Java stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MineDockError::ProcessStart("Java stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MineDockError::ProcessStart("Java stderr was not piped".into()))?;
        let (raw_tx, raw_rx) = mpsc::sync_channel(RAW_EVENT_CHANNEL_CAPACITY);
        let (reliable_tx, reliable_rx) = mpsc::channel();
        let drop_count = Arc::new(Mutex::new(0_u64));
        spawn_reader(
            stdout,
            LogStream::Stdout,
            spec.world_id,
            session_id,
            raw_tx.clone(),
            drop_count.clone(),
        );
        spawn_reader(
            stderr,
            LogStream::Stderr,
            spec.world_id,
            session_id,
            raw_tx,
            drop_count.clone(),
        );
        reliable_tx
            .send(ReliableLifecycleEvent::Spawned { pid: child.id() })
            .map_err(|_| MineDockError::Process("reliable event channel closed".into()))?;
        Ok(Self {
            child,
            stdin,
            world_id: spec.world_id,
            session_id,
            job,
            raw_rx,
            reliable_rx,
            reliable_tx,
            raw_drop_count: drop_count,
            finished: false,
        })
    }

    fn record_exit(&mut self, status: std::process::ExitStatus) -> ProcessExit {
        let exit = ProcessExit {
            code: status.code(),
            success: status.success(),
        };
        if !self.finished {
            let _ = self.reliable_tx.send(ReliableLifecycleEvent::Exited {
                code: exit.code,
                success: exit.success,
            });
        }
        self.finished = true;
        exit
    }

    fn wait_native(&mut self) -> Result<ProcessExit> {
        let status = self.child.wait().map_err(|error| {
            MineDockError::Process(format!("could not wait for server process: {error}"))
        })?;
        Ok(self.record_exit(status))
    }

    fn wait_native_bounded(&mut self, timeout: Duration) -> Result<ProcessExit> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(self.record_exit(status)),
                Ok(None) if Instant::now() >= deadline => {
                    return Err(MineDockError::ProcessControl(
                        "launch rollback process did not exit within the cleanup bound".into(),
                    ));
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    return Err(MineDockError::ProcessControl(format!(
                        "could not reap launch rollback process: {error}"
                    )));
                }
            }
        }
    }

    fn token_matches(&self, token: StopEscalationToken) -> bool {
        token.world_id() == self.world_id && token.session_id() == self.session_id
    }
}

impl ServerProcess for NativeServerProcess {
    fn world_id(&self) -> WorldId {
        self.world_id
    }

    fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send_command(&mut self, command: &ValidatedServerCommand) -> Result<()> {
        self.stdin
            .write_all(&command.wire_bytes())
            .map_err(|error| {
                MineDockError::ProcessControl(format!("could not write server command: {error}"))
            })?;
        self.stdin.flush().map_err(|error| {
            MineDockError::ProcessControl(format!("could not flush server command: {error}"))
        })
    }

    fn graceful_stop(&mut self, timeout: Duration) -> Result<GracefulStopResult> {
        self.send_command(&ValidatedServerCommand::stop())?;
        self.reliable_tx
            .send(ReliableLifecycleEvent::StopRequested)
            .map_err(|_| MineDockError::Process("reliable event channel closed".into()))?;
        let started = Instant::now();
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(GracefulStopResult::Exited { exit });
            }
            if started.elapsed() >= timeout {
                self.reliable_tx
                    .send(ReliableLifecycleEvent::StopTimedOut)
                    .map_err(|_| MineDockError::Process("reliable event channel closed".into()))?;
                return Ok(GracefulStopResult::TimedOut);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn force_terminate(&mut self, token: StopEscalationToken) -> Result<ProcessExit> {
        if !self.token_matches(token) {
            return Err(MineDockError::ProcessControl(
                "stale or mismatched force-termination token".into(),
            ));
        }
        if self.finished {
            return Err(MineDockError::ProcessControl(
                "server process has already exited".into(),
            ));
        }
        self.job.terminate(&mut self.child)?;
        self.wait_native()
    }

    fn force_cleanup_after_start_failure(&mut self) -> Result<ProcessExit> {
        if self.finished {
            return Err(MineDockError::ProcessControl(
                "server process has already exited".into(),
            ));
        }
        if let Some(exit) = self.try_wait()? {
            return Ok(exit);
        }
        // This is the bounded launch-rollback path, not normal stop. The
        // job is used only because the process was never durably committed.
        self.job.terminate(&mut self.child)?;
        self.wait_native_bounded(Duration::from_secs(5))
    }

    fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(self.record_exit(status))),
            Ok(None) => Ok(None),
            Err(error) => Err(MineDockError::Process(format!(
                "could not query server process: {error}"
            ))),
        }
    }

    fn drain_events(&self, max_raw: usize) -> Vec<ServerEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.reliable_rx.try_recv() {
            events.push(ServerEvent::Reliable(event));
        }
        for _ in 0..max_raw {
            match self.raw_rx.try_recv() {
                Ok(event) => events.push(ServerEvent::Raw(event)),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let dropped = self.raw_drop_count.lock().map_or(0, |mut count| {
            let value = *count;
            *count = 0;
            value
        });
        if dropped > 0 {
            events.push(ServerEvent::RawEventsDropped { count: dropped });
        }
        events
    }
}

fn validate_native_spec(spec: &LaunchSpec) -> Result<()> {
    spec.validate()?;
    let executable = fs::symlink_metadata(&spec.executable).map_err(|error| {
        MineDockError::ProcessStart(format!("could not inspect Java executable: {error}"))
    })?;
    if !executable.is_file() || executable.file_type().is_symlink() || is_reparse_point(&executable)
    {
        return Err(MineDockError::ProcessStart(
            "Java executable is not a regular file".into(),
        ));
    }
    validate_ancestors(&spec.executable)?;
    let current_dir = fs::symlink_metadata(&spec.current_dir).map_err(|error| {
        MineDockError::ProcessStart(format!("could not inspect world directory: {error}"))
    })?;
    if !current_dir.is_dir()
        || current_dir.file_type().is_symlink()
        || is_reparse_point(&current_dir)
    {
        return Err(MineDockError::ProcessStart(
            "world current directory is not a regular directory".into(),
        ));
    }
    validate_ancestors(&spec.current_dir)?;
    if let Some(jar_index) = spec.args.iter().position(|argument| argument == "-jar") {
        let jar = spec.args.get(jar_index + 1).ok_or_else(|| {
            MineDockError::ProcessStart("launch spec has no server JAR path".into())
        })?;
        let jar_path = std::path::PathBuf::from(jar);
        let metadata = fs::symlink_metadata(&jar_path).map_err(|error| {
            MineDockError::ProcessStart(format!("could not inspect server JAR: {error}"))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(MineDockError::ProcessStart(
                "server JAR is not a regular file".into(),
            ));
        }
        validate_ancestors(&jar_path)?;
    }
    Ok(())
}

fn validate_ancestors(path: &std::path::Path) -> Result<()> {
    let mut current = path.to_path_buf();
    loop {
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| MineDockError::ProcessStart(error.to_string()))?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(MineDockError::ProcessStart(
                "native launch path contains a symlink or reparse point".into(),
            ));
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

fn spawn_reader<R: Read + Send + 'static>(
    stream: R,
    stream_kind: LogStream,
    world_id: WorldId,
    session_id: SessionId,
    sender: SyncSender<RawLogLine>,
    dropped: Arc<Mutex<u64>>,
) {
    thread::spawn(move || {
        let mut reader = stream;
        let mut chunk = [0_u8; 4096];
        let mut bytes = Vec::with_capacity(MAX_RAW_LOG_LINE_BYTES);
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    for byte in &chunk[..count] {
                        if *byte == b'\n' {
                            while bytes.last().is_some_and(|value| *value == b'\r') {
                                bytes.pop();
                            }
                            send_raw(
                                &sender,
                                &dropped,
                                RawLogLine {
                                    world_id,
                                    session_id,
                                    stream: stream_kind,
                                    line: String::from_utf8_lossy(&bytes).replace('\0', "�"),
                                    truncated,
                                },
                            );
                            bytes.clear();
                            truncated = false;
                        } else if bytes.len() < MAX_RAW_LOG_LINE_BYTES {
                            bytes.push(*byte);
                        } else {
                            truncated = true;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        if !bytes.is_empty() {
            while bytes.last().is_some_and(|value| *value == b'\r') {
                bytes.pop();
            }
            send_raw(
                &sender,
                &dropped,
                RawLogLine {
                    world_id,
                    session_id,
                    stream: stream_kind,
                    line: String::from_utf8_lossy(&bytes).replace('\0', "�"),
                    truncated,
                },
            );
        }
    });
}

fn send_raw(sender: &SyncSender<RawLogLine>, dropped: &Arc<Mutex<u64>>, line: RawLogLine) {
    if sender.try_send(line).is_err() {
        if let Ok(mut count) = dropped.lock() {
            *count = count.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn controlled_fixture_emits_stdout_stderr_and_accepts_graceful_stop() {
        let root =
            std::env::temp_dir().join(format!("minedock-native-process-{}", std::process::id()));
        fs::create_dir_all(&root).expect("fixture root");
        let spec = LaunchSpec {
            executable: std::env::current_exe().expect("test executable"),
            args: vec![
                "--exact".into(),
                "native_process::tests::fixture_child".into(),
                "--nocapture".into(),
            ],
            current_dir: root.clone(),
            world_id: WorldId::new(),
        };
        let mut process = NativeServerProcess::spawn_without_job_for_test(&spec, SessionId::new())
            .expect("spawn");
        let result = process
            .graceful_stop(Duration::from_secs(5))
            .expect("graceful stop");
        assert!(matches!(result, GracefulStopResult::Exited { exit } if exit.success));
        let events = process.drain_events(64);
        let spawned = events.iter().position(|event| {
            matches!(
                event,
                ServerEvent::Reliable(ReliableLifecycleEvent::Spawned { .. })
            )
        });
        let stop_requested = events.iter().position(|event| {
            matches!(
                event,
                ServerEvent::Reliable(ReliableLifecycleEvent::StopRequested)
            )
        });
        assert!(spawned.is_some() && stop_requested.is_some());
        assert!(spawned < stop_requested);
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine {
                stream: LogStream::Stdout,
                ..
            })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ServerEvent::Raw(RawLogLine {
                stream: LogStream::Stderr,
                ..
            })
        )));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_child() {
        if !std::env::args().any(|argument| argument == "--exact") {
            return;
        }
        println!("fixture stdout");
        eprintln!("fixture stderr");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .expect("fixture stdin");
        assert_eq!(input, "stop\n");
    }

    #[test]
    fn exact_launch_arguments_remain_shell_free() {
        let spec = LaunchSpec {
            executable: PathBuf::from(if cfg!(windows) {
                r"C:\java.exe"
            } else {
                "/java"
            }),
            args: vec![
                "-Xms1024M".into(),
                "-Xmx4096M".into(),
                "-jar".into(),
                "server/server.jar".into(),
                "nogui".into(),
            ],
            current_dir: PathBuf::from(if cfg!(windows) { r"C:\world" } else { "/world" }),
            world_id: WorldId::new(),
        };
        assert_eq!(spec.argument_strings()[2], "-jar");
        assert!(!spec.argv().iter().any(|arg| arg == "/c" || arg == "-c"));
    }

    #[test]
    fn raw_event_channel_reports_backpressure_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(Mutex::new(0_u64));
        let line = |text: &str| RawLogLine {
            world_id: WorldId::new(),
            session_id: SessionId::new(),
            stream: LogStream::Stdout,
            line: text.into(),
            truncated: false,
        };
        send_raw(&sender, &dropped, line("first"));
        send_raw(&sender, &dropped, line("second"));
        assert_eq!(receiver.try_recv().expect("first").line, "first");
        assert_eq!(*dropped.lock().expect("drop count"), 1);
    }
}
