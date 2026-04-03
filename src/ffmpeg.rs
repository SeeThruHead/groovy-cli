use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Parameters for building FFmpeg video args (pure data, no I/O).
pub struct VideoParams {
    pub url: String,
    pub seek_secs: f64,
    pub sub_path: Option<String>,
    pub audio_map: String,
    pub w: usize,
    pub ffmpeg_h: usize,
    pub ffmpeg_fps: f64,
    pub scale: f64,
}

/// Running FFmpeg pipeline with latest-frame model.
pub struct FfmpegPipeline {
    pub video_proc: Child,
    pub audio_proc: Child,
    pub latest_frame: Arc<Mutex<Option<Vec<u8>>>>,
    pub first_frame: Arc<AtomicBool>,
    pub video_ended: Arc<AtomicBool>,
    pub audio_stdout: Option<std::process::ChildStdout>,
}

// ── Pure functions ──

/// Build FFmpeg video args from params. Pure — no I/O, fully testable.
pub fn build_video_args(p: &VideoParams) -> Vec<String> {
    let mut args: Vec<String> = vec!["-re".into()];
    if p.seek_secs > 0.0 {
        args.extend(["-ss".into(), format!("{:.3}", p.seek_secs)]);
    }
    args.extend(["-i".into(), p.url.clone()]);

    let vid_w = ((p.w as f64 * p.scale) as usize) & !1;
    let vid_h = ((p.ffmpeg_h as f64 * p.scale) as usize) & !1;
    let pad_x = (p.w - vid_w) / 2;
    let pad_y = (p.ffmpeg_h - vid_h) / 2;

    if p.scale < 1.0 {
        eprintln!(
            "Scale {:.0}%: {}x{} padded to {}x{}",
            p.scale * 100.0,
            vid_w,
            vid_h,
            p.w,
            p.ffmpeg_h
        );
    }

    if let Some(ref sp) = p.sub_path {
        args.extend([
            "-filter_complex".into(),
            format!(
                "[0:v:0]scale={}:{}[v];[v]subtitles=filename={}:original_size={}x{}[s];[s]pad={}:{}:{}:{}:black[out]",
                vid_w, vid_h, sp, vid_w, vid_h, p.w, p.ffmpeg_h, pad_x, pad_y
            ),
            "-map".into(),
            "[out]".into(),
        ]);
    } else if p.scale < 1.0 {
        args.extend([
            "-filter_complex".into(),
            format!(
                "[0:v:0]scale={}:{}[v];[v]pad={}:{}:{}:{}:black[out]",
                vid_w, vid_h, p.w, p.ffmpeg_h, pad_x, pad_y
            ),
            "-map".into(),
            "[out]".into(),
        ]);
    } else {
        args.extend([
            "-map".into(),
            "0:v:0".into(),
            "-s".into(),
            format!("{}x{}", p.w, p.ffmpeg_h),
        ]);
    }

    args.extend([
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgr24".into(),
        "-r".into(),
        format!("{:.4}", p.ffmpeg_fps),
        "-vsync".into(),
        "cfr".into(),
        "-v".into(),
        "error".into(),
        "-nostdin".into(),
        "pipe:1".into(),
    ]);
    args
}

/// Build FFmpeg audio args. Pure — no I/O, fully testable.
pub fn build_audio_args(url: &str, audio_map: &str, seek_secs: f64) -> Vec<String> {
    let mut args: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 {
        args.extend(["-ss".into(), format!("{:.3}", seek_secs)]);
    }
    args.extend(
        [
            "-i",
            url,
            "-map",
            audio_map,
            "-ac",
            "2",
            "-ar",
            "48000",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-v",
            "warning",
            "-nostdin",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

/// Build FFmpeg args for subtitle extraction. Pure.
pub fn build_sub_extract_args(
    url: &str,
    sub_map: &str,
    seek_secs: f64,
    output_path: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![];
    if seek_secs > 0.0 {
        args.extend(["-ss".into(), format!("{:.3}", seek_secs)]);
    }
    args.extend([
        "-i".into(),
        url.into(),
        "-map".into(),
        sub_map.into(),
        "-y".into(),
        "-v".into(),
        "error".into(),
        output_path.into(),
    ]);
    args
}

// ── I/O functions ──

/// Find ffmpeg binary on the system.
pub fn find_ffmpeg() -> Result<String> {
    if let Ok(o) = Command::new("which").arg("ffmpeg").output() {
        if o.status.success() {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() {
                return Ok(p);
            }
        }
    }
    for p in [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "/usr/bin/ffmpeg",
    ] {
        if std::path::Path::new(p).exists() {
            return Ok(p.to_string());
        }
    }
    bail!("FFmpeg not found. Install with: brew install ffmpeg")
}

/// Extract subtitles to a temp file. Returns (path, handle) on success.
/// `sub_map` is the FFmpeg stream map, e.g. "0:3" (absolute) or "0:s:0" (relative).
/// `ext` is the output extension, e.g. "ass" or "srt".
pub fn extract_subs(
    ffmpeg: &str,
    url: &str,
    sub_map: &str,
    ext: &str,
    seek_secs: f64,
) -> Result<Option<(String, tempfile::NamedTempFile)>> {
    let tmp = tempfile::Builder::new()
        .prefix("groovy-sub-")
        .suffix(&format!(".{}", ext))
        .tempfile()
        .context("temp sub file")?;
    let out_path = tmp.path().to_string_lossy().to_string();
    let args = build_sub_extract_args(url, sub_map, seek_secs, &out_path);
    let r = Command::new(ffmpeg).args(&args).output()?;
    if r.status.success() {
        Ok(Some((out_path, tmp)))
    } else {
        Ok(None)
    }
}

/// Start FFmpeg video+audio pipelines, wait for first frame, return handles.
pub fn start(ffmpeg: &str, vparams: &VideoParams, running: Arc<AtomicBool>) -> Result<FfmpegPipeline> {
    let vargs = build_video_args(vparams);
    let aargs = build_audio_args(&vparams.url, &vparams.audio_map, vparams.seek_secs);

    eprintln!("Starting FFmpeg...");
    let mut video_proc = Command::new(ffmpeg)
        .args(&vargs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("FFmpeg video")?;
    let mut audio_proc = Command::new(ffmpeg)
        .args(&aargs)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("FFmpeg audio")?;

    let video_stdout = video_proc.stdout.take().unwrap();
    let audio_stdout = audio_proc.stdout.take().unwrap();

    let frame_size = vparams.w * vparams.ffmpeg_h * 3;
    let latest_frame: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let first_frame = Arc::new(AtomicBool::new(false));
    let video_ended = Arc::new(AtomicBool::new(false));

    // Video reader thread — reads full frames, stores latest
    {
        let latest = latest_frame.clone();
        let first = first_frame.clone();
        let ended = video_ended.clone();
        let running = running.clone();
        std::thread::Builder::new()
            .name("video-reader".into())
            .spawn(move || {
                let mut reader = video_stdout;
                let mut buf = vec![0u8; frame_size];
                while running.load(Ordering::Relaxed) {
                    let mut filled = 0;
                    while filled < frame_size {
                        match reader.read(&mut buf[filled..]) {
                            Ok(0) | Err(_) => {
                                ended.store(true, Ordering::Relaxed);
                                return;
                            }
                            Ok(n) => filled += n,
                        }
                    }
                    *latest.lock().unwrap() = Some(buf.clone());
                    first.store(true, Ordering::Relaxed);
                }
            })?;
    }

    // Wait for first frame
    eprintln!("Waiting for first frame ({} bytes)...", frame_size);
    loop {
        if first_frame.load(Ordering::Relaxed) {
            break;
        }
        if video_ended.load(Ordering::Relaxed) {
            let _ = video_proc.wait();
            let mut err = String::new();
            if let Some(mut se) = video_proc.stderr.take() {
                let _ = se.read_to_string(&mut err);
            }
            let _ = audio_proc.kill();
            bail!("FFmpeg no video. Stderr:\n{}", err);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("Got first frame.");

    Ok(FfmpegPipeline {
        video_proc,
        audio_proc,
        latest_frame,
        first_frame,
        video_ended,
        audio_stdout: Some(audio_stdout),
    })
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn base_params() -> VideoParams {
        VideoParams {
            url: "http://plex:32400/video.mkv".into(),
            seek_secs: 0.0,
            sub_path: None,
            audio_map: "0:a:0".into(),
            w: 640,
            ffmpeg_h: 240,
            ffmpeg_fps: 59.9400,
            scale: 1.0,
        }
    }

    #[test]
    fn test_video_args_basic() {
        let p = base_params();
        let args = build_video_args(&p);

        assert_eq!(args[0], "-re");
        // No seek
        assert_eq!(args[1], "-i");
        assert_eq!(args[2], "http://plex:32400/video.mkv");
        // Simple scale (no filter_complex)
        assert!(args.contains(&"-map".to_string()));
        assert!(args.contains(&"0:v:0".to_string()));
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"640x240".to_string()));
        // Output format
        assert!(args.contains(&"rawvideo".to_string()));
        assert!(args.contains(&"bgr24".to_string()));
        assert!(args.contains(&"pipe:1".to_string()));
        assert!(args.contains(&"cfr".to_string()));
    }

    #[test]
    fn test_video_args_with_seek() {
        let p = VideoParams {
            seek_secs: 123.456,
            ..base_params()
        };
        let args = build_video_args(&p);
        assert_eq!(args[1], "-ss");
        assert_eq!(args[2], "123.456");
        assert_eq!(args[3], "-i");
    }

    #[test]
    fn test_video_args_with_subtitles() {
        let p = VideoParams {
            sub_path: Some("/tmp/groovy-sub-abc.ass".into()),
            ..base_params()
        };
        let args = build_video_args(&p);
        assert!(args.contains(&"-filter_complex".to_string()));
        let fc = args.iter().find(|a| a.contains("subtitles=")).unwrap();
        assert!(fc.contains("filename=/tmp/groovy-sub-abc.ass"));
        assert!(fc.contains("original_size=640x240"));
        assert!(args.contains(&"[out]".to_string()));
    }

    #[test]
    fn test_video_args_with_scale() {
        let p = VideoParams {
            scale: 0.9,
            ..base_params()
        };
        let args = build_video_args(&p);
        assert!(args.contains(&"-filter_complex".to_string()));
        let fc = args.iter().find(|a| a.contains("scale=")).unwrap();
        // 640 * 0.9 = 576, 240 * 0.9 = 216
        assert!(fc.contains("scale=576:216"));
        assert!(fc.contains("pad=640:240:32:12:black"));
    }

    #[test]
    fn test_video_args_scale_with_subs() {
        let p = VideoParams {
            scale: 0.8,
            sub_path: Some("/tmp/subs.ass".into()),
            ..base_params()
        };
        let args = build_video_args(&p);
        let fc = args.iter().find(|a| a.contains("subtitles=")).unwrap();
        // 640 * 0.8 = 512, 240 * 0.8 = 192
        assert!(fc.contains("scale=512:192"));
        assert!(fc.contains("subtitles=filename=/tmp/subs.ass:original_size=512x192"));
        assert!(fc.contains("pad=640:240:64:24:black"));
    }

    #[test]
    fn test_video_args_interlaced_dimensions() {
        // Interlaced: ffmpeg_h should be half of v_active (caller computes this)
        let p = VideoParams {
            w: 640,
            ffmpeg_h: 120, // 240i → 120 per field
            ffmpeg_fps: 59.94,
            ..base_params()
        };
        let args = build_video_args(&p);
        assert!(args.contains(&"640x120".to_string()));
    }

    #[test]
    fn test_video_args_fps_format() {
        let p = VideoParams {
            ffmpeg_fps: 59.9400,
            ..base_params()
        };
        let args = build_video_args(&p);
        let fps = args.iter().find(|a| a.starts_with("59.")).unwrap();
        assert_eq!(fps, "59.9400");
    }

    #[test]
    fn test_audio_args_basic() {
        let args = build_audio_args("http://plex:32400/video.mkv", "0:a:0", 0.0);
        assert_eq!(args[0], "-re");
        assert_eq!(args[1], "-i");
        assert_eq!(args[2], "http://plex:32400/video.mkv");
        assert!(args.contains(&"0:a:0".to_string()));
        assert!(args.contains(&"48000".to_string()));
        assert!(args.contains(&"s16le".to_string()));
        assert!(args.contains(&"pcm_s16le".to_string()));
        assert!(args.contains(&"pipe:1".to_string()));
    }

    #[test]
    fn test_audio_args_with_seek() {
        let args = build_audio_args("http://url", "0:1", 45.0);
        assert_eq!(args[1], "-ss");
        assert_eq!(args[2], "45.000");
        assert_eq!(args[3], "-i");
    }

    #[test]
    fn test_audio_args_no_seek() {
        let args = build_audio_args("http://url", "0:a:0", 0.0);
        // Should go straight to -i (no -ss)
        assert_eq!(args[1], "-i");
    }

    #[test]
    fn test_audio_args_specific_stream() {
        let args = build_audio_args("http://url", "0:3", 0.0);
        assert!(args.contains(&"0:3".to_string()));
    }

    #[test]
    fn test_sub_extract_args_basic() {
        let args = build_sub_extract_args("http://url", "0:5", 0.0, "/tmp/out.ass");
        assert_eq!(args, vec!["-i", "http://url", "-map", "0:5", "-y", "-v", "error", "/tmp/out.ass"]);
    }

    #[test]
    fn test_sub_extract_args_with_seek() {
        let args = build_sub_extract_args("http://url", "0:s:0", 30.0, "/tmp/out.srt");
        assert_eq!(args[0], "-ss");
        assert_eq!(args[1], "30.000");
        assert_eq!(args[2], "-i");
    }

    #[test]
    fn test_sub_extract_args_relative_map() {
        let args = build_sub_extract_args("/path/to/file.mkv", "0:s:1", 0.0, "/tmp/out.ass");
        assert!(args.contains(&"0:s:1".to_string()));
    }

    #[test]
    fn test_scale_even_alignment() {
        // Scale values should produce even dimensions (& !1 masks)
        let p = VideoParams {
            w: 641,       // odd width
            ffmpeg_h: 241, // odd height
            scale: 0.9,
            ..base_params()
        };
        let args = build_video_args(&p);
        let fc = args.iter().find(|a| a.contains("scale=")).unwrap();
        // 641 * 0.9 = 576.9 → 576 (even), 241 * 0.9 = 216.9 → 216 (even)
        assert!(fc.contains("scale=576:216"));
    }
}
