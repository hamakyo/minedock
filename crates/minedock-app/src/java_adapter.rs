//! Windows/native Java discovery adapter.
//!
//! Discovery is deliberately started on a background thread.  The result is a
//! presentation projection only; it never accepts the EULA, fetches Mojang
//! metadata, writes files, or starts a server process.

use crate::native_safety::KillOnDropJob;
use minedock_core::{
    JAVA_VERSION_OUTPUT_LIMIT, JavaCandidateInspection, JavaDiscoveryConfig, JavaProbe,
    JavaProbeOutput, JavaReadiness, JavaRequirement, MineDockError, discover_any_java,
    discover_java,
};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// Native app-side Java `-version` probe. Core owns only the parser and the
/// JavaProbe seam; process creation, bounded pipes, and Windows descendant
/// cleanup stay here.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeJavaProbe;

impl JavaProbe for NativeJavaProbe {
    fn inspect_candidate(
        &self,
        executable: &Path,
    ) -> minedock_core::Result<JavaCandidateInspection> {
        let metadata = match fs::symlink_metadata(executable) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JavaCandidateInspection::Missing);
            }
            Err(error) => {
                return Err(MineDockError::JavaUnavailable(format!(
                    "could not inspect {}: {error}",
                    executable.display()
                )));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            Ok(JavaCandidateInspection::NotRegularFile)
        } else {
            Ok(JavaCandidateInspection::Regular)
        }
    }

    fn resolve_candidate(&self, executable: &Path) -> minedock_core::Result<PathBuf> {
        fs::canonicalize(executable).map_err(|error| {
            MineDockError::JavaUnavailable(format!(
                "could not resolve Java candidate {}: {error}",
                executable.display()
            ))
        })
    }

    fn probe(
        &self,
        executable: &Path,
        timeout: Duration,
        output_limit: usize,
    ) -> minedock_core::Result<JavaProbeOutput> {
        if timeout.is_zero() || output_limit == 0 {
            return Err(MineDockError::JavaUnavailable(
                "Java probe bounds must be nonzero".into(),
            ));
        }
        let mut command = Command::new(executable);
        command
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        KillOnDropJob::configure_suspended(&mut command);
        let mut child = command.spawn().map_err(|error| {
            MineDockError::JavaUnavailable(format!(
                "could not execute {}: {error}",
                executable.display()
            ))
        })?;
        // Attach while the child is still suspended, before taking/using any
        // pipe or allowing Java code to run. Drop/termination below owns all
        // failure paths after process creation.
        let job = match KillOnDropJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if let Err(error) = job.resume(&child) {
            let _ = job.terminate(&mut child);
            let _ = child.wait();
            return Err(error);
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let output_limit = output_limit.clamp(1, JAVA_VERSION_OUTPUT_LIMIT);
        let stdout_rx = spawn_bounded_reader(stdout, output_limit);
        let stderr_rx = spawn_bounded_reader(stderr, output_limit);
        let started = Instant::now();
        let mut timed_out = false;
        let exit_code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if started.elapsed() >= timeout => {
                    timed_out = true;
                    let _ = job.terminate(&mut child);
                    let status = reap_bounded(&mut child, Duration::from_millis(100));
                    break status.and_then(|value| value.code());
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    let _ = job.terminate(&mut child);
                    let _ = child.wait();
                    return Err(MineDockError::JavaUnavailable(format!(
                        "could not wait for {}: {error}",
                        executable.display()
                    )));
                }
            }
        };
        // Reader threads communicate through channels. Never join an
        // untrusted descendant-held pipe indefinitely after a timeout.
        let remaining = timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_millis(250));
        let stdout = receive_bounded(stdout_rx, remaining);
        let stderr = receive_bounded(stderr_rx, remaining);
        drop(job);
        Ok(JavaProbeOutput {
            stdout,
            stderr,
            exit_code,
            timed_out,
        })
    }
}

fn reap_bounded(
    child: &mut std::process::Child,
    bound: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() >= deadline => return None,
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_) => return None,
        }
    }
}

fn spawn_bounded_reader(
    stream: Option<impl Read + Send + 'static>,
    limit: usize,
) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::with_capacity(limit.min(4096));
        if let Some(mut stream) = stream {
            let mut buffer = [0_u8; 4096];
            while output.len() < limit {
                let requested = (limit - output.len()).min(buffer.len());
                match stream.read(&mut buffer[..requested]) {
                    Ok(0) => break,
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                    Err(_) => break,
                }
            }
        }
        let _ = sender.send(output);
    });
    receiver
}

fn receive_bounded(receiver: Receiver<Vec<u8>>, timeout: Duration) -> Vec<u8> {
    receiver.recv_timeout(timeout).unwrap_or_default()
}

#[allow(dead_code)]
pub fn discover_java_off_event_loop(required: JavaRequirement) -> Receiver<JavaReadiness> {
    let (sender, receiver) = mpsc::channel();
    let mut config = JavaDiscoveryConfig::from_environment();
    // Explicit configured path is a seam for the Settings UI and tests.  It is
    // intentionally resolved before JAVA_HOME and PATH by the core collector.
    config.configured_path = std::env::var_os("MINEDOCK_JAVA_PATH").map(PathBuf::from);
    thread::spawn(move || {
        let result = discover_java(&config, required, &NativeJavaProbe);
        let readiness = JavaReadiness::from_discovery(required, result);
        let _ = sender.send(readiness);
    });
    receiver
}

pub fn discover_any_java_off_event_loop() -> Receiver<JavaReadiness> {
    let (sender, receiver) = mpsc::channel();
    let mut config = JavaDiscoveryConfig::from_environment();
    config.configured_path = std::env::var_os("MINEDOCK_JAVA_PATH").map(PathBuf::from);
    thread::spawn(move || {
        let result = discover_any_java(&config, &NativeJavaProbe);
        let required = JavaRequirement::minimum();
        let _ = sender.send(JavaReadiness::from_discovery(required, result));
    });
    receiver
}
