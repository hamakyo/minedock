<h1 align="center">MineDock</h1>

<p align="center">
  <strong>A Windows-first, local-first library for Minecraft Java Edition worlds.</strong>
</p>

<p align="center">
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white">
  <img alt="GPUI 0.2.2" src="https://img.shields.io/badge/GPUI-0.2.2-5A67D8">
  <img alt="Windows 11" src="https://img.shields.io/badge/Platform-Windows%2011-0078D4?logo=windows11&logoColor=white">
  <img alt="Work in progress" src="https://img.shields.io/badge/Status-Work%20in%20progress-F2C94C">
</p>

<p align="center">
  English | <a href="README.ja.md">日本語</a>
</p>

MineDock presents durable Worlds rather than server folders. A server process is an implementation detail attached to a world through a server profile; Docker is not required.

## Current status

The MVP implementation now covers the core library, lifecycle, session history,
player activity projection, safe local backups, diagnostics, and a portable
Windows package. Native clean-machine acceptance remains a manual release gate:

- a runnable GPUI 0.2.2 desktop application;
- a dark World Library with a metadata-only create-world wizard;
- embedded, strictly parsed Survival, Hardcore, and Creative YAML templates;
- schema-versioned, atomically written local metadata;
- explicit world lifecycle states and startup recovery for stale active states;
- background Java discovery and version parsing;
- an authoritative Vanilla release/JAR provider with bounded HTTPS redirects, hash/size validation, and a versioned cache;
- a persisted explicit-EULA-acceptance gate, deterministic `server.properties`, and `online-mode=true` launch validation;
- a shell-free native Java process adapter with graceful `stop`, bounded escalation, log events, per-world start reservations, and Windows Job Object containment.
- a Windows LAN endpoint adapter that uses each world's configured `server-port`, shows multiple private IPv4 candidates conservatively, and provides per-address clipboard copy actions.
- schema-versioned session records and append-only JSONL raw logs under each
  world, with startup reconciliation for unfinished sessions;
- best-effort Vanilla server-ready/join/leave/death parsing with persisted
  player activity and playtime projections;
- stopped-world backups with manifest digests, atomic publication, retention,
  symlink/reparse-point checks, and automatic backup after a successful
  graceful shutdown;
- telemetry-free sanitized diagnostics and owned temporary-download cleanup;
- a locked portable Windows ZIP workflow in
  [packaging/package-windows.ps1](packaging/package-windows.ps1), with a
  SHA-256 sidecar.

World cards enable **Start** only for stopped, unreserved worlds and expose
graceful **Stop** controls for running worlds. The detail view reloads the
latest persisted session logs, player activity, and backup metadata after an
app restart. A successful graceful stop enters **Backing Up** when the
world's policy enables shutdown backups; timeout, force-stop, and unexpected
exit paths never claim a safe backup. Opening the app or creating a world still
does not download a JAR or launch a server.

The target MVP remains the end-to-end scenario in [docs/MVP.md](docs/MVP.md); checklist status is tracked in [PLAN.md](PLAN.md).

## Run from source

Prerequisites on Windows 11:

- stable Rust with `rustfmt` and `clippy`;
- Visual Studio 2022 Build Tools with the Desktop development with C++ workload and a Windows SDK.

From the repository root:

```powershell
cargo run -p minedock-app --locked
```

MineDock stores metadata under `%LOCALAPPDATA%\MineDock` by default. For an isolated development directory:

```powershell
$env:MINEDOCK_DATA_DIR = "$PWD\.local-minedock-data"
cargo run -p minedock-app --locked
```

The override contains user/runtime state and must not be committed.

## Validate

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p minedock-app --locked
```

Tests cover template validation, metadata round trips and corruption, world state transitions, Java output parsing, provider/EULA/provision invariants, lifecycle supervision, and controlled native-process behavior. Real Mojang downloads and a real Minecraft server process are intentionally outside the offline test suite.

The native acceptance checklist is [docs/windows-acceptance.md](docs/windows-acceptance.md).
It must be completed on a clean Windows 11 machine before calling the MVP
release-ready.

## Repository layout

```text
MineDock/
├─ crates/
│  ├─ minedock-core/   # domain, persistence, templates, providers, lifecycle rules
│  └─ minedock-app/    # GPUI plus native/Windows adapters
├─ templates/          # built-in declarative world templates
├─ docs/               # MVP, UI, security, research notes, and ADRs
├─ ARCHITECTURE.md
├─ PLAN.md
└─ SPEC.md
```

`minedock-core` contains no GPUI or Windows types. Native Java probing, HTTPS transport, process creation, Job Objects, app-data locking, and GPUI presentation stay in `minedock-app`. See [ARCHITECTURE.md](ARCHITECTURE.md) for the implemented boundaries.

## Safety constraints

- MineDock never silently accepts the Minecraft EULA. Provisioning requires a separately persisted record of explicit user acceptance before it can acquire a server artifact or write `eula.txt`.
- Generated and launch-validated server configuration keeps `online-mode=true`.
- Templates cannot provide executable paths, scripts, or download URLs.
- Only allowlisted authoritative Mojang/Minecraft HTTPS authorities are accepted by the Vanilla provider.
- Minecraft server JARs, worlds, Java runtimes, logs, backups, secrets, and local app data must not be committed.
- Raw session logs are bounded and redact common credential-shaped values.
- Backups are created only from an authoritative stopped world, publish through
  a temporary directory and manifest verification, and exclude JARs, caches,
  runtime leases, logs, temporary files, and previous backups.
- The MVP does not require Docker, does not configure UPnP, and does not include mods, plugins, cloud features, or account login.

Hardcore is a built-in template, not a special architectural mode.
