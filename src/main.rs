use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lz4_flex::compress_prepend_size;
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Read;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod auth;
mod config;
mod connection;
mod ffmpeg;
#[allow(dead_code)]
mod groovy;
mod plex;

#[allow(unused_imports)]
use config::CustomModeline;
use groovy::{Modeline, MODELINES};



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



// ── Main ──

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load();

    match &cli.command {
        Commands::Auth => {
            let token = auth::plex_oauth()?;
            let path = config::config_path();
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
            let p = config::config_path();
            println!("Config: {}", p.display());
            if p.exists() { println!("{}", std::fs::read_to_string(&p)?); }
            else { println!("(not found)\n\nExample:\n  mister = \"192.168.0.115\"\n  server = \"192.168.0.29\"\n  port = 32400\n  token = \"your-token\"\n  modeline = \"640x480i NTSC\""); }
            return Ok(());
        }
        _ => {}
    }

    let rcfg = config::resolve(
        cli.mister.clone(), cli.server.clone(), cli.port,
        cli.token.clone(), cli.modeline.clone(), cli.scale, &cfg,
    )?;
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
    let ffmpeg_path = ffmpeg::find_ffmpeg()?;
    let media = plex.resolve_media(rating_key, audio_lang, sub_lang)?;
    let url = &media.direct_play_url;

    if let Some(idx) = media.subtitle_stream_index {
        eprintln!("Subtitles: stream {} ({})", idx, media.subtitle_codec.as_deref().unwrap_or("?"));
    } else {
        eprintln!("No subtitles");
    }

    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let ffmpeg_h = modeline.field_height();
    let field_rate = modeline.field_rate();

    eprintln!("{}x{}{} @ {:.2} fields/s, ffmpeg {}x{}@{:.2}, field={}B",
        w, h, if modeline.interlace { "i" } else { "p" }, field_rate,
        w, ffmpeg_h, field_rate, modeline.field_size());

    let seek_secs = seek_override.unwrap_or(media.view_offset_ms as f64 / 1000.0);
    if seek_secs > 0.0 { eprintln!("Resuming from {:.0}s", seek_secs); }

    // Extract subtitles to temp file
    let _sub_tempfile: Option<tempfile::NamedTempFile>;
    let sub_path: Option<String>;
    if let Some(sub_idx) = media.subtitle_stream_index {
        let codec = media.subtitle_codec.as_deref().unwrap_or("srt");
        let ext = match codec { "ass" | "ssa" => "ass", _ => "srt" };
        let sub_map = format!("0:{}", sub_idx);
        match ffmpeg::extract_subs(&ffmpeg_path, url, &sub_map, ext, seek_secs)? {
            Some((p, tmp)) => {
                eprintln!("Subs: {}", p);
                sub_path = Some(p); _sub_tempfile = Some(tmp);
            }
            None => {
                eprintln!("Sub extraction failed, continuing without");
                sub_path = None; _sub_tempfile = None;
            }
        }
    } else { sub_path = None; _sub_tempfile = None; }

    let audio_map = if let Some(ai) = media.audio_stream_index {
        format!("0:{}", ai)
    } else {
        "0:a:0".to_string()
    };

    let vparams = ffmpeg::VideoParams {
        url: url.clone(),
        seek_secs,
        sub_path,
        audio_map,
        w,
        ffmpeg_h,
        ffmpeg_fps: field_rate,
        scale,
    };

    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    let mut pipeline = ffmpeg::start(&ffmpeg_path, &vparams, running.clone())?;

    // Connect with FPGA feedback, LZ4, congestion control, raster sync
    let conn = Arc::new(Mutex::new(connection::GroovyConnection::connect(mister_ip)?));
    eprintln!("Connected to {}:{}", mister_ip, groovy::UDP_PORT);
    { conn.lock().unwrap().init(modeline)?; }

    // Audio thread — 3-phase sync, shared connection
    let audio_conn = conn.clone();
    let audio_running = running.clone();
    let audio_first = pipeline.first_frame.clone();
    let audio_stdout = pipeline.audio_stdout.take().unwrap();
    let audio_thread = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut reader = audio_stdout;
        let mut buf = vec![0u8; 4800];
        // Phase 1: discard until first video frame
        while audio_running.load(Ordering::Relaxed) {
            if audio_first.load(Ordering::Relaxed) { break; }
            match reader.read(&mut buf[..3840]) { Ok(0) | Err(_) => return, _ => {} }
        }
        // Phase 2: discard ~300ms
        let mut discarded = 0;
        while discarded < 57600 && audio_running.load(Ordering::Relaxed) {
            let n = std::cmp::min(3840, 57600 - discarded);
            match reader.read(&mut buf[..n]) { Ok(0) | Err(_) => return, Ok(n) => discarded += n }
        }
        // Phase 3: send audio
        while audio_running.load(Ordering::Relaxed) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => { audio_conn.lock().unwrap().audio(&buf[..n]); }
            }
        }
    })?;

    // Streaming loop with FPGA raster sync
    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let vsync = modeline.v_begin;
    let progress_interval = Duration::from_secs(10);
    let mut last_progress = Instant::now();
    let start_offset_ms = media.view_offset_ms;
    let duration_ms = media.duration_ms;
    let playback_start = Instant::now();

    eprintln!("Streaming with FPGA sync + LZ4...");

    while running.load(Ordering::Relaxed) {
        if pipeline.video_ended.load(Ordering::Relaxed) { eprintln!("Stream ended"); break; }

        let frame = pipeline.latest_frame.lock().unwrap().clone();
        let Some(frame_data) = frame else {
            spin_sleep::sleep(Duration::from_millis(1));
            continue;
        };

        let field_start = Instant::now();
        frame_count += 1;

        { conn.lock().unwrap().blit(&frame_data, frame_count, current_field, vsync); }

        if modeline.interlace {
            current_field = if current_field == 0 { 1 } else { 0 };
        }

        if frame_count == 5 || frame_count % 1800 == 0 {
            let s = conn.lock().unwrap();
            eprintln!("Frame {} (synced={} vblank={} ready={})",
                frame_count, s.status.vram_synced, s.status.vga_vblank, s.status.vram_ready);
        }

        // Raster-aware sync
        let elapsed_ns = field_start.elapsed().as_nanos() as u64;
        { conn.lock().unwrap().wait_sync(elapsed_ns); }

        // Plex progress
        if last_progress.elapsed() >= progress_interval {
            let current_ms = start_offset_ms + playback_start.elapsed().as_millis() as u64;
            let _ = plex.report_progress(rating_key, current_ms, "playing", duration_ms);
            last_progress = Instant::now();
        }
    }

    // Report final position to Plex
    let final_ms = start_offset_ms + playback_start.elapsed().as_millis() as u64;
    if duration_ms > 0 && final_ms >= duration_ms.saturating_sub(60_000) {
        eprintln!("Marking as watched");
        let _ = plex.scrobble(rating_key);
    } else {
        eprintln!("Saving position: {}s", final_ms / 1000);
        let _ = plex.report_progress(rating_key, final_ms, "stopped", duration_ms);
    }

    // Connection sends close on drop
    running.store(false, Ordering::Relaxed);
    let _ = pipeline.video_proc.kill();
    let _ = pipeline.audio_proc.kill();
    audio_thread.join().ok();
    eprintln!("Done");
    Ok(())
}

fn stream_file(
    path: &str, mister_ip: &str, modeline: &Modeline, scale: f64,
    audio_track: Option<u32>, sub_option: Option<&str>, seek_override: Option<f64>,
) -> Result<()> {
    let ffmpeg_path = ffmpeg::find_ffmpeg()?;

    if !std::path::Path::new(path).exists() {
        bail!("File not found: {}", path);
    }

    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let ffmpeg_h = modeline.field_height();
    let field_rate = modeline.field_rate();

    eprintln!("File: {}", path);
    eprintln!("{}x{}{} @ {:.2} fields/s, field={}B",
        w, h, if modeline.interlace { "i" } else { "p" }, field_rate, modeline.field_size());

    let seek_secs = seek_override.unwrap_or(0.0);
    if seek_secs > 0.0 { eprintln!("Seeking to {:.0}s", seek_secs); }

    // Extract subtitles if requested
    let _sub_tempfile: Option<tempfile::NamedTempFile>;
    let sub_path: Option<String>;
    let disabled = sub_option.map(|s| s.eq_ignore_ascii_case("none") || s == "off").unwrap_or(false);
    if !disabled {
        let sub_idx = if let Some(ref s) = sub_option {
            if let Ok(n) = s.parse::<u32>() { Some(n) } else { None }
        } else {
            Some(0)
        };
        if let Some(idx) = sub_idx {
            let sub_map = format!("0:s:{}", idx);
            match ffmpeg::extract_subs(&ffmpeg_path, path, &sub_map, "ass", seek_secs)? {
                Some((p, tmp)) => {
                    eprintln!("Subs: track {} -> {}", idx, p);
                    sub_path = Some(p); _sub_tempfile = Some(tmp);
                }
                None => {
                    eprintln!("No subtitle track {}, continuing without", idx);
                    sub_path = None; _sub_tempfile = None;
                }
            }
        } else { sub_path = None; _sub_tempfile = None; }
    } else { sub_path = None; _sub_tempfile = None; }

    let audio_map = if let Some(ai) = audio_track { format!("0:a:{}", ai) } else { "0:a:0".into() };

    let vparams = ffmpeg::VideoParams {
        url: path.into(),
        seek_secs,
        sub_path,
        audio_map,
        w,
        ffmpeg_h,
        ffmpeg_fps: field_rate,
        scale,
    };

    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    let mut pipeline = ffmpeg::start(&ffmpeg_path, &vparams, running.clone())?;

    let sock = Arc::new(Mutex::new(create_udp_socket(mister_ip)?));
    let _guard = MisterGuard { sock: sock.clone() };
    eprintln!("Connected to {}:{}", mister_ip, groovy::UDP_PORT);

    { let s = sock.lock().unwrap(); s.send(&groovy::build_init(1, 3, 2, 0))?; }
    std::thread::sleep(Duration::from_millis(200));
    { let s = sock.lock().unwrap(); s.send(&groovy::build_switchres(modeline))?; }
    std::thread::sleep(Duration::from_millis(500));

    let audio_sock = sock.clone();
    let audio_running = running.clone();
    let audio_first = pipeline.first_frame.clone();
    let audio_stdout = pipeline.audio_stdout.take().unwrap();
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
        if pipeline.video_ended.load(Ordering::Relaxed) { eprintln!("Stream ended"); break; }
        if is_sending.load(Ordering::Relaxed) {
            next_tick += field_interval;
            spin_sleep::sleep(field_interval);
            continue;
        }
        let frame = pipeline.latest_frame.lock().unwrap().clone();
        let Some(frame_data) = frame else { spin_sleep::sleep(Duration::from_millis(1)); continue; };
        is_sending.store(true, Ordering::Relaxed);

        frame_count += 1;
        let compressed = compress_prepend_size(&frame_data);
        {
            let s = sock.lock().unwrap();
            let _ = s.send(&groovy::build_blit(frame_count, current_field, vsync, Some(compressed.len() as u32)));
            let mut off = 0;
            while off < compressed.len() {
                let end = (off + mtu).min(compressed.len());
                let _ = s.send(&compressed[off..end]);
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
    let _ = pipeline.video_proc.kill();
    let _ = pipeline.audio_proc.kill();
    audio_thread.join().ok();
    eprintln!("Done");
    Ok(())
}

fn send_test_pattern(mister_ip: &str, modeline: &Modeline, scale: f64, duration_secs: u64) -> Result<()> {
    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_h = modeline.field_height();
    let field_size = modeline.field_size();
    let field_rate = modeline.field_rate();

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
            let cx_f = ix as f64 - inner_w as f64 / 2.0;
            let cy_f = iy as f64 - inner_h as f64 / 2.0;
            let half = inner_h as f64 / 2.0;
            let dist = (cx_f.powi(2) + cy_f.powi(2)).sqrt() / half;

            let (b, g, r);
            if ix == 0 || ix == inner_w - 1 || iy == 0 || iy == inner_h - 1 {
                b = 255; g = 255; r = 255;
            } else if (ix as isize - inner_w as isize / 2).unsigned_abs() < 2
                   || (iy as isize - inner_h as isize / 2).unsigned_abs() < 2 {
                b = 80; g = 80; r = 80;
            } else {
                let ring_num = (dist * 4.0) as u32;
                let frac = (dist * 4.0).fract();
                let on_ring = frac > 0.35 && frac < 0.65;
                if on_ring && ring_num < 4 {
                    match ring_num {
                        0 => { b = 255; g = 255; r = 255; }
                        1 => { b = 0;   g = 0;   r = 255; }
                        2 => { b = 0;   g = 255; r = 0;   }
                        3 => { b = 255; g = 0;   r = 0;   }
                        _ => { b = 255; g = 255; r = 255; }
                    }
                } else if dist > 1.02 {
                    b = 10; g = 10; r = 10;
                } else {
                    b = 40; g = 40; r = 40;
                }
            }

            pattern[off] = b;
            pattern[off + 1] = g;
            pattern[off + 2] = r;
        }
    }

    // Split into even/odd fields for interlaced
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

        let _ = sock.send(&groovy::build_blit(frame_count, current_field, vsync, None));
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

fn ctrlc_handler(running: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        eprintln!("\nStopping...");
        running.store(false, Ordering::Relaxed);
    }).ok();
}
