---
id: gc-tcm7
status: closed
deps: [gc-bflf, gc-njvx, gc-6x2b]
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 1
assignee: shane keulen
parent: gc-8j9n
tags: [cli, cleanup]
---
# Slim main.rs to CLI parsing and dispatch only

main.rs should only contain: CLI struct, Commands enum, main() dispatch. All logic delegated to modules. Remove dead code (MisterGuard, create_udp_socket, find_ffmpeg from main). Target <200 lines.

## Acceptance Criteria

main.rs < 200 lines. cargo build --release works. All existing CLI commands work.

