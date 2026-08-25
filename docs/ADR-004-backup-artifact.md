# ADR-004: Verified local backup artifact

## Status

Accepted for the MVP backup contract. Restore/import remains out of scope.

## Decision

MineDock stores each stopped-world backup as two published files beneath
backups/<world-id>:

- <backup-id>.zip, containing the selected world save and minimal server
  configuration;
- <backup-id>.manifest.json, containing the schema-versioned BackupManifest.

The manifest records the world and backup identity, timestamp, reason, Minecraft
version, uncompressed byte total, completed archive byte size, per-file
SHA-256 values, and the completed archive SHA-256. A world-scoped index records
the validated BackupRecord projection.

The app writes the ZIP to an owned .part path, closes and hashes it, writes the
manifest sidecar, then atomically publishes the archive before updating the
index. Retention deletes only validated artifact/manifest pairs after a new
artifact has been published.

Startup reconciliation is structural-only: it reads manifests and the ZIP
central directory and checks entry names and sizes, but does not hash archive
payloads. Normal backup listing and explicit backup reads run complete digest
verification on a background worker.

## Consequences

- Large worlds do not block GPUI startup on synchronous digest work.
- A backup record is not exposed until both the archive and its manifest are
  present and structurally consistent.
- Older development directory snapshots are preserved but are not treated as
  current verified backups; migration/restore is a separate decision.
- Age-based retention and restore/import are not part of this MVP contract.
