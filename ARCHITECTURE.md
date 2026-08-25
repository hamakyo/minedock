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

## 3. Implemented modules

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
│  └─ provider.rs
├─ process.rs
└─ runtime.rs

minedock-app/src/
├─ main.rs
├─ http_transport.rs
├─ java_adapter.rs
├─ lifecycle_adapter.rs
├─ native_process.rs
└─ native_safety.rs
```

`minedock-core` owns domain validation, templates, persistence contracts, Java requirements, Vanilla metadata/provisioning rules, launch specifications, and lifecycle supervision. It imports neither GPUI nor Windows APIs.

`minedock-app` owns the GPUI projection and all native effects: Windows app-data resolution and leasing, Java probing, HTTPS I/O, native process creation, reparse-point checks, and Job Object containment. Backup modules have not been introduced yet.

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

Implemented foundations:
- `VanillaProvider<T, S>` in core, parameterized by transport and path-safety seams;
- `JavaRuntimeProvider` in core, with system discovery as the current provider and a
  future bundled-runtime provider as a compatible seam;
- `UreqTransport` in the app for bounded, redirect-explicit HTTPS;
- `NativeProcessFactory` / `NativeServerProcess` in the app;
- `LifecycleSupervisor` in core, parameterized by process, persistence, and lease adapters.

The MVP does not download or unpack a bundled runtime. A future provider can own
its verified runtime cache and implement `JavaRuntimeProvider`, while the app keeps
the same Java readiness and launch flow.

Later:
- `PaperProvider`
- `FabricProvider`
- `DockerProcess`

## 7. Process event model

Raw process output enters the native adapter and emits bounded events:

```text
stdout/stderr
    │
    ├──────────────▶ RawLogLine
    └──────────────▶ reliable process lifecycle events
```

The app displays a bounded recent raw-log projection from the active session. Join/leave/death and server-ready parsing belongs to Phase 7 and is not implemented. Raw log text is never the authority for process exit or successful process control.

Lifecycle authority:
- OS process handle / exit status
- successfully written control commands
- timeouts

Do not make lifecycle correctness depend on fragile localized log text.

## 8. Current UI composition

GPUI stores view state but does not own business truth.

```text
MineDockView (GPUI entity)
├─ WorldLibrary<JsonWorldRepository>
├─ LifecycleSupervisor<app/native adapters>
├─ built-in TemplateCatalog
├─ create-world wizard state
├─ EULA confirmation / pending-start state
├─ asynchronous Start operation state
├─ Java settings and recovery state
├─ selected world detail panel
├─ bounded recent raw-log cache
├─ startup error / Java readiness projection
└─ AppDataLease for this process lifetime
```

At startup the app acquires an exclusive app-data lease, reconciles persisted active lifecycle states to `Failed`, loads the world library, and probes Java off the GPUI event loop. A lease or metadata failure disables mutating actions.

The UI issues create-world commands and connects Start to explicit EULA acknowledgement, authoritative Vanilla resolution, provisioning, Java readiness, and the existing lifecycle/process adapters. Java recovery accepts an explicit executable path, persists it, and reruns discovery off the GPUI event loop. World details and a bounded recent raw-log view are presentation projections; persistent log files, player parsing, and backup commands remain later integration work.

## 9. Persistence

The current repository uses schema-versioned JSON and atomic replacement.

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

The app-data root is `%LOCALAPPDATA%\MineDock` on Windows, or the explicit `MINEDOCK_DATA_DIR` override. `minedock.json` stores metadata only; world content and server artifacts remain separate. Persisted world data paths are canonical relative paths rooted beneath app data.

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
- app-data lifecycle lease
- reparse-point/path checks
- suspended process creation and Job Object assignment
- firewall diagnostics (future)

## 12. Cloud boundary

Cloudflare must not become necessary to start a local Minecraft world.

Post-MVP Cloudflare candidates:
- remote web control plane
- Access-protected management page
- R2 encrypted/offsite backup
- status relay

Minecraft game traffic remains separate from the control plane.
