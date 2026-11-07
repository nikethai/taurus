//! The app icon drawn in code: a rounded square in the Aldebaran strong
//! accent (#C2410C) with a white downward arrow. Used for the tray and the
//! window, and written to files by `cargo xtask icons`.

const ACCENT: [u8; 3] = [0xC2, 0x41, 0x0C];
/// Samples per pixel side, for smooth edges.
const SAMPLES: u32 = 4;

/// `size`×`size` pixels, RGBA, row by row (straight alpha).
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    let n = (SAMPLES * SAMPLES) as f32;
    for y in 0..size {
        for x in 0..size {
            let (mut fill, mut arrow) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    // Sample point in 0..1 icon coordinates.
                    let u = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    if in_rounded_square(u, v) {
                        fill += 1;
                        if in_arrow(u, v) {
                            arrow += 1;
                        }
                    }
                }
            }
            if fill == 0 {
                px.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            // White share among the covered samples.
            let white = arrow as f32 / fill as f32;
            let mix = |c: u8| (c as f32 + (255.0 - c as f32) * white).round() as u8;
            let alpha = (fill as f32 / n * 255.0).round() as u8;
            px.extend_from_slice(&[mix(ACCENT[0]), mix(ACCENT[1]), mix(ACCENT[2]), alpha]);
        }
    }
    px
}

/// The whole square with corners of radius 0.22.
fn in_rounded_square(u: f32, v: f32) -> bool {
    const R: f32 = 0.22;
    let cx = u.clamp(R, 1.0 - R);
    let cy = v.clamp(R, 1.0 - R);
    (u - cx).powi(2) + (v - cy).powi(2) <= R * R
}

/// A shaft from the top, a head pointing down, and a base line.
fn in_arrow(u: f32, v: f32) -> bool {
    let shaft = (u - 0.5).abs() <= 0.065 && (0.2..=0.56).contains(&v);
    // Triangle: 0.26 wide each side at v = 0.44, narrowing to the tip at 0.7.
    let head = (0.44..=0.7).contains(&v) && (u - 0.5).abs() <= 0.26 * (0.7 - v) / 0.26;
    let base = (0.76..=0.83).contains(&v) && (u - 0.5).abs() <= 0.27;
    shaft || head || base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_has_right_size() {
        for size in [16u32, 32, 64] {
            let px = app_icon_rgba(size);
            assert_eq!(px.len(), (size * size * 4) as usize);
            let at = |x: u32, y: u32| &px[((y * size + x) * 4) as usize..][..4];
            assert_eq!(at(0, 0)[3], 0, "corner is transparent");
            assert_eq!(at(size / 2, size / 2)[3], 255, "center is opaque");
        }
        // Accent where there is no arrow, white on the arrow's shaft.
        let px = app_icon_rgba(64);
        let at = |x: u32, y: u32| px[((y * 64 + x) * 4) as usize..][..4].to_vec();
        assert_eq!(at(8, 32), vec![0xC2, 0x41, 0x0C, 255]);
        assert_eq!(at(32, 20), vec![255, 255, 255, 255]);
    }
}
