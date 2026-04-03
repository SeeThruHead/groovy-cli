//! Core streaming loop: FFmpeg pipeline → GroovyConnection (or raw UDP).
//!
//! Two entry points:
//! - `stream_to_mister` — Plex media with progress reporting
//! - `stream_file` — local file, raw UDP (no GroovyConnection)

use crate::connection;
use crate::ffmpeg;
use crate::groovy::{self, Modeline};
use crate::plex;
use anyhow::{bail, Result};
use lz4_flex::compress_prepend_size;
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Read;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Plex streaming (uses GroovyConnection) ──

pub fn stream_to_mister(
    plex: &plex::PlexClient,
    rating_key: u64,
    mister_ip: &str,
    modeline: &Modeline,
    scale: f64,
    audio_lang: Option<&str>,
    sub_lang: Option<&str>,
    seek_override: Option<f64>,
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

// ── Local file streaming (raw UDP, no GroovyConnection) ──

pub fn stream_file(
    path: &str,
    mister_ip: &str,
    modeline: &Modeline,
    scale: f64,
    audio_track: Option<u32>,
    sub_option: Option<&str>,
    seek_override: Option<f64>,
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

// ── Helpers ──

/// Guard that sends CMD_CLOSE to MiSTer on drop (for raw UDP path).
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
