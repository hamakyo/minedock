# MineDock Implementation Plan

## Phase 0 — Stabilize scaffold

Goal: a clean workspace that Codex can iterate on.

- [ ] Confirm current stable Rust.
- [ ] Confirm current official GPUI + `gpui_platform` dependency/API.
- [ ] Pin a working lockfile.
- [ ] `cargo fmt --all`.
- [ ] `cargo check --workspace`.
- [ ] Keep all GPUI-specific changes inside `minedock-app`.

Exit:
- GPUI window opens.
- Core tests run.

## Phase 1 — Domain + persistence

- [ ] Finalize `WorldId`, `World`, `ServerProfile`, `WorldStatus`.
- [ ] Add schema-versioned app metadata.
- [ ] Implement app-data directory resolution.
- [ ] Implement atomic JSON persistence.
- [ ] Add repository/service interfaces.
- [ ] Test round-trip persistence.
- [ ] Test invalid/corrupt metadata failure behavior.

Exit:
- MineDock can create/list metadata-only worlds across app restarts.

## Phase 2 — Template engine

- [ ] Parse built-in YAML.
- [ ] Validate template IDs.
- [ ] Validate enum/property values.
- [ ] Prevent executable/script/download fields.
- [ ] Generate `ServerProfile` and world settings from template.
- [ ] Create world wizard UI.

Exit:
- Survival, Hardcore, Creative can be selected and persisted.

## Phase 3 — Java runtime resolution

- [ ] Detect usable Java.
- [ ] Parse `java -version`.
- [ ] Model required Java version from selected Minecraft server version.
- [ ] Provide actionable failure UI.
- [ ] Design bundled-runtime provider but do not overbuild it.

Exit:
- App can tell whether a selected server can run.

## Phase 4 — Vanilla distribution provider

- [ ] Resolve Minecraft versions from an authoritative Mojang/Minecraft source.
- [ ] Resolve server JAR metadata.
- [ ] Download into content-addressed/versioned cache.
- [ ] Verify size/hash where authoritative metadata provides it.
- [ ] Never use arbitrary URLs from templates.
- [ ] Add explicit Minecraft EULA acknowledgement flow.
- [ ] Generate `eula.txt` only after explicit acceptance.
- [ ] Generate `server.properties`.

Exit:
- A world directory can be provisioned reproducibly.

## Phase 5 — Process lifecycle

- [ ] Spawn Java with piped stdin/stdout/stderr.
- [ ] Capture PID/handle.
- [ ] Stream raw log lines.
- [ ] Add state machine.
- [ ] Detect process exit.
- [ ] Implement graceful `stop`.
- [ ] Implement stop timeout and explicit escalation.
- [ ] Prevent concurrent starts of same world.
- [ ] Recover stale "running" metadata after app crash.

Exit:
- Vanilla world reliably starts/stops from core.

## Phase 6 — GPUI World Library

- [ ] Render persisted world cards.
- [ ] Start / Stop actions.
- [ ] Lifecycle indicators.
- [ ] Player count if known.
- [ ] World detail panel.
- [ ] Recent logs.
- [ ] Disable invalid actions according to state.

Exit:
- Main workflow is usable without terminal commands.

## Phase 7 — Log parser + player events

- [ ] Raw logs first.
- [ ] Parse join/leave.
- [ ] Parse common death messages best-effort.
- [ ] Session record.
- [ ] Total playtime accounting.
- [ ] Do not depend on death parsing for critical state.

Exit:
- Useful live world/session information appears in UI.

## Phase 8 — Backups

- [ ] Backup only from safe stopped state initially.
- [ ] Create timestamped local backup.
- [ ] Record metadata and size.
- [ ] Auto-backup on successful shutdown when enabled.
- [ ] Retention policy.
- [ ] Guard against path traversal during future restore.

Exit:
- A stopped world is automatically recoverable from local backup artifacts.

## Phase 9 — Networking UX

- [ ] Show listening port.
- [ ] Show LAN address(es).
- [ ] Copy connection address.
- [ ] Basic bind/port diagnostics.
- [ ] Document direct internet hosting without silently changing router/firewall.

Exit:
- Friends on the same network can connect with minimal friction.

## Phase 10 — MVP hardening

- [ ] Crash recovery.
- [ ] Corrupt metadata handling.
- [ ] Partial download recovery.
- [ ] Disk space checks.
- [ ] Process timeout tests.
- [ ] Windows packaging.
- [ ] Basic telemetry-free diagnostics bundle.
- [ ] README user setup.

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
