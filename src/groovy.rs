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

pub fn build_init(compression: u8, sample_rate: u8, channels: u8, rgb_mode: u8) -> Vec<u8> {
    vec![0x02, compression, sample_rate, channels, rgb_mode]
}

pub fn build_switchres(m: &Modeline) -> Vec<u8> {
    let mut data = vec![0u8; 26];
    data[0] = 0x03;
    data[1..9].copy_from_slice(&m.p_clock.to_le_bytes());
    data[9..11].copy_from_slice(&m.h_active.to_le_bytes());
    data[11..13].copy_from_slice(&m.h_begin.to_le_bytes());
    data[13..15].copy_from_slice(&m.h_end.to_le_bytes());
    data[15..17].copy_from_slice(&m.h_total.to_le_bytes());
    data[17..19].copy_from_slice(&m.v_active.to_le_bytes());
    data[19..21].copy_from_slice(&m.v_begin.to_le_bytes());
    data[21..23].copy_from_slice(&m.v_end.to_le_bytes());
    data[23..25].copy_from_slice(&m.v_total.to_le_bytes());
    data[25] = if m.interlace { 1 } else { 0 };
    data
}

pub fn build_blit_field_vsync(frame: u32, field: u8, vsync: u16, compressed_size: Option<u32>) -> Vec<u8> {
    if let Some(csize) = compressed_size {
        let mut data = vec![0u8; 12];
        data[0] = 0x07;
        data[1..5].copy_from_slice(&frame.to_le_bytes());
        data[5] = field;
        data[6..8].copy_from_slice(&vsync.to_le_bytes());
        data[8..12].copy_from_slice(&csize.to_le_bytes());
        data
    } else {
        let mut data = vec![0u8; 8];
        data[0] = 0x07;
        data[1..5].copy_from_slice(&frame.to_le_bytes());
        data[5] = field;
        data[6..8].copy_from_slice(&vsync.to_le_bytes());
        data
    }
}

pub fn build_audio(size: u16) -> Vec<u8> {
    let mut data = vec![0u8; 3];
    data[0] = 0x04;
    data[1..3].copy_from_slice(&size.to_le_bytes());
    data
}

pub fn build_close() -> Vec<u8> {
    vec![0x01]
}
