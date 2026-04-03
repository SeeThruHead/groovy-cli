use crate::groovy::{self, FpgaStatus, Modeline};
use anyhow::{Context, Result};
use lz4_flex::compress_prepend_size;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const CONGESTION_SIZE: usize = 500_000;
const CONGESTION_TIME_NS: u64 = 110_000; // 110μs

pub struct GroovyConnection {
    sock: UdpSocket,
    pub status: FpgaStatus,
    mtu: usize,
    frame_time_ns: u64,   // nanoseconds per frame (or per field for interlace)
    stream_time_ns: u64,  // how long last blit took to send
    last_congestion: Instant,
    do_congestion: bool,
    v_total: u16,
    interlace: bool,
}

impl GroovyConnection {
    pub fn connect(mister_ip: &str) -> Result<Self> {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_send_buffer_size(2 * 1024 * 1024)?;
        s.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())?;
        let dest: std::net::SocketAddr = format!("{}:{}", mister_ip, groovy::UDP_PORT).parse()?;
        s.connect(&dest.into())?;
        // Non-blocking for polling ACKs
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

    /// Send init command and wait for ACK from FPGA
    pub fn init(&mut self, modeline: &Modeline) -> Result<()> {
        // LZ4 compression, 48kHz audio, stereo, BGR24
        self.send_blocking(&groovy::build_init(1, 3, 2, 0))?;

        // Wait for ACK with 2s timeout
        if !self.wait_ack(2000) {
            eprintln!("Warning: no ACK from FPGA after init (continuing anyway)");
        } else {
            eprintln!("FPGA ACK received (synced={})", self.status.vram_synced);
        }

        std::thread::sleep(Duration::from_millis(100));

        // Switchres
        self.send_blocking(&groovy::build_switchres(modeline))?;
        std::thread::sleep(Duration::from_millis(500));

        // Calculate timing
        let frame_rate = modeline.p_clock * 1_000_000.0
            / (modeline.h_total as f64 * modeline.v_total as f64);
        let field_rate = if modeline.interlace { frame_rate * 2.0 } else { frame_rate };
        self.frame_time_ns = (1_000_000_000.0 / field_rate) as u64;
        self.v_total = modeline.v_total;
        self.interlace = modeline.interlace;

        Ok(())
    }

    /// Send a blit with LZ4 compression and congestion control
    pub fn blit(&mut self, frame_data: &[u8], frame_num: u32, field: u8, vsync: u16) {
        // Congestion control: if last frame was large, wait before sending
        if self.do_congestion {
            let elapsed = self.last_congestion.elapsed().as_nanos() as u64;
            if elapsed < CONGESTION_TIME_NS {
                spin_sleep::sleep(Duration::from_nanos(CONGESTION_TIME_NS - elapsed));
            }
        }

        // LZ4 compress
        let compressed = compress_prepend_size(frame_data);

        // Send header
        let header = groovy::build_blit_field_vsync(
            frame_num, field, vsync, Some(compressed.len() as u32),
        );

        let start = Instant::now();
        let _ = self.send_blocking(&header);

        // Send compressed payload in MTU chunks
        let mut off = 0;
        while off < compressed.len() {
            let end = (off + self.mtu).min(compressed.len());
            let _ = self.send_blocking(&compressed[off..end]);
            off = end;
        }

        self.stream_time_ns = start.elapsed().as_nanos() as u64;
        self.last_congestion = Instant::now();
        self.do_congestion = compressed.len() > CONGESTION_SIZE;

        // Poll for ACK (non-blocking)
        self.poll_ack();
    }

    /// Send audio: header then chunked payload
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

    /// Raster-aware sync: sleep until it's time to send next frame,
    /// adjusting based on FPGA's reported raster position
    pub fn wait_sync(&mut self, emulation_time_ns: u64) {
        let sleep_ns = if emulation_time_ns >= self.frame_time_ns {
            0
        } else {
            self.frame_time_ns - emulation_time_ns
        };

        // Poll ACK to get latest raster position
        self.poll_ack();

        // Adjust sleep based on raster feedback
        let adjusted_ns = if self.status.frame_echo > 0 {
            // Calculate where the raster should be vs where it is
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

    /// Calculate timing difference based on FPGA raster position
    fn diff_time_raster(&mut self) -> i64 {
        if self.v_total == 0 { return 0; }
        let width_time_ns = self.frame_time_ns / (self.v_total as u64 >> if self.interlace { 1 } else { 0 });
        let shift = if self.interlace { 1 } else { 0 };

        let vcount1 = (((self.status.frame_echo.wrapping_sub(1)) as u64 * self.v_total as u64
            + self.status.vcount_echo as u64) >> shift) as i64;
        let vcount2 = ((self.status.frame as u64 * self.v_total as u64
            + self.status.vcount as u64) >> shift) as i64;
        let dif = (vcount1 - vcount2) / 2; // dichotomous

        (width_time_ns as i64) * dif
    }

    pub fn close(&self) {
        let _ = self.send_blocking(&groovy::build_close());
    }

    // -- internal --

    fn send_blocking(&self, data: &[u8]) -> Result<()> {
        // Socket is non-blocking, so we need to handle WouldBlock
        loop {
            match self.sock.send(data) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(10));
                }
                Err(e) if e.raw_os_error() == Some(55) => {
                    // ENOBUFS
                    std::thread::sleep(Duration::from_micros(100));
                }
                Err(e) => return Err(e).context("UDP send"),
            }
        }
    }

    /// Wait for ACK with timeout (milliseconds). Returns true if received.
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
                Ok(_) => {} // wrong size, ignore
                Err(_) => {}
            }
            if Instant::now() >= deadline { return false; }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Non-blocking poll for latest ACK
    fn poll_ack(&mut self) {
        let mut buf = [0u8; 16];
        // Drain all pending ACKs, keep the latest
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

    /// Get underlying socket for audio thread (arc+mutex pattern)
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }
}

impl Drop for GroovyConnection {
    fn drop(&mut self) {
        eprintln!("Sending close to MiSTer...");
        self.close();
    }
}
