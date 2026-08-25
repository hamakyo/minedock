# MineDock Product Specification

## 1. Definition

MineDock is a local-first desktop manager for Minecraft Java Edition worlds and self-hosted server sessions.

It turns server administration concepts into a world library:

- create a world from a template;
- start/stop it;
- see current state and players;
- retain its server profile and settings;
- archive/backup it;
- reopen it later.

## 2. Target user

Primary:
- 2–8 friends playing on a PC-hosted Minecraft server;
- host is comfortable installing a desktop app but should not need to maintain a server directory manually.

Secondary:
- technical users who want multiple isolated local worlds and reproducible server profiles.

## 3. Core domain

### World

A durable player-facing item.

Required metadata:
- `id`
- `name`
- `created_at`
- `last_played_at`
- `template_id`
- `server_profile_id`
- `status`
- `total_playtime_seconds`
- local data path

### WorldTemplate

Reusable defaults for generating a world + server profile.

Examples:
- Vanilla Survival
- Hardcore
- Creative

A template is configuration, not executable code.

### ServerProfile

Defines how a world is hosted.

MVP fields:
- edition: Java
- distribution: Vanilla
- Minecraft version selector
- memory minimum / maximum
- server port
- max players
- online mode
- whitelist enabled
- gameplay/server properties

### Runtime

Execution requirements for a profile.

MVP:
- Java executable resolution
- Minecraft Vanilla server JAR

Later:
- bundled Java runtime
- Paper/Fabric/NeoForge providers
- container backend

### ServerSession

One start-to-stop execution of a world.

Fields:
- session id
- world id
- started at
- ended at
- exit reason
- player activity/events
- log path

## 4. Built-in templates

### Vanilla Survival

- gamemode: survival
- difficulty: normal
- hardcore: false
- whitelist: true
- online-mode: true
- max players: 4

### Hardcore

- gamemode: survival
- difficulty: hard
- hardcore: true
- whitelist: true
- online-mode: true
- max players: 4
- shutdown backup: true

### Creative

- gamemode: creative
- difficulty: peaceful
- hardcore: false
- whitelist: true
- online-mode: true
- max players: 4

## 5. World creation

Default wizard:

1. Name
2. Template
3. Minecraft version
4. Max players
5. Seed (optional)
6. Network visibility
7. Create

Advanced fields are collapsed.

Creation must not start a server until provisioning succeeds.

## 6. Lifecycle

Allowed high-level states:

```text
Stopped
  -> Preparing
  -> Starting
  -> Running
  -> Stopping
  -> BackingUp
  -> Stopped

Any active state -> Failed
Failed -> Stopped (after cleanup/recovery)
```

Starting twice must be impossible.

Stopping while already stopped is idempotent.

Closing MineDock while a managed server is running must prompt or intentionally preserve/stop the process according to a clearly defined policy. MVP should prefer safe stop.

## 7. Process behavior

For Vanilla Java:

```text
java <memory flags> -jar server.jar nogui
```

MineDock captures:
- stdin
- stdout
- stderr
- process id
- exit code

MineDock may send server console commands.

Graceful shutdown:
1. mark `Stopping`;
2. send `stop\n`;
3. wait up to configured timeout;
4. if process exits, mark safe shutdown;
5. only after timeout offer/escalate forced termination;
6. optionally backup;
7. mark `Stopped`.

## 8. Logs and events

Convert raw server logs to structured events where reliable:

- server ready
- player joined
- player left
- player died
- server stopping
- server stopped
- warning/error

Raw logs remain available even when parsing fails.

Log parsing must be best-effort and must not control critical lifecycle transitions unless the signal is specifically reliable and tested.

## 9. Backup

MVP backup type:
- local verified ZIP archive after server shutdown; a pre-archive directory
  snapshot is not a current backup contract.

Backup metadata:
- world id
- timestamp
- size
- uncompressed size and completed archive size
- reason (`manual`, `shutdown`, `pre-upgrade`)
- Minecraft version
- per-file and completed-archive integrity digest

Restore is post-v0.1 unless it can be implemented safely without delaying the MVP.

Never archive a partially written world through a naive copy while the server is actively saving.

## 10. Version/runtime acquisition

The runtime layer must be provider-based.

Conceptual interfaces:

```rust
trait JavaRuntimeProvider { /* resolve/install */ }
trait ServerDistributionProvider { /* versions/download/provision */ }
trait ServerProcess { /* start/command/stop/events */ }
```

MVP provider:
- Vanilla Java Edition.

Version and download data should come from Mojang/Minecraft-authoritative endpoints. Do not hard-code a single current release URL into domain logic.

## 11. EULA requirement

Before MineDock downloads/runs Minecraft server software for the first time:
- show an explicit EULA acknowledgement;
- link/open the official EULA;
- require affirmative user action;
- store acknowledgement metadata;
- do not pre-check the box.

## 12. Networking

MVP modes:

### LAN
Show host LAN address and port.

### Direct
Allow configuring the server port and show diagnostics/instructions. Do not silently change router/firewall configuration.

Post-MVP:
- Tailscale/private network
- remote status/control
- optional Cloudflare services for control plane/backups

## 13. Persistence

MineDock owns metadata, not the user's Minecraft identity.

All records need schema versions.

Writes to important metadata should be atomic:
- write temp
- fsync where appropriate
- rename/replace

World directories must remain recoverable even if MineDock metadata is damaged.

## 14. UI

Top-level navigation in MVP:

- Worlds
- Backups (can initially be per-world only)
- Settings

World card:
- name
- template/distribution/version
- lifecycle state
- player count if known
- start/stop action
- last played

World detail:
- status
- connection info
- players
- recent log
- settings
- backup action

## 15. Non-functional requirements

- Windows 11 first.
- No admin rights required for normal app execution.
- No Docker prerequisite.
- Must not freeze UI during server I/O or compression.
- Common failures should result in actionable messages.
- Domain logic unit-testable without GPUI.
- Never execute a file downloaded from an untrusted arbitrary URL supplied by a world template.
