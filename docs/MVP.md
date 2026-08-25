# MVP Definition

> Implementation status: the current build supports the GPUI World Library, built-in template selection, metadata-only world creation/persistence, explicit EULA confirmation, Start/Stop lifecycle wiring, Java path recovery settings, world details, persisted session raw logs, best-effort player activity, LAN endpoint display/copy, stopped-world backups, automatic successful-shutdown backups, telemetry-free diagnostics, and a portable Windows package workflow. The clean Windows 11 acceptance run remains a manual release gate. See [../PLAN.md](../PLAN.md) and [windows-acceptance.md](windows-acceptance.md).

## Goal

A Windows user can install MineDock, create one of three Vanilla Java Edition world types, start it locally, let friends on the LAN join, stop it safely, and receive an automatic local backup.

## Required

### App
- GPUI desktop window
- World Library
- create-world wizard
- world details
- settings sufficient for Java/runtime paths

### Worlds
- Survival
- Hardcore
- Creative
- custom name
- optional seed
- max players
- whitelist setting
- selected Minecraft release

### Runtime
- Java detection
- Vanilla server acquisition
- explicit EULA acceptance
- server.properties generation
- direct Java process launch

### Lifecycle
- start
- server-ready feedback
- raw logs
- stop
- process exit handling
- no double start
- failed state and useful errors

### Storage
- persistent MineDock metadata
- persistent world folders
- local logs
- automatic safe shutdown backup
- schema-versioned session records and replayable player activity
- backup manifest, digest, retention, and restart-visible status

### Networking
- local address + port display
- copy connection endpoint

## Not required

- internet NAT traversal
- UPnP
- Tailscale
- Cloudflare
- Docker
- mods
- plugins
- Paper/Fabric/NeoForge
- Bedrock
- server marketplace
- Minecraft account authentication
- embedded terminal
- multi-host orchestration

## Acceptance scenario

1. Fresh Windows 11 machine with Minecraft client installed.
2. Launch MineDock.
3. MineDock detects Java or explains exactly what is missing.
4. Click `New World`.
5. Choose `Hardcore`.
6. Name it `Sunday Hardcore`.
7. Keep defaults: 4 players, whitelist enabled.
8. Accept Minecraft EULA explicitly when requested.
9. MineDock provisions the server.
10. Click `Start`.
11. UI progresses `Preparing -> Starting -> Running`.
12. Copy `192.168.x.x:25565`.
13. Another Java Edition client on LAN connects.
14. Logs and best-effort player activity update; unknown remains visible until
    the server provides a reliable ready baseline.
15. Click `Stop`.
16. Server stops gracefully.
17. MineDock creates a local backup.
18. Reopen MineDock.
19. `Sunday Hardcore` remains in the library and can start again.

If this scenario works reliably, MVP is functionally successful.

The session log and backup records must also remain visible after relaunch;
the clean Windows 11 evidence for this scenario is tracked in
[windows-acceptance.md](windows-acceptance.md).
