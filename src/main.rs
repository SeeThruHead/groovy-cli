use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Read;
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod auth;
mod groovy;
mod plex;

use groovy::{Modeline, MODELINES};

// ── Config ──

#[derive(serde::Deserialize, Default)]
struct Config {
    mister: Option<String>,
    server: Option<String>,
    port: Option<u16>,
    token: Option<String>,
    modeline: Option<String>,
    /// Scale video to fit CRT (0.5-1.0). 1.0 = full size, 0.9 = 90% with black border.
    scale: Option<f64>,
    /// Custom modeline overrides. If present, used instead of preset.
    custom_modeline: Option<CustomModeline>,
}

#[derive(serde::Deserialize, Clone)]
struct CustomModeline {
    p_clock: f64,
    h_active: u16,
    h_begin: u16,
    h_end: u16,
    h_total: u16,
    v_active: u16,
    v_begin: u16,
    v_end: u16,
    v_total: u16,
    interlace: bool,
}

fn config_path() -> std::path::PathBuf {
    let xdg = dirs::home_dir()
        .map(|h| h.join(".config").join("groovy-cli").join("config.toml"))
        .unwrap_or_default();
    if xdg.exists() { return xdg; }
    dirs::config_dir()
        .map(|d| d.join("groovy-cli").join("config.toml"))
        .unwrap_or(xdg)
}

fn load_config() -> Config {
    let path = config_path();
    if path.exists() {
        toml::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).unwrap_or_default()
    } else { Config::default() }
}

// ── CLI ──

#[derive(Parser)]
#[command(name = "groovy-cli", about = "Cast Plex media to MiSTer FPGA via Groovy protocol")]
struct Cli {
    #[arg(short, long, env = "GROOVY_MISTER")]
    mister: Option<String>,
    #[arg(short, long, env = "GROOVY_PLEX_SERVER")]
    server: Option<String>,
    #[arg(long, env = "GROOVY_PLEX_PORT")]
    port: Option<u16>,
    #[arg(short, long, env = "PLEX_TOKEN")]
    token: Option<String>,
    #[arg(long)]
    modeline: Option<String>,
    /// Scale video (0.5-1.0). 0.9 = 90% size with black border.
    #[arg(long)]
    scale: Option<f64>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Search { query: String },
    Episodes { name: String },
    Play {
        name: String,
        #[arg(short, long)] season: Option<u32>,
        #[arg(short, long)] episode: Option<u32>,
        /// Audio language (e.g. "japanese", "english", "jpn", "eng")
        #[arg(short, long)] audio: Option<String>,
        /// Subtitle language (e.g. "english", "eng", "none" to disable)
        #[arg(long)] subs: Option<String>,
        /// Seek to position in seconds
        #[arg(long)] seek: Option<f64>,
    },
    PlayKey {
        key: u64,
        #[arg(short, long)] audio: Option<String>,
        #[arg(long)] subs: Option<String>,
    },
    /// Play a local video file
    File {
        /// Path to video file
        path: String,
        /// Audio track index (0-based)
        #[arg(short, long)] audio: Option<u32>,
        /// Subtitle track index (0-based), or "none"
        #[arg(long)] subs: Option<String>,
        /// Seek to position in seconds
        #[arg(long)] seek: Option<f64>,
    },
    #[command(alias = "ondeck")]
    Continue,
    Libraries, Modelines, Config, Auth, Stop,
    /// Send a test pattern to MiSTer for modeline calibration
    TestPattern {
        /// How long to display in seconds
        #[arg(short, long, default_value = "60")]
        duration: u64,
    },
}

struct ResolvedConfig {
    mister: String, server: String, port: u16, token: String,
    modeline_name: String,
    scale: f64,
    custom_modeline: Option<CustomModeline>,
}

impl ResolvedConfig {
    fn from(cli: &Cli, cfg: &Config) -> Result<Self> {
        Ok(Self {
            mister: cli.mister.clone().or_else(|| cfg.mister.clone()).unwrap_or_else(|| "192.168.0.115".into()),
            server: cli.server.clone().or_else(|| cfg.server.clone()).unwrap_or_else(|| "localhost".into()),
            port: cli.port.or(cfg.port).unwrap_or(32400),
            token: cli.token.clone().or_else(|| cfg.token.clone()).context(
                "No Plex token. Set via --token, PLEX_TOKEN env, or 'token' in ~/.config/groovy-cli/config.toml")?,
            modeline_name: cli.modeline.clone().or_else(|| cfg.modeline.clone()).unwrap_or_else(|| "640x480i NTSC".into()),
            scale: cli.scale.or(cfg.scale).unwrap_or(1.0).clamp(0.3, 1.0),
            custom_modeline: cfg.custom_modeline.clone(),
        })
    }
    fn modeline(&self) -> Result<Modeline> {
        if let Some(ref cm) = self.custom_modeline {
            return Ok(Modeline {
                name: "custom",
                p_clock: cm.p_clock, h_active: cm.h_active, h_begin: cm.h_begin,
                h_end: cm.h_end, h_total: cm.h_total, v_active: cm.v_active,
                v_begin: cm.v_begin, v_end: cm.v_end, v_total: cm.v_total,
                interlace: cm.interlace,
            });
        }
        MODELINES.iter().find(|m| m.name == self.modeline_name).copied().with_context(|| {
            format!("Unknown modeline '{}'. Available:\n  {}", self.modeline_name,
                MODELINES.iter().map(|m| m.name).collect::<Vec<_>>().join("\n  "))
        })
    }
    fn plex(&self) -> plex::PlexClient { plex::PlexClient::new(&self.server, self.port, &self.token) }
}

// ── Main ──

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = load_config();

    match &cli.command {
        Commands::Auth => {
            let token = auth::plex_oauth()?;
            let path = config_path();
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            let mut t = if path.exists() {
                std::fs::read_to_string(&path)?.parse::<toml::Table>().unwrap_or_default()
            } else { toml::Table::new() };
            t.insert("token".into(), toml::Value::String(token));
            std::fs::write(&path, toml::to_string_pretty(&t)?)?;
            println!("Token saved to {}", path.display());
            return Ok(());
        }
        Commands::Modelines => { for m in MODELINES { println!("{}", m.name); } return Ok(()); }
        Commands::Config => {
            let p = config_path();
            println!("Config: {}", p.display());
            if p.exists() { println!("{}", std::fs::read_to_string(&p)?); }
            else { println!("(not found)\n\nExample:\n  mister = \"192.168.0.115\"\n  server = \"192.168.0.29\"\n  port = 32400\n  token = \"your-token\"\n  modeline = \"640x480i NTSC\""); }
            return Ok(());
        }
        _ => {}
    }

    let rcfg = ResolvedConfig::from(&cli, &cfg)?;
    let plex = rcfg.plex();

    match &cli.command {
        Commands::Libraries => {
            for lib in &plex.libraries()? { println!("{}: {} (type={})", lib.key, lib.title, lib.lib_type); }
        }
        Commands::Search { query } => {
            let shows = plex.search(query)?;
            if shows.is_empty() { println!("No results for '{}'", query); }
            for s in &shows { println!("{} (key={}, library={})", s.title, s.rating_key, s.library); }
        }
        Commands::Episodes { name } => {
            let shows = plex.search(name)?;
            let show = shows.first().with_context(|| format!("No show found for '{}'", name))?;
            println!("{}:", show.title);
            for ep in &plex.get_episodes(show.rating_key)? {
                let w = if ep.view_count > 0 { "✓" } else { " " };
                println!("  [{}] S{}E{} - {} (key={})", w, ep.season, ep.episode, ep.title, ep.rating_key);
            }
        }
        Commands::Continue => {
            let items = plex.on_deck()?;
            if items.is_empty() { println!("Nothing on deck."); }
            else {
                println!("Continue watching:");
                for (i, item) in items.iter().enumerate() {
                    let off = if item.view_offset > 0 { format!(" ({}m in)", item.view_offset / 60_000) } else { String::new() };
                    if item.item_type == "episode" {
                        println!("  {}. {} — S{}E{}: {}{} (key={})", i+1, item.show_title, item.season, item.episode, item.title, off, item.rating_key);
                    } else {
                        println!("  {}. {}{} (key={})", i+1, item.title, off, item.rating_key);
                    }
                }
            }
        }
        Commands::Play { name, season, episode, audio, subs, seek } => {
            let shows = plex.search(name)?;
            let show = shows.first().with_context(|| format!("No show found for '{}'", name))?;
            let episodes = plex.get_episodes(show.rating_key)?;
            let ep = if let (Some(s), Some(e)) = (season, episode) {
                episodes.iter().find(|ep| ep.season == *s && ep.episode == *e)
                    .with_context(|| format!("S{}E{} not found", s, e))?
            } else {
                episodes.iter().find(|ep| ep.view_count == 0).or_else(|| episodes.first()).context("No episodes")?
            };
            println!("Playing: {} S{}E{} - {}", show.title, ep.season, ep.episode, ep.title);
            stream_to_mister(&plex, ep.rating_key, &rcfg.mister, &rcfg.modeline()?, rcfg.scale, audio.as_deref(), subs.as_deref(), *seek)?;
        }
        Commands::PlayKey { key, audio, subs } => {
            println!("Playing key={}", key);
            stream_to_mister(&plex, *key, &rcfg.mister, &rcfg.modeline()?, rcfg.scale, audio.as_deref(), subs.as_deref(), None)?;
        }
        Commands::File { path, audio, subs, seek } => {
            let modeline = rcfg.modeline()?;
            stream_file(path, &rcfg.mister, &modeline, rcfg.scale, *audio, subs.as_deref(), *seek)?;
        }
        Commands::Stop => {
            let sock = UdpSocket::bind("0.0.0.0:0")?;
            sock.send_to(&groovy::build_close(), format!("{}:{}", rcfg.mister, groovy::UDP_PORT))?;
            println!("Sent stop");
        }
        Commands::TestPattern { duration } => {
            let modeline = rcfg.modeline()?;
            send_test_pattern(&rcfg.mister, &modeline, rcfg.scale, *duration)?;
        }
        Commands::Modelines | Commands::Config | Commands::Auth => unreachable!(),
    }
    Ok(())
}

// ── MiSTer close guard — always sends CMD_CLOSE when dropped ──

struct MisterGuard {
    sock: Arc<Mutex<UdpSocket>>,
}

impl Drop for MisterGuard {
    fn drop(&mut self) {
        eprintln!("Sending close to MiSTer...");
        if let Ok(s) = self.sock.lock() {
            let _ = s.send(&groovy::build_close());
        }
    }
}

// ── Streaming ──

fn stream_to_mister(
    plex: &plex::PlexClient, rating_key: u64, mister_ip: &str, modeline: &Modeline, scale: f64, audio_lang: Option<&str>, sub_lang: Option<&str>, seek_override: Option<f64>,
) -> Result<()> {
    let ffmpeg = find_ffmpeg()?;
    let media = plex.resolve_media(rating_key, audio_lang, sub_lang)?;
    let url = &media.direct_play_url;

    if let Some(idx) = media.subtitle_stream_index {
        eprintln!("Subtitles: stream {} ({})", idx, media.subtitle_codec.as_deref().unwrap_or("?"));
    } else {
        eprintln!("No subtitles");
    }

    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_h = if modeline.interlace { h / 2 } else { h };
    let field_size = w * field_h * 3;
    let frame_rate = modeline.p_clock * 1_000_000.0 / (modeline.h_total as f64 * modeline.v_total as f64);
    let field_rate = if modeline.interlace { frame_rate * 2.0 } else { frame_rate };
    let ffmpeg_fps = field_rate;
    let ffmpeg_h = field_h;
    let ffmpeg_frame_size = field_size;

    eprintln!("{}x{}{} @ {:.2} fields/s, ffmpeg {}x{}@{:.2}, field={}B",
        w, h, if modeline.interlace { "i" } else { "p" }, field_rate,
        w, ffmpeg_h, ffmpeg_fps, field_size);

    let seek_secs = seek_override.unwrap_or(media.view_offset_ms as f64 / 1000.0);
    if seek_secs > 0.0 { eprintln!("Resuming from {:.0}s", seek_secs); }

    // Extract subtitles to temp file
    let _sub_tempfile: Option<tempfile::NamedTempFile>;
    let sub_path: Option<String>;
    if let Some(sub_idx) = media.subtitle_stream_index {
        let codec = media.subtitle_codec.as_deref().unwrap_or("srt");
        let ext = match codec { "ass" | "ssa" => "ass", _ => "srt" };
        let tmp = tempfile::Builder::new().prefix("groovy-sub-").suffix(&format!(".{}", ext))
            .tempfile().context("temp sub file")?;
        let mut sub_args: Vec<String> = vec![];
        if seek_secs > 0.0 { sub_args.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
        sub_args.extend(["-i".into(), url.into(), "-map".into(), format!("0:{}", sub_idx),
            "-y".into(), "-v".into(), "error".into(), tmp.path().to_string_lossy().into()]);
        let r = Command::new(&ffmpeg).args(&sub_args).output()?;
        if r.status.success() {
            let p = tmp.path().to_string_lossy().to_string();
            eprintln!("Subs: {}", p);
            sub_path = Some(p); _sub_tempfile = Some(tmp);
        } else {
            eprintln!("Sub extraction failed, continuing without");
            sub_path = None; _sub_tempfile = None;
        }
    } else { sub_path = None; _sub_tempfile = None; }

    // FFmpeg video args
    let mut vargs: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 { vargs.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    vargs.extend(["-i".into(), url.clone()]);

    let vid_w = ((w as f64 * scale) as usize) & !1;
    let vid_h = ((ffmpeg_h as f64 * scale) as usize) & !1;
    let pad_x = (w - vid_w) / 2;
    let pad_y = (ffmpeg_h - vid_h) / 2;
    if scale < 1.0 {
        eprintln!("Scale {:.0}%: video {}x{} padded to {}x{}", scale * 100.0, vid_w, vid_h, w, ffmpeg_h);
    }

    if let Some(ref sp) = sub_path {
        vargs.extend([
            "-filter_complex".into(),
            format!("[0:v:0]scale={}:{}[v];[v]subtitles=filename={}:original_size={}x{}[s];[s]pad={}:{}:{}:{}:black[out]",
                vid_w, vid_h, sp, vid_w, vid_h, w, ffmpeg_h, pad_x, pad_y),
            "-map".into(), "[out]".into(),
        ]);
    } else {
        if scale < 1.0 {
            vargs.extend([
                "-filter_complex".into(),
                format!("[0:v:0]scale={}:{}[v];[v]pad={}:{}:{}:{}:black[out]", vid_w, vid_h, w, ffmpeg_h, pad_x, pad_y),
                "-map".into(), "[out]".into(),
            ]);
        } else {
            vargs.extend(["-map".into(), "0:v:0".into(), "-s".into(), format!("{}x{}", w, ffmpeg_h)]);
        }
    }
    vargs.extend(["-f".into(), "rawvideo".into(), "-pix_fmt".into(), "bgr24".into(),
        "-r".into(), format!("{:.4}", ffmpeg_fps), "-vsync".into(), "cfr".into(),
        "-v".into(), "error".into(), "-nostdin".into(), "pipe:1".into()]);

    // FFmpeg audio args
    let mut aargs: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 { aargs.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    let audio_map = if let Some(ai) = media.audio_stream_index {
        format!("0:{}", ai)
    } else {
        "0:a:0".to_string()
    };
    aargs.extend(["-i", url, "-map", &audio_map, "-ac", "2", "-ar", "48000",
        "-f", "s16le", "-acodec", "pcm_s16le", "-v", "warning", "-nostdin", "pipe:1",
    ].iter().map(|s| s.to_string()));

    // Start FFmpeg
    eprintln!("Starting FFmpeg...");
    let mut video_proc = Command::new(&ffmpeg).args(&vargs)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("FFmpeg video")?;
    let mut audio_proc = Command::new(&ffmpeg).args(&aargs)
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("FFmpeg audio")?;
    let video_stdout = video_proc.stdout.take().unwrap();
    let audio_stdout = audio_proc.stdout.take().unwrap();

    // Latest-frame model — each FFmpeg frame = one field at field_rate
    let latest_frame: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let first_video_frame = Arc::new(AtomicBool::new(false));
    let video_ended = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    // Video reader thread
    {
        let latest = latest_frame.clone();
        let first = first_video_frame.clone();
        let ended = video_ended.clone();
        let running = running.clone();
        std::thread::Builder::new().name("video-reader".into()).spawn(move || {
            let mut reader = video_stdout;
            let mut buf = vec![0u8; ffmpeg_frame_size];
            while running.load(Ordering::Relaxed) {
                let mut filled = 0;
                while filled < ffmpeg_frame_size {
                    match reader.read(&mut buf[filled..]) {
                        Ok(0) | Err(_) => { ended.store(true, Ordering::Relaxed); return; }
                        Ok(n) => filled += n,
                    }
                }
                *latest.lock().unwrap() = Some(buf.clone());
                first.store(true, Ordering::Relaxed);
            }
        })?;
    }

    // Wait for first frame before connecting
    eprintln!("Waiting for first frame ({} bytes)...", ffmpeg_frame_size);
    loop {
        if first_video_frame.load(Ordering::Relaxed) { break; }
        if video_ended.load(Ordering::Relaxed) {
            let _ = video_proc.wait();
            let mut err = String::new();
            if let Some(mut se) = video_proc.stderr.take() { let _ = se.read_to_string(&mut err); }
            let _ = audio_proc.kill();
            bail!("FFmpeg no video. Stderr:\n{}", err);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("Got first frame.");

    // Single socket for video + audio
    let sock = Arc::new(Mutex::new(create_udp_socket(mister_ip)?));
    eprintln!("Connected to {}:{}", mister_ip, groovy::UDP_PORT);

    // Init + Switchres
    {
        let s = sock.lock().unwrap();
        s.send(&groovy::build_init(0, 3, 2, 0))?;
    }
    std::thread::sleep(Duration::from_millis(200));
    {
        let s = sock.lock().unwrap();
        s.send(&groovy::build_switchres(modeline))?;
    }
    std::thread::sleep(Duration::from_millis(500));

    // Audio thread — 3-phase sync: discard until video ready, skip 300ms, then send
    let audio_sock = sock.clone();
    let audio_running = running.clone();
    let audio_first = first_video_frame.clone();
    let audio_thread = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut reader = audio_stdout;
        let mut buf = vec![0u8; 4800];

        // Phase 1: discard until first video frame
        while audio_running.load(Ordering::Relaxed) {
            if audio_first.load(Ordering::Relaxed) { break; }
            match reader.read(&mut buf[..3840]) {
                Ok(0) | Err(_) => return,
                _ => {}
            }
        }

        // Phase 2: discard ~300ms (48000Hz * 2ch * 2bytes * 0.3s = 57600)
        let mut discarded = 0;
        while discarded < 57600 && audio_running.load(Ordering::Relaxed) {
            let n = std::cmp::min(3840, 57600 - discarded);
            match reader.read(&mut buf[..n]) {
                Ok(0) | Err(_) => return,
                Ok(n) => discarded += n,
            }
        }

        // Phase 3: send audio via shared socket (wireLock = Mutex)
        // Groovy protocol: header then chunked payload
        // = header as one send(), then payload chunked in MTU-sized sends
        let mtu = groovy::DEFAULT_MTU;
        while audio_running.load(Ordering::Relaxed) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let hdr = groovy::build_audio(n as u16);
                    let s = audio_sock.lock().unwrap();
                    let _ = s.send(&hdr);
                    // Chunk payload into MTU-sized sends
                    let mut off = 0;
                    while off < n {
                        let end = (off + mtu).min(n);
                        let _ = s.send(&buf[off..end]);
                        off = end;
                    }
                    drop(s);
                }
            }
        }
    })?;

    // Field-rate send loop:
    // - Timer fires at field_rate
    // - If still sending previous frame, SKIP (isSending guard)
    // - Grab latest frame, split fields if interlaced, send
    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let mtu = groovy::DEFAULT_MTU;
    let vsync = modeline.v_begin;
    let field_interval_us = std::cmp::max(8000, (1_000_000.0 / field_rate) as u64);
    let field_interval = Duration::from_micros(field_interval_us);
    let is_sending = AtomicBool::new(false);
    let mut next_tick = Instant::now();

    // Progress reporting to Plex
    let progress_interval = Duration::from_secs(10);
    let mut last_progress = Instant::now();
    let start_offset_ms = media.view_offset_ms;
    let duration_ms = media.duration_ms;
    let playback_start = Instant::now();

    eprintln!("Streaming (interval={}us)...", field_interval_us);

    while running.load(Ordering::Relaxed) {
        if video_ended.load(Ordering::Relaxed) { eprintln!("Stream ended"); break; }

        // isSending guard: skip if previous send still in progress
        if is_sending.load(Ordering::Relaxed) {
            next_tick += field_interval;
            spin_sleep::sleep(field_interval);
            continue;
        }

        let frame = latest_frame.lock().unwrap().clone();
        let Some(frame_data) = frame else {
            spin_sleep::sleep(Duration::from_millis(1));
            continue;
        };

        is_sending.store(true, Ordering::Relaxed);

        // Each FFmpeg frame = one field (at field_rate)
        // For interlaced: alternate field 0/1. For progressive: always field 0.
        frame_count += 1;
        {
            let s = sock.lock().unwrap();
            let _ = s.send(&groovy::build_blit_field_vsync(frame_count, current_field, vsync, None));
            let mut off = 0;
            while off < frame_data.len() {
                let end = (off + mtu).min(frame_data.len());
                let _ = s.send(&frame_data[off..end]);
                off = end;
            }
        }

        if modeline.interlace {
            current_field = if current_field == 0 { 1 } else { 0 };
        }

        is_sending.store(false, Ordering::Relaxed);

        if frame_count == 5 || frame_count % 1800 == 0 {
            eprintln!("Frame {}", frame_count);
        }

        // Precise sleep until next tick (spin_sleep hybrid: sleep then spin for last ~1ms)
        next_tick += field_interval;
        let now = Instant::now();
        if next_tick > now {
            spin_sleep::sleep(next_tick - now);
        } else {
            // Fell behind — skip to now instead of bursting
            next_tick = now;
        }

        // Report progress to Plex every 10s
        if last_progress.elapsed() >= progress_interval {
            let elapsed_ms = playback_start.elapsed().as_millis() as u64;
            let current_ms = start_offset_ms + elapsed_ms;
            let _ = plex.report_progress(rating_key, current_ms, "playing", duration_ms);
            last_progress = Instant::now();
        }
    }

    // Report final position to Plex
    let elapsed_ms = playback_start.elapsed().as_millis() as u64;
    let final_ms = start_offset_ms + elapsed_ms;
    if duration_ms > 0 && final_ms >= duration_ms.saturating_sub(60_000) {
        // Within last minute — mark as watched
        eprintln!("Marking as watched");
        let _ = plex.scrobble(rating_key);
    } else {
        eprintln!("Saving position: {}s", final_ms / 1000);
        let _ = plex.report_progress(rating_key, final_ms, "stopped", duration_ms);
    }

    // Guard sends close on drop
    running.store(false, Ordering::Relaxed);
    let _ = video_proc.kill();
    let _ = audio_proc.kill();
    audio_thread.join().ok();
    eprintln!("Done");
    Ok(())
}

fn stream_file(
    path: &str, mister_ip: &str, modeline: &Modeline, scale: f64,
    audio_track: Option<u32>, sub_option: Option<&str>, seek_override: Option<f64>,
) -> Result<()> {
    let ffmpeg = find_ffmpeg()?;
    let url = path;

    if !std::path::Path::new(path).exists() {
        bail!("File not found: {}", path);
    }

    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_h = if modeline.interlace { h / 2 } else { h };
    let field_size = w * field_h * 3;
    let frame_rate = modeline.p_clock * 1_000_000.0 / (modeline.h_total as f64 * modeline.v_total as f64);
    let field_rate = if modeline.interlace { frame_rate * 2.0 } else { frame_rate };
    let ffmpeg_fps = field_rate;
    let ffmpeg_h = field_h;
    let ffmpeg_frame_size = field_size;

    eprintln!("File: {}", path);
    eprintln!("{}x{}{} @ {:.2} fields/s, field={}B",
        w, h, if modeline.interlace { "i" } else { "p" }, field_rate, field_size);

    let seek_secs = seek_override.unwrap_or(0.0);
    if seek_secs > 0.0 { eprintln!("Seeking to {:.0}s", seek_secs); }

    // Extract subtitles if requested
    let _sub_tempfile: Option<tempfile::NamedTempFile>;
    let sub_path: Option<String>;
    let disabled = sub_option.map(|s| s.eq_ignore_ascii_case("none") || s == "off").unwrap_or(false);
    if !disabled {
        // Try to extract subtitle track
        let sub_idx = if let Some(ref s) = sub_option {
            if let Ok(n) = s.parse::<u32>() { Some(n) } else { None }
        } else {
            Some(0) // default: first subtitle track
        };
        if let Some(idx) = sub_idx {
            let tmp = tempfile::Builder::new().prefix("groovy-sub-").suffix(".ass")
                .tempfile().context("temp sub file")?;
            let mut sub_args: Vec<String> = vec![];
            if seek_secs > 0.0 { sub_args.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
            sub_args.extend(["-i".into(), url.into(), "-map".into(), format!("0:s:{}", idx),
                "-y".into(), "-v".into(), "error".into(), tmp.path().to_string_lossy().into()]);
            let r = Command::new(&ffmpeg).args(&sub_args).output()?;
            if r.status.success() {
                let p = tmp.path().to_string_lossy().to_string();
                eprintln!("Subs: track {} -> {}", idx, p);
                sub_path = Some(p); _sub_tempfile = Some(tmp);
            } else {
                eprintln!("No subtitle track {}, continuing without", idx);
                sub_path = None; _sub_tempfile = None;
            }
        } else { sub_path = None; _sub_tempfile = None; }
    } else { sub_path = None; _sub_tempfile = None; }

    // FFmpeg video args
    let mut vargs: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 { vargs.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    vargs.extend(["-i".into(), url.into()]);

    let vid_w = ((w as f64 * scale) as usize) & !1;
    let vid_h = ((ffmpeg_h as f64 * scale) as usize) & !1;
    let pad_x = (w - vid_w) / 2;
    let pad_y = (ffmpeg_h - vid_h) / 2;
    if scale < 1.0 {
        eprintln!("Scale {:.0}%: {}x{} padded to {}x{}", scale * 100.0, vid_w, vid_h, w, ffmpeg_h);
    }

    if let Some(ref sp) = sub_path {
        vargs.extend([
            "-filter_complex".into(),
            format!("[0:v:0]scale={}:{}[v];[v]subtitles=filename={}:original_size={}x{}[s];[s]pad={}:{}:{}:{}:black[out]",
                vid_w, vid_h, sp, vid_w, vid_h, w, ffmpeg_h, pad_x, pad_y),
            "-map".into(), "[out]".into(),
        ]);
    } else if scale < 1.0 {
        vargs.extend([
            "-filter_complex".into(),
            format!("[0:v:0]scale={}:{}[v];[v]pad={}:{}:{}:{}:black[out]", vid_w, vid_h, w, ffmpeg_h, pad_x, pad_y),
            "-map".into(), "[out]".into(),
        ]);
    } else {
        vargs.extend(["-map".into(), "0:v:0".into(), "-s".into(), format!("{}x{}", w, ffmpeg_h)]);
    }
    vargs.extend(["-f".into(), "rawvideo".into(), "-pix_fmt".into(), "bgr24".into(),
        "-r".into(), format!("{:.4}", ffmpeg_fps), "-vsync".into(), "cfr".into(),
        "-v".into(), "error".into(), "-nostdin".into(), "pipe:1".into()]);

    // FFmpeg audio args
    let mut aargs: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 { aargs.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    let audio_map = if let Some(ai) = audio_track { format!("0:a:{}", ai) } else { "0:a:0".into() };
    aargs.extend(["-i", url, "-map", &audio_map, "-ac", "2", "-ar", "48000",
        "-f", "s16le", "-acodec", "pcm_s16le", "-v", "warning", "-nostdin", "pipe:1",
    ].iter().map(|s| s.to_string()));

    // Start FFmpeg
    eprintln!("Starting FFmpeg...");
    let mut video_proc = Command::new(&ffmpeg).args(&vargs)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("FFmpeg video")?;
    let mut audio_proc = Command::new(&ffmpeg).args(&aargs)
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("FFmpeg audio")?;
    let video_stdout = video_proc.stdout.take().unwrap();
    let audio_stdout = audio_proc.stdout.take().unwrap();

    let latest_frame: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let first_video_frame = Arc::new(AtomicBool::new(false));
    let video_ended = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    {
        let latest = latest_frame.clone();
        let first = first_video_frame.clone();
        let ended = video_ended.clone();
        let running = running.clone();
        std::thread::Builder::new().name("video-reader".into()).spawn(move || {
            let mut reader = video_stdout;
            let mut buf = vec![0u8; ffmpeg_frame_size];
            while running.load(Ordering::Relaxed) {
                let mut filled = 0;
                while filled < ffmpeg_frame_size {
                    match reader.read(&mut buf[filled..]) {
                        Ok(0) | Err(_) => { ended.store(true, Ordering::Relaxed); return; }
                        Ok(n) => filled += n,
                    }
                }
                *latest.lock().unwrap() = Some(buf.clone());
                first.store(true, Ordering::Relaxed);
            }
        })?;
    }

    eprintln!("Waiting for first frame...");
    loop {
        if first_video_frame.load(Ordering::Relaxed) { break; }
        if video_ended.load(Ordering::Relaxed) {
            let _ = video_proc.wait();
            let mut err = String::new();
            if let Some(mut se) = video_proc.stderr.take() { let _ = se.read_to_string(&mut err); }
            let _ = audio_proc.kill();
            bail!("FFmpeg no video. Stderr:\n{}", err);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("Got first frame.");

    let sock = Arc::new(Mutex::new(create_udp_socket(mister_ip)?));
    let _guard = MisterGuard { sock: sock.clone() };
    eprintln!("Connected to {}:{}", mister_ip, groovy::UDP_PORT);

    { let s = sock.lock().unwrap(); s.send(&groovy::build_init(0, 3, 2, 0))?; }
    std::thread::sleep(Duration::from_millis(200));
    { let s = sock.lock().unwrap(); s.send(&groovy::build_switchres(modeline))?; }
    std::thread::sleep(Duration::from_millis(500));

    let audio_sock = sock.clone();
    let audio_running = running.clone();
    let audio_first = first_video_frame.clone();
    let audio_thread = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut reader = audio_stdout;
        let mut buf = vec![0u8; 4800];
        while audio_running.load(Ordering::Relaxed) {
            if audio_first.load(Ordering::Relaxed) { break; }
            match reader.read(&mut buf[..3840]) { Ok(0) | Err(_) => return, _ => {} }
        }
        let mut discarded = 0;
        while discarded < 57600 && audio_running.load(Ordering::Relaxed) {
            let n = std::cmp::min(3840, 57600 - discarded);
            match reader.read(&mut buf[..n]) { Ok(0) | Err(_) => return, Ok(n) => discarded += n }
        }
        let mtu = groovy::DEFAULT_MTU;
        while audio_running.load(Ordering::Relaxed) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let hdr = groovy::build_audio(n as u16);
                    let s = audio_sock.lock().unwrap();
                    let _ = s.send(&hdr);
                    let mut off = 0;
                    while off < n { let end = (off + mtu).min(n); let _ = s.send(&buf[off..end]); off = end; }
                    drop(s);
                }
            }
        }
    })?;

    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let mtu = groovy::DEFAULT_MTU;
    let vsync = modeline.v_begin;
    let field_interval_us = std::cmp::max(8000, (1_000_000.0 / field_rate) as u64);
    let field_interval = Duration::from_micros(field_interval_us);
    let is_sending = AtomicBool::new(false);
    let mut next_tick = Instant::now();

    eprintln!("Streaming...");

    while running.load(Ordering::Relaxed) {
        if video_ended.load(Ordering::Relaxed) { eprintln!("Stream ended"); break; }
        if is_sending.load(Ordering::Relaxed) {
            next_tick += field_interval;
            spin_sleep::sleep(field_interval);
            continue;
        }
        let frame = latest_frame.lock().unwrap().clone();
        let Some(frame_data) = frame else { spin_sleep::sleep(Duration::from_millis(1)); continue; };
        is_sending.store(true, Ordering::Relaxed);

        frame_count += 1;
        {
            let s = sock.lock().unwrap();
            let _ = s.send(&groovy::build_blit_field_vsync(frame_count, current_field, vsync, None));
            let mut off = 0;
            while off < frame_data.len() {
                let end = (off + mtu).min(frame_data.len());
                let _ = s.send(&frame_data[off..end]);
                off = end;
            }
        }
        if modeline.interlace { current_field = if current_field == 0 { 1 } else { 0 }; }
        is_sending.store(false, Ordering::Relaxed);

        next_tick += field_interval;
        let now = Instant::now();
        if next_tick > now { spin_sleep::sleep(next_tick - now); } else { next_tick = now; }
    }

    // Guard sends close on drop
    running.store(false, Ordering::Relaxed);
    let _ = video_proc.kill();
    let _ = audio_proc.kill();
    audio_thread.join().ok();
    eprintln!("Done");
    Ok(())
}

fn send_test_pattern(mister_ip: &str, modeline: &Modeline, scale: f64, duration_secs: u64) -> Result<()> {
    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_h = if modeline.interlace { h / 2 } else { h };
    let field_size = w * field_h * 3;
    let frame_rate = modeline.p_clock * 1_000_000.0 / (modeline.h_total as f64 * modeline.v_total as f64);
    let field_rate = if modeline.interlace { frame_rate * 2.0 } else { frame_rate };

    eprintln!("Test pattern: {}x{}{} @ {:.2} fields/s", w, h,
        if modeline.interlace { "i" } else { "p" }, field_rate);
    eprintln!("Duration: {}s. Ctrl+C to stop.", duration_secs);

    // Generate test pattern with scale applied
    // Generate at FULL height, then split into fields for interlaced (matching video path)
    let full_h = h;
    let inner_w = ((w as f64) * scale) as usize & !1;
    let inner_h = ((full_h as f64) * scale) as usize & !1;
    let pad_x = (w - inner_w) / 2;
    let pad_y = (full_h - inner_h) / 2;

    eprintln!("Scale: {:.0}% — inner {}x{}, padding {}x{}", scale * 100.0, inner_w, inner_h, pad_x, pad_y);

    let frame_size = w * full_h * 3;
    let mut pattern = vec![0u8; frame_size]; // black background
    let bpp = 3;
    for y in 0..full_h {
        for x in 0..w {
            let off = (y * w + x) * bpp;
            // Check if inside inner rect
            let ix = x as isize - pad_x as isize;
            let iy = y as isize - pad_y as isize;
            if ix < 0 || iy < 0 || ix >= inner_w as isize || iy >= inner_h as isize {
                continue; // black padding
            }
            let ix = ix as usize;
            let iy = iy as usize;

            // Distance from center, normalized so 1.0 = edge of inner rect
            // No aspect correction — just use raw pixel distance scaled to half-height
            // The CRT's 4:3 display will make it look right
            let cx_f = ix as f64 - inner_w as f64 / 2.0;
            let cy_f = iy as f64 - inner_h as f64 / 2.0;
            let half = inner_h as f64 / 2.0; // circle radius = half height
            let dist = (cx_f.powi(2) + cy_f.powi(2)).sqrt() / half;

            let (b, g, r);
            if ix == 0 || ix == inner_w - 1 || iy == 0 || iy == inner_h - 1 {
                // White border
                b = 255; g = 255; r = 255;
            } else if (ix as isize - inner_w as isize / 2).unsigned_abs() < 2
                   || (iy as isize - inner_h as isize / 2).unsigned_abs() < 2 {
                // Thick crosshair (4px wide)
                b = 80; g = 80; r = 80;
            } else {
                // 4 thick concentric rings at 25%, 50%, 75%, 100% of radius
                let ring_num = (dist * 4.0) as u32;
                let frac = (dist * 4.0).fract();
                let on_ring = frac > 0.35 && frac < 0.65; // thick ~30% band
                if on_ring && ring_num < 4 {
                    match ring_num {
                        0 => { b = 255; g = 255; r = 255; } // white
                        1 => { b = 0;   g = 0;   r = 255; } // red
                        2 => { b = 0;   g = 255; r = 0;   } // green
                        3 => { b = 255; g = 0;   r = 0;   } // blue
                        _ => { b = 255; g = 255; r = 255; }
                    }
                } else if dist > 1.02 {
                    b = 10; g = 10; r = 10; // outside
                } else {
                    b = 40; g = 40; r = 40; // between rings
                }
            }

            pattern[off] = b;
            pattern[off + 1] = g;
            pattern[off + 2] = r;
        }
    }

    // Split into even/odd fields for interlaced (same as video path)
    let (field0, field1) = if modeline.interlace {
        let mut f0 = vec![0u8; field_size];
        let mut f1 = vec![0u8; field_size];
        let rb = w * 3;
        for y in 0..field_h {
            let dst = y * rb;
            f0[dst..dst + rb].copy_from_slice(&pattern[y * 2 * rb..(y * 2 + 1) * rb]);
            if y * 2 + 1 < full_h {
                f1[dst..dst + rb].copy_from_slice(&pattern[(y * 2 + 1) * rb..(y * 2 + 2) * rb]);
            }
        }
        (f0, f1)
    } else {
        (pattern.clone(), vec![])
    };

    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    let sock = create_udp_socket(mister_ip)?;
    sock.send(&groovy::build_init(0, 0, 0, 0))?;
    std::thread::sleep(Duration::from_millis(200));
    sock.send(&groovy::build_switchres(modeline))?;
    std::thread::sleep(Duration::from_millis(500));

    let mtu = groovy::DEFAULT_MTU;
    let vsync = modeline.v_begin;
    let field_interval = Duration::from_micros(std::cmp::max(8000, (1_000_000.0 / field_rate) as u64));
    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let mut next_tick = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(duration_secs);

    eprintln!("Sending test pattern...");
    eprintln!("White border, checkerboard, corners: TL=red TR=green BL=blue BR=yellow");

    while running.load(Ordering::Relaxed) && Instant::now() < deadline {
        frame_count += 1;
        let data = if modeline.interlace {
            if current_field == 0 { &field0 } else { &field1 }
        } else {
            &field0
        };

        let _ = sock.send(&groovy::build_blit_field_vsync(frame_count, current_field, vsync, None));
        let mut off = 0;
        while off < data.len() {
            let end = (off + mtu).min(data.len());
            let _ = sock.send(&data[off..end]);
            off = end;
        }

        if modeline.interlace {
            current_field = if current_field == 0 { 1 } else { 0 };
        }

        next_tick += field_interval;
        let now = Instant::now();
        if next_tick > now { spin_sleep::sleep(next_tick - now); }
        else { next_tick = now; }
    }

    let _ = sock.send(&groovy::build_close());
    eprintln!("Done");
    Ok(())
}

fn create_udp_socket(mister_ip: &str) -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_send_buffer_size(2 * 1024 * 1024)?;
    s.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())?;
    let dest: std::net::SocketAddr = format!("{}:{}", mister_ip, groovy::UDP_PORT).parse()?;
    s.connect(&dest.into())?;
    s.set_nonblocking(false)?;
    Ok(s.into())
}

fn find_ffmpeg() -> Result<String> {
    if let Ok(o) = Command::new("which").arg("ffmpeg").output() {
        if o.status.success() {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() { return Ok(p); }
        }
    }
    for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
        if std::path::Path::new(p).exists() { return Ok(p.to_string()); }
    }
    bail!("FFmpeg not found. Install with: brew install ffmpeg")
}

fn ctrlc_handler(running: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        eprintln!("\nStopping...");
        running.store(false, Ordering::Relaxed);
    }).ok();
}
