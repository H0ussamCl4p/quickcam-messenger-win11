//! Simple demosaic and PNG output.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// Colour of the Bayer cell at (x, y) for a GRBG sensor: 0=R, 1=G, 2=B.
fn grbg(x: usize, y: usize) -> usize {
    match (y & 1, x & 1) {
        (0, 0) => 1,
        (0, 1) => 0,
        (1, 0) => 2,
        _ => 1,
    }
}

/// Simple demosaic: each output channel is the mean of that color's samples in the 3x3 neighbourhood.
/// Then a gray-world white balance so the old sensor's color cast is less extreme.
pub fn debayer_grbg(raw: &[u8], w: usize, h: usize, white_balance: bool) -> Vec<u8> {
    let mut rgb = vec![0f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 3];
            let mut cnt = [0u32; 3];
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let (nx, ny) = (nx as usize, ny as usize);
                    let c = grbg(nx, ny);
                    sum[c] += raw[ny * w + nx] as u32;
                    cnt[c] += 1;
                }
            }
            for c in 0..3 {
                rgb[(y * w + x) * 3 + c] = if cnt[c] > 0 { sum[c] as f32 / cnt[c] as f32 } else { 0.0 };
            }
        }
    }
    let mut gains = [1f32; 3];
    if white_balance {
        let mut mean = [0f64; 3];
        for px in rgb.chunks_exact(3) {
            for c in 0..3 {
                mean[c] += px[c] as f64;
            }
        }
        let g = mean[1].max(1.0);
        for c in 0..3 {
            gains[c] = (g / mean[c].max(1.0)).clamp(0.25, 4.0) as f32;
        }
    }
    rgb.chunks_exact(3)
        .flat_map(|px| (0..3).map(move |c| (px[c] * gains[c]).round().clamp(0.0, 255.0) as u8).collect::<Vec<_>>())
        .collect()
}

pub fn save_png(path: &Path, data: &[u8], w: usize, h: usize, color: bool) -> std::io::Result<()> {
    let f = BufWriter::new(File::create(path)?);
    let mut enc = png::Encoder::new(f, w as u32, h as u32);
    enc.set_color(if color { png::ColorType::Rgb } else { png::ColorType::Grayscale });
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(std::io::Error::other)?;
    wr.write_image_data(data).map_err(std::io::Error::other)?;
    Ok(())
}

pub fn mean(raw: &[u8]) -> f64 {
    raw.iter().map(|&v| v as f64).sum::<f64>() / raw.len().max(1) as f64
}
