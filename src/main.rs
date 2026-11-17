//! qcsnap – grab still pictures from a Logitech QuickCam Messenger (046d:08f6)
//! on Windows without a vendor driver, using WinUSB from user mode.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use qccam::st6422::{self, Controls, FrameAssembler, Mode, MODE_LARGE, MODE_SMALL};
use qccam::winusb::{self, Device, Result};
use qccam::image;

const USAGE: &str = "\
qcsnap – Logitech QuickCam Messenger (046d:08f6) snapshot tool

USAGE:
  qcsnap info                 Show what Windows/WinUSB sees on the camera
  qcsnap snap [options]       Capture frames and save PNGs

SNAP OPTIONS:
  --small                     162x120 instead of 324x240
  --frames N                  Frames to capture (default 15; the last one is saved as snapshot.png)
  --all                       Save every captured frame, not just the last
  --exposure N                0..1023 (camera default 256)
  --gain N                    0..255  (camera default 56)
  --brightness N              0..31   (camera default 3)
  --no-wb                     Disable automatic white balance
  --out DIR                   Output folder (default: captures)
  --timeout S                 Give up after S seconds (default 10)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("info") => info(),
        Some("snap") => snap(&args[1..]),
        _ => {
            print!("{USAGE}");
            return;
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn open() -> Result<Device> {
    let path = winusb::find_device_path(st6422::VID, st6422::PID)?;
    println!("device path: {path}");
    Device::open(&path)
}

fn pipe_kind(k: i32) -> &'static str {
    match k {
        0 => "control",
        1 => "isochronous",
        2 => "bulk",
        3 => "interrupt",
        _ => "?",
    }
}

fn info() -> Result<()> {
    let dev = open()?;
    println!("WinUSB opened OK");
    match dev.speed() {
        Some(1) => println!("speed: low/full (USB 1.1)"),
        Some(3) => println!("speed: high (USB 2.0)"),
        Some(s) => println!("speed code: {s}"),
        None => println!("speed: unknown"),
    }
    for alt in 0u8..8 {
        let Some(d) = dev.interface(alt) else { break };
        println!(
            "alt {alt}: class {:02x}/{:02x}/{:02x}, {} endpoint(s)",
            d.bInterfaceClass, d.bInterfaceSubClass, d.bInterfaceProtocol, d.bNumEndpoints
        );
        for i in 0..d.bNumEndpoints {
            if let Some(p) = dev.pipe(alt, i) {
                println!(
                    "    ep 0x{:02x} {:<11} maxpacket {:4}  bytes/interval {:4}  interval {}",
                    p.id, pipe_kind(p.kind), p.max_packet, p.bytes_per_interval, p.interval
                );
            }
        }
    }
    // Reading a few bridge registers proves the control channel works.
    for reg in [0x1440u16, 0x1505, 0x15c1, 0x143a] {
        match st6422::read_reg(&dev, reg) {
            Ok(v) => println!("reg 0x{reg:04x} = 0x{v:02x}"),
            Err(e) => println!("reg 0x{reg:04x}: {e}"),
        }
    }
    Ok(())
}

fn arg_num<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .and_then(|v| v.parse().ok())
            .map(Some)
            .ok_or_else(|| format!("{name} needs a numeric value")),
    }
}

fn snap(args: &[String]) -> Result<()> {
    let mode: Mode = if args.iter().any(|a| a == "--small") { MODE_SMALL } else { MODE_LARGE };
    let want: usize = arg_num(args, "--frames")?.unwrap_or(25).max(1);
    let save_all = args.iter().any(|a| a == "--all");
    let wb = !args.iter().any(|a| a == "--no-wb");
    let timeout = Duration::from_secs(arg_num(args, "--timeout")?.unwrap_or(10));
    let out: PathBuf = match args.iter().position(|a| a == "--out") {
        Some(i) => args.get(i + 1).ok_or("--out needs a folder")?.into(),
        None => "captures".into(),
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;

    let dev = open()?;
    dev.set_alt(0)?;
    println!("initialising ST6422 bridge…");
    st6422::init(&dev)?;

    let exposure: Option<u16> = arg_num(args, "--exposure")?;
    let gain: Option<u8> = arg_num(args, "--gain")?;
    let brightness: Option<u8> = arg_num(args, "--brightness")?;
    if exposure.is_some() || gain.is_some() || brightness.is_some() {
        st6422::apply_controls(
            &dev,
            &Controls { exposure: exposure.unwrap_or(256), gain: gain.unwrap_or(0x38), brightness: brightness.unwrap_or(3) },
        )?;
    }

    let d1 = dev.interface(1).ok_or("camera has no alternate setting 1 for streaming")?;
    if d1.bNumEndpoints == 0 {
        return Err("alt setting 1 has no endpoints".into());
    }
    let pipe = dev.pipe(1, 0).ok_or("could not query the streaming endpoint")?;
    println!(
        "stream endpoint 0x{:02x} ({}), maxpacket {}, bytes/interval {}",
        pipe.id, pipe_kind(pipe.kind), pipe.max_packet, pipe.bytes_per_interval
    );
    if pipe.kind != 1 {
        return Err("streaming endpoint is not isochronous – unexpected for this camera".into());
    }
    let pkt = mode.packet_size.min(pipe.max_packet);
    dev.set_alt(1)?;
    println!("starting {}x{} stream, camera packet size {pkt}", mode.width, mode.height);
    st6422::start(&dev, &mode, pkt)?;

    // Auto-exposure unless the user set exposure/gain by hand; the full ISP for 324x240.
    let auto = exposure.is_none() && gain.is_none();
    let mut ae = qccam::isp::AutoExposure::default();
    if auto {
        st6422::apply_controls(&dev, &Controls { exposure: ae.exposure, gain: ae.gain, brightness: brightness.unwrap_or(3) })?;
    }
    let use_isp = mode.width == qccam::isp::SRC_W;
    let mut isp = qccam::isp::Isp::new(qccam::isp::IspParams { white_balance: wb, ..Default::default() });
    qccam::config::Settings::load().apply(&mut isp.p);
    let mut nv12 = vec![0u8; qccam::isp::NV12_SIZE];
    let mut processed: Vec<Vec<u8>> = Vec::new();
    let mut ctrl_err = None;

    let mut asm = FrameAssembler::new(mode);
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let t0 = Instant::now();
    let stream = dev.iso_stream(pipe.id, pipe.bytes_per_interval.max(pipe.max_packet as u32), 8, 8, |p| {
        if let Some(f) = asm.push(p) {
            let m = image::mean(&f);
            if auto && ae.update(m) {
                let c = Controls { exposure: ae.exposure, gain: ae.gain, brightness: brightness.unwrap_or(3) };
                if let Err(e) = st6422::apply_controls(&dev, &c) {
                    ctrl_err = Some(e);
                    return false;
                }
            }
            println!("  frame {:2}: raw mean {m:5.1}  exposure {:4} gain {:3}", frames.len() + 1, ae.exposure, ae.gain);
            if use_isp {
                isp.process(&f, &mut nv12);
                processed.push(isp.rgb().to_vec());
            }
            frames.push(f);
        }
        frames.len() < want && t0.elapsed() < timeout
    });
    if let Some(e) = ctrl_err {
        return Err(e);
    }
    let _ = st6422::stop(&dev);
    let _ = dev.set_alt(0);
    let stats = stream?;

    println!(
        "\nUSB: {} transfers ({} failed), {} packets ({} errors, {} empty), {} bytes in {:.1}s",
        stats.transfers, stats.xfer_errors, stats.packets, stats.packet_errors, stats.empty_packets, stats.bytes,
        t0.elapsed().as_secs_f32()
    );
    println!("frames: {} start / {} end markers, {} complete, {} broken", asm.sof_seen, asm.eof_seen, frames.len(), asm.bad_frames);

    if frames.is_empty() {
        println!("chunk ids seen: {:?}", asm.chunk_ids.iter().map(|(k, v)| format!("{k:04x}x{v}")).collect::<Vec<_>>());
        return Err("no complete frame received".into());
    }

    let (w, h) = (mode.width, mode.height);
    let to_save: Vec<(usize, &Vec<u8>)> = if save_all {
        frames.iter().enumerate().collect()
    } else {
        vec![(frames.len() - 1, frames.last().unwrap())]
    };
    for (i, f) in to_save {
        let name = if save_all { format!("frame_{:03}.png", i + 1) } else { "snapshot.png".into() };
        let p = out.join(&name);
        if use_isp {
            let (iw, ih) = (qccam::isp::W, qccam::isp::H);
            image::save_png(&p, &processed[i], iw, ih, true).map_err(|e| e.to_string())?;
        } else {
            image::save_png(&p, &image::debayer_grbg(f, w, h, wb), w, h, true).map_err(|e| e.to_string())?;
        }
        println!("saved {}", p.display());
    }
    let raw = out.join("last_raw_bayer.png");
    image::save_png(&raw, frames.last().unwrap(), w, h, false).map_err(|e| e.to_string())?;
    println!("saved {} (unprocessed sensor data)", raw.display());
    Ok(())
}
