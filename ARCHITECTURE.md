# groovy-cli Architecture

## Modes

### 1. Local Mode (current)
CLI runs FFmpeg locally, sends UDP directly to MiSTer.
```
groovy-cli play "Gundam Wing"
```

### 2. Daemon Mode (planned)
Server runs on homelab (Docker), CLI sends commands over HTTP.
```
groovy-cli play "Gundam Wing"  # auto-detects daemon from config
groovy-cli --daemon http://groovycast.sthlock.vip play "Gundam Wing"
```

### 3. Fallback
Config specifies daemon URL. If daemon is unreachable, fall back to local execution.
```toml
# ~/.config/groovy-cli/config.toml
daemon = "http://groovycast.sthlock.vip"  # or "http://192.168.0.9:7542"
fallback = "local"  # "local" | "remote-ssh" | "error"
```

## Daemon Architecture

```
┌─────────────────────────────────────────────────────────┐
│ NUC13 Pro (Proxmox)                                     │
│  └── Ubuntu VM (GPU passthrough - Intel Quick Sync)     │
│       ├── Plex Docker (media at /media, QSV transcode)  │
│       └── groovyd Docker                                │
│            ├── /media (bind mount - direct file access)  │
│            ├── FFmpeg decode (VAAPI/QSV hw accel)        │
│            ├── Plex API → localhost:32400                │
│            ├── Groovy UDP → 192.168.0.115 (MiSTer)      │
│            └── HTTP API :7542 ← CLI commands             │
│                 (reverse proxied to groovycast.sthlock.vip)
└─────────────────────────────────────────────────────────┘
```

## HTTP API

Simple REST. No auth initially (homelab only), add token auth later if exposed.

```
POST /play          { "query": "Gundam Wing", "audio": "jpn", "subs": "eng" }
POST /play-key      { "key": 70844, "audio": "jpn", "seek": 300 }
POST /play-file     { "path": "/media/Anime/movie.mkv", "subs": "0" }
POST /stop          {}
POST /seek          { "offset_secs": 300 }
POST /pause         {}
POST /resume        {}
GET  /status        → { "state": "playing", "title": "...", "position_ms": 12345, ... }
GET  /continue      → [{ "title": "...", "key": 70844, "offset": 12345 }, ...]
GET  /search?q=gun  → [{ "title": "Gundam Wing", "key": 70840 }, ...]
GET  /episodes/70840 → [{ "s": 1, "e": 4, "title": "...", "watched": false }, ...]
GET  /health        → { "ok": true, "mister": "connected", "plex": "connected" }
```

## CLI Resolution Flow

```
1. Parse CLI args
2. Load config (token, daemon URL, mister IP, etc.)
3. If daemon URL configured:
   a. Try HTTP POST to daemon
   b. If reachable → done (daemon handles everything)
   c. If unreachable and fallback=local → execute locally
   d. If unreachable and fallback=error → exit with error
4. If no daemon URL → execute locally
5. If local and on WiFi → SSH to remote host (existing behavior)
```

## Docker Image

```dockerfile
FROM rust:slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev
COPY . /src
WORKDIR /src
RUN cargo build --release

FROM ubuntu:24.04
RUN apt-get update && apt-get install -y ffmpeg libass9 vainfo
COPY --from=builder /src/target/release/groovy-cli /usr/local/bin/
COPY --from=builder /src/target/release/groovy-cli /usr/local/bin/groovyd

EXPOSE 7542
ENV GROOVY_MISTER=192.168.0.115
ENV PLEX_TOKEN=your-token
ENV GROOVY_PLEX_SERVER=localhost

# Mount points
VOLUME /media
VOLUME /config

CMD ["groovyd", "--config", "/config/config.toml"]
```

Docker run:
```bash
docker run -d \
  --name groovyd \
  --device /dev/dri:/dev/dri \
  --network host \
  -v /mnt/media:/media:ro \
  -v /etc/groovy-cli:/config \
  -e PLEX_TOKEN=xxx \
  ghcr.io/seethruhead/groovy-cli:latest
```

`--network host` required for:
- UDP to MiSTer (can't NAT UDP multicast well)
- Access to Plex on localhost

## Module Structure

```
src/
  main.rs          CLI parsing, mode dispatch (local vs daemon client)
  config.rs        Config file, resolution, validation (tested)
  groovy.rs        Protocol: Modeline, FpgaStatus, packet builders (pure, tested)
  connection.rs    GroovyConnection: UDP socket, FPGA sync, LZ4 blit, congestion
  ffmpeg.rs        FFmpeg arg building (pure, tested), process lifecycle
  streamer.rs      Core loop: FFmpeg pipeline → GroovyConnection, audio thread
  plex.rs          Plex API: search, episodes, on-deck, media resolve, progress
  auth.rs          Plex OAuth PIN flow
  test_pattern.rs  Pattern generation (pure, tested) + send loop
  api.rs           HTTP API types (serde, shared by client + server)
  server.rs        (future) axum/actix HTTP server — daemon mode
  client.rs        (future) reqwest HTTP client — sends commands to daemon
```

## Key Design Principles

1. **Commands are data** — Play, Stop, Seek are serializable structs, same type used by CLI dispatch and HTTP API
2. **Streaming engine is transport-agnostic** — doesn't know if it was started by CLI or HTTP request
3. **Pure functions where possible** — packet builders, FFmpeg arg construction, test pattern generation all testable without I/O
4. **GroovyConnection owns the MiSTer lifecycle** — Drop sends close, congestion control and FPGA sync are internal
5. **Config cascades** — CLI flags > env vars > config file > defaults
