# Security Notes

## Defaults

- `online-mode=true`
- whitelist enabled in built-in friend-world templates
- no auto-UPnP in MVP
- no arbitrary template scripts
- no arbitrary template download URLs

## Trust boundaries

Untrusted:
- imported archives
- future mods/plugins
- user-provided paths
- network responses
- downloaded runtime/server artifacts until validated

Trusted only after validation:
- MineDock-owned metadata
- artifacts from configured authoritative providers

## Archive rules

When backup restore/import is implemented:
- reject absolute archive paths;
- reject `..` traversal;
- reject links escaping target root;
- extract into temporary directory;
- validate before atomic move.

## Process rules

- never concatenate user values into shell command strings;
- use `std::process::Command` argument arrays;
- validate Java executable path;
- server templates cannot specify executable paths.

## Secrets

MVP should need no cloud secret.

Future tokens:
- OS credential store where available;
- never plaintext in world YAML;
- never logs.

## Network

A Minecraft server listening on all interfaces is externally reachable if host/router/firewall permit it.

UI must clearly distinguish:
- LAN
- explicitly configured direct exposure
- future private-network modes
