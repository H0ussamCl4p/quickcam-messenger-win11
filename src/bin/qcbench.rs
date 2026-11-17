//! qcbench: frame rate and latency measurements.

use std::time::{Duration, Instant};

use qccam::capture::bayer_to_nv12;
use qccam::isp::Isp;
use qccam::st6422::{self, Controls, FrameAssembler, MODE_LARGE, MODE_SMALL};
use qccam::winusb::{self, Device};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // Conversion cost on synthetic data
    let raw: Vec<u8> = (0..324 * 240).map(|i| (i * 7 % 251) as u8).collect();
    let mut isp = Isp::default();
    let t = Instant::now();
    for _ in 0..20 {
        std::hint::black_box(bayer_to_nv12(&raw, &mut isp));
    }
    println!("convert 324x240 bayer -> 640x480 NV12: {:.1} ms/frame", t.elapsed().as_secs_f64() * 1000.0 / 20.0);

    let path = winusb::find_device_path(st6422::VID, st6422::PID)?;
    let dev = Device::open(&path)?;
    let pipe = dev.pipe(1, 0).ok_or("no pipe")?;
    let bpi = pipe.bytes_per_interval.max(pipe.max_packet as u32);

    let only = std::env::args().nth(1);
    let configs: Vec<(&str, st6422::Mode, u16, u32)> = vec![
        ("large", MODE_LARGE, 847, 32),
        ("large", MODE_LARGE, 1023, 32),
        ("large", MODE_LARGE, 1023, 8),
        ("small", MODE_SMALL, 300, 8),
        ("small", MODE_SMALL, 1023, 8),
    ];
    println!("\n{:<6} {:>5} {:>4} {:>5} {:>4} | {:>5} {:>8} {:>8} {:>6} {:>7}", "mode", "pkt", "ppx", "expo", "gain", "fps", "readout", "frm-int", "mean", "broken");
    for (name, mode, pkt, ppx) in configs {
        if let Some(o) = &only {
            if o != name {
                continue;
            }
        }
        for expo in [50u16, 150, 300, 500, 800, 1023] {
            dev.set_alt(0)?;
            st6422::init(&dev)?;
            st6422::apply_controls(&dev, &Controls { exposure: expo, gain: 0x38, brightness: 3 })?;
            dev.set_alt(1)?;
            st6422::start(&dev, &mode, pkt)?;
            let mut asm = FrameAssembler::new(mode);
            let mut frames = 0u32;
            let mut sof_at: Option<Instant> = None;
            let mut last_sof = 0;
            let mut readouts = Vec::new();
            let mut times = Vec::new();
            let mut means = 0.0;
            let t0 = Instant::now();
            dev.iso_stream(pipe.id, bpi, ppx, 6, |p| {
                let now = Instant::now();
                if let Some(f) = asm.push(p) {
                    frames += 1;
                    if frames > 2 {
                        times.push(now);
                        if let Some(s) = sof_at {
                            readouts.push(now - s);
                        }
                        means += qccam::image::mean(&f);
                    }
                }
                if asm.sof_seen != last_sof {
                    last_sof = asm.sof_seen;
                    sof_at = Some(now);
                }
                t0.elapsed() < Duration::from_millis(3500)
            })?;
            st6422::stop(&dev)?;
            let n = times.len().max(1) as f64;
            let fps = if times.len() > 1 { (times.len() - 1) as f64 / (*times.last().unwrap() - times[0]).as_secs_f64() } else { 0.0 };
            let ro = readouts.iter().map(|d| d.as_secs_f64()).sum::<f64>() / readouts.len().max(1) as f64 * 1000.0;
            println!(
                "{:<6} {:>5} {:>4} {:>5} {:>4} | {:>5.1} {:>6.1}ms {:>6.1}ms {:>6.1} {:>7}",
                name, pkt, ppx, expo, 0x38, fps, ro, if fps > 0.0 { 1000.0 / fps } else { 0.0 }, means / n, asm.bad_frames
            );
        }
    }
    dev.set_alt(0)?;
    Ok(())
}
