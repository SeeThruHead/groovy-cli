use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Stdio, Child};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::groovy::Modeline;

pub struct FfmpegPipeline {
    pub video_proc: Child,
    pub audio_proc: Child,
    pub latest_frame: Arc<Mutex<Option<Vec<u8>>>>,
    pub first_frame: Arc<AtomicBool>,
    pub video_ended: Arc<AtomicBool>,
    pub audio_stdout: Option<std::process::ChildStdout>,
}

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

pub fn find_ffmpeg() -> Result<String> {
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

pub fn extract_subs(ffmpeg: &str, url: &str, sub_idx: u32, seek_secs: f64) -> Result<Option<(String, tempfile::NamedTempFile)>> {
    let tmp = tempfile::Builder::new().prefix("groovy-sub-").suffix(".ass")
        .tempfile().context("temp sub file")?;
    let mut args: Vec<String> = vec![];
    if seek_secs > 0.0 { args.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    args.extend(["-i".into(), url.into(), "-map".into(), format!("0:{}", sub_idx),
        "-y".into(), "-v".into(), "error".into(), tmp.path().to_string_lossy().into()]);
    let r = Command::new(ffmpeg).args(&args).output()?;
    if r.status.success() {
        let p = tmp.path().to_string_lossy().to_string();
        Ok(Some((p, tmp)))
    } else {
        Ok(None)
    }
}

pub fn build_video_args(p: &VideoParams) -> Vec<String> {
    let mut args: Vec<String> = vec!["-re".into()];
    if p.seek_secs > 0.0 { args.extend(["-ss".into(), format!("{:.3}", p.seek_secs)]); }
    args.extend(["-i".into(), p.url.clone()]);

    let vid_w = ((p.w as f64 * p.scale) as usize) & !1;
    let vid_h = ((p.ffmpeg_h as f64 * p.scale) as usize) & !1;
    let pad_x = (p.w - vid_w) / 2;
    let pad_y = (p.ffmpeg_h - vid_h) / 2;

    if p.scale < 1.0 {
        eprintln!("Scale {:.0}%: {}x{} padded to {}x{}", p.scale * 100.0, vid_w, vid_h, p.w, p.ffmpeg_h);
    }

    if let Some(ref sp) = p.sub_path {
        args.extend([
            "-filter_complex".into(),
            format!("[0:v:0]scale={}:{}[v];[v]subtitles=filename={}:original_size={}x{}[s];[s]pad={}:{}:{}:{}:black[out]",
                vid_w, vid_h, sp, vid_w, vid_h, p.w, p.ffmpeg_h, pad_x, pad_y),
            "-map".into(), "[out]".into(),
        ]);
    } else if p.scale < 1.0 {
        args.extend([
            "-filter_complex".into(),
            format!("[0:v:0]scale={}:{}[v];[v]pad={}:{}:{}:{}:black[out]", vid_w, vid_h, p.w, p.ffmpeg_h, pad_x, pad_y),
            "-map".into(), "[out]".into(),
        ]);
    } else {
        args.extend(["-map".into(), "0:v:0".into(), "-s".into(), format!("{}x{}", p.w, p.ffmpeg_h)]);
    }

    args.extend(["-f".into(), "rawvideo".into(), "-pix_fmt".into(), "bgr24".into(),
        "-r".into(), format!("{:.4}", p.ffmpeg_fps), "-vsync".into(), "cfr".into(),
        "-v".into(), "error".into(), "-nostdin".into(), "pipe:1".into()]);
    args
}

pub fn build_audio_args(url: &str, audio_map: &str, seek_secs: f64) -> Vec<String> {
    let mut args: Vec<String> = vec!["-re".into()];
    if seek_secs > 0.0 { args.extend(["-ss".into(), format!("{:.3}", seek_secs)]); }
    args.extend(["-i", url, "-map", audio_map, "-ac", "2", "-ar", "48000",
        "-f", "s16le", "-acodec", "pcm_s16le", "-v", "warning", "-nostdin", "pipe:1",
    ].iter().map(|s| s.to_string()));
    args
}

/// Start FFmpeg pipelines, wait for first frame, return pipeline handles
pub fn start(ffmpeg: &str, vparams: &VideoParams, running: Arc<AtomicBool>) -> Result<FfmpegPipeline> {
    let vargs = build_video_args(vparams);
    let aargs = build_audio_args(&vparams.url, &vparams.audio_map, vparams.seek_secs);

    eprintln!("Starting FFmpeg...");
    let mut video_proc = Command::new(ffmpeg).args(&vargs)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("FFmpeg video")?;
    let mut audio_proc = Command::new(ffmpeg).args(&aargs)
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("FFmpeg audio")?;

    let video_stdout = video_proc.stdout.take().unwrap();
    let audio_stdout = audio_proc.stdout.take().unwrap();

    let frame_size = vparams.w * vparams.ffmpeg_h * 3;
    let latest_frame: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let first_frame = Arc::new(AtomicBool::new(false));
    let video_ended = Arc::new(AtomicBool::new(false));

    // Video reader thread
    {
        let latest = latest_frame.clone();
        let first = first_frame.clone();
        let ended = video_ended.clone();
        let running = running.clone();
        std::thread::Builder::new().name("video-reader".into()).spawn(move || {
            let mut reader = video_stdout;
            let mut buf = vec![0u8; frame_size];
            while running.load(Ordering::Relaxed) {
                let mut filled = 0;
                while filled < frame_size {
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

    // Wait for first frame
    eprintln!("Waiting for first frame ({} bytes)...", frame_size);
    loop {
        if first_frame.load(Ordering::Relaxed) { break; }
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

    Ok(FfmpegPipeline {
        video_proc,
        audio_proc,
        latest_frame,
        first_frame,
        video_ended,
        audio_stdout: Some(audio_stdout),
    })
}
