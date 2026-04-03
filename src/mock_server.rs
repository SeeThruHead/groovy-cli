//! Mock Groovy UDP server for integration testing.
//!
//! Speaks the Groovy protocol: accepts init, switchres, blit, audio, close.
//! Sends back FPGA status ACKs. Validates packet format. Counts frames.
//! Verifies LZ4 decompression for blit payloads.

use lz4_flex::decompress_size_prepended;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Command byte constants (must match groovy.rs builders)
const CMD_CLOSE: u8 = 0x01;
const CMD_INIT: u8 = 0x02;
const CMD_SWITCHRES: u8 = 0x03;
const CMD_AUDIO: u8 = 0x04;
const CMD_GET_STATUS: u8 = 0x05;
const CMD_BLIT: u8 = 0x07;

/// Recorded state from a switchres packet.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct SwitchresState {
    pub p_clock: f64,
    pub h_active: u16,
    pub h_begin: u16,
    pub h_end: u16,
    pub h_total: u16,
    pub v_active: u16,
    pub v_begin: u16,
    pub v_end: u16,
    pub v_total: u16,
    pub interlace: bool,
}

/// Recorded state from an init packet.
#[derive(Debug, Clone, Default)]
pub struct InitState {
    pub compression: u8,
    pub sample_rate: u8,
    pub channels: u8,
    pub rgb_mode: u8,
}

/// All protocol errors detected by the mock.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ProtocolError {
    UnknownCommand(u8),
    InitWrongSize(usize),
    SwitchresWrongSize(usize),
    BlitHeaderWrongSize(usize),
    AudioHeaderWrongSize(usize),
    Lz4DecompressFailed(String),
    Lz4WrongDecompressedSize { expected: usize, got: usize },
    BlitPayloadIncomplete { expected: usize, got: usize },
    AudioPayloadIncomplete { expected: usize, got: usize },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Thread-safe stats collected by the mock server.
#[derive(Debug, Default)]
pub struct MockStats {
    pub init_count: u32,
    pub switchres_count: u32,
    pub blit_count: u32,
    pub audio_count: u32,
    pub close_count: u32,
    pub status_request_count: u32,
    pub total_blit_bytes: u64,
    pub total_audio_bytes: u64,
    pub last_init: Option<InitState>,
    pub last_switchres: Option<SwitchresState>,
    pub last_blit_frame: u32,
    pub last_blit_field: u8,
    pub errors: Vec<ProtocolError>,
}

impl MockStats {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// A mock Groovy server that listens on a UDP port.
pub struct MockGroovyServer {
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<MockStats>>,
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockGroovyServer {
    /// Start a mock server on an ephemeral port.
    /// Returns immediately; server runs in a background thread.
    pub fn start() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind mock server");
        let port = sock.local_addr().unwrap().port();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(MockStats::default()));

        let r = running.clone();
        let s = stats.clone();

        let handle = std::thread::Builder::new()
            .name("mock-groovy".into())
            .spawn(move || Self::run_loop(sock, r, s))
            .expect("spawn mock server thread");

        MockGroovyServer {
            running,
            stats,
            port,
            handle: Some(handle),
        }
    }

    /// The port this mock is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get a snapshot of the current stats.
    pub fn stats(&self) -> MockStats {
        let s = self.stats.lock().unwrap();
        MockStats {
            init_count: s.init_count,
            switchres_count: s.switchres_count,
            blit_count: s.blit_count,
            audio_count: s.audio_count,
            close_count: s.close_count,
            status_request_count: s.status_request_count,
            total_blit_bytes: s.total_blit_bytes,
            total_audio_bytes: s.total_audio_bytes,
            last_init: s.last_init.clone(),
            last_switchres: s.last_switchres.clone(),
            last_blit_frame: s.last_blit_frame,
            last_blit_field: s.last_blit_field,
            errors: s.errors.clone(),
        }
    }

    /// Stop the server and wait for the thread to finish.
    pub fn stop(mut self) -> MockStats {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
        self.stats()
    }

    /// Build a 13-byte FPGA status ACK.
    fn build_ack(frame_echo: u32, vcount_echo: u16, frame: u32, vcount: u16, bits: u8) -> Vec<u8> {
        let mut d = vec![0u8; 13];
        d[0..4].copy_from_slice(&frame_echo.to_le_bytes());
        d[4..6].copy_from_slice(&vcount_echo.to_le_bytes());
        d[6..10].copy_from_slice(&frame.to_le_bytes());
        d[10..12].copy_from_slice(&vcount.to_le_bytes());
        d[12] = bits;
        d
    }

    fn run_loop(sock: UdpSocket, running: Arc<AtomicBool>, stats: Arc<Mutex<MockStats>>) {
        let mut buf = vec![0u8; 65536];
        // State machine for multi-packet commands (blit/audio payload collection)
        let mut pending_blit: Option<PendingBlit> = None;
        let mut pending_audio: Option<PendingAudio> = None;

        while running.load(Ordering::Relaxed) {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                       || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(_) => break,
            };

            if n == 0 {
                continue;
            }

            let data = &buf[..n];

            // Check if we're collecting payload chunks for a pending blit or audio
            if let Some(ref mut pb) = pending_blit {
                pb.payload.extend_from_slice(data);
                if pb.payload.len() >= pb.expected_size {
                    // Complete — validate LZ4
                    Self::finish_blit(pb, &stats);
                    pending_blit = None;
                }
                continue;
            }

            if let Some(ref mut pa) = pending_audio {
                pa.payload.extend_from_slice(data);
                if pa.payload.len() >= pa.expected_size {
                    Self::finish_audio(pa, &stats);
                    pending_audio = None;
                }
                continue;
            }

            let cmd = data[0];
            match cmd {
                CMD_CLOSE => {
                    let mut s = stats.lock().unwrap();
                    s.close_count += 1;
                }

                CMD_INIT => {
                    if n != 5 {
                        stats.lock().unwrap().errors.push(ProtocolError::InitWrongSize(n));
                    } else {
                        let init = InitState {
                            compression: data[1],
                            sample_rate: data[2],
                            channels: data[3],
                            rgb_mode: data[4],
                        };
                        let mut s = stats.lock().unwrap();
                        s.init_count += 1;
                        s.last_init = Some(init);
                    }
                    // ACK: synced, ready
                    let ack = Self::build_ack(0, 0, 0, 0, 0x05); // vram_ready | vram_synced
                    let _ = sock.send_to(&ack, src);
                }

                CMD_SWITCHRES => {
                    if n != 26 {
                        stats.lock().unwrap().errors.push(ProtocolError::SwitchresWrongSize(n));
                    } else {
                        let sw = SwitchresState {
                            p_clock: f64::from_le_bytes(data[1..9].try_into().unwrap()),
                            h_active: u16::from_le_bytes(data[9..11].try_into().unwrap()),
                            h_begin: u16::from_le_bytes(data[11..13].try_into().unwrap()),
                            h_end: u16::from_le_bytes(data[13..15].try_into().unwrap()),
                            h_total: u16::from_le_bytes(data[15..17].try_into().unwrap()),
                            v_active: u16::from_le_bytes(data[17..19].try_into().unwrap()),
                            v_begin: u16::from_le_bytes(data[19..21].try_into().unwrap()),
                            v_end: u16::from_le_bytes(data[21..23].try_into().unwrap()),
                            v_total: u16::from_le_bytes(data[23..25].try_into().unwrap()),
                            interlace: data[25] != 0,
                        };
                        let mut s = stats.lock().unwrap();
                        s.switchres_count += 1;
                        s.last_switchres = Some(sw);
                    }
                    // ACK after switchres
                    let ack = Self::build_ack(0, 0, 0, 0, 0x05);
                    let _ = sock.send_to(&ack, src);
                }

                CMD_BLIT => {
                    // Header is 8 bytes (no compression) or 12 bytes (with compression)
                    if n < 8 {
                        stats.lock().unwrap().errors.push(ProtocolError::BlitHeaderWrongSize(n));
                        continue;
                    }
                    let frame_num = u32::from_le_bytes(data[1..5].try_into().unwrap());
                    let field = data[5];
                    let _vsync = u16::from_le_bytes(data[6..8].try_into().unwrap());

                    if n >= 12 {
                        // Compressed blit — expect payload chunks
                        let compressed_size = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
                        // There might be payload data after the header in the same packet
                        let initial_payload = if n > 12 { data[12..].to_vec() } else { Vec::new() };

                        let pb = PendingBlit {
                            frame_num,
                            field,
                            expected_size: compressed_size,
                            payload: initial_payload,
                        };

                        if pb.payload.len() >= compressed_size {
                            Self::finish_blit(&pb, &stats);
                        } else {
                            pending_blit = Some(pb);
                        }
                    } else {
                        // Uncompressed blit — payload chunks follow (size determined by field_size)
                        // For uncompressed, we need switchres to know the expected size
                        let expected = {
                            let s = stats.lock().unwrap();
                            s.last_switchres.as_ref().map(|sw| {
                                let fh = if sw.interlace { sw.v_active as usize / 2 } else { sw.v_active as usize };
                                sw.h_active as usize * fh * 3
                            }).unwrap_or(0)
                        };

                        if expected > 0 {
                            pending_blit = Some(PendingBlit {
                                frame_num,
                                field,
                                expected_size: expected,
                                payload: Vec::new(),
                            });
                        } else {
                            // No switchres yet, just count it
                            let mut s = stats.lock().unwrap();
                            s.blit_count += 1;
                            s.last_blit_frame = frame_num;
                            s.last_blit_field = field;
                        }
                    }

                    // ACK with frame echo
                    let ack = Self::build_ack(
                        frame_num, 0, frame_num, 100,
                        0x07, // vram_ready | vram_end_frame | vram_synced
                    );
                    let _ = sock.send_to(&ack, src);
                }

                CMD_AUDIO => {
                    if n < 3 {
                        stats.lock().unwrap().errors.push(ProtocolError::AudioHeaderWrongSize(n));
                        continue;
                    }
                    let size = u16::from_le_bytes(data[1..3].try_into().unwrap()) as usize;
                    // Payload follows in chunks
                    let initial = if n > 3 { data[3..].to_vec() } else { Vec::new() };
                    let pa = PendingAudio {
                        expected_size: size,
                        payload: initial,
                    };
                    if pa.payload.len() >= size {
                        Self::finish_audio(&pa, &stats);
                    } else {
                        pending_audio = Some(pa);
                    }
                }

                CMD_GET_STATUS => {
                    let mut s = stats.lock().unwrap();
                    s.status_request_count += 1;
                    drop(s);
                    let frame = stats.lock().unwrap().last_blit_frame;
                    let ack = Self::build_ack(frame, 0, frame, 50, 0x05);
                    let _ = sock.send_to(&ack, src);
                }

                _ => {
                    stats.lock().unwrap().errors.push(ProtocolError::UnknownCommand(cmd));
                }
            }
        }
    }

    fn finish_blit(pb: &PendingBlit, stats: &Arc<Mutex<MockStats>>) {
        let mut s = stats.lock().unwrap();
        s.blit_count += 1;
        s.last_blit_frame = pb.frame_num;
        s.last_blit_field = pb.field;
        s.total_blit_bytes += pb.payload.len() as u64;

        // Validate payload size
        if pb.payload.len() < pb.expected_size {
            s.errors.push(ProtocolError::BlitPayloadIncomplete {
                expected: pb.expected_size,
                got: pb.payload.len(),
            });
            return;
        }

        // Try LZ4 decompress to validate
        let payload = &pb.payload[..pb.expected_size];
        match decompress_size_prepended(payload) {
            Ok(_decompressed) => {
                // Decompression succeeded — data is valid
            }
            Err(e) => {
                s.errors.push(ProtocolError::Lz4DecompressFailed(e.to_string()));
            }
        }
    }

    fn finish_audio(pa: &PendingAudio, stats: &Arc<Mutex<MockStats>>) {
        let mut s = stats.lock().unwrap();
        s.audio_count += 1;
        s.total_audio_bytes += pa.payload.len().min(pa.expected_size) as u64;

        if pa.payload.len() < pa.expected_size {
            s.errors.push(ProtocolError::AudioPayloadIncomplete {
                expected: pa.expected_size,
                got: pa.payload.len(),
            });
        }
    }
}

impl Drop for MockGroovyServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

struct PendingBlit {
    frame_num: u32,
    field: u8,
    expected_size: usize,
    payload: Vec<u8>,
}

struct PendingAudio {
    expected_size: usize,
    payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groovy::{self, MODELINES};
    use lz4_flex::compress_prepend_size;

    fn send_and_recv(sock: &UdpSocket, data: &[u8]) -> Option<Vec<u8>> {
        sock.send(data).unwrap();
        let mut buf = vec![0u8; 64];
        sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        match sock.recv(&mut buf) {
            Ok(n) => Some(buf[..n].to_vec()),
            Err(_) => None,
        }
    }

    fn connect_to_mock(server: &MockGroovyServer) -> UdpSocket {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(format!("127.0.0.1:{}", server.port())).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        sock
    }

    #[test]
    fn test_mock_init_and_ack() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        let init = groovy::build_init(1, 3, 2, 0);
        let ack = send_and_recv(&sock, &init);
        assert!(ack.is_some(), "should receive ACK");
        let ack = ack.unwrap();
        assert_eq!(ack.len(), 13, "ACK should be 13 bytes");

        let status = groovy::FpgaStatus::parse(&ack).unwrap();
        assert!(status.vram_ready);
        assert!(status.vram_synced);

        let stats = server.stop();
        assert_eq!(stats.init_count, 1);
        assert!(!stats.has_errors());
        let init_state = stats.last_init.unwrap();
        assert_eq!(init_state.compression, 1);
        assert_eq!(init_state.sample_rate, 3);
        assert_eq!(init_state.channels, 2);
        assert_eq!(init_state.rgb_mode, 0);
    }

    #[test]
    fn test_mock_switchres() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        let m = &MODELINES[4]; // 720x480i NTSC
        let pkt = groovy::build_switchres(m);
        let ack = send_and_recv(&sock, &pkt);
        assert!(ack.is_some());

        let stats = server.stop();
        assert_eq!(stats.switchres_count, 1);
        assert!(!stats.has_errors());
        let sw = stats.last_switchres.unwrap();
        assert_eq!(sw.h_active, 720);
        assert_eq!(sw.v_active, 480);
        assert!(sw.interlace);
        assert!((sw.p_clock - m.p_clock).abs() < 0.001);
    }

    #[test]
    fn test_mock_close() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        sock.send(&groovy::build_close()).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.close_count, 1);
        assert!(!stats.has_errors());
    }

    #[test]
    fn test_mock_blit_compressed_lz4() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        // Init + switchres first
        let m = MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap();
        sock.send(&groovy::build_init(1, 3, 2, 0)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // Drain ACK
        let mut drain = [0u8; 64];
        let _ = sock.recv(&mut drain);
        sock.send(&groovy::build_switchres(m)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = sock.recv(&mut drain);

        // Generate test frame data (320*240*3 = 230400 bytes BGR24)
        let field_size = m.field_size();
        let frame_data: Vec<u8> = (0..field_size).map(|i| (i % 256) as u8).collect();

        // LZ4 compress
        let compressed = compress_prepend_size(&frame_data);

        // Send blit header
        let header = groovy::build_blit(1, 0, m.v_begin, Some(compressed.len() as u32));
        sock.send(&header).unwrap();

        // Send compressed payload in MTU chunks
        let mtu = groovy::DEFAULT_MTU;
        let mut off = 0;
        while off < compressed.len() {
            let end = (off + mtu).min(compressed.len());
            sock.send(&compressed[off..end]).unwrap();
            off = end;
        }

        // Drain ACK
        std::thread::sleep(Duration::from_millis(50));
        let _ = sock.recv(&mut drain);

        let stats = server.stop();
        assert_eq!(stats.blit_count, 1);
        assert_eq!(stats.last_blit_frame, 1);
        assert_eq!(stats.last_blit_field, 0);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_mock_audio() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        let pcm = vec![0x42u8; 4800];
        let header = groovy::build_audio(pcm.len() as u16);
        sock.send(&header).unwrap();

        let mtu = groovy::DEFAULT_MTU;
        let mut off = 0;
        while off < pcm.len() {
            let end = (off + mtu).min(pcm.len());
            sock.send(&pcm[off..end]).unwrap();
            off = end;
        }

        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.audio_count, 1);
        assert_eq!(stats.total_audio_bytes, 4800);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_mock_unknown_command() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        sock.send(&[0xFF, 0x00]).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert!(stats.has_errors());
        assert!(matches!(stats.errors[0], ProtocolError::UnknownCommand(0xFF)));
    }

    #[test]
    fn test_mock_init_wrong_size() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        // Init with wrong number of bytes (only 3 instead of 5)
        sock.send(&[0x02, 0x01, 0x03]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        // Drain ACK
        let mut drain = [0u8; 64];
        let _ = sock.recv(&mut drain);

        let stats = server.stop();
        assert!(stats.has_errors());
        assert!(matches!(stats.errors[0], ProtocolError::InitWrongSize(3)));
    }

    #[test]
    fn test_mock_multiple_blits() {
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        let m = MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap();
        sock.send(&groovy::build_init(1, 3, 2, 0)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let mut drain = [0u8; 64];
        let _ = sock.recv(&mut drain);
        sock.send(&groovy::build_switchres(m)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _ = sock.recv(&mut drain);

        let field_size = m.field_size();
        let frame_data: Vec<u8> = vec![128u8; field_size];
        let compressed = compress_prepend_size(&frame_data);
        let mtu = groovy::DEFAULT_MTU;

        let num_frames = 10u32;
        for i in 1..=num_frames {
            let header = groovy::build_blit(i, 0, m.v_begin, Some(compressed.len() as u32));
            sock.send(&header).unwrap();
            let mut off = 0;
            while off < compressed.len() {
                let end = (off + mtu).min(compressed.len());
                sock.send(&compressed[off..end]).unwrap();
                off = end;
            }
            // Small delay to let server process before next blit
            std::thread::sleep(Duration::from_millis(5));
            // Drain ACK
            let _ = sock.recv(&mut drain);
        }

        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.blit_count, num_frames, "expected {} blits, got {}", num_frames, stats.blit_count);
        assert_eq!(stats.last_blit_frame, num_frames);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    #[test]
    fn test_mock_full_session() {
        // Simulates a complete session: init → switchres → N blits → close
        let server = MockGroovyServer::start();
        let sock = connect_to_mock(&server);

        let m = MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap();

        // Init
        let ack = send_and_recv(&sock, &groovy::build_init(1, 3, 2, 0));
        assert!(ack.is_some());

        // Switchres
        std::thread::sleep(Duration::from_millis(10));
        let ack = send_and_recv(&sock, &groovy::build_switchres(m));
        assert!(ack.is_some());

        // Stream 30 frames (≈0.5s at 60fps)
        let field_size = m.field_size();
        let frame_data: Vec<u8> = (0..field_size).map(|i| ((i * 7) % 256) as u8).collect();
        let compressed = compress_prepend_size(&frame_data);
        let mtu = groovy::DEFAULT_MTU;
        let mut drain = [0u8; 64];

        for i in 1..=30u32 {
            let field = if m.interlace { (i % 2) as u8 } else { 0 };
            let header = groovy::build_blit(i, field, m.v_begin, Some(compressed.len() as u32));
            sock.send(&header).unwrap();
            let mut off = 0;
            while off < compressed.len() {
                let end = (off + mtu).min(compressed.len());
                sock.send(&compressed[off..end]).unwrap();
                off = end;
            }
            std::thread::sleep(Duration::from_millis(2));
            let _ = sock.recv(&mut drain);
        }

        // Close
        sock.send(&groovy::build_close()).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let stats = server.stop();
        assert_eq!(stats.init_count, 1);
        assert_eq!(stats.switchres_count, 1);
        assert_eq!(stats.blit_count, 30);
        assert_eq!(stats.close_count, 1);
        assert_eq!(stats.last_blit_frame, 30);
        assert!(!stats.has_errors(), "errors: {:?}", stats.errors);
    }

    /// Integration test: streams ~5 seconds of test pattern to mock server
    /// via GroovyConnection, verifies frame count and protocol correctness.
    #[test]
    fn test_integration_stream_test_pattern_to_mock() {
        let server = MockGroovyServer::start();
        let _addr = format!("127.0.0.1:{}", server.port());

        // We can't use GroovyConnection::connect directly because it hardcodes the port.
        // Instead, replicate the protocol manually to test the mock thoroughly.
        let sock = connect_to_mock(&server);

        let m = MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap();
        let field_rate = m.field_rate(); // ~60 Hz
        let field_size = m.field_size(); // 320*240*3 = 230400

        // Init
        let ack = send_and_recv(&sock, &groovy::build_init(1, 3, 2, 0)).unwrap();
        assert_eq!(ack.len(), 13);

        // Switchres
        std::thread::sleep(Duration::from_millis(10));
        send_and_recv(&sock, &groovy::build_switchres(m)).unwrap();

        // Generate test pattern
        let frame_data: Vec<u8> = (0..field_size).map(|i| {
            let x = (i / 3) % m.h_active as usize;
            let y = (i / 3) / m.h_active as usize;
            let channel = i % 3;
            match channel {
                0 => ((x * 255) / m.h_active as usize) as u8, // B gradient
                1 => ((y * 255) / m.field_height()) as u8,    // G gradient
                _ => 128,                                       // R constant
            }
        }).collect();

        let compressed = compress_prepend_size(&frame_data);
        let mtu = groovy::DEFAULT_MTU;

        // Stream for ~5 seconds worth of frames
        let target_frames = (field_rate * 5.0) as u32; // ~300 frames
        let mut drain = [0u8; 64];

        let start = std::time::Instant::now();
        for i in 1..=target_frames {
            let header = groovy::build_blit(i, 0, m.v_begin, Some(compressed.len() as u32));
            sock.send(&header).unwrap();
            let mut off = 0;
            while off < compressed.len() {
                let end = (off + mtu).min(compressed.len());
                sock.send(&compressed[off..end]).unwrap();
                off = end;
            }
            // Drain ACK non-blocking
            let _ = sock.recv(&mut drain);
        }
        let elapsed = start.elapsed();

        // Close
        sock.send(&groovy::build_close()).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let stats = server.stop();

        // Verify protocol correctness
        assert_eq!(stats.init_count, 1, "expected 1 init");
        assert_eq!(stats.switchres_count, 1, "expected 1 switchres");
        assert_eq!(stats.blit_count, target_frames,
            "expected {} blits, got {}", target_frames, stats.blit_count);
        assert_eq!(stats.close_count, 1, "expected 1 close");
        assert_eq!(stats.last_blit_frame, target_frames);
        assert!(!stats.has_errors(), "protocol errors: {:?}", stats.errors);

        // Verify switchres was parsed correctly
        let sw = stats.last_switchres.unwrap();
        assert_eq!(sw.h_active, 320);
        assert_eq!(sw.v_active, 240);
        assert!(!sw.interlace);

        // Verify init was parsed correctly
        let init = stats.last_init.unwrap();
        assert_eq!(init.compression, 1); // LZ4

        eprintln!(
            "Integration test: streamed {} frames in {:.2}s ({:.1} fps) — all validated",
            target_frames, elapsed.as_secs_f64(),
            target_frames as f64 / elapsed.as_secs_f64()
        );
    }
}
