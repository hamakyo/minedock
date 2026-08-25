# MineDock Implementation Plan

Checklist status reflects code and tests in the repository, not the final MVP
acceptance scenario. The implementation now includes persisted session logs,
best-effort player activity, safe backups, telemetry-free diagnostics, and a
portable Windows package workflow. The native Windows acceptance scenario is
still a manual release gate.

## Phase 0 — Stabilize scaffold

Goal: a clean workspace that Codex can iterate on.

- [x] Confirm current stable Rust.
- [x] Confirm and pin the official GPUI dependency/API (`gpui` 0.2.2 includes its platform backend).
- [x] Pin a working lockfile.
- [x] `cargo fmt --all`.
- [x] `cargo check --workspace`.
- [x] Keep all GPUI-specific changes inside `minedock-app`.

Exit:
- GPUI window opens.
- Core tests run.

## Phase 1 — Domain + persistence

- [x] Finalize `WorldId`, `World`, `ServerProfile`, `WorldStatus`.
- [x] Add schema-versioned app metadata.
- [x] Implement app-data directory resolution.
- [x] Implement atomic JSON persistence.
- [x] Add repository/service interfaces.
- [x] Test round-trip persistence.
- [x] Test invalid/corrupt metadata failure behavior.

Exit:
- MineDock can create/list metadata-only worlds across app restarts.

## Phase 2 — Template engine

- [x] Parse built-in YAML.
- [x] Validate template IDs.
- [x] Validate enum/property values.
- [x] Prevent executable/script/download fields.
- [x] Generate `ServerProfile` and world settings from template.
- [x] Create world wizard UI.

Exit:
- Survival, Hardcore, Creative can be selected and persisted.

## Phase 3 — Java runtime resolution

- [x] Detect usable Java.
- [x] Parse `java -version`.
- [x] Model required Java version from selected Minecraft server version.
- [x] Provide actionable failure UI with a persisted Java executable path and retry.
- [x] Design the bundled-runtime provider seam without shipping a bundled download yet.

Exit:
- App can tell whether a selected server can run.

## Phase 4 — Vanilla distribution provider

- [x] Resolve Minecraft versions from an authoritative Mojang/Minecraft source.
- [x] Resolve server JAR metadata.
- [x] Download into content-addressed/versioned cache.
- [x] Verify size/hash where authoritative metadata provides it.
- [x] Never use arbitrary URLs from templates.
- [x] Add explicit Minecraft EULA acknowledgement flow.
- [x] Persist explicit acceptance and generate `eula.txt` only after that acceptance.
- [x] Generate `server.properties`.

Exit:
- A world directory can be provisioned reproducibly.

## Phase 5 — Process lifecycle

- [x] Spawn Java with piped stdin/stdout/stderr.
- [x] Capture PID/handle.
- [x] Stream raw log lines.
- [x] Add state machine.
- [x] Detect process exit.
- [x] Implement graceful `stop`.
- [x] Implement stop timeout and explicit escalation.
- [x] Prevent concurrent starts of same world.
- [x] Recover stale "running" metadata after app crash.

Exit:
- Vanilla world reliably starts/stops from core.

## Phase 6 — GPUI World Library

- [x] Render persisted world cards.
- [x] Start action: resolve, provision, and launch through the lifecycle runtime.
- [x] Stop action.
- [x] Lifecycle indicators.
- [x] Player count if known.
- [x] World detail panel.
- [x] Recent raw logs for the current and latest persisted server session.
- [x] Disable invalid actions according to state.

Exit:
- Main workflow is usable without terminal commands.

## Phase 7 — Log parser + player events

- [x] Raw logs first.
- [x] Parse join/leave.
- [x] Parse common death messages best-effort.
- [x] Session record.
- [x] Total playtime accounting.
- [x] Do not depend on death parsing for critical state.

Exit:
- Useful live world/session information appears in UI.

## Phase 8 — Backups

- [x] Backup only from safe stopped state initially.
- [x] Create timestamped local backup.
- [x] Record metadata, size, and SHA-256 digests.
- [x] Auto-backup on successful shutdown when enabled.
- [x] Retention policy.
- [x] Guard against path traversal during future restore.

Exit:
- A stopped world is automatically recoverable from local backup artifacts.

## Phase 9 — Networking UX

- [x] Show the configured listening port.
- [x] Show usable LAN IPv4 address(es) without guessing when candidates are ambiguous.
- [x] Copy connection address.
- [x] Show basic no-address and invalid-configured-port diagnostics.
- [x] Document direct internet hosting without silently changing router/firewall.

Exit:
- Friends on the same network can connect with minimal friction.

## Phase 10 — MVP hardening

- [x] Crash recovery.
- [x] Corrupt metadata handling.
- [x] Partial download recovery.
- [x] Disk space checks.
- [x] Process timeout tests.
- [x] Windows packaging.
- [x] Basic telemetry-free diagnostics bundle.
- [x] README user setup.

## Post-MVP backlog

Only after MVP:
- Paper
- Fabric
- NeoForge
- Modrinth
- CurseForge
- plugin/mod dependency management
- world import/export
- backup restore UI
- Tailscale adapter
- Cloudflare control plane
- R2 backups
- remote start/stop
- Bedrock
