# AGENTS.md — MineDock

## Mission

Build MineDock: a local-first desktop app that makes self-hosted Minecraft Java Edition worlds feel like a library of playable sessions rather than folders of server files.

The default user experience should be:

**Create world -> choose template -> start -> play -> stop -> automatic safe backup.**

## Product principles

1. **World-first mental model**
   - The primary object users see is a World.
   - A server process is an implementation detail attached to a World.

2. **Local-first**
   - Worlds, metadata, logs, and backups live locally in MVP.
   - Cloud integrations must remain optional adapters.

3. **One-click common path**
   - Do not expose Java flags, server properties, RCON, ports, or file paths in the normal flow unless needed.

4. **Power-user escape hatch**
   - Advanced settings may expose raw server properties later.
   - Never make advanced configuration the default UX.

5. **Safe lifecycle**
   - Do not kill a healthy server process first.
   - Prefer graceful console command `stop`.
   - Back up only after a known-safe state or use a coordinated save sequence.

6. **No Docker dependency in MVP**
   - MineDock launches the Java server process directly.
   - Keep a runtime abstraction so alternative execution backends can be added later.

7. **Do not special-case Hardcore in the architecture**
   - Hardcore, Survival, and Creative are templates.

## Technical rules

### Workspace boundaries

- `minedock-core`
  - domain models
  - persistence interfaces
  - world lifecycle orchestration
  - runtime/download provider traits
  - template parsing
  - Minecraft configuration generation
  - process-independent business rules
  - NO GPUI imports

- `minedock-app`
  - GPUI
  - desktop lifecycle
  - user interaction
  - adapters to core
  - Windows-specific integration where unavoidable

If a function can be unit tested without a window, it probably belongs in `minedock-core`.

### Rust

- Stable Rust.
- Prefer explicit domain types over strings.
- Use `PathBuf` for paths.
- Use `Result<T, MineDockError>` across core boundaries.
- No `unwrap()` in non-test lifecycle/download/persistence code.
- Do not block the GPUI event loop with process I/O or filesystem compression.
- Use structured events for server log/lifecycle updates.

### Async/concurrency

Server output, downloads, backups, and process waiting are asynchronous background jobs.

Model process state explicitly:

```text
Stopped
Preparing
Starting
Running
Stopping
BackingUp
Failed
```

Do not infer authoritative process state only from button clicks.

### Persistence

Metadata and actual world contents are separate.

Suggested app data:

```text
MineDockData/
├─ minedock.json
├─ worlds/
│  └─ <world-id>/
│     ├─ minedock-world.json
│     ├─ server/
│     ├─ world/
│     └─ logs/
├─ runtimes/
├─ downloads/
└─ backups/
```

Do not store absolute paths in template files.

### Minecraft EULA

MineDock must not silently accept the Minecraft EULA for the user.

Before the first Mojang server download/run:
- show that server software is subject to the Minecraft EULA;
- require an explicit user action;
- persist only that MineDock has recorded the user's acceptance;
- then create/write the server's required EULA file as part of provisioning.

### Networking

MVP:
- LAN is supported.
- Direct internet hosting may expose instructions/status but must not attempt unsafe automatic router reconfiguration.
- Do not ship UPnP port-forward automation in v0.1.

Later:
- Tailscale/private networking adapter
- remote-control plane
- Cloudflare-backed optional backup/control features

### Security

- Default `online-mode=true`.
- Recommend/enable whitelist for friend servers.
- Never log access tokens or credentials.
- Treat world imports, plugin/mod files, and archive extraction as untrusted input.
- Prevent archive path traversal.
- Validate any downloaded artifact before execution when an authoritative hash is available.

## Implementation order

Follow `PLAN.md`. Do not jump directly to mods, Paper, Cloudflare, or polished animations.

Highest priority:
1. core domain and persistence;
2. template engine;
3. server process lifecycle;
4. minimal GPUI world library;
5. safe stop + backup;
6. create-world wizard;
7. quality/tests.

## Definition of done for MVP

See `docs/MVP.md`.

## Commands

Expected eventually:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p minedock-app
```

## When GPUI API differs

GPUI is pre-1.0. If the scaffold no longer compiles:

1. consult the current official `zed-industries/zed` GPUI README/examples;
2. make the smallest API adaptation inside `minedock-app`;
3. do not change core architecture just to match GPUI;
4. pin working dependency versions in `Cargo.lock`;
5. note any compatibility workaround in a short ADR if substantial.

## Non-goals for the first pass

Do NOT implement:
- Bedrock Edition
- Paper/Fabric/NeoForge
- CurseForge/Modrinth integration
- plugin/mod dependency solving
- Cloudflare Workers
- R2 backups
- mobile apps
- public server discovery
- account login
- auto-UPnP
- Docker
