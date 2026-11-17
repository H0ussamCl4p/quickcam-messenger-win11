//! qcvcam – register/remove the "QuickCam Messenger" virtual camera with Windows 11,
//! and self-test the capture pipeline.
//!
//!   qcvcam test      run camera + NV12 conversion for a few seconds, save captures\vcam_test.png
//!   qcvcam source    create our media source in-process (COM-free check of the DLL logic)
//!   qcvcam install   register the virtual camera (admin; use install.ps1)
//!   qcvcam remove    unregister it (admin; use uninstall.ps1)

use std::time::{Duration, Instant};

use qccam::capture::{self, OUT_H, OUT_W};
use qccam::vcam::{Source, CLSID_QCVCAM_STR};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

const NAME: &str = "QuickCam Messenger (Rust)";

fn main() {
    std::env::set_var("QCVCAM_STDERR", "1");
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let r = match cmd.as_str() {
        "test" => test(),
        "source" => source(),
        "install" => install(),
        "remove" => remove(),
        "live" => live(),
        "set" => set(),
        "config" => show_config(),
        "preview" => preview(),
        _ => {
            eprintln!("usage: qcvcam test [secs] | source | live [secs] | preview | config | set <mirror|filter|strength> <value> | install | remove");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    println!("ok");
}

fn com() -> windows::core::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(0x0002_0070, 0)
    }
}

fn test() -> Result<(), String> {
    let hub = capture::hub();
    let t_start = Instant::now();
    hub.acquire();
    let mut seq = 0;
    let mut live_frames = 0u32;
    let mut first_at = None;
    let mut t_first_live = None;
    let mut ages = Vec::new();
    let mut last = None;
    let secs = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(8u64);
    while t_start.elapsed() < Duration::from_secs(secs) {
        let (f, s) = hub.wait_frame(seq, Duration::from_millis(500));
        if s != seq && f.live {
            // age = time from last USB packet of this frame until a consumer holds it
            ages.push((capture::now_100ns() - f.time) as f64 / 10_000.0);
            live_frames += 1;
            t_first_live.get_or_insert(t_start.elapsed());
            first_at.get_or_insert(Instant::now());
            last = Some(f);
        }
        seq = s;
    }
    hub.release();
    hub.shutdown();
    let fps = first_at.map(|t| (live_frames.saturating_sub(1)) as f64 / t.elapsed().as_secs_f64()).unwrap_or(0.0);
    ages.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("time to first frame: {:?}", t_start.elapsed().min(t_first_live.unwrap_or_default()));
    println!("live frames: {live_frames} in {secs} s  => {fps:.1} fps (steady state)");
    if !ages.is_empty() {
        println!(
            "frame age at consumer (USB end -> ready NV12): median {:.1} ms, max {:.1} ms",
            ages[ages.len() / 2],
            ages[ages.len() - 1]
        );
    }
    let f = last.ok_or("no live frames from the camera")?;
    let f = &f.nv12;
    std::fs::create_dir_all("captures").map_err(|e| e.to_string())?;
    // Convert NV12 back to RGB for a viewable check of the exact bytes apps will get.
    let mut rgb = vec![0u8; OUT_W * OUT_H * 3];
    let (yp, uv) = f.split_at(OUT_W * OUT_H);
    for y in 0..OUT_H {
        for x in 0..OUT_W {
            let yy = (yp[y * OUT_W + x] as f32 - 16.0) * 1.164;
            let ci = ((y / 2) * (OUT_W / 2) + x / 2) * 2;
            let (u, v) = (uv[ci] as f32 - 128.0, uv[ci + 1] as f32 - 128.0);
            let px = &mut rgb[(y * OUT_W + x) * 3..][..3];
            px[0] = (yy + 1.596 * v).clamp(0.0, 255.0) as u8;
            px[1] = (yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8;
            px[2] = (yy + 2.017 * u).clamp(0.0, 255.0) as u8;
        }
    }
    let p = std::path::Path::new("captures/vcam_test.png");
    qccam::image::save_png(p, &rgb, OUT_W, OUT_H, true).map_err(|e| e.to_string())?;
    println!("saved {}", p.display());
    Ok(())
}

/// Drives the media source like Frame Server would: Start, request samples, Stop, Shutdown.
fn source() -> Result<(), String> {
    com().map_err(|e| e.to_string())?;
    unsafe {
        let src = Source::create().map_err(|e| e.to_string())?;
        let pd = src.CreatePresentationDescriptor().map_err(|e| e.to_string())?;
        src.Start(&pd, std::ptr::null(), std::ptr::null()).map_err(|e| format!("Start: {e}"))?;
        let mut stream: Option<IMFMediaStream> = None;
        let mut samples = 0;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(8) && samples < 20 {
            if let Ok(ev) = src.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                let t = ev.GetType().map_err(|e| e.to_string())?;
                println!("source event {t}");
                if t == MENewStream.0 as u32 {
                    let v = ev.GetValue().map_err(|e| e.to_string())?;
                    let unk = windows::core::IUnknown::try_from(&v).map_err(|e| e.to_string())?;
                    let s: IMFMediaStream = windows::core::Interface::cast(&unk).map_err(|e| e.to_string())?;
                    s.RequestSample(None).map_err(|e| format!("RequestSample: {e}"))?;
                    stream = Some(s);
                }
            }
            if let Some(s) = &stream {
                if let Ok(ev) = s.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    let t = ev.GetType().map_err(|e| e.to_string())?;
                    if t == MEMediaSample.0 as u32 {
                        samples += 1;
                        s.RequestSample(None).map_err(|e| e.to_string())?;
                    } else {
                        println!("stream event {t}");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!("samples received: {samples} in {:.1}s", t0.elapsed().as_secs_f32());
        src.Stop().map_err(|e| e.to_string())?;
        src.Shutdown().map_err(|e| e.to_string())?;
        if samples == 0 {
            return Err("media source produced no samples".into());
        }
    }
    Ok(())
}

fn create() -> windows::core::Result<IMFVirtualCamera> {
    com()?;
    let name = HSTRING::from(NAME);
    let id = HSTRING::from(CLSID_QCVCAM_STR);
    unsafe {
        MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_AllUsers,
            PCWSTR(name.as_ptr()),
            PCWSTR(id.as_ptr()),
            None,
        )
    }
}

fn install() -> Result<(), String> {
    let vc = create().map_err(|e| format!("MFCreateVirtualCamera: {e} (run from an admin terminal via install.ps1)"))?;
    unsafe {
        vc.Start(None::<&IMFAsyncCallback>).map_err(|e| format!("IMFVirtualCamera::Start: {e}"))?;
        let _ = vc.Shutdown();
    }
    println!("virtual camera \"{NAME}\" registered");
    Ok(())
}

fn remove() -> Result<(), String> {
    let vc = create().map_err(|e| format!("MFCreateVirtualCamera: {e}"))?;
    unsafe {
        vc.Remove().map_err(|e| format!("IMFVirtualCamera::Remove: {e}"))?;
    }
    println!("virtual camera \"{NAME}\" removed");
    Ok(())
}

/// Opens the installed virtual camera through the normal Windows camera API (Frame Server),
/// exactly like an app, and measures fps, duplicate frames, timestamp sanity and latency.
fn live() -> Result<(), String> {
    use windows::core::Interface;
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(15);
    com().map_err(|e| e.to_string())?;
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| e.to_string())?;
        let attrs = attrs.unwrap();
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(|e| e.to_string())?;
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFEnumDeviceSources(&attrs, &mut arr, &mut n).map_err(|e| format!("MFEnumDeviceSources: {e}"))?;
        let devs = std::slice::from_raw_parts(arr, n as usize);
        let mut found = None;
        for d in devs.iter().flatten() {
            let mut p = windows::core::PWSTR::null();
            let mut len = 0u32;
            if d.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).is_ok() {
                let name = p.to_string().unwrap_or_default();
                println!("camera: {name}");
                if name.starts_with(NAME) {
                    found = Some(d.clone());
                }
            }
        }
        let act = found.ok_or(format!("\"{NAME}\" not listed by Windows"))?;
        let t_open = Instant::now();
        let src: IMFMediaSource = act.ActivateObject().map_err(|e| format!("ActivateObject: {e}"))?;
        let reader = MFCreateSourceReaderFromMediaSource(&src, None).map_err(|e| format!("source reader: {e}"))?;
        let mt = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32).map_err(|e| e.to_string())?;
        let fs = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let fr = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        println!("format: {}x{} @ {}/{} fps", fs >> 32, fs & 0xffff_ffff, fr >> 32, fr & 0xffff_ffff);

        let t0 = Instant::now();
        let (mut samples, mut dups, mut nonmono, mut first) = (0u32, 0u32, 0u32, None);
        let mut last_ts = i64::MIN;
        let mut last_sig = 0u64;
        let mut lat = Vec::new();
        while t0.elapsed() < Duration::from_secs(secs) {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut s: Option<IMFSample> = None;
            reader
                .ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))
                .map_err(|e| format!("ReadSample: {e}"))?;
            let Some(s) = s else { continue };
            let now = MFGetSystemTime();
            first.get_or_insert(t_open.elapsed());
            samples += 1;
            if ts <= last_ts {
                nonmono += 1;
            }
            last_ts = ts;
            if let Ok(st) = s.GetSampleTime() {
                lat.push((now - st) as f64 / 10_000.0);
            }
            // signature of the frame content to spot repeated frames
            let buf = s.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| e.to_string())?;
            let data = std::slice::from_raw_parts(p, len as usize);
            let sig = data.iter().step_by(997).fold(1469598103934665603u64, |h, &b| (h ^ b as u64).wrapping_mul(1099511628211));
            buf.Unlock().ok();
            if sig == last_sig {
                dups += 1;
            }
            last_sig = sig;
        }
        let _ = src.Shutdown();
        let _ = act.ShutdownObject();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("first sample after {:?}", first.unwrap_or_default());
        println!("samples: {samples} in {secs} s => {:.1} fps, repeated frames: {dups}, non-increasing timestamps: {nonmono}", samples as f64 / secs as f64);
        if !lat.is_empty() {
            println!("sample age at app (capture -> app): median {:.1} ms, p95 {:.1} ms", lat[lat.len() / 2], lat[lat.len() * 95 / 100]);
        }
        let _ = first.ok_or("no samples received")?;
        let _ = Interface::as_raw(&reader);
    }
    Ok(())
}

fn show_config() -> Result<(), String> {
    let s = qccam::config::Settings::load();
    println!("{}\nmirror = {}\nfilter = {}\nstrength = {:.2}", qccam::config::path().display(), s.mirror, s.filter.name(), s.strength);
    Ok(())
}

/// qcvcam set filter vintage | set mirror off | set strength 0.6  (applies live within ~1 s)
fn set() -> Result<(), String> {
    let a: Vec<String> = std::env::args().skip(2).collect();
    let (k, v) = match a.as_slice() {
        [k, v] => (k, v),
        _ => return Err("usage: qcvcam set <mirror|filter|strength> <value>".into()),
    };
    let mut s = qccam::config::Settings::load();
    s.set(k, v)?;
    s.save().map_err(|e| format!("cannot write {}: {e}", qccam::config::path().display()))?;
    show_config()
}

/// Grab ~3 s from the camera and render the same frame with every look:
/// captures/look_none.png, look_vintage.png, look_noir.png (640x480, as apps receive it).
fn preview() -> Result<(), String> {
    use qccam::isp::{AutoExposure, Filter, Isp, IspParams, NV12_SIZE};
    use qccam::st6422::{self, Controls, FrameAssembler, MODE_LARGE};
    let dev = qccam::winusb::Device::open(&qccam::winusb::find_device_path(st6422::VID, st6422::PID)?)?;
    let pipe = dev.pipe(1, 0).ok_or("no streaming endpoint")?;
    dev.set_alt(0)?;
    st6422::init(&dev)?;
    let mut ae = AutoExposure::default();
    st6422::apply_controls(&dev, &Controls { exposure: ae.exposure, gain: ae.gain, brightness: 3 })?;
    dev.set_alt(1)?;
    st6422::start(&dev, &MODE_LARGE, 847)?;
    let mirror = qccam::config::Settings::load().mirror;
    let looks = [Filter::None, Filter::Vintage, Filter::Noir];
    let mut isps: Vec<Isp> = looks.iter().map(|&f| Isp::new(IspParams { filter: f, mirror, ..Default::default() })).collect();
    let mut outs = vec![vec![0u8; NV12_SIZE]; looks.len()];
    let mut asm = FrameAssembler::new(MODE_LARGE);
    let mut n = 0;
    let mut err = None;
    let r = dev.iso_stream(pipe.id, pipe.bytes_per_interval.max(pipe.max_packet as u32), 8, 8, |p| {
        if let Some(raw) = asm.push(p) {
            n += 1;
            if ae.update(qccam::image::mean(&raw)) {
                if let Err(e) = st6422::apply_controls(&dev, &Controls { exposure: ae.exposure, gain: ae.gain, brightness: 3 }) {
                    err = Some(e);
                    return false;
                }
            }
            for (isp, out) in isps.iter_mut().zip(outs.iter_mut()) {
                isp.process(&raw, out);
            }
        }
        n < 30
    });
    let _ = st6422::stop(&dev);
    let _ = dev.set_alt(0);
    r?;
    if let Some(e) = err {
        return Err(e);
    }
    std::fs::create_dir_all("captures").map_err(|e| e.to_string())?;
    for (f, out) in looks.iter().zip(&outs) {
        let p = format!("captures/look_{}.png", f.name());
        qccam::image::save_png(std::path::Path::new(&p), &nv12_to_rgb(out), OUT_W, OUT_H, true).map_err(|e| e.to_string())?;
        println!("saved {p}");
    }
    Ok(())
}

fn nv12_to_rgb(f: &[u8]) -> Vec<u8> {
    let mut rgb = vec![0u8; OUT_W * OUT_H * 3];
    let (yp, uv) = f.split_at(OUT_W * OUT_H);
    for y in 0..OUT_H {
        for x in 0..OUT_W {
            let yy = (yp[y * OUT_W + x] as f32 - 16.0) * 1.164;
            let ci = ((y / 2) * (OUT_W / 2) + x / 2) * 2;
            let (u, v) = (uv[ci] as f32 - 128.0, uv[ci + 1] as f32 - 128.0);
            let px = &mut rgb[(y * OUT_W + x) * 3..][..3];
            px[0] = (yy + 1.596 * v).clamp(0.0, 255.0) as u8;
            px[1] = (yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8;
            px[2] = (yy + 2.017 * u).clamp(0.0, 255.0) as u8;
        }
    }
    rgb
}
