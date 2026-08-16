# MineDock Architecture

## 1. System context

```text
                        optional later
                    ┌────────────────────┐
                    │ Cloudflare / R2    │
                    └─────────▲──────────┘
                              │ adapter
┌─────────────────────────────┴─────────────────────────────┐
│ MineDock desktop application                              │
│                                                           │
│  ┌───────────────────┐        ┌─────────────────────────┐ │
│  │ GPUI presentation │ ─────▶ │ Application services    │ │
│  │ minedock-app      │ events │ minedock-core           │ │
│  └───────────────────┘        └────────────┬────────────┘ │
│                                            │              │
│                     ┌──────────────────────┼────────────┐ │
│                     ▼                      ▼            ▼ │
│                 metadata              process       backup│
│                 storage               adapter       adapter│
└─────────────────────┬──────────────────────┬───────────────┘
                      │                      │
                      ▼                      ▼
                 local files          Java / server.jar
```

## 2. Architectural boundary

`minedock-core` is the product.

`minedock-app` is one presentation shell.

Future clients should be possible without rewriting world lifecycle logic:
- CLI
- remote API
- test harness

## 3. Suggested modules

```text
minedock-core/src/
├─ lib.rs
├─ domain.rs
├─ error.rs
├─ template.rs
├─ lifecycle.rs
├─ persistence.rs
├─ minecraft/
│  ├─ mod.rs
│  ├─ properties.rs
│  ├─ version.rs
│  └─ vanilla.rs
├─ process/
│  ├─ mod.rs
│  └─ events.rs
└─ backup/
   └─ mod.rs
```

The starter currently keeps some of these concepts compact; Codex may split modules as implementation grows.

## 4. World vs Server separation

Never define World as "the running server".

A World survives:
- app restarts;
- server upgrades;
- runtime changes;
- stopped periods;
- backups.

A ServerSession is ephemeral.

This enables:
- running the same world with a newer compatible Vanilla server;
- migrations later;
- world import/export;
- session history.

## 5. Template boundary

Templates can provide declarative values only.

Allowed:
- game mode
- difficulty
- hardcore
- max players
- server properties from an allowlist
- backup defaults

Not allowed:
- shell commands
- arbitrary executable paths
- arbitrary download URLs
- post-install scripts

This keeps community templates possible later without immediately becoming RCE packages.

## 6. Runtime provider boundary

Use a provider abstraction:

```text
World
  │
ServerProfile
  │
DistributionProvider
  ├─ resolve version
  ├─ acquire server artifact
  ├─ verify artifact where possible
  └─ build launch specification
        │
        ▼
ProcessAdapter
```

MVP:
- `VanillaProvider`
- `NativeJavaProcess`

Later:
- `PaperProvider`
- `FabricProvider`
- `DockerProcess`

## 7. Process event model

Raw process output enters an adapter and emits events:

```text
stdout/stderr
    │
    ├──────────────▶ RawLogLine
    │
    └─ parser ─────▶ PlayerJoined
                     PlayerLeft
                     PlayerDied
                     ServerReady
```

Lifecycle authority:
- OS process handle / exit status
- successfully written control commands
- timeouts

Do not make lifecycle correctness depend on fragile localized log text.

## 8. UI state model

GPUI stores view state but does not own business truth.

Suggested:

```text
AppState (GPUI entity)
├─ selected_world
├─ wizard state
├─ transient dialogs
└─ projection of CoreSnapshot
```

Core emits snapshots/events.

UI requests commands:
- `CreateWorld`
- `StartWorld`
- `StopWorld`
- `CreateBackup`

## 9. Persistence

Use a schema-versioned JSON format initially.

Why JSON:
- inspectable
- easy migration
- low operational overhead

Do not introduce SQLite until queries/scale justify it.

Example:

```json
{
  "schema_version": 1,
  "worlds": []
}
```

Use IDs rather than names as stable references.

## 10. Error model

Errors should carry:
- category
- human-readable context
- source error
- recoverability/action hint where appropriate

Example categories:
- Configuration
- Persistence
- JavaUnavailable
- Download
- Verification
- ProcessStart
- ProcessControl
- Backup
- InvalidState

## 11. Windows-first decisions

MVP validates Windows 11 first.

Avoid Windows-only types in core.

OS-specific concerns belong in adapters:
- Java discovery
- application data path
- firewall diagnostics
- process creation flags

## 12. Cloud boundary

Cloudflare must not become necessary to start a local Minecraft world.

Post-MVP Cloudflare candidates:
- remote web control plane
- Access-protected management page
- R2 encrypted/offsite backup
- status relay

Minecraft game traffic remains separate from the control plane.
