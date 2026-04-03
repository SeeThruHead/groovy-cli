---
id: gc-njvx
status: closed
deps: [gc-jiqo, gc-cbne]
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 1
assignee: shane keulen
parent: gc-8j9n
tags: [streaming, core]
---
# Clean streamer.rs - core streaming loop

Single stream() function that wires FFmpeg pipeline to GroovyConnection. Audio thread with 3-phase sync. FPGA raster sync loop. Used by both Plex play and file play.

## Acceptance Criteria

No duplicated streaming code. Both play and file commands use the same streamer.

