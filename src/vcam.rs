//! Media Foundation "software camera source" for the Windows 11 virtual camera API.
//!
//! Object graph (as Frame Server sees it):
//!   CLSID  --DllGetClassObject-->  Factory (IClassFactory)
//!          --CreateInstance-->     Activate (IMFActivate)
//!          --ActivateObject-->     Source (IMFMediaSourceEx, IKsControl, IMFGetService)
//!                                    └─ Stream (IMFMediaStream2, IKsControl): NV12 640x480
//!
//! Samples are delivered from a per-stream worker thread that pairs each
//! RequestSample token with the newest frame from `capture::hub()`.

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Media::KernelStreaming::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

use crate::capture::{self, lock, Frame, NV12_SIZE, OUT_H, OUT_W};
use crate::vlog;

/// CLSID of our media source activator. Must match install.ps1.
pub const CLSID_QCVCAM: GUID = GUID::from_u128(0x7b1e4c2a_9d3f_4e8b_a6c1_0f2d5e8b3a91);
pub const CLSID_QCVCAM_STR: &str = "{7B1E4C2A-9D3F-4E8B-A6C1-0F2D5E8B3A91}";
const FPS: u64 = capture::FPS as u64;

fn not_found() -> Error {
    Error::from(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0))
}

/// Wrapper to move COM pointers into our worker thread (MF objects here are free-threaded).
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}
unsafe impl<T> Sync for SendBox<T> {}

// ───────────────────────────── media type helpers ─────────────────────────────

fn media_type() -> Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((OUT_W as u64) << 32) | OUT_H as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, (FPS << 32) | 1)?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        mt.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        mt.SetUINT32(&MF_MT_SAMPLE_SIZE, NV12_SIZE as u32)?;
        mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, OUT_W as u32)?;
        Ok(mt)
    }
}

fn set_stream_attrs(a: &IMFAttributes) -> Result<()> {
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        Ok(())
    }
}

fn new_attributes(n: u32) -> Result<IMFAttributes> {
    let mut a = None;
    unsafe { MFCreateAttributes(&mut a, n)? };
    a.ok_or_else(|| Error::from(E_FAIL))
}

fn make_sample(frame: &Frame, time: i64) -> Result<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(NV12_SIZE as u32)?;
        let mut p: *mut u8 = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(frame.nv12.as_ptr(), p, NV12_SIZE.min(frame.nv12.len()));
        buf.Unlock()?;
        buf.SetCurrentLength(NV12_SIZE as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        // Capture time (not delivery time): correct A/V sync and pacing when recording.
        s.SetSampleTime(time)?;
        s.SetSampleDuration(10_000_000 / FPS as i64)?;
        s.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        Ok(s)
    }
}

// ───────────────────────────────── stream ─────────────────────────────────

struct StreamState {
    state: MF_STREAM_STATE,
    tokens: VecDeque<Option<IUnknown>>,
    shutdown: bool,
    worker: Option<JoinHandle<()>>,
    hub_held: bool,
}

struct StreamShared {
    m: Mutex<StreamState>,
    cv: Condvar,
}
unsafe impl Send for StreamShared {}
unsafe impl Sync for StreamShared {}

#[implement(IMFMediaStream2, IKsControl)]
pub struct Stream {
    source: Mutex<Option<SendBox<IMFMediaSource>>>,
    sd: IMFStreamDescriptor,
    queue: IMFMediaEventQueue,
    sh: Arc<StreamShared>,
}

impl Stream {
    fn new(source: IMFMediaSource, sd: IMFStreamDescriptor) -> Result<ComObject<Stream>> {
        Ok(ComObject::new(Stream {
            source: Mutex::new(Some(SendBox(source))),
            sd,
            queue: unsafe { MFCreateEventQueue()? },
            sh: Arc::new(StreamShared {
                m: Mutex::new(StreamState {
                    state: MF_STREAM_STATE_STOPPED,
                    tokens: VecDeque::new(),
                    shutdown: false,
                    worker: None,
                    hub_held: false,
                }),
                cv: Condvar::new(),
            }),
        }))
    }

    fn start(&self, startpos: *const PROPVARIANT) -> Result<()> {
        {
            let mut st = lock(&self.sh.m);
            if st.shutdown {
                return Err(MF_E_SHUTDOWN.into());
            }
            st.state = MF_STREAM_STATE_RUNNING;
            if !st.hub_held {
                capture::hub().acquire();
                st.hub_held = true;
            }
            if st.worker.is_none() {
                let sh = self.sh.clone();
                let q = SendBox(self.queue.clone());
                st.worker = Some(std::thread::spawn(move || deliver(sh, q)));
            }
        }
        vlog!("stream: started");
        unsafe { self.queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, startpos) }
    }

    fn halt(&self) {
        let (worker, held) = {
            let mut st = lock(&self.sh.m);
            st.state = MF_STREAM_STATE_STOPPED;
            st.tokens.clear();
            let held = std::mem::replace(&mut st.hub_held, false);
            (st.worker.take(), held)
        };
        self.sh.cv.notify_all();
        if let Some(w) = worker {
            let _ = w.join();
        }
        if held {
            capture::hub().release();
        }
    }

    fn stop(&self) -> Result<()> {
        self.halt();
        vlog!("stream: stopped");
        unsafe { self.queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }

    fn shutdown(&self) {
        lock(&self.sh.m).shutdown = true;
        self.halt();
        unsafe {
            let _ = self.queue.Shutdown();
        }
        lock(&self.source).take();
    }
}

/// Worker: for every pending RequestSample token, wait for a fresh frame and deliver it.
fn deliver(sh: Arc<StreamShared>, queue: SendBox<IMFMediaEventQueue>) {
    let hub = capture::hub();
    let mut last_seq = 0u64;
    let mut last_time = 0i64;
    let mut sent = 0u64;
    loop {
        let token = {
            let mut st = lock(&sh.m);
            loop {
                if st.shutdown || st.state != MF_STREAM_STATE_RUNNING {
                    return;
                }
                if let Some(t) = st.tokens.pop_front() {
                    break t;
                }
                st = sh.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Hand over a frame the moment it is published. If none arrives in time
        // (sensor runs at 10 fps = 100 ms), repeat the last one so apps never stall.
        // The very first sample may wait longer so apps don't flash the placeholder.
        let wait = if last_seq == 0 { 1500 } else { 180 };
        let (frame, seq) = hub.wait_frame(last_seq, Duration::from_millis(wait));
        let time = if seq != last_seq { frame.time } else { capture::now_100ns() };
        let time = time.max(last_time + 1); // strictly increasing timestamps
        last_seq = seq;
        last_time = time;
        let r = make_sample(&frame, time).and_then(|s| unsafe {
            if let Some(t) = &token {
                s.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            queue.0.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &s)
        });
        match r {
            Ok(()) => {
                sent += 1;
                if sent == 1 || sent % 300 == 0 {
                    vlog!("stream: delivered {sent} samples");
                }
            }
            Err(e) => {
                vlog!("stream: sample delivery failed: {e}");
                return;
            }
        }
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, f: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue.GetEvent(f.0) }
    }
    fn BeginGetEvent(&self, cb: Option<&IMFAsyncCallback>, st: Option<&IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(cb, st) }
    }
    fn EndGetEvent(&self, r: Option<&IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(r) }
    }
    fn QueueEvent(&self, met: u32, g: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, g, hr, v) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        match &*lock(&self.source) {
            Some(s) => Ok(s.0.clone()),
            None => Err(MF_E_SHUTDOWN.into()),
        }
    }
    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }
    fn RequestSample(&self, token: Option<&IUnknown>) -> Result<()> {
        let mut st = lock(&self.sh.m);
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        if st.state != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
        }
        st.tokens.push_back(token.cloned());
        drop(st);
        self.sh.cv.notify_all();
        Ok(())
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        vlog!("stream: SetStreamState({})", value.0);
        if value == MF_STREAM_STATE_RUNNING {
            self.start(std::ptr::null())
        } else {
            self.halt();
            lock(&self.sh.m).state = value;
            Ok(())
        }
    }
    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(lock(&self.sh.m).state)
    }
}

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
}

// ───────────────────────────────── source ─────────────────────────────────

struct SourceState {
    stream: Option<ComObject<Stream>>,
    announced: bool,
    shutdown: bool,
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
pub struct Source {
    queue: IMFMediaEventQueue,
    pd: IMFPresentationDescriptor,
    attrs: IMFAttributes,
    stream_attrs: IMFAttributes,
    st: Mutex<SourceState>,
}

impl Source {
    pub fn create() -> Result<IMFMediaSource> {
        unsafe {
            let mt = media_type()?;
            let sd = MFCreateStreamDescriptor(0, &[Some(mt.clone())])?;
            sd.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
            set_stream_attrs(&sd.cast()?)?;
            let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
            pd.SelectStream(0)?;
            let stream_attrs = new_attributes(4)?;
            set_stream_attrs(&stream_attrs)?;

            let obj = ComObject::new(Source {
                queue: MFCreateEventQueue()?,
                pd,
                attrs: new_attributes(4)?,
                stream_attrs,
                st: Mutex::new(SourceState { stream: None, announced: false, shutdown: false }),
            });
            let src: IMFMediaSource = obj.to_interface::<IMFMediaSourceEx>().cast()?;
            let stream = Stream::new(src.clone(), sd)?;
            lock(&obj.st).stream = Some(stream);
            vlog!("source: created");
            Ok(src)
        }
    }

    fn check(&self) -> Result<std::sync::MutexGuard<'_, SourceState>> {
        let g = lock(&self.st);
        if g.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(g)
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, f: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue.GetEvent(f.0) }
    }
    fn BeginGetEvent(&self, cb: Option<&IMFAsyncCallback>, st: Option<&IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(cb, st) }
    }
    fn EndGetEvent(&self, r: Option<&IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(r) }
    }
    fn QueueEvent(&self, met: u32, g: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, g, hr, v) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        drop(self.check()?);
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }
    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        drop(self.check()?);
        unsafe { self.pd.Clone() }
    }
    fn Start(&self, _pd: Option<&IMFPresentationDescriptor>, _fmt: *const GUID, startpos: *const PROPVARIANT) -> Result<()> {
        let stream = {
            let mut g = self.check()?;
            let s = g.stream.clone().ok_or_else(|| Error::from(MF_E_NOT_INITIALIZED))?;
            let unk: IUnknown = s.to_interface();
            let ev = if g.announced { MEUpdatedStream } else { MENewStream };
            g.announced = true;
            unsafe { self.queue.QueueEventParamUnk(ev.0 as u32, &GUID::zeroed(), S_OK, &unk)? };
            s
        };
        vlog!("source: Start");
        stream.start(startpos)?;
        unsafe { self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, startpos) }
    }
    fn Stop(&self) -> Result<()> {
        let stream = self.check()?.stream.clone();
        vlog!("source: Stop");
        if let Some(s) = stream {
            s.stop()?;
        }
        unsafe { self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }
    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }
    fn Shutdown(&self) -> Result<()> {
        let stream = {
            let mut g = lock(&self.st);
            if g.shutdown {
                return Ok(());
            }
            g.shutdown = true;
            g.stream.take()
        };
        vlog!("source: Shutdown");
        if let Some(s) = stream {
            s.shutdown();
        }
        unsafe { self.queue.Shutdown() }
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        drop(self.check()?);
        Ok(self.attrs.clone())
    }
    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        drop(self.check()?);
        if id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        Ok(self.stream_attrs.clone())
    }
    fn SetD3DManager(&self, _: Option<&IUnknown>) -> Result<()> {
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(&self, _: *const GUID, _: *const GUID, _: *mut *mut c_void) -> Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _: *const KSIDENTIFIER, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> Result<()> {
        Err(not_found())
    }
}

// ──────────────────────────────── activate ────────────────────────────────

#[implement(IMFActivate)]
pub struct Activate {
    attrs: IMFAttributes,
    source: Mutex<Option<SendBox<IMFMediaSource>>>,
}

impl Activate {
    pub fn new() -> Result<IMFActivate> {
        Ok(Activate { attrs: new_attributes(1)?, source: Mutex::new(None) }.into())
    }
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        let mut g = lock(&self.source);
        if g.is_none() {
            *g = Some(SendBox(Source::create().inspect_err(|e| vlog!("activate: create failed: {e}"))?));
        }
        unsafe { g.as_ref().unwrap().0.query(riid, ppv).ok() }
    }
    fn ShutdownObject(&self) -> Result<()> {
        if let Some(s) = lock(&self.source).take() {
            unsafe {
                let _ = s.0.Shutdown();
            }
        }
        Ok(())
    }
    fn DetachObject(&self) -> Result<()> {
        lock(&self.source).take();
        Ok(())
    }
}

impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItem(k, (!v.is_null()).then_some(v)) }
    }
    fn GetItemType(&self, k: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        unsafe { self.attrs.GetItemType(k) }
    }
    fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<BOOL> {
        unsafe { self.attrs.CompareItem(k, v) }
    }
    fn Compare(&self, t: Option<&IMFAttributes>, m: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> {
        unsafe { self.attrs.Compare(t, m) }
    }
    fn GetUINT32(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetUINT32(k) }
    }
    fn GetUINT64(&self, k: *const GUID) -> Result<u64> {
        unsafe { self.attrs.GetUINT64(k) }
    }
    fn GetDouble(&self, k: *const GUID) -> Result<f64> {
        unsafe { self.attrs.GetDouble(k) }
    }
    fn GetGUID(&self, k: *const GUID) -> Result<GUID> {
        unsafe { self.attrs.GetGUID(k) }
    }
    fn GetStringLength(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetStringLength(k) }
    }
    fn GetString(&self, k: *const GUID, p: PWSTR, n: u32, len: *mut u32) -> Result<()> {
        unsafe {
            let buf = std::slice::from_raw_parts_mut(p.0, n as usize);
            self.attrs.GetString(k, buf, (!len.is_null()).then_some(len))
        }
    }
    fn GetAllocatedString(&self, k: *const GUID, p: *mut PWSTR, len: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedString(k, p, len) }
    }
    fn GetBlobSize(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetBlobSize(k) }
    }
    fn GetBlob(&self, k: *const GUID, p: *mut u8, n: u32, sz: *mut u32) -> Result<()> {
        unsafe {
            let buf = std::slice::from_raw_parts_mut(p, n as usize);
            self.attrs.GetBlob(k, buf, (!sz.is_null()).then_some(sz))
        }
    }
    fn GetAllocatedBlob(&self, k: *const GUID, p: *mut *mut u8, sz: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedBlob(k, p, sz) }
    }
    fn GetUnknown(&self, k: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        unsafe {
            let u: IUnknown = self.attrs.GetUnknown(k)?;
            u.query(riid, ppv).ok()
        }
    }
    fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.SetItem(k, v) }
    }
    fn DeleteItem(&self, k: *const GUID) -> Result<()> {
        unsafe { self.attrs.DeleteItem(k) }
    }
    fn DeleteAllItems(&self) -> Result<()> {
        unsafe { self.attrs.DeleteAllItems() }
    }
    fn SetUINT32(&self, k: *const GUID, v: u32) -> Result<()> {
        unsafe { self.attrs.SetUINT32(k, v) }
    }
    fn SetUINT64(&self, k: *const GUID, v: u64) -> Result<()> {
        unsafe { self.attrs.SetUINT64(k, v) }
    }
    fn SetDouble(&self, k: *const GUID, v: f64) -> Result<()> {
        unsafe { self.attrs.SetDouble(k, v) }
    }
    fn SetGUID(&self, k: *const GUID, v: *const GUID) -> Result<()> {
        unsafe { self.attrs.SetGUID(k, v) }
    }
    fn SetString(&self, k: *const GUID, v: &PCWSTR) -> Result<()> {
        unsafe { self.attrs.SetString(k, *v) }
    }
    fn SetBlob(&self, k: *const GUID, p: *const u8, n: u32) -> Result<()> {
        unsafe { self.attrs.SetBlob(k, std::slice::from_raw_parts(p, n as usize)) }
    }
    fn SetUnknown(&self, k: *const GUID, u: Option<&IUnknown>) -> Result<()> {
        unsafe { self.attrs.SetUnknown(k, u) }
    }
    fn LockStore(&self) -> Result<()> {
        unsafe { self.attrs.LockStore() }
    }
    fn UnlockStore(&self) -> Result<()> {
        unsafe { self.attrs.UnlockStore() }
    }
    fn GetCount(&self) -> Result<u32> {
        unsafe { self.attrs.GetCount() }
    }
    fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItemByIndex(i, k, (!v.is_null()).then_some(v)) }
    }
    fn CopyAllItems(&self, d: Option<&IMFAttributes>) -> Result<()> {
        unsafe { self.attrs.CopyAllItems(d) }
    }
}

// ─────────────────────────── class factory + exports ───────────────────────────

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Option<&IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        unsafe {
            let _ = MFStartup(0x0002_0070, 0);
        }
        vlog!("factory: CreateInstance");
        let a = Activate::new()?;
        unsafe { a.query(riid, ppv).ok() }
    }
    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if rclsid.is_null() || *rclsid != CLSID_QCVCAM {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let f: IClassFactory = Factory.into();
    f.query(riid, ppv)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // Worker threads may still be winding down; stay loaded for the life of the process.
    S_FALSE
}
