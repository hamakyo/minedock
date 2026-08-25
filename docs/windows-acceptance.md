# Windows MVP acceptance record

This checklist is the manual evidence record for Issue #13. It must be run on
a clean Windows 11 machine with a compatible Java runtime and a second device
on the same LAN. The repository's unit tests do not replace this native run.

Release scope: `.github/workflows/windows-release.yml` currently builds the
portable ZIP and uploads it as a GitHub Actions artifact. It does not create a
GitHub Release. The ZIP plus SHA-256 sidecar is the current package contract;
an installer and code signing are separate post-MVP work and are not implied
by the Actions artifact. Issue #19 remains open until this checklist is
completed and its evidence is recorded.

1. Verify the ZIP SHA-256 sidecar and extract it to a new directory.
2. Launch MineDock.exe.
3. Confirm the app reports Java readiness or gives the Java settings path.
4. Create one Survival, Creative, and Hardcore world.
5. Confirm no server JAR or world save is downloaded merely by creating a world.
6. Start one world and explicitly accept the Minecraft EULA.
7. Confirm the server becomes Running and the configured LAN IP:port is shown.
8. Copy the address and join from a second LAN device.
9. Confirm a player join appears in the persisted session activity.
10. Stop gracefully and wait for the automatic backup to complete.
11. Confirm the world reaches Stopped and a backup manifest contains the save
    but not the JAR, cache, raw logs, or temporary files.
12. Relaunch MineDock and confirm the world, latest session log, player
    activity, and backup remain visible.
13. Exercise a Java-missing start, invalid Java path, no-LAN-address, and
    graceful-stop-timeout scenario; verify each has an actionable recovery
    message and no data-loss claim.
14. Record Windows version, MineDock version, Java major version, package
    checksum, date, and screenshots in the release evidence outside the repo.

The repository does not claim this checklist is complete until a native run is
recorded.
