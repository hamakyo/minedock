# ADR-001: MineDock is local-first

Status: Accepted

## Context

MineDock was initially considered alongside Cloudflare hosting/control options. Minecraft game server traffic and world state are fundamentally local server/runtime concerns for the intended 2–8 player use case.

## Decision

The MVP requires no cloud service.

Minecraft server processes, worlds, metadata, and backups are local.

Cloudflare may later provide optional:
- control plane
- remote status
- R2 offsite backup

## Consequences

Positive:
- no monthly hosting dependency;
- works offline/LAN;
- simpler latency/runtime model;
- private worlds stay on host by default.

Negative:
- host PC must be on;
- direct internet hosting remains a networking problem;
- remote start/stop is deferred.
