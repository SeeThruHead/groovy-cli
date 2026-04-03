---
id: gc-6x2b
status: open
deps: [gc-jiqo]
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 2
assignee: shane keulen
parent: gc-8j9n
tags: [test-pattern, pure]
---
# Clean test_pattern.rs - pattern generation and send

Pure generate_pattern(w, h, scale) -> Vec<u8> function. Send loop uses GroovyConnection. Tests for pattern dimensions.

## Acceptance Criteria

cargo test test_pattern passes. Pattern generation is pure.

