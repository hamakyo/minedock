# Codex entry prompt

Implement MineDock from this repository.

Read in this order:

1. `AGENTS.md`
2. `SPEC.md`
3. `ARCHITECTURE.md`
4. `docs/MVP.md`
5. `PLAN.md`
6. `docs/UI.md`

Then execute **Phase 0 -> Phase 1 -> Phase 2** from `PLAN.md` before expanding scope.

## Immediate objective

Produce a runnable Windows-first GPUI application with a real Rust core where:

- the application opens to a World Library screen;
- built-in YAML templates load from `templates/`;
- a user can create a metadata-only world from Survival / Hardcore / Creative;
- world metadata persists locally;
- the UI reflects world lifecycle state;
- core tests cover template parsing and world state transitions.

After that is stable, implement the Vanilla Java server process adapter.

## Constraints

- GPUI only for desktop UI. Do not replace it with Tauri, egui, Iced, Slint, Electron, or a webview.
- Do not make GPUI types leak into `minedock-core`.
- Do not require Docker.
- Do not silently accept Minecraft EULA.
- Keep `online-mode=true` by default.
- No mods/plugins/cloud features in MVP.
- Prefer incremental compilable commits.
- If current GPUI APIs differ from this starter, adapt `minedock-app` to current official APIs while preserving the architecture.

## Desired first visible UI

A dark desktop library:

```text
MineDock                                      Settings

Worlds                                         + New World

┌─────────────────────────────────────────────────────┐
│ Friends Survival                              STOP │
│ Vanilla · current release                           │
│ Running · 2 / 4 players                             │
└─────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────┐
│ Hardcore #01                                  START │
│ Vanilla · current release                           │
│ Stopped                                             │
└─────────────────────────────────────────────────────┘
```

A polished component system is not required in the first pass. Correct state flow is more important.
