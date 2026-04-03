# groovy-cli

A Rust CLI for streaming Plex media to a CRT via MiSTer FPGA's [Groovy core](https://github.com/psakhis/Groovy_MiSTer). Decodes video with FFmpeg, burns in subtitles, splits interlaced fields, and sends raw frames over UDP using the Groovy protocol.

## Features

- Stream any Plex media to MiSTer → analog out → CRT/PVM
- Burned-in subtitles (ASS, SRT, and all FFmpeg-supported formats)
- Audio streaming (PCM 48kHz stereo)
- Resume from last position / Plex watch state sync
- Plex OAuth authentication
- Configurable video scaling for CRT overscan
- All standard NTSC/PAL modelines (240p, 480i, 576i)
- Test pattern generator for CRT calibration

## Requirements

- [MiSTer FPGA](https://github.com/MiSTer-devel/Main_MiSTer/wiki) with [Groovy core](https://github.com/psakhis/Groovy_MiSTer) loaded
- FFmpeg with libass (`brew install homebrew-ffmpeg/ffmpeg/ffmpeg --with-libass`)
- A Plex server on your network

## Install

```bash
brew tap seethruhead/tap
brew install groovy-cli
```

Or build from source:

```bash
cargo build --release
```

## Setup

```bash
# Authenticate with Plex (opens browser)
groovy-cli auth

# Edit config
$EDITOR ~/.config/groovy-cli/config.toml
```

Config file (`~/.config/groovy-cli/config.toml`):

```toml
mister = "192.168.0.115"
server = "192.168.0.29"
port = 32400
token = "auto-saved-by-auth"
modeline = "640x480i NTSC"
scale = 0.90
```

All settings can also come from CLI flags or env vars (`PLEX_TOKEN`, `GROOVY_MISTER`, `GROOVY_PLEX_SERVER`).

## Usage

```bash
# What was I watching?
groovy-cli continue

# Search all libraries
groovy-cli search "Gundam"

# List episodes
groovy-cli episodes "Gundam Wing"

# Play next unwatched episode
groovy-cli play "Gundam Wing"

# Play with Japanese audio and English subs
groovy-cli play "Gundam Wing" -a japanese --subs english

# Play specific episode, seek to 5 minutes
groovy-cli play "Gundam Wing" -s 1 -e 4 --seek 300

# Play by Plex rating key
groovy-cli play-key 70844

# Send test pattern for CRT calibration
groovy-cli test-pattern

# Stop playback
groovy-cli stop

# List available modelines
groovy-cli modelines

# List Plex libraries
groovy-cli libraries
```

## Scaling

CRTs overscan — the edges of the image may be cut off. Use the `scale` config option (0.3–1.0) to shrink the video with black padding:

```toml
scale = 0.90  # 90% size, centered with black border
```

Use `groovy-cli test-pattern` to calibrate.

## Architecture

```
Plex Server → direct play URL
  ↓
FFmpeg (two subprocesses):
  - Video: decode → scale → burn subtitles → pad → raw BGR24 fields at 60fps
  - Audio: decode → PCM s16le 48kHz stereo
  ↓
groovy-cli: latest-frame model, field-rate send loop, single UDP socket
  ↓
MiSTer FPGA (Groovy core) → analog out → CRT/PVM
```

## License

MIT
