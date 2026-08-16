# MineDock

> Local-first Minecraft Java Edition world library and one-click server manager.

MineDock is a desktop application for creating, starting, stopping, backing up, and organizing Minecraft worlds without making users manually maintain server folders or Docker containers.

The product model is **World Library first, Server Manager second**:

- each world is a durable library item;
- server runtime/configuration is attached to the world through a server profile;
- common play styles are expressed as reusable templates;
- starting a world should be close to one click;
- advanced Minecraft/server concepts stay available, but are hidden from the default flow.

## Current direction

- Desktop UI: **GPUI**
- Language: **Rust**
- Architecture: **Rust core independent from GPUI**
- Initial platform: **Windows 11**
- Initial Minecraft target: **Java Edition**
- Initial runtimes: **Vanilla only**
- Process model: **launch Java directly; Docker is not required**
- Storage: **local-first**
- Networking in MVP: **LAN + direct/manual exposure**
- Cloud/remote management: **post-MVP**

Hardcore is **not a special product mode**. It is one built-in world template.

## Repository layout

```text
MineDock/
├─ AGENTS.md
├─ CODEX_PROMPT.md
├─ README.md
├─ SPEC.md
├─ ARCHITECTURE.md
├─ PLAN.md
├─ Cargo.toml
├─ rust-toolchain.toml
├─ crates/
│  ├─ minedock-core/
│  └─ minedock-app/
├─ docs/
│  ├─ ADR-001-local-first.md
│  ├─ ADR-002-world-server-separation.md
│  ├─ MVP.md
│  ├─ UI.md
│  └─ SECURITY.md
└─ templates/
   ├─ vanilla-survival.yml
   ├─ hardcore.yml
   └─ creative.yml
```

## First implementation target

The first useful milestone is:

1. launch the GPUI shell;
2. load built-in YAML templates;
3. create a world record from a template;
4. persist the MineDock metadata;
5. launch a local Vanilla server process for that world;
6. stream logs into the UI;
7. stop gracefully using `stop`;
8. make a backup after shutdown.

See `CODEX_PROMPT.md` and `PLAN.md`.

## Important constraints

- Never commit Minecraft server JARs, worlds, Java runtimes, secrets, or user backups.
- Do not silently accept Minecraft's EULA on behalf of a user. MineDock should present an explicit first-run acceptance flow before downloading/running server software.
- Keep Minecraft runtime/download logic behind provider traits so Vanilla/Paper/Fabric support can be added later.
- Avoid coupling domain state to GPUI entities.
- GPUI is pre-1.0 and may introduce breaking changes. Keep UI-specific code isolated in `minedock-app`.

## Status

This archive is an implementation starter for Codex. The UI and process manager are intentionally thin: architecture, invariants, domain types, templates, and work order are defined so the implementation can proceed without redesigning the project first.
