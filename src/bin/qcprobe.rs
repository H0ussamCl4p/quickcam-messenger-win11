//! qcprobe – quick experiments: fps/brightness with an extra register write or a gain value.
//! usage: qcprobe <expo> <gain> [reg=val ...]
use std::time::{Duration, Instant};
use qccam::st6422::{self, Controls, FrameAssembler, MODE_LARGE};
use qccam::winusb::{self, Device};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let expo: u16 = a.first().and_then(|s| s.parse().ok()).unwrap_or(150);
    let gain: u8 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(0x38);
    let extra: Vec<(u16, u16)> = a.iter().skip(2).filter_map(|s| {
        let (r, v) = s.split_once('=')?;
        Some((u16::from_str_radix(r.trim_start_matches("0x"), 16).ok()?, u16::from_str_radix(v.trim_start_matches("0x"), 16).ok()?))
    }).collect();
    let dev = Device::open(&winusb::find_device_path(st6422::VID, st6422::PID).unwrap()).unwrap();
    let pipe = dev.pipe(1, 0).unwrap();
    dev.set_alt(0).unwrap();
    st6422::init(&dev).unwrap();
    st6422::apply_controls(&dev, &Controls { exposure: expo, gain, brightness: 3 }).unwrap();
    for &(r, v) in &extra { st6422::write_reg(&dev, r, v).unwrap(); }
    if !extra.is_empty() { st6422::write_reg(&dev, 0x143f, 1).unwrap(); }
    dev.set_alt(1).unwrap();
    st6422::start(&dev, &MODE_LARGE, 1023).unwrap();
    let mut asm = FrameAssembler::new(MODE_LARGE);
    let (mut n, mut first, mut last, mut m) = (0u32, None, None, 0.0);
    let t0 = Instant::now();
    let r = dev.iso_stream(pipe.id, 1023, 8, 6, |p| {
        if let Some(f) = asm.push(p) { n += 1; if n > 2 { let now = Instant::now(); first.get_or_insert(now); last = Some(now); m = qccam::image::mean(&f); } }
        t0.elapsed() < Duration::from_millis(2500)
    });
    let _ = st6422::stop(&dev);
    let _ = dev.set_alt(0);
    let fps = match (first, last) { (Some(a), Some(b)) if n > 3 => (n - 3) as f64 / (b - a).as_secs_f64(), _ => 0.0 };
    println!("expo {expo:4} gain {gain:3} {extra:x?} -> fps {fps:5.1} mean {m:5.1} broken {} {:?}", asm.bad_frames, r.err());
}
