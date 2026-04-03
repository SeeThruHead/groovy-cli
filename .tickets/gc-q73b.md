---
id: gc-q73b
status: closed
deps: [gc-tcm7, gc-hopt]
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 0
assignee: shane keulen
parent: gc-8j9n
tags: [integration, testing]
---
# Integration: cargo test + cargo build --release + verify CLI

Run cargo test (all pass), cargo build --release (no warnings), verify groovy-cli --help, search, episodes, continue all work against Plex.

## Acceptance Criteria

Zero warnings. All tests pass. CLI commands return expected output.

