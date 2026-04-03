//! Test pattern generation and send loop for MiSTer modeline calibration.
//!
//! Pattern generation is pure (no I/O) and fully testable.
//! The send loop uses GroovyConnection for protocol-correct transport.

use crate::connection::GroovyConnection;
use crate::groovy::Modeline;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Pure functions (testable) ──

/// Generate a BGR24 test pattern at full frame resolution.
///
/// Pattern: white border, crosshair center, concentric colored rings.
/// Returns `w * h * 3` bytes in BGR24 format.
pub fn generate_pattern(w: usize, h: usize, scale: f64) -> Vec<u8> {
    let inner_w = ((w as f64) * scale) as usize & !1;
    let inner_h = ((h as f64) * scale) as usize & !1;
    let pad_x = (w - inner_w) / 2;
    let pad_y = (h - inner_h) / 2;

    let bpp = 3;
    let mut pattern = vec![0u8; w * h * bpp]; // black background

    for y in 0..h {
        for x in 0..w {
            let off = (y * w + x) * bpp;
            let ix = x as isize - pad_x as isize;
            let iy = y as isize - pad_y as isize;
            if ix < 0 || iy < 0 || ix >= inner_w as isize || iy >= inner_h as isize {
                continue; // black padding
            }
            let ix = ix as usize;
            let iy = iy as usize;

            let cx_f = ix as f64 - inner_w as f64 / 2.0;
            let cy_f = iy as f64 - inner_h as f64 / 2.0;
            let half = inner_h as f64 / 2.0;
            let dist = (cx_f.powi(2) + cy_f.powi(2)).sqrt() / half;

            let (b, g, r);
            if ix == 0 || ix == inner_w - 1 || iy == 0 || iy == inner_h - 1 {
                // White border
                b = 255; g = 255; r = 255;
            } else if (ix as isize - inner_w as isize / 2).unsigned_abs() < 2
                   || (iy as isize - inner_h as isize / 2).unsigned_abs() < 2 {
                // Center crosshair
                b = 80; g = 80; r = 80;
            } else {
                let ring_num = (dist * 4.0) as u32;
                let frac = (dist * 4.0).fract();
                let on_ring = frac > 0.35 && frac < 0.65;
                if on_ring && ring_num < 4 {
                    match ring_num {
                        0 => { b = 255; g = 255; r = 255; }
                        1 => { b = 0;   g = 0;   r = 255; }
                        2 => { b = 0;   g = 255; r = 0;   }
                        3 => { b = 255; g = 0;   r = 0;   }
                        _ => { b = 255; g = 255; r = 255; }
                    }
                } else if dist > 1.02 {
                    b = 10; g = 10; r = 10;
                } else {
                    b = 40; g = 40; r = 40;
                }
            }

            pattern[off] = b;
            pattern[off + 1] = g;
            pattern[off + 2] = r;
        }
    }
    pattern
}

/// Split a full-frame pattern into even/odd fields for interlaced output.
///
/// Returns `(field0, field1)`. For progressive, `field1` is empty.
pub fn split_fields(pattern: &[u8], w: usize, full_h: usize, field_h: usize, interlace: bool) -> (Vec<u8>, Vec<u8>) {
    if !interlace {
        return (pattern.to_vec(), vec![]);
    }
    let rb = w * 3;
    let field_size = w * field_h * 3;
    let mut f0 = vec![0u8; field_size];
    let mut f1 = vec![0u8; field_size];
    for y in 0..field_h {
        let dst = y * rb;
        f0[dst..dst + rb].copy_from_slice(&pattern[y * 2 * rb..(y * 2 + 1) * rb]);
        if y * 2 + 1 < full_h {
            f1[dst..dst + rb].copy_from_slice(&pattern[(y * 2 + 1) * rb..(y * 2 + 2) * rb]);
        }
    }
    (f0, f1)
}

// ── I/O ──

/// Send a test pattern to MiSTer for the given duration.
pub fn send_test_pattern(
    mister_ip: &str,
    modeline: &Modeline,
    scale: f64,
    duration_secs: u64,
) -> Result<()> {
    let w = modeline.h_active as usize;
    let h = modeline.v_active as usize;
    let field_h = modeline.field_height();
    let field_size = modeline.field_size();
    let field_rate = modeline.field_rate();

    eprintln!("Test pattern: {}x{}{} @ {:.2} fields/s", w, h,
        if modeline.interlace { "i" } else { "p" }, field_rate);
    eprintln!("Duration: {}s. Ctrl+C to stop.", duration_secs);

    let inner_w = ((w as f64) * scale) as usize & !1;
    let inner_h = ((h as f64) * scale) as usize & !1;
    let pad_x = (w - inner_w) / 2;
    let pad_y = (h - inner_h) / 2;
    eprintln!("Scale: {:.0}% — inner {}x{}, padding {}x{}", scale * 100.0, inner_w, inner_h, pad_x, pad_y);

    let pattern = generate_pattern(w, h, scale);
    let (field0, field1) = split_fields(&pattern, w, h, field_h, modeline.interlace);
    assert_eq!(field0.len(), field_size);

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            eprintln!("\nStopping...");
            r.store(false, Ordering::Relaxed);
        }).ok();
    }

    let mut conn = GroovyConnection::connect(mister_ip)?;
    conn.init(modeline)?;

    let vsync = modeline.v_begin;
    let mut frame_count: u32 = 0;
    let mut current_field: u8 = 0;
    let deadline = Instant::now() + Duration::from_secs(duration_secs);

    eprintln!("Sending test pattern...");

    while running.load(Ordering::Relaxed) && Instant::now() < deadline {
        let start = Instant::now();
        frame_count += 1;
        let data = if modeline.interlace {
            if current_field == 0 { &field0 } else { &field1 }
        } else {
            &field0
        };

        conn.blit(data, frame_count, current_field, vsync);

        if modeline.interlace {
            current_field = if current_field == 0 { 1 } else { 0 };
        }

        let elapsed_ns = start.elapsed().as_nanos() as u64;
        conn.wait_sync(elapsed_ns);
    }

    // conn.close() called automatically via Drop
    eprintln!("Done");
    Ok(())
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groovy::MODELINES;

    fn modeline_320x240() -> &'static Modeline {
        MODELINES.iter().find(|m| m.name == "320x240 NTSC").unwrap()
    }

    fn modeline_640x480i() -> &'static Modeline {
        MODELINES.iter().find(|m| m.name == "640x480i NTSC").unwrap()
    }

    #[test]
    fn test_generate_pattern_size() {
        let p = generate_pattern(320, 240, 1.0);
        assert_eq!(p.len(), 320 * 240 * 3);
    }

    #[test]
    fn test_generate_pattern_size_640x480() {
        let p = generate_pattern(640, 480, 1.0);
        assert_eq!(p.len(), 640 * 480 * 3);
    }

    #[test]
    fn test_generate_pattern_not_all_black() {
        let p = generate_pattern(320, 240, 1.0);
        assert!(p.iter().any(|&b| b != 0), "pattern should not be all black");
    }

    #[test]
    fn test_generate_pattern_has_white_border() {
        let w = 320usize;
        let h = 240usize;
        let p = generate_pattern(w, h, 1.0);
        // Top-left corner (0,0) should be white border
        assert_eq!(p[0], 255, "B at (0,0)");
        assert_eq!(p[1], 255, "G at (0,0)");
        assert_eq!(p[2], 255, "R at (0,0)");
        // Bottom-right corner
        let off = ((h - 1) * w + (w - 1)) * 3;
        assert_eq!(p[off], 255, "B at bottom-right");
    }

    #[test]
    fn test_generate_pattern_scaled_has_black_padding() {
        let w = 320usize;
        let h = 240usize;
        let p = generate_pattern(w, h, 0.8);
        // Top-left corner should be black (padding)
        assert_eq!(p[0], 0);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 0);
        // But center area should have content
        let cx = w / 2;
        let cy = h / 2;
        let off = (cy * w + cx) * 3;
        // Center crosshair = gray (80,80,80)
        assert_eq!(p[off], 80);
        assert_eq!(p[off + 1], 80);
        assert_eq!(p[off + 2], 80);
    }

    #[test]
    fn test_generate_pattern_center_crosshair() {
        let w = 320usize;
        let h = 240usize;
        let p = generate_pattern(w, h, 1.0);
        // Center pixel should be crosshair gray
        let cx = w / 2;
        let cy = h / 2;
        let off = (cy * w + cx) * 3;
        assert_eq!(p[off], 80, "crosshair B");
        assert_eq!(p[off + 1], 80, "crosshair G");
        assert_eq!(p[off + 2], 80, "crosshair R");
    }

    #[test]
    fn test_split_fields_progressive() {
        let w = 320;
        let h = 240;
        let pattern = vec![42u8; w * h * 3];
        let (f0, f1) = split_fields(&pattern, w, h, h, false);
        assert_eq!(f0.len(), w * h * 3);
        assert!(f1.is_empty());
        assert_eq!(f0, pattern);
    }

    #[test]
    fn test_split_fields_interlaced() {
        let w = 4; // small for easy verification
        let h = 4;
        let field_h = 2;
        let rb = w * 3;
        // Pattern: row 0 = 0x00, row 1 = 0x11, row 2 = 0x22, row 3 = 0x33
        let mut pattern = vec![0u8; w * h * 3];
        for y in 0..h {
            let val = (y * 0x11) as u8;
            for x in 0..rb {
                pattern[y * rb + x] = val;
            }
        }
        let (f0, f1) = split_fields(&pattern, w, h, field_h, true);
        assert_eq!(f0.len(), w * field_h * 3);
        assert_eq!(f1.len(), w * field_h * 3);
        // f0 = even rows (0, 2) → values 0x00, 0x22
        assert!(f0[..rb].iter().all(|&b| b == 0x00));
        assert!(f0[rb..].iter().all(|&b| b == 0x22));
        // f1 = odd rows (1, 3) → values 0x11, 0x33
        assert!(f1[..rb].iter().all(|&b| b == 0x11));
        assert!(f1[rb..].iter().all(|&b| b == 0x33));
    }

    #[test]
    fn test_split_fields_modeline_sizes() {
        let m = modeline_640x480i();
        let w = m.h_active as usize;
        let h = m.v_active as usize;
        let field_h = m.field_height();
        let pattern = generate_pattern(w, h, 1.0);
        let (f0, f1) = split_fields(&pattern, w, h, field_h, true);
        assert_eq!(f0.len(), m.field_size());
        assert_eq!(f1.len(), m.field_size());
    }

    #[test]
    fn test_split_fields_progressive_modeline() {
        let m = modeline_320x240();
        let w = m.h_active as usize;
        let h = m.v_active as usize;
        let field_h = m.field_height();
        let pattern = generate_pattern(w, h, 1.0);
        let (f0, f1) = split_fields(&pattern, w, h, field_h, false);
        assert_eq!(f0.len(), m.field_size());
        assert!(f1.is_empty());
    }

    #[test]
    fn test_generate_pattern_various_scales() {
        for &scale in &[0.3, 0.5, 0.8, 0.9, 1.0] {
            let p = generate_pattern(640, 480, scale);
            assert_eq!(p.len(), 640 * 480 * 3, "scale={}", scale);
            // Should have some non-black content
            assert!(p.iter().any(|&b| b != 0), "all black at scale={}", scale);
        }
    }

    #[test]
    fn test_generate_pattern_deterministic() {
        let p1 = generate_pattern(320, 240, 1.0);
        let p2 = generate_pattern(320, 240, 1.0);
        assert_eq!(p1, p2);
    }
}
