# ADR-003: Windows MVP release artifact

## Status

Accepted for the MVP portable package. A signed installer and an update
service remain post-MVP release work.

## Decision

MineDock ships a versioned, portable Windows x64 ZIP produced by
packaging/package-windows.ps1. The package contains MineDock.exe, the English
and Japanese setup notes, and a VERSION file. The script builds with
cargo build --locked --release, writes a SHA-256 sidecar, and never moves or
deletes the user's %LOCALAPPDATA%\MineDock data.

The application continues to use %LOCALAPPDATA%\MineDock by default. Updating
the executable therefore preserves metadata, world saves, session history,
and local backups. Java remains a user-supplied prerequisite and the existing
Java recovery screen is intentionally retained.

## Consequences

- A clean-machine acceptance run must install/extract the ZIP, launch the
  executable, create a world, complete the EULA step, start, join over LAN,
  stop, verify the backup, relaunch, and confirm the saved world remains.
- Release automation must publish the ZIP and its SHA-256 sidecar together.
- The ZIP is not a security boundary or an installer; Windows SmartScreen and
  code signing policy must be handled before a public distribution.
