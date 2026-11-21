# Networking notes

MineDock MVP supports direct LAN play. It displays usable private IPv4
candidates and each world's configured server port. If several interfaces are
available, MineDock leaves the choice to the user instead of guessing between
physical, VPN, WSL, Hyper-V, Docker, or Tailscale adapters.

Direct internet hosting is outside the automatic setup path. A user who
chooses to expose a server must configure the Windows firewall, router/NAT, and
any provider security controls independently, understand the risk of exposing
Minecraft to the public internet, and verify the selected port. MineDock does
not configure UPnP, router forwarding, or firewall rules and does not claim
that a copied LAN address is internet-reachable.
