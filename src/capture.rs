//! Capture thread shared by all consumers in the process.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::image::mean;
use crate::isp::{AutoExposure, Isp};
use crate::st6422::{self, Controls, FrameAssembler, MODE_LARGE};
use crate::vlog;
use crate::winusb::{self, Device};

pub use crate::isp::{NV12_SIZE, OUT_H, OUT_W};

pub const FPS: u32 = 10;
const LINGER: Duration = Duration::from_secs(8);
const PACKETS_PER_XFER: u32 = 8;
const XFERS: usize = 8;

pub struct Frame {
    pub nv12: Vec<u8>,
    /// MFGetSystemTime() (100 ns units, QPC based) when the last packet arrived.
    pub time: i64,
    pub live: bool,
}

struct Shared {
    latest: Arc<Frame>,
    seq: u64,
}

pub struct Hub {
    shared: Mutex<Shared>,
    cv: Condvar,
    stop: AtomicBool,
    users: AtomicUsize,
    idle_since: Mutex<Option<Instant>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Poison-tolerant lock, so a panic never spreads into Frame Server.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now_100ns() -> i64 {
    unsafe { windows::Win32::Media::MediaFoundation::MFGetSystemTime() }
}

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

pub fn hub() -> Arc<Hub> {
    HUB.get_or_init(|| {
        Arc::new(Hub {
            shared: Mutex::new(Shared { latest: Arc::new(placeholder(0)), seq: 0 }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            users: AtomicUsize::new(0),
            idle_since: Mutex::new(None),
            thread: Mutex::new(None),
        })
    })
    .clone()
}

impl Hub {
    /// One more consumer; starts the camera if needed (or cancels a pending linger stop).
    pub fn acquire(self: &Arc<Self>) {
        self.users.fetch_add(1, Ordering::SeqCst);
        *lock(&self.idle_since) = None;
        let mut t = lock(&self.thread);
        if t.as_ref().is_some_and(|h| h.is_finished()) {
            let _ = t.take().unwrap().join();
        }
        if t.is_none() {
            self.stop.store(false, Ordering::SeqCst);
            let me = self.clone();
            *t = Some(std::thread::spawn(move || worker(me)));
            vlog!("capture: worker started");
        }
    }

    /// One consumer gone; the worker stops by itself after LINGER with no consumers.
    pub fn release(&self) {
        let prev = self.users.fetch_sub(1, Ordering::SeqCst);
        if prev <= 1 {
            self.users.store(0, Ordering::SeqCst);
            *lock(&self.idle_since) = Some(Instant::now());
        }
    }

    /// Stop now (used by tools at exit).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cv.notify_all();
        if let Some(h) = lock(&self.thread).take() {
            let _ = h.join();
        }
    }

    fn should_stop(&self) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return true;
        }
        if self.users.load(Ordering::SeqCst) == 0 {
            if let Some(t) = *lock(&self.idle_since) {
                return t.elapsed() > LINGER;
            }
        }
        false
    }

    /// Wait for a frame newer than `after_seq`; after `timeout` returns the latest anyway.
    pub fn wait_frame(&self, after_seq: u64, timeout: Duration) -> (Arc<Frame>, u64) {
        let g = lock(&self.shared);
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |s| s.seq <= after_seq).unwrap_or_else(|e| e.into_inner());
        (g.latest.clone(), g.seq)
    }

    fn publish(&self, f: Frame) {
        let mut s = lock(&self.shared);
        s.latest = Arc::new(f);
        s.seq += 1;
        drop(s);
        self.cv.notify_all();
    }
}

fn worker(hub: Arc<Hub>) {
    let mut n = 0u64;
    let mut ae = AutoExposure::default(); // persists across reconnects: fast re-convergence
    let mut isp = Isp::default();
    while !hub.should_stop() {
        let r = catch_unwind(AssertUnwindSafe(|| run_camera(&hub, &mut ae, &mut isp)));
        let err = match r {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => e,
            Err(_) => "capture thread panicked".to_string(),
        };
        vlog!("capture: camera error: {err}");
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until && !hub.should_stop() {
            n += 1;
            hub.publish(placeholder(n));
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    vlog!("capture: worker stopped");
}

fn run_camera(hub: &Hub, ae: &mut AutoExposure, isp: &mut Isp) -> Result<(), String> {
    let t_open = Instant::now();
    let path = winusb::find_device_path(st6422::VID, st6422::PID)?;
    let dev = Device::open(&path)?;
    dev.set_alt(0)?;
    st6422::init(&dev)?;
    let controls = |ae: &AutoExposure| Controls { exposure: ae.exposure, gain: ae.gain, brightness: 3 };
    st6422::apply_controls(&dev, &controls(ae))?;

    let mode = MODE_LARGE;
    let pipe = dev.pipe(1, 0).ok_or("no streaming endpoint on alt 1")?;
    dev.set_alt(1)?;
    st6422::start(&dev, &mode, mode.packet_size.min(pipe.max_packet))?;
    vlog!("capture: streaming {}x{} (open took {} ms)", mode.width, mode.height, t_open.elapsed().as_millis());

    let mut asm = FrameAssembler::new(mode);
    let mut frames = 0u64;
    let mut ctrl_err = None;
    let mut proc_ms = 0f64;
    let mut watcher = crate::config::Watcher::default();
    let res = dev.iso_stream(pipe.id, pipe.bytes_per_interval.max(pipe.max_packet as u32), PACKETS_PER_XFER, XFERS, |p| {
        if let Some(raw) = asm.push(p) {
            if let Some(s) = watcher.poll() {
                if (s.mirror, s.filter, s.strength) != (isp.p.mirror, isp.p.filter, isp.p.strength) {
                    vlog!("capture: settings mirror={} filter={} strength={:.2}", s.mirror, s.filter.name(), s.strength);
                }
                s.apply(&mut isp.p);
            }
            let time = now_100ns();
            let t = Instant::now();
            frames += 1;
            if ae.update(mean(&raw)) {
                if let Err(e) = st6422::apply_controls(&dev, &controls(ae)) {
                    ctrl_err = Some(e);
                    return false;
                }
            }
            let mut nv12 = vec![0u8; NV12_SIZE];
            isp.process(&raw, &mut nv12);
            hub.publish(Frame { nv12, time, live: true });
            proc_ms += t.elapsed().as_secs_f64() * 1000.0;
            if frames == 1 {
                vlog!("capture: first frame {} ms after open", t_open.elapsed().as_millis());
            } else if frames % 600 == 0 {
                vlog!("capture: {frames} frames, avg processing {:.2} ms, exposure {} gain {}", proc_ms / frames as f64, ae.exposure, ae.gain);
            }
        }
        !hub.should_stop()
    });
    let _ = st6422::stop(&dev);
    let _ = dev.set_alt(0);
    res?;
    if let Some(e) = ctrl_err {
        return Err(e);
    }
    if !hub.should_stop() {
        return Err("stream ended unexpectedly".into());
    }
    Ok(())
}

/// Shown while the camera isn't delivering.
pub fn placeholder(n: u64) -> Frame {
    let mut f = vec![0u8; NV12_SIZE];
    let (yp, uv) = f.split_at_mut(OUT_W * OUT_H);
    yp.fill(40);
    for c in uv.chunks_exact_mut(2) {
        c[0] = 150;
        c[1] = 120;
    }
    let bar = (n as usize * 8) % OUT_W;
    for y in OUT_H / 2 - 10..OUT_H / 2 + 10 {
        for x in bar..(bar + 40).min(OUT_W) {
            yp[y * OUT_W + x] = 200;
        }
    }
    Frame { nv12: f, time: now_100ns(), live: false }
}

/// Convenience for tools: run the ISP once on a raw frame.
pub fn bayer_to_nv12(raw: &[u8], isp: &mut Isp) -> Vec<u8> {
    let mut out = vec![0u8; NV12_SIZE];
    isp.process(raw, &mut out);
    out
}
