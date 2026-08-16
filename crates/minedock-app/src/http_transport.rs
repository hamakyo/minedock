//! Bounded, redirect-explicit HTTPS transport for Mojang authority endpoints.
//!
//! This adapter is intentionally app-side: the core provider receives only a
//! narrow `AuthoritativeTransport` response and remains independent of HTTP,
//! TLS, and operating-system concerns. Automatic redirects are disabled so an
//! untrusted `Location` is validated before any follow-up request is made.

use minedock_core::{
    AuthoritativeTransport, MAX_REDIRECT_HOPS, MineDockError, PathSafety, Result,
    TransportResponse, validate_authoritative_url,
};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const METADATA_AUTHORITIES: &[&str] = &["piston-meta.mojang.com", "launchermeta.mojang.com"];
const ARTIFACT_AUTHORITIES: &[&str] = &["piston-data.mojang.com", "launcher.mojang.com"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Production path-safety adapter. Windows reparse-point inspection stays in
/// minedock-app; core only consumes this injected boundary.
#[derive(Debug, Clone, Copy, Default)]
pub struct AppPathSafety;

impl PathSafety for AppPathSafety {
    fn validate_existing_ancestors(&self, path: &Path) -> Result<()> {
        let mut current = Path::new(path).to_path_buf();
        loop {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() || is_reparse(&metadata) => {
                    return Err(MineDockError::Persistence(
                        "path contains a symlink or reparse point".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(MineDockError::Persistence(error.to_string())),
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

    fn reject_path_type(&self, path: &Path, directory: bool) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(MineDockError::Persistence(error.to_string())),
        };
        if metadata.file_type().is_symlink()
            || is_reparse(&metadata)
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(MineDockError::Persistence(
                "path is a reparse point or has an unexpected type".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

#[derive(Debug, Clone)]
pub struct MojangHttpsTransport {
    agent: ureq::Agent,
}

impl MojangHttpsTransport {
    pub fn new() -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(REQUEST_TIMEOUT)
            .build();
        Ok(Self { agent })
    }

    pub fn with_timeout(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(MineDockError::Download(
                "HTTPS transport timeout must be nonzero".into(),
            ));
        }
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(timeout)
            .build();
        Ok(Self { agent })
    }

    fn request_once(&self, url: &str, max_bytes: usize) -> Result<(u16, Option<String>, Vec<u8>)> {
        let response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => {
                return Err(MineDockError::Download(format!(
                    "HTTPS request failed: {error}"
                )));
            }
        };
        let status = response.status();
        let location = response.header("Location").map(str::to_owned);
        if let Some(content_length) = response.header("Content-Length") {
            let declared = content_length.parse::<u64>().map_err(|_| {
                MineDockError::Download("HTTPS response has an invalid Content-Length".into())
            })?;
            if declared > max_bytes as u64 {
                return Err(MineDockError::Download(
                    "HTTPS response exceeds the byte ceiling".into(),
                ));
            }
        }
        let mut body = Vec::new();
        let read_limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut reader = response.into_reader().take(read_limit);
        reader.read_to_end(&mut body).map_err(|error| {
            MineDockError::Download(format!("could not read HTTPS response: {error}"))
        })?;
        if body.len() > max_bytes {
            return Err(MineDockError::Download(
                "HTTPS response exceeds the byte ceiling".into(),
            ));
        }
        Ok((status, location, body))
    }

    fn allowed_authorities(initial_url: &str) -> Result<&'static [&'static str]> {
        let (_, rest) = initial_url
            .split_once("://")
            .ok_or_else(|| MineDockError::Download("HTTPS URL has no authority".into()))?;
        let authority = rest.split('/').next().unwrap_or_default();
        if METADATA_AUTHORITIES.contains(&authority) {
            Ok(METADATA_AUTHORITIES)
        } else if ARTIFACT_AUTHORITIES.contains(&authority) {
            Ok(ARTIFACT_AUTHORITIES)
        } else {
            Err(MineDockError::Download(
                "HTTPS URL is not an allowed Mojang endpoint".into(),
            ))
        }
    }
}

impl AuthoritativeTransport for MojangHttpsTransport {
    fn get(&self, url: &str, max_bytes: usize) -> Result<TransportResponse> {
        if max_bytes == 0 {
            return Err(MineDockError::Download(
                "HTTPS byte ceiling must be nonzero".into(),
            ));
        }
        let authorities = Self::allowed_authorities(url)?;
        let mut current = url.to_owned();
        let mut redirects = Vec::new();
        for _ in 0..=MAX_REDIRECT_HOPS {
            // Validate the exact URL before opening a socket for this hop.
            validate_authoritative_url(&current, authorities)?;
            let (status, location, body) = self.request_once(&current, max_bytes)?;
            if (300..400).contains(&status) {
                let next = location.ok_or_else(|| {
                    MineDockError::Download("HTTPS redirect has no Location".into())
                })?;
                // Mojang metadata URLs are absolute. Reject relative locations
                // rather than resolving attacker-controlled paths locally.
                validate_authoritative_url(&next, authorities)?;
                redirects.push(next.clone());
                current = next;
                continue;
            }
            return Ok(TransportResponse {
                status,
                requested_url: url.to_owned(),
                final_url: current,
                redirects,
                body,
            });
        }
        Err(MineDockError::Download(format!(
            "HTTPS redirect chain exceeded {} hops",
            MAX_REDIRECT_HOPS
        )))
    }
}

impl Default for MojangHttpsTransport {
    fn default() -> Self {
        // Agent construction is infallible for the ureq 2.x API.
        Self {
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .timeout(REQUEST_TIMEOUT)
                .build(),
        }
    }
}

pub fn production_vanilla_provider(
    downloads_root: impl Into<std::path::PathBuf>,
) -> Result<minedock_core::VanillaProvider<MojangHttpsTransport, AppPathSafety>> {
    Ok(
        minedock_core::VanillaProvider::new(MojangHttpsTransport::new()?, downloads_root)
            .with_path_safety(AppPathSafety),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_authorities_are_rejected_before_any_request() {
        let transport = MojangHttpsTransport::default();
        for url in [
            "https://user@piston-meta.mojang.com/version.json",
            "https://piston-meta.mojang.com:443/version.json",
            "https://evil.example/version.json",
            "http://piston-meta.mojang.com/version.json",
            "https://piston-meta.mojang.com.evil.example/version.json",
        ] {
            assert!(
                transport.get(url, 1024).is_err(),
                "URL must be rejected: {url}"
            );
        }
    }

    #[test]
    fn malformed_redirect_targets_are_rejected_before_contact() {
        // The production adapter has no local listener or fake network
        // dependency; malformed targets fail before a socket is opened.
        for url in [
            "https://piston-meta.mojang.com/version.json?redirect=https://evil.example",
            "https://piston-meta.mojang.com/version.json#evil",
        ] {
            assert!(MojangHttpsTransport::default().get(url, 1024).is_err());
        }
    }
}
