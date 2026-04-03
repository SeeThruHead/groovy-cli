---
id: gc-cbne
status: open
deps: []
links: []
created: 2026-04-03T21:40:07Z
type: task
priority: 0
assignee: shane keulen
parent: gc-8j9n
tags: [ffmpeg, pure]
---
# Clean ffmpeg.rs - FFmpeg arg building and process management

Pure functions: build_video_args, build_audio_args, find_ffmpeg. Process management: extract_subs, start pipeline with latest-frame model. Tests for arg building.

## Acceptance Criteria

cargo test ffmpeg passes. Arg building functions are pure and tested.

