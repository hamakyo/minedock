# UI Direction

## Product metaphor

MineDock should feel closer to a game/library launcher than a hosting control panel.

Avoid default screens dominated by:
- shell output
- JVM flags
- raw property grids
- ports
- file paths

Those belong in advanced settings/debug views.

## World Library

```text
┌──────────────────────────────────────────────────────────────┐
│ MineDock                                      Settings  ⚙    │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│ Worlds                                           + New World │
│                                                              │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Friends Survival                                  STOP  │ │
│ │ Vanilla · current release                               │ │
│ │ ● Running · 2 / 4 players                              │ │
│ │ 192.168.1.20:25565                         Copy Address │ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Hardcore #01                                    START  │ │
│ │ Vanilla · current release                               │ │
│ │ ○ Stopped · Last played Sunday                          │ │
│ └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

## New World

Default wizard:

```text
Create a World

Name
[ Sunday Hardcore                         ]

Template
[ Survival ] [ Hardcore ] [ Creative ]

Minecraft
[ Current release ▼ ]

Players
[ 4 ]

Seed
[ Random                                   ]

Network
[ LAN ▼ ]

                         [ Cancel ] [ Create ]
```

Advanced settings collapsed below.

## World Detail

```text
Sunday Hardcore                                  ● Running

[ Stop ]

Connection
192.168.1.20:25565    [ Copy ]

Players
hamakyo
friend01

Session
Started 21:04
Playtime 1h 22m

Recent activity
21:33 friend01 joined
21:19 hamakyo earned ...
21:04 Server ready

[ Logs ] [ Settings ] [ Backup ]
```

## Lifecycle presentation

- Stopped: neutral
- Preparing: progress/activity
- Starting: progress/activity
- Running: positive live status
- Stopping: progress/activity
- BackingUp: progress/activity
- Failed: clear error + recovery action

Do not use color as the only state indicator.

## Error UX

Bad:
> Process exited 1.

Good:
> Minecraft could not start because MineDock could not find a compatible Java runtime.

Actions:
- Re-check
- Choose Java
- Open setup help
- View technical details

## Design constraints

- Native desktop density.
- Keyboard accessible.
- Scales acceptably at Windows 125%/150%.
- Avoid excessive animation.
- Raw server log is monospace and visually secondary.
- Main screen should remain useful at ~900×650 logical pixels.
