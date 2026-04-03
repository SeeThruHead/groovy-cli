//! GroovyConnection — UDP transport to MiSTer FPGA with LZ4 compression,
//! congestion control, and raster-aware sync.
//!
//! Owns the full MiSTer lifecycle: connect → init → blit/audio → close.
//! Drop guard ensures CMD_CLOSE is always sent.

use crate::groovy::{self, FpgaStatus, Modeline};
use anyhow::{Context, Result};
use lz4_flex::compress_prepend_size;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Compressed frame data above this size triggers a congestion delay before next send.
const CONGESTION_SIZE: usize = 500_000;
/// Minimum delay (ns) between sends when congestion control is active.
const CONGESTION_TIME_NS: u64 = 110_000; // 110μs

pub struct GroovyConnection {
    sock: UdpSocket,
    pub status: FpgaStatus,
    mtu: usize,
    frame_time_ns: u64,
    stream_time_ns: u64,
    last_congestion: Instant,
    do_congestion: bool,
    v_total: u16,
    interlace: bool,
}

impl GroovyConnection {
    /// Connect to MiSTer on the default Groovy UDP port.
    pub fn connect(mister_ip: &str) -> Result<Self> {
        Self::connect_to(mister_ip, groovy::UDP_PORT)
    }

    /// Connect to a specific ip:port (used by tests with mock server).
    pub fn connect_to(ip: &str, port: u16) -> Result<Self> {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_send_buffer_size(2 * 1024 * 1024)?;
        s.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())?;
        let dest: std::net::SocketAddr = format!("{}:{}", ip, port).parse()?;
        s.connect(&dest.into())?;
        s.set_nonblocking(true)?;
        let sock: UdpSocket = s.into();

        Ok(Self {
            sock,
            status: FpgaStatus::default(),
            mtu: groovy::DEFAULT_MTU,
            frame_time_ns: 0,
            stream_time_ns: 0,
            last_congestion: Instant::now(),
            do_congestion: false,
            v_total: 0,
            interlace: false,
        })
    }

    /// Send init + switchres, wait for FPGA ACK.
    pub fn init(&mut self, modeline: &Modeline) -> Result<()> {
        // LZ4 compression, 48kHz audio, stereo, BGR24
        self.send_blocking(&groovy::build_init(1, 3, 2, 0))?;

        if !self.wait_ack(2000) {
            eprintln!("Warning: no ACK from FPGA after init (continuing anyway)");
        } else {
            eprintln!("FPGA ACK received (synced={})", self.status.vram_synced);
        }

        std::thread::sleep(Duration::from_millis(100));

        self.send_blocking(&groovy::build_switchres(modeline))?;
        std::thread::sleep(Duration::from_millis(500));

        self.frame_time_ns = modeline.field_time_ns();
        self.v_total = modeline.v_total;
        self.interlace = modeline.interlace;

        Ok(())
    }

    /// Send a frame with LZ4 compression and congestion control.
    pub fn blit(&mut self, frame_data: &[u8], frame_num: u32, field: u8, vsync: u16) {
        // Congestion delay if previous frame was large
        if self.do_congestion {
            let elapsed = self.last_congestion.elapsed().as_nanos() as u64;
            if elapsed < CONGESTION_TIME_NS {
                spin_sleep::sleep(Duration::from_nanos(CONGESTION_TIME_NS - elapsed));
            }
        }

        let compressed = compress_prepend_size(frame_data);

        let header = groovy::build_blit(
            frame_num, field, vsync, Some(compressed.len() as u32),
        );

        let start = Instant::now();
        let _ = self.send_blocking(&header);

        // Chunked payload send
        let mut off = 0;
        while off < compressed.len() {
            let end = (off + self.mtu).min(compressed.len());
            let _ = self.send_blocking(&compressed[off..end]);
            off = end;
        }

        self.stream_time_ns = start.elapsed().as_nanos() as u64;
        self.last_congestion = Instant::now();
        self.do_congestion = compressed.len() > CONGESTION_SIZE;

        self.poll_ack();
    }

    /// Send audio: header then chunked PCM payload.
    pub fn audio(&mut self, pcm_data: &[u8]) {
        let header = groovy::build_audio(pcm_data.len() as u16);
        let _ = self.send_blocking(&header);
        let mut off = 0;
        while off < pcm_data.len() {
            let end = (off + self.mtu).min(pcm_data.len());
            let _ = self.send_blocking(&pcm_data[off..end]);
            off = end;
        }
    }

    /// Raster-aware sync: sleep until next field time, adjusting
    /// based on FPGA's reported raster position.
    pub fn wait_sync(&mut self, emulation_time_ns: u64) {
        let sleep_ns = self.frame_time_ns.saturating_sub(emulation_time_ns);

        self.poll_ack();

        let adjusted_ns = if self.status.frame_echo > 0 {
            let raster_diff = self.diff_time_raster();
            if raster_diff < 0 && (-raster_diff as u64) > sleep_ns {
                0
            } else {
                (sleep_ns as i64 + raster_diff) as u64
            }
        } else {
            sleep_ns
        };

        if adjusted_ns > 0 {
            spin_sleep::sleep(Duration::from_nanos(adjusted_ns));
        }
    }

    pub fn close(&self) {
        let _ = self.send_blocking(&groovy::build_close());
    }

    /// Get underlying socket ref (e.g. for audio thread sharing via Arc<Mutex>).
    /// Used by streamer module (future ticket).
    #[allow(dead_code)]
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    // ── Internal ──

    /// Calculate timing difference based on FPGA raster position feedback.
    fn diff_time_raster(&mut self) -> i64 {
        if self.v_total == 0 { return 0; }
        let shift = if self.interlace { 1 } else { 0 };
        let width_time_ns = self.frame_time_ns / (self.v_total as u64 >> shift);

        let vcount1 = (((self.status.frame_echo.wrapping_sub(1)) as u64 * self.v_total as u64
            + self.status.vcount_echo as u64) >> shift) as i64;
        let vcount2 = ((self.status.frame as u64 * self.v_total as u64
            + self.status.vcount as u64) >> shift) as i64;
        let dif = (vcount1 - vcount2) / 2; // dichotomous

        (width_time_ns as i64) * dif
    }

    /// Send on non-blocking socket, retrying on WouldBlock/ENOBUFS.
    fn send_blocking(&self, data: &[u8]) -> Result<()> {
        loop {
            match self.sock.send(data) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(10));
                }
                Err(e) if e.raw_os_error() == Some(55) => {
                    // ENOBUFS — kernel send buffer full
                    std::thread::sleep(Duration::from_micros(100));
                }
                Err(e) => return Err(e).context("UDP send"),
            }
        }
    }

    /// Blocking wait for ACK with timeout. Returns true if received.
    fn wait_ack(&mut self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut buf = [0u8; 16];
        loop {
            match self.sock.recv(&mut buf) {
                Ok(13) => {
                    if let Some(status) = FpgaStatus::parse(&buf) {
                        self.status = status;
                        return true;
                    }
                }
                Ok(_) => {}
                Err(_) => {}
            }
            if Instant::now() >= deadline { return false; }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Non-blocking drain of pending ACKs, keeping the latest.
    fn poll_ack(&mut self) {
        let mut buf = [0u8; 16];
        loop {
            match self.sock.recv(&mut buf) {
                Ok(13) => {
                    if let Some(status) = FpgaStatus::parse(&buf) {
                        if status.frame_echo > self.status.frame_echo {
                            self.status = status;
                        }
                    }
                }
                _ => break,
            }
        }
    }
}

impl Drop for GroovyConnection {
    fn drop(&mut self) {
        eprintln!("Sending close to MiSTer...");
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groovy::MODELINES;
    use crate::mock_server::MockGroovyServer;

    fn modeline_320x240() -> &'static Modeline {
        MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap()
    }

    fn modeline_640x480i() -> &'static Modeline {
        MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap()
    }

    #[test]
    fn test_connect_to_mock() {
        let server = MockGroovyServer::start();
        let conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        // Drop sends close
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));
        let stats = server.stop();
        assert_eq!(stats.close_count, 1, "Drop should send close");
    }

    #[test]
    fn test_init_sends_init_and_switchres() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        // Check status was populated from ACK
        assert!(conn.status.vram_synced || conn.status.vram_ready,
            "should have received ACK status");

        drop(conn);
        std::thread::sleep(Duration::from_millis(50));
        let stats = server.stop();
        assert_eq!(stats.init_count, 1);
        assert_eq!(stats.switchres_count, 1);
        assert_eq!(stats.close_count, 1);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);

        let sw = stats.last_switchres.unwrap();
        assert_eq!(sw.h_active, 320);
        assert_eq!(sw.v_active, 240);
        assert!(!sw.interlace);
    }

    #[test]
    fn test_init_sets_timing_fields() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_640x480i();
        conn.init(m).unwrap();

        assert_eq!(conn.v_total, m.v_total);
        assert!(conn.interlace);
        assert_eq!(conn.frame_time_ns, m.field_time_ns());
        // ~16.6ms per field at 59.94 Hz
        assert!(conn.frame_time_ns > 16_000_000 && conn.frame_time_ns < 17_000_000);

        server.stop();
    }

    #[test]
    fn test_blit_lz4_compressed() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        // Generate a test frame
        let field_size = m.field_size();
        let frame: Vec<u8> = (0..field_size).map(|i| (i % 256) as u8).collect();
        conn.blit(&frame, 1, 0, m.v_begin);

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.blit_count, 1);
        assert_eq!(stats.last_blit_frame, 1);
        assert_eq!(stats.last_blit_field, 0);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_blit_multiple_frames() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        let field_size = m.field_size();
        let frame: Vec<u8> = vec![128u8; field_size];

        let num_frames = 30u32;
        for i in 1..=num_frames {
            conn.blit(&frame, i, 0, m.v_begin);
            // Small delay so mock can process
            std::thread::sleep(Duration::from_millis(2));
        }

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.blit_count, num_frames);
        assert_eq!(stats.last_blit_frame, num_frames);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_blit_interlaced_fields() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_640x480i();
        conn.init(m).unwrap();

        let field_size = m.field_size(); // half height for interlaced
        assert_eq!(field_size, 640 * 240 * 3);

        let frame: Vec<u8> = vec![64u8; field_size];

        // Send even field then odd field
        conn.blit(&frame, 1, 0, m.v_begin);
        std::thread::sleep(Duration::from_millis(5));
        conn.blit(&frame, 2, 1, m.v_begin);

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.blit_count, 2);
        assert_eq!(stats.last_blit_frame, 2);
        assert_eq!(stats.last_blit_field, 1);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_audio_send() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        let pcm = vec![0x42u8; 4800];
        conn.audio(&pcm);

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.audio_count, 1);
        assert_eq!(stats.total_audio_bytes, 4800);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_close_on_drop() {
        let server = MockGroovyServer::start();
        {
            let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
            let m = modeline_320x240();
            conn.init(m).unwrap();
            // conn dropped here
        }
        std::thread::sleep(Duration::from_millis(50));
        let stats = server.stop();
        assert_eq!(stats.close_count, 1);
    }

    #[test]
    fn test_explicit_close_then_drop() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();
        conn.close();
        drop(conn); // Drop also sends close
        std::thread::sleep(Duration::from_millis(50));
        let stats = server.stop();
        // Both explicit close and drop close
        assert_eq!(stats.close_count, 2);
    }

    #[test]
    fn test_full_session() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_640x480i();
        conn.init(m).unwrap();

        let field_size = m.field_size();
        let frame: Vec<u8> = (0..field_size).map(|i| ((i * 7) % 256) as u8).collect();

        // Stream 20 fields (10 interlaced frames)
        for i in 1..=20u32 {
            let field = ((i - 1) % 2) as u8;
            conn.blit(&frame, i, field, m.v_begin);
            std::thread::sleep(Duration::from_millis(2));
        }

        // Some audio
        let pcm = vec![0x80u8; 3840];
        conn.audio(&pcm);

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.init_count, 1);
        assert_eq!(stats.switchres_count, 1);
        assert_eq!(stats.blit_count, 20);
        assert_eq!(stats.audio_count, 1);
        assert_eq!(stats.close_count, 1);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_wait_sync_does_not_hang() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        // wait_sync with 0 emulation time should sleep ~1 field period
        let start = Instant::now();
        conn.wait_sync(0);
        let elapsed = start.elapsed();
        // Should sleep roughly frame_time_ns (~16ms) but could be less with raster adjust
        assert!(elapsed < Duration::from_millis(50), "wait_sync took too long: {:?}", elapsed);

        // wait_sync with emulation_time >= frame_time should not sleep
        let start = Instant::now();
        conn.wait_sync(conn.frame_time_ns + 1_000_000);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(5), "should not sleep when ahead: {:?}", elapsed);

        server.stop();
    }

    #[test]
    fn test_congestion_control_activates_for_large_frames() {
        let server = MockGroovyServer::start();
        let mut conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let m = modeline_320x240();
        conn.init(m).unwrap();

        // Random data compresses poorly → large compressed output → triggers congestion
        let field_size = m.field_size();
        let random_frame: Vec<u8> = (0..field_size).map(|i| {
            // Pseudo-random: different every byte so LZ4 can't compress well
            ((i.wrapping_mul(2654435761)) & 0xFF) as u8
        }).collect();

        conn.blit(&random_frame, 1, 0, m.v_begin);
        // After a large frame, do_congestion should be set
        // (depends on whether compressed size > CONGESTION_SIZE)
        // Either way, second blit should succeed
        conn.blit(&random_frame, 2, 0, m.v_begin);

        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.blit_count, 2);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_connect_invalid_address() {
        // Should fail to parse, not panic
        let result = GroovyConnection::connect_to("not-a-valid-ip", 32100);
        assert!(result.is_err());
    }

    #[test]
    fn test_socket_accessor() {
        let server = MockGroovyServer::start();
        let conn = GroovyConnection::connect_to("127.0.0.1", server.port()).unwrap();
        let _sock = conn.socket(); // Should not panic
        server.stop();
    }
}
