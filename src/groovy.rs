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

    // ── Modeline tests ──

    #[test]
    fn test_modeline_640x480i_ntsc() {
        let m = MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap();
        assert!(m.interlace);
        assert_eq!(m.h_active, 640);
        assert_eq!(m.v_active, 480);
        assert_eq!(m.field_height(), 240);
        assert_eq!(m.field_size(), 640 * 240 * 3);
        assert!((m.frame_rate() - 29.97).abs() < 0.1);
        assert!((m.field_rate() - 59.94).abs() < 0.1);
    }

    #[test]
    fn test_modeline_320x240p_ntsc() {
        let m = MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap();
        assert!(!m.interlace);
        assert_eq!(m.field_height(), 240);
        assert_eq!(m.field_size(), 320 * 240 * 3);
        // Progressive: field_rate == frame_rate
        assert!((m.field_rate() - m.frame_rate()).abs() < 0.001);
        assert!((m.field_rate() - 60.0).abs() < 0.2);
    }

    #[test]
    fn test_modeline_720x576i_pal() {
        let m = MODELINES.iter().find(|m| m.name == "720x576i PAL").unwrap();
        assert!(m.interlace);
        assert_eq!(m.h_active, 720);
        assert_eq!(m.v_active, 576);
        assert_eq!(m.field_height(), 288);
        assert_eq!(m.field_size(), 720 * 288 * 3);
        assert!((m.frame_rate() - 25.0).abs() < 0.1);
        assert!((m.field_rate() - 50.0).abs() < 0.1);
    }

    #[test]
    fn test_modeline_field_time_ns() {
        let m = MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap();
        // ~59.94 fields/s → ~16_683_350 ns per field
        let ns = m.field_time_ns();
        assert!(ns > 16_000_000 && ns < 17_000_000, "field_time_ns={}", ns);
    }

    #[test]
    fn test_modeline_progressive_field_rate_equals_frame_rate() {
        for m in MODELINES.iter().filter(|m| !m.interlace) {
            assert!(
                (m.field_rate() - m.frame_rate()).abs() < 0.001,
                "{}: field_rate={} != frame_rate={}", m.name, m.field_rate(), m.frame_rate()
            );
        }
    }

    #[test]
    fn test_modeline_interlaced_field_rate_double_frame_rate() {
        for m in MODELINES.iter().filter(|m| m.interlace) {
            assert!(
                (m.field_rate() - m.frame_rate() * 2.0).abs() < 0.01,
                "{}: field_rate={} != 2*frame_rate={}", m.name, m.field_rate(), m.frame_rate() * 2.0
            );
        }
    }

    #[test]
    fn test_all_modelines_sane() {
        assert!(!MODELINES.is_empty());
        for m in MODELINES {
            assert!(m.h_active > 0 && m.v_active > 0, "{}: zero resolution", m.name);
            assert!(m.h_total >= m.h_active, "{}: h_total < h_active", m.name);
            assert!(m.v_total >= m.v_active, "{}: v_total < v_active", m.name);
            assert!(m.p_clock > 0.0, "{}: zero pixel clock", m.name);
            assert!(m.field_rate() > 20.0 && m.field_rate() < 80.0,
                "{}: field_rate={} out of range", m.name, m.field_rate());
            assert!(m.field_size() > 0, "{}: zero field size", m.name);
            assert!(m.field_time_ns() > 0, "{}: zero field time", m.name);
        }
    }

    // ── Packet builder tests ──

    #[test]
    fn test_build_init() {
        let p = build_init(1, 3, 2, 0);
        assert_eq!(p, vec![0x02, 1, 3, 2, 0]);
    }

    #[test]
    fn test_build_init_all_zeros() {
        let p = build_init(0, 0, 0, 0);
        assert_eq!(p, vec![0x02, 0, 0, 0, 0]);
    }

    #[test]
    fn test_build_close() {
        assert_eq!(build_close(), vec![0x01]);
    }

    #[test]
    fn test_build_get_status() {
        assert_eq!(build_get_status(), vec![0x05]);
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
        assert_eq!(p[0], 0x07);
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), 1);
        assert_eq!(p[5], 0);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 488);
        assert_eq!(u32::from_le_bytes([p[8], p[9], p[10], p[11]]), 12345);
    }

    #[test]
    fn test_build_blit_max_frame() {
        let p = build_blit(u32::MAX, 1, u16::MAX, Some(u32::MAX));
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), u32::MAX);
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), u16::MAX);
        assert_eq!(u32::from_le_bytes([p[8], p[9], p[10], p[11]]), u32::MAX);
    }

    #[test]
    fn test_build_audio() {
        let p = build_audio(4800);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], 0x04);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 4800);
    }

    #[test]
    fn test_build_audio_zero() {
        let p = build_audio(0);
        assert_eq!(p, vec![0x04, 0, 0]);
    }

    #[test]
    fn test_build_switchres_roundtrip() {
        let m = &MODELINES[4]; // 720x480i NTSC
        let p = build_switchres(m);
        assert_eq!(p.len(), 26);
        assert_eq!(p[0], 0x03);
        // Verify all fields decode back
        assert_eq!(f64::from_le_bytes(p[1..9].try_into().unwrap()), m.p_clock);
        assert_eq!(u16::from_le_bytes(p[9..11].try_into().unwrap()), m.h_active);
        assert_eq!(u16::from_le_bytes(p[11..13].try_into().unwrap()), m.h_begin);
        assert_eq!(u16::from_le_bytes(p[13..15].try_into().unwrap()), m.h_end);
        assert_eq!(u16::from_le_bytes(p[15..17].try_into().unwrap()), m.h_total);
        assert_eq!(u16::from_le_bytes(p[17..19].try_into().unwrap()), m.v_active);
        assert_eq!(u16::from_le_bytes(p[19..21].try_into().unwrap()), m.v_begin);
        assert_eq!(u16::from_le_bytes(p[21..23].try_into().unwrap()), m.v_end);
        assert_eq!(u16::from_le_bytes(p[23..25].try_into().unwrap()), m.v_total);
        assert_eq!(p[25], 1); // interlaced
    }

    #[test]
    fn test_build_switchres_progressive() {
        let m = &MODELINES[1]; // 320x240 NTSC (progressive)
        let p = build_switchres(m);
        assert_eq!(p[25], 0); // not interlaced
    }

    #[test]
    fn test_build_switchres_all_modelines() {
        for m in MODELINES {
            let p = build_switchres(m);
            assert_eq!(p.len(), 26, "{}: wrong switchres packet size", m.name);
            assert_eq!(p[0], 0x03, "{}: wrong command byte", m.name);
            assert_eq!(p[25], if m.interlace { 1 } else { 0 }, "{}: wrong interlace flag", m.name);
        }
    }

    // ── Unique command byte tests ──

    #[test]
    fn test_command_bytes_distinct() {
        let cmds = [
            build_close()[0],
            build_init(0, 0, 0, 0)[0],
            build_switchres(&MODELINES[0])[0],
            build_audio(0)[0],
            build_get_status()[0],
            build_blit(0, 0, 0, None)[0],
        ];
        let mut seen = std::collections::HashSet::new();
        for c in &cmds {
            assert!(seen.insert(c), "duplicate command byte 0x{:02x}", c);
        }
    }

    // ── FpgaStatus tests ──

    #[test]
    fn test_fpga_status_parse() {
        let mut data = vec![0u8; 13];
        data[0..4].copy_from_slice(&100u32.to_le_bytes());
        data[4..6].copy_from_slice(&200u16.to_le_bytes());
        data[6..10].copy_from_slice(&101u32.to_le_bytes());
        data[10..12].copy_from_slice(&50u16.to_le_bytes());
        data[12] = 0x45; // vram_ready | vram_synced | audio

        let s = FpgaStatus::parse(&data).unwrap();
        assert_eq!(s.frame_echo, 100);
        assert_eq!(s.vcount_echo, 200);
        assert_eq!(s.frame, 101);
        assert_eq!(s.vcount, 50);
        assert!(s.vram_ready);
        assert!(!s.vram_end_frame);
        assert!(s.vram_synced);
        assert!(!s.vga_frameskip);
        assert!(!s.vga_vblank);
        assert!(!s.vga_f1);
        assert!(s.audio);
        assert!(!s.vram_queue);
    }

    #[test]
    fn test_fpga_status_all_bits_set() {
        let mut data = vec![0u8; 13];
        data[12] = 0xFF;
        let s = FpgaStatus::parse(&data).unwrap();
        assert!(s.vram_ready);
        assert!(s.vram_end_frame);
        assert!(s.vram_synced);
        assert!(s.vga_frameskip);
        assert!(s.vga_vblank);
        assert!(s.vga_f1);
        assert!(s.audio);
        assert!(s.vram_queue);
    }

    #[test]
    fn test_fpga_status_all_bits_clear() {
        let mut data = vec![0u8; 13];
        data[12] = 0x00;
        let s = FpgaStatus::parse(&data).unwrap();
        assert!(!s.vram_ready);
        assert!(!s.vram_end_frame);
        assert!(!s.vram_synced);
        assert!(!s.vga_frameskip);
        assert!(!s.vga_vblank);
        assert!(!s.vga_f1);
        assert!(!s.audio);
        assert!(!s.vram_queue);
    }

    #[test]
    fn test_fpga_status_each_bit() {
        for bit in 0..8u8 {
            let mut data = vec![0u8; 13];
            data[12] = 1 << bit;
            let s = FpgaStatus::parse(&data).unwrap();
            assert_eq!(s.vram_ready,     bit == 0);
            assert_eq!(s.vram_end_frame, bit == 1);
            assert_eq!(s.vram_synced,    bit == 2);
            assert_eq!(s.vga_frameskip,  bit == 3);
            assert_eq!(s.vga_vblank,     bit == 4);
            assert_eq!(s.vga_f1,         bit == 5);
            assert_eq!(s.audio,          bit == 6);
            assert_eq!(s.vram_queue,     bit == 7);
        }
    }

    #[test]
    fn test_fpga_status_parse_too_short() {
        assert!(FpgaStatus::parse(&[]).is_none());
        assert!(FpgaStatus::parse(&[0; 12]).is_none());
    }

    #[test]
    fn test_fpga_status_parse_extra_bytes_ok() {
        let mut data = vec![0u8; 20]; // extra padding
        data[0..4].copy_from_slice(&999u32.to_le_bytes());
        data[12] = 0x01;
        let s = FpgaStatus::parse(&data).unwrap();
        assert_eq!(s.frame_echo, 999);
        assert!(s.vram_ready);
    }

    #[test]
    fn test_fpga_status_default() {
        let s = FpgaStatus::default();
        assert_eq!(s.frame_echo, 0);
        assert_eq!(s.vcount_echo, 0);
        assert_eq!(s.frame, 0);
        assert_eq!(s.vcount, 0);
        assert!(!s.vram_ready);
        assert!(!s.vram_synced);
        assert!(!s.audio);
    }
}
