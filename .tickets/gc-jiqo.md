---
id: gc-jiqo
status: open
deps: [gc-c2l9]
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 0
assignee: shane keulen
parent: gc-8j9n
tags: [protocol, networking]
---
# Clean connection.rs - GroovyConnection with FPGA sync

GroovyConnection struct: connect, init, blit (LZ4 + congestion control), audio, wait_sync (raster feedback), close. Drop sends close. Non-blocking socket for ACK polling.

## Acceptance Criteria

Compiles. Drop guard works. Uses groovy.rs packet builders.

