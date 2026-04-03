---
id: gc-bflf
status: closed
deps: []
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 0
assignee: shane keulen
parent: gc-8j9n
---
# Clean config.rs - config loading and resolution

Config struct, CustomModeline, ResolvedConfig, config_path(), load(), resolve(). Tests for defaults, overrides, clamping, modeline lookup.

## Acceptance Criteria

cargo test config passes. All config logic extracted from main.rs.

