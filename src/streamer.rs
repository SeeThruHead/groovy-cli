//! Core streaming loop: FFmpeg pipeline → GroovyConnection.
//!
//! Single `stream()` engine used by both Plex and local-file playback.
//! - `stream_to_mister` — Plex media with progress reporting
//! - `stream_file` — local file, no Plex

use crate::connection::GroovyConnection;
use crate::ffmpeg;
use crate::groovy::Modeline;
use crate::plex;
use anyhow::{bail, Result};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Plex progress tracker (optional) ──

/// When present, the streaming loop reports playback progress to Plex
/// and marks media as watched when it reaches the end.
pub struct PlexProgress<'a> {
    pub plex: &'a plex::PlexClient,
    pub rating_key: u64,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
}

// ── Public entry points (thin wrappers around `stream`) ──

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
        eprintln!(
            "Subtitles: stream {} ({})",
            idx,
            media.subtitle_codec.as_deref().unwrap_or("?")
        );
    } else {
        eprintln!("No subtitles");
    }

    let w = modeline.h_active as usize;
    let ffmpeg_h = modeline.field_height();
    let field_rate = modeline.field_rate();
    let seek_secs = seek_override.unwrap_or(media.view_offset_ms as f64 / 1000.0);

    if seek_secs > 0.0 {
        eprintln!("Resuming from {:.0}s", seek_secs);
    }

    // Extract subtitles to temp file
    let (_sub_tempfile, sub_path) = extract_plex_subs(
        &ffmpeg_path,
        url,
        media.subtitle_stream_index,
        media.subtitle_codec.as_deref(),
        seek_secs,
    )?;

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

    let progress = PlexProgress {
        plex,
        rating_key,
        start_offset_ms: media.view_offset_ms,
        duration_ms: media.duration_ms,
    };

    stream(&ffmpeg_path, &vparams, mister_ip, modeline, Some(&progress))
}

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
    let ffmpeg_h = modeline.field_height();
    let field_rate = modeline.field_rate();
    let seek_secs = seek_override.unwrap_or(0.0);

    eprintln!("File: {}", path);
    if seek_secs > 0.0 {
        eprintln!("Seeking to {:.0}s", seek_secs);
    }

    // Extract subtitles if requested
    let (_sub_tempfile, sub_path) =
        extract_file_subs(&ffmpeg_path, path, sub_option, seek_secs)?;

    let audio_map = if let Some(ai) = audio_track {
        format!("0:a:{}", ai)
    } else {
        "0:a:0".into()
    };

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

    stream(&ffmpeg_path, &vparams, mister_ip, modeline, None)
}

// ── Core streaming engine ──

/// Unified streaming loop. Wires FFmpeg pipeline to GroovyConnection.
/// Audio thread with 3-phase sync. FPGA raster sync loop.
/// Optional Plex progress reporting.
fn stream(
    ffmpeg_path: &str,
    vparams: &ffmpeg::VideoParams,
    mister_ip: &str,
    modeline: &Modeline,
    progress: Option<&PlexProgress>,
) -> Result<()> {
    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_rate = modeline.field_rate();

    eprintln!(
        "{}x{}{} @ {:.2} fields/s, ffmpeg {}x{}@{:.2}, field={}B",
        w,
        h,
        if modeline.interlace { "i" } else { "p" },
        field_rate,
        vparams.w,
        vparams.ffmpeg_h,
        vparams.ffmpeg_fps,
        modeline.field_size()
    );

    let running = Arc::new(AtomicBool::new(true));
    ctrlc_handler(running.clone());

    let mut pipeline = ffmpeg::start(ffmpeg_path, vparams, running.clone())?;

    // GroovyConnection: UDP socket, FPGA sync, LZ4 blit, congestion control
    let conn = Arc::new(Mutex::new(GroovyConnection::connect(mister_ip)?));
    eprintln!("Connected to {}:{}", mister_ip, crate::groovy::UDP_PORT);
    conn.lock().unwrap().init(modeline)?;

    // Audio thread — 3-phase sync, shared connection
    let audio_thread = spawn_audio_thread(
        conn.clone(),
        running.clone(),
        pipeline.first_frame.clone(),
        pipeline.audio_stdout.take().unwrap(),
    )?;

    // Streaming loop with FPGA raster sync
    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let vsync = modeline.v_begin;
    let progress_interval = Duration::from_secs(10);
    let mut last_progress = Instant::now();
    let playback_start = Instant::now();

    eprintln!("Streaming with FPGA sync + LZ4...");

    while running.load(Ordering::Relaxed) {
        if pipeline.video_ended.load(Ordering::Relaxed) {
            eprintln!("Stream ended");
            break;
        }

        let frame = pipeline.latest_frame.lock().unwrap().clone();
        let Some(frame_data) = frame else {
            spin_sleep::sleep(Duration::from_millis(1));
            continue;
        };

        let field_start = Instant::now();
        frame_count += 1;

        conn.lock().unwrap().blit(&frame_data, frame_count, current_field, vsync);

        if modeline.interlace {
            current_field = if current_field == 0 { 1 } else { 0 };
        }

        if frame_count == 5 || frame_count % 1800 == 0 {
            let s = conn.lock().unwrap();
            eprintln!(
                "Frame {} (synced={} vblank={} ready={})",
                frame_count, s.status.vram_synced, s.status.vga_vblank, s.status.vram_ready
            );
        }

        // Raster-aware sync
        let elapsed_ns = field_start.elapsed().as_nanos() as u64;
        conn.lock().unwrap().wait_sync(elapsed_ns);

        // Plex progress reporting
        if let Some(p) = progress {
            if last_progress.elapsed() >= progress_interval {
                let current_ms =
                    p.start_offset_ms + playback_start.elapsed().as_millis() as u64;
                let _ = p.plex.report_progress(p.rating_key, current_ms, "playing", p.duration_ms);
                last_progress = Instant::now();
            }
        }
    }

    // Finalize Plex progress
    if let Some(p) = progress {
        let final_ms = p.start_offset_ms + playback_start.elapsed().as_millis() as u64;
        if p.duration_ms > 0 && final_ms >= p.duration_ms.saturating_sub(60_000) {
            eprintln!("Marking as watched");
            let _ = p.plex.scrobble(p.rating_key);
        } else {
            eprintln!("Saving position: {}s", final_ms / 1000);
            let _ =
                p.plex
                    .report_progress(p.rating_key, final_ms, "stopped", p.duration_ms);
        }
    }

    // Connection sends close on drop
    running.store(false, Ordering::Relaxed);
    let _ = pipeline.video_proc.kill();
    let _ = pipeline.audio_proc.kill();
    audio_thread.join().ok();
    eprintln!("Done");
    Ok(())
}

// ── Audio thread — 3-phase sync ──

/// Spawn audio thread with 3-phase synchronization:
/// 1. Discard audio until first video frame arrives
/// 2. Discard ~300ms of audio to align AV sync
/// 3. Stream audio through GroovyConnection
fn spawn_audio_thread(
    conn: Arc<Mutex<GroovyConnection>>,
    running: Arc<AtomicBool>,
    first_frame: Arc<AtomicBool>,
    audio_stdout: std::process::ChildStdout,
) -> Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut reader = audio_stdout;
            let mut buf = vec![0u8; 4800];

            // Phase 1: discard until first video frame
            while running.load(Ordering::Relaxed) {
                if first_frame.load(Ordering::Relaxed) {
                    break;
                }
                match reader.read(&mut buf[..3840]) {
                    Ok(0) | Err(_) => return,
                    _ => {}
                }
            }

            // Phase 2: discard ~300ms (57600 bytes at 48kHz stereo 16-bit)
            let mut discarded = 0;
            while discarded < 57600 && running.load(Ordering::Relaxed) {
                let n = std::cmp::min(3840, 57600 - discarded);
                match reader.read(&mut buf[..n]) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => discarded += n,
                }
            }

            // Phase 3: send audio via GroovyConnection
            while running.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        conn.lock().unwrap().audio(&buf[..n]);
                    }
                }
            }
        })?)
}

// ── Subtitle extraction helpers ──

/// Extract subtitles for Plex media (uses absolute stream index).
fn extract_plex_subs(
    ffmpeg_path: &str,
    url: &str,
    sub_index: Option<u32>,
    sub_codec: Option<&str>,
    seek_secs: f64,
) -> Result<(Option<tempfile::NamedTempFile>, Option<String>)> {
    let Some(sub_idx) = sub_index else {
        return Ok((None, None));
    };
    let codec = sub_codec.unwrap_or("srt");
    let ext = match codec {
        "ass" | "ssa" => "ass",
        _ => "srt",
    };
    let sub_map = format!("0:{}", sub_idx);
    match ffmpeg::extract_subs(ffmpeg_path, url, &sub_map, ext, seek_secs)? {
        Some((path, tmp)) => {
            eprintln!("Subs: {}", path);
            Ok((Some(tmp), Some(path)))
        }
        None => {
            eprintln!("Sub extraction failed, continuing without");
            Ok((None, None))
        }
    }
}

/// Extract subtitles for local file (uses relative stream index or "none").
fn extract_file_subs(
    ffmpeg_path: &str,
    path: &str,
    sub_option: Option<&str>,
    seek_secs: f64,
) -> Result<(Option<tempfile::NamedTempFile>, Option<String>)> {
    let disabled = sub_option
        .map(|s| s.eq_ignore_ascii_case("none") || s == "off")
        .unwrap_or(false);
    if disabled {
        return Ok((None, None));
    }

    let sub_idx = if let Some(s) = sub_option {
        if let Ok(n) = s.parse::<u32>() {
            Some(n)
        } else {
            None
        }
    } else {
        Some(0)
    };

    let Some(idx) = sub_idx else {
        return Ok((None, None));
    };

    let sub_map = format!("0:s:{}", idx);
    match ffmpeg::extract_subs(ffmpeg_path, path, &sub_map, "ass", seek_secs)? {
        Some((p, tmp)) => {
            eprintln!("Subs: track {} -> {}", idx, p);
            Ok((Some(tmp), Some(p)))
        }
        None => {
            eprintln!("No subtitle track {}, continuing without", idx);
            Ok((None, None))
        }
    }
}

// ── Helpers ──

fn ctrlc_handler(running: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        eprintln!("\nStopping...");
        running.store(false, Ordering::Relaxed);
    })
    .ok();
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_plex_subs_none() {
        let (tmp, path) = extract_plex_subs("ffmpeg", "http://url", None, None, 0.0).unwrap();
        assert!(tmp.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn test_extract_file_subs_disabled_none() {
        let (tmp, path) = extract_file_subs("ffmpeg", "/fake", Some("none"), 0.0).unwrap();
        assert!(tmp.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn test_extract_file_subs_disabled_off() {
        let (tmp, path) = extract_file_subs("ffmpeg", "/fake", Some("off"), 0.0).unwrap();
        assert!(tmp.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn test_extract_file_subs_non_numeric() {
        // Non-numeric, non-"none" string → no subs
        let (tmp, path) = extract_file_subs("ffmpeg", "/fake", Some("english"), 0.0).unwrap();
        assert!(tmp.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn test_stream_file_not_found() {
        let modeline = crate::groovy::MODELINES
            .iter()
            .find(|m| m.name == "320x240 NTSC")
            .unwrap();
        let result = stream_file(
            "/nonexistent/video.mkv",
            "127.0.0.1",
            modeline,
            1.0,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("File not found"));
    }
}
