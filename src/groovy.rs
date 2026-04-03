//! Groovy MiSTer UDP protocol — packet builders and types.
//! All functions are pure (no I/O) and independently testable.

pub const UDP_PORT: u16 = 32100;
pub const DEFAULT_MTU: usize = 1472;

#[derive(Debug, Clone, Copy)]
pub struct Modeline {
    pub name: &'static str,
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

impl Modeline {
    /// Frame rate in Hz (full frames, not fields)
    pub fn frame_rate(&self) -> f64 {
        self.p_clock * 1_000_000.0 / (self.h_total as f64 * self.v_total as f64)
    }

    /// Field rate in Hz (= frame_rate * 2 for interlaced)
    pub fn field_rate(&self) -> f64 {
        if self.interlace { self.frame_rate() * 2.0 } else { self.frame_rate() }
    }

    /// Height of one field (half for interlaced, full for progressive)
    pub fn field_height(&self) -> usize {
        if self.interlace { self.v_active as usize / 2 } else { self.v_active as usize }
    }

    /// Size in bytes of one BGR24 field
    pub fn field_size(&self) -> usize {
        self.h_active as usize * self.field_height() * 3
    }

    /// Nanoseconds per field
    pub fn field_time_ns(&self) -> u64 {
        (1_000_000_000.0 / self.field_rate()) as u64
    }
}

pub static MODELINES: &[Modeline] = &[
    Modeline { name: "256x240 NTSC",  p_clock: 4.905,  h_active: 256, h_begin: 264, h_end: 287, h_total: 312, v_active: 240, v_begin: 241, v_end: 244, v_total: 262, interlace: false },
    Modeline { name: "320x240 NTSC",  p_clock: 6.700,  h_active: 320, h_begin: 336, h_end: 367, h_total: 426, v_active: 240, v_begin: 244, v_end: 247, v_total: 262, interlace: false },
    Modeline { name: "320x480i NTSC", p_clock: 6.700,  h_active: 320, h_begin: 336, h_end: 367, h_total: 426, v_active: 480, v_begin: 488, v_end: 493, v_total: 525, interlace: true },
    Modeline { name: "640x480i NTSC", p_clock: 12.336, h_active: 640, h_begin: 662, h_end: 720, h_total: 784, v_active: 480, v_begin: 488, v_end: 494, v_total: 525, interlace: true },
    Modeline { name: "720x480i NTSC", p_clock: 13.846, h_active: 720, h_begin: 744, h_end: 809, h_total: 880, v_active: 480, v_begin: 488, v_end: 494, v_total: 525, interlace: true },
    Modeline { name: "256x240 PAL",   p_clock: 5.320,  h_active: 256, h_begin: 269, h_end: 294, h_total: 341, v_active: 240, v_begin: 270, v_end: 273, v_total: 312, interlace: false },
    Modeline { name: "320x240 PAL",   p_clock: 6.660,  h_active: 320, h_begin: 336, h_end: 367, h_total: 426, v_active: 240, v_begin: 270, v_end: 273, v_total: 312, interlace: false },
    Modeline { name: "320x480i PAL",  p_clock: 6.660,  h_active: 320, h_begin: 336, h_end: 367, h_total: 426, v_active: 480, v_begin: 540, v_end: 545, v_total: 625, interlace: true },
    Modeline { name: "640x480i PAL",  p_clock: 13.320, h_active: 640, h_begin: 672, h_end: 734, h_total: 852, v_active: 480, v_begin: 540, v_end: 545, v_total: 625, interlace: true },
    Modeline { name: "720x576i PAL",  p_clock: 13.875, h_active: 720, h_begin: 741, h_end: 806, h_total: 888, v_active: 576, v_begin: 581, v_end: 586, v_total: 625, interlace: true },
];

// ── Packet builders (pure functions) ──

pub fn build_init(compression: u8, sample_rate: u8, channels: u8, rgb_mode: u8) -> Vec<u8> {
    vec![0x02, compression, sample_rate, channels, rgb_mode]
}

pub fn build_switchres(m: &Modeline) -> Vec<u8> {
    let mut d = vec![0u8; 26];
    d[0] = 0x03;
    d[1..9].copy_from_slice(&m.p_clock.to_le_bytes());
    d[9..11].copy_from_slice(&m.h_active.to_le_bytes());
    d[11..13].copy_from_slice(&m.h_begin.to_le_bytes());
    d[13..15].copy_from_slice(&m.h_end.to_le_bytes());
    d[15..17].copy_from_slice(&m.h_total.to_le_bytes());
    d[17..19].copy_from_slice(&m.v_active.to_le_bytes());
    d[19..21].copy_from_slice(&m.v_begin.to_le_bytes());
    d[21..23].copy_from_slice(&m.v_end.to_le_bytes());
    d[23..25].copy_from_slice(&m.v_total.to_le_bytes());
    d[25] = if m.interlace { 1 } else { 0 };
    d
}

pub fn build_blit(frame: u32, field: u8, vsync: u16, compressed_size: Option<u32>) -> Vec<u8> {
    if let Some(csize) = compressed_size {
        let mut d = vec![0u8; 12];
        d[0] = 0x07;
        d[1..5].copy_from_slice(&frame.to_le_bytes());
        d[5] = field;
        d[6..8].copy_from_slice(&vsync.to_le_bytes());
        d[8..12].copy_from_slice(&csize.to_le_bytes());
        d
    } else {
        let mut d = vec![0u8; 8];
        d[0] = 0x07;
        d[1..5].copy_from_slice(&frame.to_le_bytes());
        d[5] = field;
        d[6..8].copy_from_slice(&vsync.to_le_bytes());
        d
    }
}

pub fn build_audio(size: u16) -> Vec<u8> {
    let mut d = vec![0u8; 3];
    d[0] = 0x04;
    d[1..3].copy_from_slice(&size.to_le_bytes());
    d
}

pub fn build_close() -> Vec<u8> { vec![0x01] }
pub fn build_get_status() -> Vec<u8> { vec![0x05] }

// ── FPGA status ──

#[derive(Debug, Clone, Default)]
pub struct FpgaStatus {
    pub frame_echo: u32,
    pub vcount_echo: u16,
    pub frame: u32,
    pub vcount: u16,
    pub vram_ready: bool,
    pub vram_end_frame: bool,
    pub vram_synced: bool,
    pub vga_frameskip: bool,
    pub vga_vblank: bool,
    pub vga_f1: bool,
    pub audio: bool,
    pub vram_queue: bool,
}

impl FpgaStatus {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 13 { return None; }
        let bits = data[12];
        Some(Self {
            frame_echo: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            vcount_echo: u16::from_le_bytes([data[4], data[5]]),
            frame: u32::from_le_bytes([data[6], data[7], data[8], data[9]]),
            vcount: u16::from_le_bytes([data[10], data[11]]),
            vram_ready:     bits & 0x01 != 0,
            vram_end_frame: bits & 0x02 != 0,
            vram_synced:    bits & 0x04 != 0,
            vga_frameskip:  bits & 0x08 != 0,
            vga_vblank:     bits & 0x10 != 0,
            vga_f1:         bits & 0x20 != 0,
            audio:          bits & 0x40 != 0,
            vram_queue:     bits & 0x80 != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modeline_640x480i() {
        let m = MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap();
        assert_eq!(m.field_height(), 240);
        assert_eq!(m.field_size(), 640 * 240 * 3);
        assert!((m.frame_rate() - 29.97).abs() < 0.1);
        assert!((m.field_rate() - 59.94).abs() < 0.1);
    }

    #[test]
    fn test_modeline_320x240p() {
        let m = MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap();
        assert_eq!(m.field_height(), 240);
        assert_eq!(m.field_size(), 320 * 240 * 3);
        assert!((m.field_rate() - 60.0).abs() < 0.1);
    }

    #[test]
    fn test_build_init() {
        let p = build_init(1, 3, 2, 0);
        assert_eq!(p, vec![0x02, 1, 3, 2, 0]);
    }

    #[test]
    fn test_build_blit_no_compression() {
        let p = build_blit(42, 1, 488, None);
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], 0x07);
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), 42);
        assert_eq!(p[5], 1);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 488);
    }

    #[test]
    fn test_build_blit_with_compression() {
        let p = build_blit(1, 0, 488, Some(12345));
        assert_eq!(p.len(), 12);
        assert_eq!(u32::from_le_bytes([p[8], p[9], p[10], p[11]]), 12345);
    }

    #[test]
    fn test_build_audio() {
        let p = build_audio(4800);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], 0x04);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 4800);
    }

    #[test]
    fn test_build_switchres_size() {
        let m = MODELINES[0];
        let p = build_switchres(&m);
        assert_eq!(p.len(), 26);
        assert_eq!(p[0], 0x03);
    }

    #[test]
    fn test_fpga_status_parse() {
        let mut data = vec![0u8; 13];
        // frame_echo = 100
        data[0..4].copy_from_slice(&100u32.to_le_bytes());
        // vcount_echo = 200
        data[4..6].copy_from_slice(&200u16.to_le_bytes());
        // frame = 101
        data[6..10].copy_from_slice(&101u32.to_le_bytes());
        // vcount = 50
        data[10..12].copy_from_slice(&50u16.to_le_bytes());
        // bits: vram_ready + vram_synced + audio = 0x01 | 0x04 | 0x40 = 0x45
        data[12] = 0x45;

        let s = FpgaStatus::parse(&data).unwrap();
        assert_eq!(s.frame_echo, 100);
        assert_eq!(s.vcount_echo, 200);
        assert_eq!(s.frame, 101);
        assert_eq!(s.vcount, 50);
        assert!(s.vram_ready);
        assert!(!s.vram_end_frame);
        assert!(s.vram_synced);
        assert!(!s.vga_vblank);
        assert!(s.audio);
    }

    #[test]
    fn test_fpga_status_parse_too_short() {
        assert!(FpgaStatus::parse(&[0; 12]).is_none());
    }
}
