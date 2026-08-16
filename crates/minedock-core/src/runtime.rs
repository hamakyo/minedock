//! Java runtime discovery and compatibility checks.
//!
//! This module intentionally contains no Windows, GPUI, or native process
//! adapters. The app injects a bounded JavaProbe implementation; tests inject
//! deterministic fakes.

use crate::{MineDockError, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const JAVA_VERSION_OUTPUT_LIMIT: usize = 64 * 1024;
pub const JAVA_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JavaMajor(u16);

impl JavaMajor {
    pub fn new(value: u16) -> Result<Self> {
        if value == 0 || value > 255 {
            return Err(MineDockError::InvalidConfiguration(
                "Java major version must be between 1 and 255".into(),
            ));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for JavaMajor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<JavaMajor> for u16 {
    fn from(value: JavaMajor) -> Self {
        value.0
    }
}

impl TryFrom<u16> for JavaMajor {
    type Error = MineDockError;

    fn try_from(value: u16) -> Result<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaVersion {
    pub raw: String,
    pub major: JavaMajor,
    pub components: Vec<u32>,
}

impl JavaVersion {
    pub fn parse(output: impl AsRef<[u8]>) -> Result<Self> {
        parse_java_version_output(output)
    }

    pub fn major(&self) -> JavaMajor {
        self.major
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRequirement {
    pub major: JavaMajor,
}

impl JavaRequirement {
    pub const fn minimum() -> Self {
        Self {
            major: JavaMajor(1),
        }
    }

    pub const fn from_major(major: JavaMajor) -> Self {
        Self { major }
    }

    pub fn new(major: u16) -> Result<Self> {
        Ok(Self {
            major: JavaMajor::new(major)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaCompatibility {
    Exact,
    NewerUnverified,
    TooOld,
}

impl JavaCompatibility {
    pub fn for_requirement(actual: JavaMajor, required: JavaMajor) -> Self {
        match actual.get().cmp(&required.get()) {
            std::cmp::Ordering::Equal => Self::Exact,
            std::cmp::Ordering::Greater => Self::NewerUnverified,
            std::cmp::Ordering::Less => Self::TooOld,
        }
    }

    pub fn usable(self) -> bool {
        !matches!(self, Self::TooOld)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntime {
    pub executable: PathBuf,
    pub version: JavaVersion,
    pub compatibility: JavaCompatibility,
    pub source: JavaCandidateSource,
}

impl JavaRuntime {
    pub fn is_usable(&self) -> bool {
        self.compatibility.usable()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaCandidateSource {
    Configured,
    JavaHome,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaDiscoveryConfig {
    pub configured_path: Option<PathBuf>,
    pub java_home: Option<PathBuf>,
    pub path: Option<OsString>,
    pub probe_timeout: Duration,
    pub output_limit: usize,
}

impl Default for JavaDiscoveryConfig {
    fn default() -> Self {
        Self {
            configured_path: None,
            java_home: None,
            path: std::env::var_os("PATH"),
            probe_timeout: JAVA_PROBE_TIMEOUT,
            output_limit: JAVA_VERSION_OUTPUT_LIMIT,
        }
    }
}

impl JavaDiscoveryConfig {
    pub fn from_environment() -> Self {
        Self {
            java_home: std::env::var_os("JAVA_HOME").map(PathBuf::from),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCandidate {
    pub path: PathBuf,
    pub source: JavaCandidateSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaProbeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

impl JavaProbeOutput {
    pub fn combined(&self, limit: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.stdout.len().saturating_add(self.stderr.len()));
        output.extend_from_slice(&self.stdout);
        output.extend_from_slice(&self.stderr);
        output.truncate(limit);
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaRecoveryAction {
    ChooseJavaExecutable,
    InstallCompatibleJava,
    CheckJavaHome,
    CheckPath,
    RetryDiscovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaUnavailableReason {
    NoCandidates,
    CandidateMissing {
        path: PathBuf,
    },
    CandidateNotRegularFile {
        path: PathBuf,
    },
    ProbeFailed {
        path: PathBuf,
        detail: String,
    },
    TimedOut {
        path: PathBuf,
    },
    NonZeroExit {
        path: PathBuf,
        code: Option<i32>,
        diagnostics: String,
    },
    MalformedVersion {
        path: PathBuf,
        diagnostics: String,
    },
    IncompatibleMajor {
        path: PathBuf,
        actual: JavaMajor,
        required: JavaMajor,
    },
}

impl JavaUnavailableReason {
    pub fn recovery_action(&self) -> JavaRecoveryAction {
        match self {
            Self::NoCandidates
            | Self::CandidateMissing { .. }
            | Self::CandidateNotRegularFile { .. } => JavaRecoveryAction::ChooseJavaExecutable,
            Self::IncompatibleMajor { .. } => JavaRecoveryAction::InstallCompatibleJava,
            Self::ProbeFailed { .. } | Self::TimedOut { .. } | Self::NonZeroExit { .. } => {
                JavaRecoveryAction::RetryDiscovery
            }
            Self::MalformedVersion { .. } => JavaRecoveryAction::CheckJavaHome,
        }
    }

    pub fn safe_diagnostics(&self) -> String {
        match self {
            Self::NoCandidates => "No Java executable candidates were found.".into(),
            Self::CandidateMissing { path } => {
                format!("Java candidate does not exist: {}", path.display())
            }
            Self::CandidateNotRegularFile { path } => {
                format!("Java candidate is not a regular file: {}", path.display())
            }
            Self::ProbeFailed { path, detail } => {
                format!("Could not run {}: {detail}", path.display())
            }
            Self::TimedOut { path } => format!("Java version probe timed out: {}", path.display()),
            Self::NonZeroExit {
                path,
                code,
                diagnostics,
            } => {
                format!("{} exited {:?}: {}", path.display(), code, diagnostics)
            }
            Self::MalformedVersion { path, diagnostics } => {
                format!(
                    "Could not parse Java version from {}: {}",
                    path.display(),
                    diagnostics
                )
            }
            Self::IncompatibleMajor {
                path,
                actual,
                required,
            } => format!(
                "{} reports Java {}, but Java {} is required",
                path.display(),
                actual,
                required
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaDiscoveryResult {
    pub selected: Option<JavaRuntime>,
    pub candidates: Vec<JavaCandidate>,
    pub reasons: Vec<JavaUnavailableReason>,
}

pub trait JavaProbe {
    /// Native adapters classify and resolve a candidate. Tests can use the
    /// default regular-file result without touching the filesystem.
    fn inspect_candidate(&self, _executable: &Path) -> Result<JavaCandidateInspection> {
        Ok(JavaCandidateInspection::Regular)
    }

    fn resolve_candidate(&self, executable: &Path) -> Result<PathBuf> {
        Ok(executable.to_path_buf())
    }

    fn probe(
        &self,
        executable: &Path,
        timeout: Duration,
        output_limit: usize,
    ) -> Result<JavaProbeOutput>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaCandidateInspection {
    Missing,
    NotRegularFile,
    Regular,
}

pub fn discover_java<P: JavaProbe>(
    config: &JavaDiscoveryConfig,
    required: JavaRequirement,
    probe: &P,
) -> JavaDiscoveryResult {
    let candidates = collect_java_candidates(config);
    let mut result = JavaDiscoveryResult {
        selected: None,
        candidates: candidates.clone(),
        reasons: Vec::new(),
    };
    if candidates.is_empty() {
        result.reasons.push(JavaUnavailableReason::NoCandidates);
        return result;
    }
    for candidate in candidates {
        let path = &candidate.path;
        let inspection = match probe.inspect_candidate(path) {
            Ok(inspection) => inspection,
            Err(error) => {
                result.reasons.push(JavaUnavailableReason::ProbeFailed {
                    path: path.clone(),
                    detail: safe_error_text(&error),
                });
                continue;
            }
        };
        match inspection {
            JavaCandidateInspection::Missing => {
                result
                    .reasons
                    .push(JavaUnavailableReason::CandidateMissing { path: path.clone() });
                continue;
            }
            JavaCandidateInspection::NotRegularFile => {
                result
                    .reasons
                    .push(JavaUnavailableReason::CandidateNotRegularFile { path: path.clone() });
                continue;
            }
            JavaCandidateInspection::Regular => {}
        }
        let resolved_path = match probe.resolve_candidate(path) {
            Ok(path) => path,
            Err(error) => {
                result.reasons.push(JavaUnavailableReason::ProbeFailed {
                    path: path.clone(),
                    detail: safe_error_text(&error),
                });
                continue;
            }
        };
        let output = match probe.probe(&resolved_path, config.probe_timeout, config.output_limit) {
            Ok(output) => output,
            Err(error) => {
                result.reasons.push(JavaUnavailableReason::ProbeFailed {
                    path: resolved_path.clone(),
                    detail: safe_error_text(&error),
                });
                continue;
            }
        };
        let diagnostics = safe_output_text(output.combined(config.output_limit));
        if output.timed_out {
            result.reasons.push(JavaUnavailableReason::TimedOut {
                path: resolved_path.clone(),
            });
            continue;
        }
        let version = match parse_java_version_output(output.combined(config.output_limit)) {
            Ok(version) => version,
            Err(_) => {
                result
                    .reasons
                    .push(JavaUnavailableReason::MalformedVersion {
                        path: resolved_path.clone(),
                        diagnostics,
                    });
                continue;
            }
        };
        if output.exit_code.is_some_and(|code| code != 0) {
            result.reasons.push(JavaUnavailableReason::NonZeroExit {
                path: resolved_path.clone(),
                code: output.exit_code,
                diagnostics,
            });
            continue;
        }
        let compatibility = JavaCompatibility::for_requirement(version.major, required.major);
        if !compatibility.usable() {
            result
                .reasons
                .push(JavaUnavailableReason::IncompatibleMajor {
                    path: resolved_path.clone(),
                    actual: version.major,
                    required: required.major,
                });
            continue;
        }
        result.selected = Some(JavaRuntime {
            executable: resolved_path,
            version,
            compatibility,
            source: candidate.source,
        });
        break;
    }
    result
}

/// Probe for an installed Java executable when no authoritative Minecraft
/// version metadata has been resolved yet.  This is a readiness projection
/// only; a launch path must call `discover_java` with the resolved Java
/// requirement and exact compatibility policy.
pub fn discover_any_java<P: JavaProbe>(
    config: &JavaDiscoveryConfig,
    probe: &P,
) -> JavaDiscoveryResult {
    let required = JavaRequirement::minimum();
    discover_java(config, required, probe)
}

pub fn collect_java_candidates(config: &JavaDiscoveryConfig) -> Vec<JavaCandidate> {
    let mut candidates = Vec::new();
    if let Some(path) = config.configured_path.as_ref() {
        push_candidate(
            &mut candidates,
            path.clone(),
            JavaCandidateSource::Configured,
        );
    }
    if let Some(home) = config.java_home.as_ref() {
        let executable = if cfg!(windows) {
            home.join("bin").join("java.exe")
        } else {
            home.join("bin").join("java")
        };
        push_candidate(&mut candidates, executable, JavaCandidateSource::JavaHome);
    }
    if let Some(path_value) = config.path.as_ref() {
        for directory in std::env::split_paths(path_value) {
            push_candidate(
                &mut candidates,
                directory.join(if cfg!(windows) { "java.exe" } else { "java" }),
                JavaCandidateSource::Path,
            );
        }
    }
    candidates
}

fn push_candidate(candidates: &mut Vec<JavaCandidate>, path: PathBuf, source: JavaCandidateSource) {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| PathBuf::from("."))
    };
    let key = path.to_string_lossy().to_ascii_lowercase();
    if candidates
        .iter()
        .all(|candidate| candidate.path.to_string_lossy().to_ascii_lowercase() != key)
    {
        candidates.push(JavaCandidate { path, source });
    }
}

pub fn parse_java_version_output(output: impl AsRef<[u8]>) -> Result<JavaVersion> {
    let bytes = output.as_ref();
    let bounded = &bytes[..bytes.len().min(JAVA_VERSION_OUTPUT_LIMIT)];
    let text = String::from_utf8_lossy(bounded);
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let token = extract_version_token(line);
        if !(lower.contains("version")
            || lower.contains("java")
            || lower.contains("openjdk")
            || token.as_ref().is_some_and(|value| value.contains('.')))
        {
            continue;
        }
        if let Some(token) = token {
            if let Some(version) = parse_version_token(&token) {
                return Ok(JavaVersion {
                    raw: token,
                    major: version.0,
                    components: version.1,
                });
            }
        }
    }
    Err(MineDockError::JavaUnavailable(
        "java -version output did not contain a supported version".into(),
    ))
}

/// Compatibility alias used by adapters that model the parser as a function
/// named after the executable command.
pub fn parse_java_version(output: impl AsRef<[u8]>) -> Result<JavaVersion> {
    parse_java_version_output(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaReadiness {
    pub required: JavaRequirement,
    pub runtime: Option<JavaRuntime>,
    pub reasons: Vec<JavaUnavailableReason>,
}

impl JavaReadiness {
    pub fn from_discovery(required: JavaRequirement, result: JavaDiscoveryResult) -> Self {
        Self {
            required,
            runtime: result.selected,
            reasons: result.reasons,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.runtime.as_ref().is_some_and(JavaRuntime::is_usable)
    }

    pub fn status_text(&self) -> String {
        if let Some(runtime) = &self.runtime {
            return format!(
                "Java {} ready ({})",
                runtime.version.major,
                runtime.executable.display()
            );
        }
        self.reasons.first().map_or_else(
            || "Java runtime unavailable".into(),
            JavaUnavailableReason::safe_diagnostics,
        )
    }
}

#[derive(Debug, Clone)]
pub struct JavaRuntimeDiscovery<P> {
    pub config: JavaDiscoveryConfig,
    pub required: JavaRequirement,
    pub probe: P,
}

impl<P> JavaRuntimeDiscovery<P> {
    pub fn new(config: JavaDiscoveryConfig, required: JavaRequirement, probe: P) -> Self {
        Self {
            config,
            required,
            probe,
        }
    }

    pub fn with_probe<Q: JavaProbe>(self, probe: Q) -> JavaRuntimeDiscovery<Q> {
        JavaRuntimeDiscovery {
            config: self.config,
            required: self.required,
            probe,
        }
    }
}

impl<P: JavaProbe> JavaRuntimeDiscovery<P> {
    pub fn discover(&self) -> JavaReadiness {
        JavaReadiness::from_discovery(
            self.required,
            discover_java(&self.config, self.required, &self.probe),
        )
    }
}

fn extract_version_token(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let current = bytes[index] as char;
        if current.is_ascii_digit() {
            let start = index;
            while index < bytes.len() {
                let c = bytes[index] as char;
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+') {
                    index += 1;
                } else {
                    break;
                }
            }
            let token = &line[start..index];
            if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return Some(token.to_owned());
            }
        }
        index += 1;
    }
    None
}

fn parse_version_token(token: &str) -> Option<(JavaMajor, Vec<u32>)> {
    let core = token
        .split_once('+')
        .map_or(token, |(prefix, _)| prefix)
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '-')
        .trim();
    let mut components = Vec::new();
    for part in core.split(['.', '_', '-']) {
        if part.is_empty() {
            continue;
        }
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        components.push(digits.parse::<u32>().ok()?);
    }
    if components.is_empty() {
        return None;
    }
    let major_number = if components[0] == 1 && components.len() >= 2 {
        components[1]
    } else {
        components[0]
    };
    Some((
        JavaMajor::new(u16::try_from(major_number).ok()?).ok()?,
        components,
    ))
}

fn safe_output_text(bytes: Vec<u8>) -> String {
    const DIAGNOSTIC_LIMIT: usize = 4 * 1024;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    text.truncate(
        text.char_indices()
            .nth(DIAGNOSTIC_LIMIT)
            .map_or(text.len(), |(i, _)| i),
    );
    text.replace('\0', "�")
}

fn safe_error_text(error: &MineDockError) -> String {
    let mut text = error.to_string();
    text.truncate(4 * 1024);
    text.replace('\0', "�")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_legacy_modern_and_vendor_forms() {
        for (input, expected) in [
            (r#"java version ""1.8.0_381"""#, 8),
            (r#"openjdk version ""17.0.11"" 2024-04-16"#, 17),
            ("Eclipse Adoptium OpenJDK 21.0.2+13", 21),
            ("Zulu 17.52.17", 17),
            ("java 11.0.22 2024-01-16", 11),
        ] {
            assert_eq!(
                parse_java_version_output(input).expect(input).major.get(),
                expected
            );
        }
    }

    #[test]
    fn malformed_and_truncated_output_fail() {
        assert!(parse_java_version_output("garbage").is_err());
        let huge = format!(
            "openjdk version {}",
            "x".repeat(JAVA_VERSION_OUTPUT_LIMIT + 20)
        );
        assert!(parse_java_version_output(huge).is_err());
    }

    #[test]
    fn compatibility_distinguishes_exact_newer_and_old() {
        let required = JavaMajor::new(17).expect("major");
        assert_eq!(
            JavaCompatibility::for_requirement(JavaMajor::new(17).expect("major"), required),
            JavaCompatibility::Exact
        );
        assert_eq!(
            JavaCompatibility::for_requirement(JavaMajor::new(21).expect("major"), required),
            JavaCompatibility::NewerUnverified
        );
        assert_eq!(
            JavaCompatibility::for_requirement(JavaMajor::new(8).expect("major"), required),
            JavaCompatibility::TooOld
        );
    }

    #[test]
    fn configured_then_java_home_then_path_order_and_dedupe() {
        let root = TempDir::new().expect("temp");
        let configured = root.path().join("configured");
        let home = root.path().join("home");
        let path_dir = root.path().join("path");
        std::fs::create_dir_all(home.join("bin")).expect("home");
        std::fs::create_dir_all(&path_dir).expect("path");
        std::fs::write(&configured, b"java").expect("configured");
        std::fs::write(
            home.join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" }),
            b"java",
        )
        .expect("home java");
        std::fs::write(
            path_dir.join(if cfg!(windows) { "java.exe" } else { "java" }),
            b"java",
        )
        .expect("path java");
        let config = JavaDiscoveryConfig {
            configured_path: Some(configured.clone()),
            java_home: Some(home),
            path: Some(std::env::join_paths([path_dir]).expect("path")),
            ..JavaDiscoveryConfig::default()
        };
        let candidates = collect_java_candidates(&config);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].source, JavaCandidateSource::Configured);
        assert_eq!(candidates[1].source, JavaCandidateSource::JavaHome);
        assert_eq!(candidates[2].source, JavaCandidateSource::Path);
    }
}
