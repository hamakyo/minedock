# Research Notes (2026-08-16)

These notes capture assumptions used for the starter. Re-check them during implementation because GPUI and Minecraft distribution details can change.

## GPUI

The current official GPUI README in `zed-industries/zed` describes GPUI as pre-1.0, recommends the latest stable Rust, and shows standalone apps starting through `gpui_platform::application().run(...)`.

The same README states:
- Windows requires no extra `gpui_platform` features;
- Windows uses Win32 for windowing and DirectWrite for text;
- GPUI has an async executor integrated with the platform event loop.

Because GPUI is actively developed, Phase 0 explicitly requires pinning a working version combination and adapting only the presentation crate if APIs move.

## Minecraft Java server

Minecraft's official server download page states that the self-hosted Java server is for Java Edition and requires Java to be usable from the command line.

The page also states that downloading the server software means accepting the Minecraft EULA. MineDock therefore requires an explicit EULA acknowledgement flow rather than silently setting acceptance.

## Design consequence

Do not bake the currently downloadable server filename/version into the domain model. Use a provider that resolves authoritative version metadata at runtime.
