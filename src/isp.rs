//! Image signal processor: raw 324x240 GRBG Bayer -> 640x480 NV12 (and 320x240 RGB),
//! tuned for low latency (integer math, no per-frame allocations, ~1-3 ms/frame).
//!
//! Pipeline: crop 320x240 -> bilinear demosaic -> smoothed gray-world white balance
//! -> smoothed auto-levels + gamma (one LUT) -> saturation boost -> YUV
//! -> mild luma sharpen -> 2x luma upscale (chroma is already at NV12 resolution).

pub const SRC_W: usize = 324;
pub const SRC_H: usize = 240;
pub const W: usize = 320;
pub const H: usize = 240;
pub const OUT_W: usize = 640;
pub const OUT_H: usize = 480;
pub const NV12_SIZE: usize = OUT_W * OUT_H * 3 / 2;
// The sensor's rightmost columns are unreliable (bright/coloured stripe), so crop from the left.
const X0: usize = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Clean, natural colour.
    None,
    /// Warm faded film: lifted blacks, creamy highlights, softer colour, vignette, grain.
    Vintage,
    /// Black & white with punchy contrast, vignette and grain.
    Noir,
}

impl Filter {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "natural" => Some(Self::None),
            "vintage" | "film" | "retro" => Some(Self::Vintage),
            "noir" | "bw" | "mono" => Some(Self::Noir),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Vintage => "vintage",
            Self::Noir => "noir",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IspParams {
    pub gamma: f32,      // output = input^(1/gamma)
    pub saturation: f32, // 1.0 = unchanged
    pub sharpen: f32,    // 0 = off
    pub auto_levels: bool,
    pub white_balance: bool,
    /// Flip horizontally (left/right swapped).
    pub mirror: bool,
    pub filter: Filter,
    /// 0..1, how strongly the filter is applied.
    pub strength: f32,
}

impl Default for IspParams {
    fn default() -> Self {
        Self {
            gamma: 1.5,
            saturation: 1.25,
            sharpen: 0.3,
            auto_levels: true,
            white_balance: true,
            mirror: true,
            filter: Filter::None,
            strength: 1.0,
        }
    }
}

/// Precomputed per-filter tables (rebuilt only when the filter/strength changes).
struct Look {
    key: (Filter, u32),
    curve: [[u8; 256]; 3],
    vignette: Vec<u16>, // W*H, 256 = no darkening
    saturation: f32,
    grain: i32,
    sharpen: f32,
    mono: bool,
}

impl Look {
    fn build(f: Filter, strength: f32, p: &IspParams) -> Self {
        let s = strength.clamp(0.0, 1.0);
        let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
        let mut curve = [[0u8; 256]; 3];
        // (lift, scale, s-curve amount) per channel
        let ch: [(f32, f32, f32); 3] = match f {
            Filter::None => [(0.0, 1.0, 0.0); 3],
            Filter::Vintage => [(22.0, 0.98, 0.30), (16.0, 0.94, 0.30), (36.0, 0.78, 0.25)],
            Filter::Noir => [(10.0, 0.94, 0.65); 3],
        };
        for (c, &(lift, scale, sc)) in ch.iter().enumerate() {
            for v in 0..256 {
                let t = v as f32 / 255.0;
                let t = t + (smooth(t) - t) * sc;
                let out = lift + t * 255.0 * scale;
                let out = v as f32 + (out - v as f32) * s;
                curve[c][v] = out.round().clamp(0.0, 255.0) as u8;
            }
        }
        let vig_amt = match f {
            Filter::None => 0.0,
            Filter::Vintage => 0.38,
            Filter::Noir => 0.55,
        } * s;
        let mut vignette = vec![256u16; W * H];
        if vig_amt > 0.0 {
            for y in 0..H {
                for x in 0..W {
                    let dx = (x as f32 - W as f32 / 2.0) / (W as f32 / 2.0);
                    let dy = (y as f32 - H as f32 / 2.0) / (H as f32 / 2.0);
                    let r2 = ((dx * dx + dy * dy) / 2.0).min(1.0);
                    vignette[y * W + x] = ((1.0 - vig_amt * r2 * r2.sqrt()) * 256.0) as u16;
                }
            }
        }
        let (saturation, grain, sharpen, mono) = match f {
            Filter::None => (p.saturation, 0, p.sharpen, false),
            Filter::Vintage => (p.saturation + (0.72 - p.saturation) * s, (8.0 * s) as i32, p.sharpen * (1.0 - s), false),
            Filter::Noir => (0.0, (11.0 * s) as i32, p.sharpen * (1.0 - 0.5 * s), true),
        };
        Look { key: (f, (s * 1000.0) as u32), curve, vignette, saturation, grain, sharpen, mono }
    }
}

pub struct Isp {
    pub p: IspParams,
    rgb: Vec<u16>, // demosaiced, W*H*3
    out_rgb: Vec<u8>,
    ysmall: Vec<u8>,
    wb: [f32; 3],
    lo: f32,
    hi: f32,
    first: bool,
    look: Option<Look>,
    rng: u32,
}

impl Default for Isp {
    fn default() -> Self {
        Self::new(IspParams::default())
    }
}

impl Isp {
    pub fn new(p: IspParams) -> Self {
        Self {
            p,
            rgb: vec![0; W * H * 3],
            out_rgb: vec![0; W * H * 3],
            ysmall: vec![0; W * H],
            wb: [1.0; 3],
            lo: 0.0,
            hi: 255.0,
            first: true,
            look: None,
            rng: 0x9E37_79B9,
        }
    }

    /// The last processed frame as 320x240 RGB (after WB/levels/gamma/saturation).
    pub fn rgb(&self) -> &[u8] {
        &self.out_rgb
    }

    #[inline(always)]
    fn demosaic(&mut self, raw: &[u8]) {
        let px = |x: isize, y: isize| -> u16 {
            let x = x.clamp(0, SRC_W as isize - 1) as usize;
            let y = y.clamp(0, SRC_H as isize - 1) as usize;
            raw[y * SRC_W + x] as u16
        };
        for y in 0..H {
            let yi = y as isize;
            for x in 0..W {
                let sx = (x + X0) as isize;
                let c = px(sx, yi);
                let (r, g, b) = match (y & 1, (x + X0) & 1) {
                    // GRBG: row0 = G R G R, row1 = B G B G
                    (0, 0) => ((px(sx - 1, yi) + px(sx + 1, yi)) / 2, c, (px(sx, yi - 1) + px(sx, yi + 1)) / 2),
                    (0, _) => (
                        c,
                        (px(sx - 1, yi) + px(sx + 1, yi) + px(sx, yi - 1) + px(sx, yi + 1)) / 4,
                        (px(sx - 1, yi - 1) + px(sx + 1, yi - 1) + px(sx - 1, yi + 1) + px(sx + 1, yi + 1)) / 4,
                    ),
                    (_, 0) => (
                        (px(sx - 1, yi - 1) + px(sx + 1, yi - 1) + px(sx - 1, yi + 1) + px(sx + 1, yi + 1)) / 4,
                        (px(sx - 1, yi) + px(sx + 1, yi) + px(sx, yi - 1) + px(sx, yi + 1)) / 4,
                        c,
                    ),
                    _ => ((px(sx, yi - 1) + px(sx, yi + 1)) / 2, c, (px(sx - 1, yi) + px(sx + 1, yi)) / 2),
                };
                let i = (y * W + x) * 3;
                self.rgb[i] = r;
                self.rgb[i + 1] = g;
                self.rgb[i + 2] = b;
            }
        }
    }

    fn update_stats(&mut self) {
        let mut sum = [0u64; 3];
        let mut hist = [0u32; 256];
        let mut n = 0u32;
        for y in (0..H).step_by(2) {
            for x in (0..W).step_by(2) {
                let i = (y * W + x) * 3;
                let (r, g, b) = (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]);
                sum[0] += r as u64;
                sum[1] += g as u64;
                sum[2] += b as u64;
                let l = ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8).min(255);
                hist[l as usize] += 1;
                n += 1;
            }
        }
        // White balance: geometric mean of gray-world and "bright pixels are neutral"
        // (top ~25% luma, excluding near-clipped). Gray-world alone tints the image when one
        // coloured surface dominates; bright-pixel alone is noisy. Smoothed over time.
        if self.p.white_balance {
            let mut lim_lo = 0usize;
            let mut acc = 0u32;
            for (v, &c) in hist.iter().enumerate().rev() {
                acc += c;
                if acc > n / 4 {
                    lim_lo = v;
                    break;
                }
            }
            let mut bs = [0u64; 3];
            for y in (0..H).step_by(2) {
                for x in (0..W).step_by(2) {
                    let i = (y * W + x) * 3;
                    let (r, g, b) = (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]);
                    let l = ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as usize;
                    if l >= lim_lo && r < 250 && g < 250 && b < 250 {
                        bs[0] += r as u64;
                        bs[1] += g as u64;
                        bs[2] += b as u64;
                    }
                }
            }
            let gw = |s: &[u64; 3], c: usize| s[1].max(1) as f32 / s[c].max(1) as f32;
            let est = |c: usize| if bs[1] > 0 { (gw(&sum, c) * gw(&bs, c)).sqrt() } else { gw(&sum, c) };
            let target = [est(0).clamp(0.5, 2.5), 1.0, est(2).clamp(0.5, 2.5)];
            let a = if self.first { 1.0 } else { 0.08 };
            for c in 0..3 {
                self.wb[c] += (target[c] - self.wb[c]) * a;
            }
        } else {
            self.wb = [1.0; 3];
        }
        // Smoothed auto-levels from the 0.5% / 99.5% luma percentiles.
        if self.p.auto_levels {
            let pct = |q: f32| {
                let lim = (n as f32 * q) as u32;
                let mut acc = 0;
                for (v, &c) in hist.iter().enumerate() {
                    acc += c;
                    if acc > lim {
                        return v as f32;
                    }
                }
                255.0
            };
            let wb_g = (self.wb[0] + 1.0 + self.wb[2]) / 3.0;
            let mut lo = pct(0.005) * 0.85 * wb_g;
            let mut hi = (pct(0.995) * wb_g).max(lo + 150.0).min(255.0 * 1.6);
            if hi - lo < 150.0 {
                lo = (hi - 150.0).max(0.0);
            }
            hi = hi.max(lo + 150.0);
            let a = if self.first { 1.0 } else { 0.12 };
            self.lo += (lo - self.lo) * a;
            self.hi += (hi - self.hi) * a;
        } else {
            self.lo = 0.0;
            self.hi = 255.0;
        }
        self.first = false;
    }

    fn lut(&self) -> [u8; 1024] {
        let mut lut = [0u8; 1024];
        let span = (self.hi - self.lo).max(1.0);
        let inv_g = 1.0 / self.p.gamma.max(0.1);
        for (v, o) in lut.iter_mut().enumerate() {
            let t = ((v as f32 - self.lo) / span).clamp(0.0, 1.0);
            *o = (t.powf(inv_g) * 255.0 + 0.5) as u8;
        }
        lut
    }

    /// Process one raw frame into `nv12` (NV12_SIZE bytes, 640x480).
    pub fn process(&mut self, raw: &[u8], nv12: &mut [u8]) {
        assert!(raw.len() >= SRC_W * SRC_H && nv12.len() >= NV12_SIZE);
        self.demosaic(raw);
        self.update_stats();
        let lut = self.lut();
        let g = [(self.wb[0] * 256.0) as u32, (self.wb[1] * 256.0) as u32, (self.wb[2] * 256.0) as u32];
        let key = (self.p.filter, (self.p.strength.clamp(0.0, 1.0) * 1000.0) as u32);
        if self.look.as_ref().map(|l| l.key) != Some(key) {
            self.look = Some(Look::build(self.p.filter, self.p.strength, &self.p));
        }
        let look = self.look.as_ref().unwrap();
        let sat = (look.saturation * 256.0) as i32;
        let (cr, cg, cb) = (&look.curve[0], &look.curve[1], &look.curve[2]);
        let grain = look.grain;
        let mut rng = self.rng;
        let (yplane, uv) = nv12.split_at_mut(OUT_W * OUT_H);

        for y0 in 0..H {
            for x0 in 0..W {
                let i = y0 * W + x0;
                // destination pixel (mirrored horizontally if requested)
                let o = if self.p.mirror { y0 * W + (W - 1 - x0) } else { i };
                let vg = look.vignette[o] as i32;
                let r = lut[((self.rgb[i * 3] as u32 * g[0]) >> 8).min(1023) as usize] as i32;
                let gg = lut[((self.rgb[i * 3 + 1] as u32 * g[1]) >> 8).min(1023) as usize] as i32;
                let b = lut[((self.rgb[i * 3 + 2] as u32 * g[2]) >> 8).min(1023) as usize] as i32;
                let r = cr[((r * vg) >> 8).clamp(0, 255) as usize] as i32;
                let gg = cg[((gg * vg) >> 8).clamp(0, 255) as usize] as i32;
                let b = cb[((b * vg) >> 8).clamp(0, 255) as usize] as i32;
                // BT.601 limited range, fixed point (x256)
                let mut y = (16 * 256 + 66 * r + 129 * gg + 25 * b + 128) >> 8;
                let (u, v) = if look.mono {
                    (128, 128)
                } else {
                    let u = (-38 * r - 74 * gg + 112 * b) >> 8;
                    let v = (112 * r - 94 * gg - 18 * b) >> 8;
                    (((u * sat) >> 8) + 128, ((v * sat) >> 8) + 128)
                };
                if grain > 0 {
                    // xorshift32 film grain, strongest in the mid-tones
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    let n = ((rng >> 24) as i32 - 128) * grain >> 7;
                    let mid = 255 - (2 * (y - 126)).abs().min(255);
                    y += (n * (128 + mid / 2)) >> 8;
                }
                self.ysmall[o] = y.clamp(16, 235) as u8;
                uv[o * 2] = u.clamp(16, 240) as u8;
                uv[o * 2 + 1] = v.clamp(16, 240) as u8;
                // keep an RGB copy (reconstructed from the final YUV) for stills
                let (yf, uf, vf) = ((y.clamp(16, 235) - 16) * 298, ((u - 128).clamp(-112, 112)), ((v - 128).clamp(-112, 112)));
                self.out_rgb[o * 3] = ((yf + 409 * vf + 128) >> 8).clamp(0, 255) as u8;
                self.out_rgb[o * 3 + 1] = ((yf - 100 * uf - 208 * vf + 128) >> 8).clamp(0, 255) as u8;
                self.out_rgb[o * 3 + 2] = ((yf + 516 * uf + 128) >> 8).clamp(0, 255) as u8;
            }
        }
        self.rng = rng;

        // Mild unsharp mask on the small luma (cross-shaped blur), then 2x bilinear upscale.
        let k = (look.sharpen * 256.0) as i32;
        let ys = &self.ysmall;
        let sharp = |x: usize, y: usize| -> i32 {
            let c = ys[y * W + x] as i32;
            if k == 0 {
                return c;
            }
            let l = ys[y * W + x.saturating_sub(1)] as i32;
            let r = ys[y * W + (x + 1).min(W - 1)] as i32;
            let u = ys[y.saturating_sub(1) * W + x] as i32;
            let d = ys[(y + 1).min(H - 1) * W + x] as i32;
            let blur = (c * 4 + l + r + u + d) >> 3;
            (c + (((c - blur) * k) >> 8)).clamp(16, 235)
        };
        let mut row_a = vec![0i32; W];
        let mut row_b = vec![0i32; W];
        for x in 0..W {
            row_b[x] = sharp(x, 0);
        }
        for sy in 0..H {
            std::mem::swap(&mut row_a, &mut row_b);
            let ny = (sy + 1).min(H - 1);
            for x in 0..W {
                row_b[x] = sharp(x, ny);
            }
            // Output rows 2*sy+1 and 2*sy+2 lie between source rows sy and sy+1 (weights 3:1, 1:3).
            for (dy, (wa, wb)) in [(1usize, (3, 1)), (2usize, (1, 3))] {
                let oy = 2 * sy + dy;
                if oy >= OUT_H {
                    continue;
                }
                let orow = &mut yplane[oy * OUT_W..(oy + 1) * OUT_W];
                for x in 0..W {
                    let c0 = row_a[x] * wa + row_b[x] * wb;
                    let c1 = row_a[(x + 1).min(W - 1)] * wa + row_b[(x + 1).min(W - 1)] * wb;
                    let ox = 2 * x + 1;
                    orow[ox] = ((c0 * 3 + c1 + 8) >> 4) as u8;
                    if ox + 1 < OUT_W {
                        orow[ox + 1] = ((c0 + c1 * 3 + 8) >> 4) as u8;
                    }
                }
                orow[0] = orow[1];
            }
        }
        // top row
        let (first, rest) = yplane.split_at_mut(OUT_W);
        first.copy_from_slice(&rest[..OUT_W]);
    }
}

/// Auto-exposure that protects the frame rate: exposure is capped (default 260 units
/// ≈ 100 ms = 10 fps) and quantised to 10 ms steps (26 units) against 50 Hz light flicker;
/// gain covers the rest.
pub struct AutoExposure {
    pub exposure: u16,
    pub gain: u8,
    pub target: f64,
    pub max_exposure: u16,
    pub flicker_step: u16, // 26 = 50 Hz, 22 = 60 Hz, 1 = off
    settle: u8,
}

impl Default for AutoExposure {
    fn default() -> Self {
        Self { exposure: 156, gain: 96, target: 100.0, max_exposure: 260, flicker_step: 26, settle: 0 }
    }
}

impl AutoExposure {
    /// Feed the raw-frame mean; returns true if (exposure, gain) changed.
    pub fn update(&mut self, mean: f64) -> bool {
        if self.settle > 0 {
            // the next frame was already being exposed with the old settings
            self.settle -= 1;
            return false;
        }
        let err = self.target / mean.max(1.0);
        if (0.88..1.12).contains(&err) {
            return false;
        }
        // damped proportional step in "exposure x gain" space
        let step = err.powf(0.7).clamp(0.5, 2.0);
        let total = (self.exposure as f64 * self.gain as f64 * step).max(1.0);
        let base_gain = 64.0;
        let mut e = (total / base_gain).clamp(1.0, self.max_exposure as f64);
        if self.flicker_step > 1 && e >= self.flicker_step as f64 {
            e = (e / self.flicker_step as f64).floor() * self.flicker_step as f64;
        }
        let g = (total / e).clamp(16.0, 255.0);
        let (ne, ng) = (e.round() as u16, g.round() as u8);
        let changed = ne != self.exposure || ng != self.gain;
        self.exposure = ne;
        self.gain = ng;
        if changed {
            self.settle = 1;
        }
        changed
    }
}
