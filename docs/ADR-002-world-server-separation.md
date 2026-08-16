# ADR-002: World and ServerSession are separate domain objects

Status: Accepted

## Context

A simple launcher often models each server folder as "a server". MineDock's product concept is a persistent World Library.

## Decision

`World` is durable.

`ServerProfile` describes hosting configuration.

`ServerSession` represents one execution.

## Consequences

MineDock can later support:
- version migration;
- alternate distributions;
- session history;
- world import/export;
- backups independent of process lifecycle.

The UI must still hide unnecessary architecture from normal users.
