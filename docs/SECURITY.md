# Security Notes

This document distinguishes controls present in the Phase 0–5 foundations from the initial Phase 6 GPUI Start/EULA workflow and the remaining MVP work. The current GPUI can begin provider-backed server preparation from a stopped world; Stop, backups, and networking UX remain open.

## Defaults

- `online-mode=true`
- whitelist enabled in built-in friend-world templates
- no auto-UPnP in MVP
- no arbitrary template scripts
- no arbitrary template download URLs

## Trust boundaries

Untrusted:
- imported archives
- future mods/plugins
- user-provided paths
- network responses
- downloaded runtime/server artifacts until validated

Trusted only after validation:
- MineDock-owned metadata
- artifacts from configured authoritative providers

## Implemented metadata and template controls

- Template YAML uses strict schemas and rejects unknown or executable/download/script fields.
- World data paths are canonical relative paths under the app-data root; absolute paths and traversal components are rejected.
- Schema-v1 metadata is fully validated before it replaces the in-memory library projection.
- Metadata and lifecycle records use atomic file replacement.
- The Windows app holds an exclusive app-data lease. If it cannot acquire the lease, mutating UI actions are disabled.

## Implemented EULA and configuration controls

- A caller must explicitly record acceptance through the EULA service. Passing `false` does not record acceptance.
- Artifact acquisition and provisioning return `EulaRequired` before proceeding without a valid acceptance record.
- `eula.txt` is written only during accepted provisioning; an existing `eula.txt` is not treated as evidence that MineDock recorded acceptance.
- Provisioning persists an acceptance record separately from `eula.txt`, and launch validation checks both.
- `server.properties` is generated from an allowlist with `online-mode=true`.
- Launch validation rejects missing or modified provision records, JARs, EULA state, and properties that remove or disable online mode.

The GPUI EULA notice and confirmation dialog now connect the explicit `I Agree` action to the guarded service before enabling Start. Cancel, link-opening failures, and acceptance persistence failures keep provisioning blocked.

## Implemented download controls

- Templates cannot specify download URLs.
- The Vanilla provider accepts HTTPS only and exact allowlisted Mojang authorities for metadata and server artifacts.
- Automatic redirects are disabled. Each redirect target is parsed and revalidated, and redirect count and response sizes are bounded.
- Metadata/artifact sizes and SHA-1 values are checked when authoritative metadata supplies them; incomplete downloads are not promoted to the cache.
- Provision and launch paths are revalidated against symlinks/reparse points at the native boundary.

## Archive rules

When backup restore/import is implemented:
- reject absolute archive paths;
- reject `..` traversal;
- reject links escaping target root;
- extract into temporary directory;
- validate before atomic move.

## Process rules

- never concatenate user values into shell command strings;
- use `std::process::Command` argument arrays;
- validate Java executable path;
- server templates cannot specify executable paths.

The native adapter also:

- uses exact argument arrays with piped standard streams;
- spawns suspended on Windows, attaches the process to a kill-on-close Job Object, then resumes it;
- scopes force termination to an opaque token issued only after a graceful-stop timeout;
- reserves each world against concurrent starts and rolls back failed starts;
- bounds Java probe output/time and server log buffering;
- reconciles persisted active states to `Failed` after an app restart under the app-data lease.

## Secrets

MVP should need no cloud secret.

Future tokens:
- OS credential store where available;
- never plaintext in world YAML;
- never logs.

## Network

A Minecraft server listening on all interfaces is externally reachable if host/router/firewall permit it.

UI must clearly distinguish:
- LAN
- explicitly configured direct exposure
- future private-network modes

The current UI does not start a listening server or configure firewall/router rules. LAN address/port display and direct-exposure guidance remain Phase 9 work.

## Verification limits

The offline tests exercise provider validation, EULA/provision invariants, lifecycle transitions, timeout authorization, and a controlled native child process. Remaining coverage debt includes direct execution of the production Windows Job Object path, lifecycle lease contention, Java descendant cleanup on timeout, and a scripted real HTTP 3xx hop through the production transport. These are test gaps, not known bypasses, and should be closed before declaring the MVP hardened.
