---
id: gc-hopt
status: closed
deps: []
links: []
created: 2026-04-03T21:40:32Z
type: task
priority: 0
assignee: shane keulen
parent: gc-8j9n
tags: [testing, mock]
---
# Mock Groovy server for integration testing

UDP server that speaks Groovy protocol: accepts init, switchres, blit, audio, close. Sends back FPGA status ACKs. Validates packet format. Counts frames. Verifies LZ4 decompression. Can be used as --mister 127.0.0.1 for tests.

## Acceptance Criteria

Mock server validates all protocol packets. Integration test streams 5 seconds of test pattern to mock, verifies frame count and protocol correctness.

