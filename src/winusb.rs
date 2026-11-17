//! Thin safe-ish wrapper over the WinUSB user-mode API.
//! The camera must have WinUSB bound as its driver (e.g. via Zadig).

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Usb::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};

/// GUID_DEVINTERFACE_USB_DEVICE – registered by the USB hub for every device.
const GUID_DEVINTERFACE_USB_DEVICE: GUID = GUID {
    data1: 0xA5DCBF10,
    data2: 0x6530,
    data3: 0x11D2,
    data4: [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED],
};

pub type Result<T> = std::result::Result<T, String>;

fn last_err(what: &str) -> String {
    let code = unsafe { GetLastError() };
    let hint = match code {
        2 => " (file not found)",
        5 => " (access denied – is another program using the camera?)",
        6 => " (invalid handle – is WinUSB really the camera's driver?)",
        31 => " (device not functioning)",
        87 => " (invalid parameter)",
        121 => " (timeout)",
        995 => " (operation aborted)",
        1167 => " (device not connected)",
        _ => "",
    };
    format!("{what} failed: Win32 error {code}{hint}")
}

/// Find the device path (\\?\usb#vid_xxxx&pid_xxxx#...) for a VID/PID.
pub fn find_device_path(vid: u16, pid: u16) -> Result<String> {
    let needle = format!("vid_{vid:04x}&pid_{pid:04x}");
    unsafe {
        let set = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_USB_DEVICE,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set as isize == -1 {
            return Err(last_err("SetupDiGetClassDevsW"));
        }
        let mut found = None;
        let mut idx = 0u32;
        loop {
            let mut ifd: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &GUID_DEVINTERFACE_USB_DEVICE, idx, &mut ifd) == 0 {
                break;
            }
            idx += 1;
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &ifd, null_mut(), 0, &mut needed, null_mut());
            if needed == 0 {
                continue;
            }
            // u64 backing store keeps the struct properly aligned.
            let mut buf = vec![0u64; (needed as usize + 7) / 8];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &ifd, detail, needed, null_mut(), null_mut()) == 0 {
                continue;
            }
            let p = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            if path.to_lowercase().contains(&needle) {
                found = Some(path);
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found.ok_or_else(|| format!("no present USB device with {needle} found – is the camera plugged in?"))
    }
}

pub struct PipeInfo {
    pub id: u8,
    pub kind: i32,
    pub max_packet: u16,
    pub bytes_per_interval: u32,
    pub interval: u8,
}

pub struct Device {
    file: HANDLE,
    usb: WINUSB_INTERFACE_HANDLE,
}

impl Device {
    pub fn open(path: &str) -> Result<Self> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let file = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if file == INVALID_HANDLE_VALUE {
                return Err(last_err("CreateFileW"));
            }
            let mut usb: WINUSB_INTERFACE_HANDLE = null_mut();
            if WinUsb_Initialize(file, &mut usb) == 0 {
                let e = last_err("WinUsb_Initialize");
                CloseHandle(file);
                return Err(format!("{e}\n  -> WinUSB is probably not installed for this camera yet (see README)."));
            }
            Ok(Self { file, usb })
        }
    }

    pub fn speed(&self) -> Option<u8> {
        let mut v = 0u8;
        let mut len = 1u32;
        let ok = unsafe { WinUsb_QueryDeviceInformation(self.usb, DEVICE_SPEED, &mut len, &mut v as *mut u8 as *mut c_void) };
        (ok != 0).then_some(v)
    }

    pub fn interface(&self, alt: u8) -> Option<USB_INTERFACE_DESCRIPTOR> {
        unsafe {
            let mut d: USB_INTERFACE_DESCRIPTOR = std::mem::zeroed();
            (WinUsb_QueryInterfaceSettings(self.usb, alt, &mut d) != 0).then_some(d)
        }
    }

    pub fn pipe(&self, alt: u8, index: u8) -> Option<PipeInfo> {
        unsafe {
            let mut p: WINUSB_PIPE_INFORMATION_EX = std::mem::zeroed();
            if WinUsb_QueryPipeEx(self.usb, alt, index, &mut p) == 0 {
                return None;
            }
            Some(PipeInfo {
                id: p.PipeId,
                kind: p.PipeType,
                max_packet: p.MaximumPacketSize,
                bytes_per_interval: p.MaximumBytesPerInterval,
                interval: p.Interval,
            })
        }
    }

    pub fn set_alt(&self, alt: u8) -> Result<()> {
        if unsafe { WinUsb_SetCurrentAlternateSetting(self.usb, alt) } == 0 {
            return Err(last_err(&format!("SetCurrentAlternateSetting({alt})")));
        }
        Ok(())
    }

    fn control(&self, request_type: u8, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize> {
        let setup = WINUSB_SETUP_PACKET {
            RequestType: request_type,
            Request: request,
            Value: value,
            Index: index,
            Length: buf.len() as u16,
        };
        let mut done = 0u32;
        let ok = unsafe { WinUsb_ControlTransfer(self.usb, setup, buf.as_mut_ptr(), buf.len() as u32, &mut done, null()) };
        if ok == 0 {
            return Err(last_err(&format!("control transfer req=0x{request:02x} val=0x{value:04x}")));
        }
        Ok(done as usize)
    }

    pub fn vendor_write(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        let mut b = data.to_vec();
        self.control(0x40, request, value, 0, &mut b).map(|_| ())
    }

    pub fn vendor_read(&self, request: u8, value: u16, len: usize) -> Result<Vec<u8>> {
        let mut b = vec![0u8; len];
        let n = self.control(0xC0, request, value, 0, &mut b)?;
        b.truncate(n);
        Ok(b)
    }

    /// Start an isochronous IN stream. `on_packet` gets each received packet's bytes;
    /// return `false` from it to stop.
    pub fn iso_stream(
        &self,
        pipe: u8,
        bytes_per_interval: u32,
        packets_per_xfer: u32,
        n_xfers: usize,
        mut on_packet: impl FnMut(&[u8]) -> bool,
    ) -> Result<IsoStats> {
        let xfer_len = (bytes_per_interval * packets_per_xfer) as usize;
        let mut buf = vec![0u8; xfer_len * n_xfers];
        let mut stats = IsoStats::default();
        unsafe {
            let _ = WinUsb_ResetPipe(self.usb, pipe);
            let mut bh: *mut c_void = null_mut();
            if WinUsb_RegisterIsochBuffer(self.usb, pipe, buf.as_mut_ptr(), buf.len() as u32, &mut bh) == 0 {
                return Err(last_err("WinUsb_RegisterIsochBuffer"));
            }
            let mut ovs: Vec<OVERLAPPED> = (0..n_xfers).map(|_| std::mem::zeroed()).collect();
            let mut descs: Vec<Vec<USBD_ISO_PACKET_DESCRIPTOR>> =
                (0..n_xfers).map(|_| vec![std::mem::zeroed(); packets_per_xfer as usize]).collect();
            for ov in ovs.iter_mut() {
                ov.hEvent = CreateEventW(null(), 1, 0, null());
            }

            let submit = |i: usize, cont: bool, ovs: &mut Vec<OVERLAPPED>, descs: &mut Vec<Vec<USBD_ISO_PACKET_DESCRIPTOR>>| -> Result<()> {
                for c in [cont, false] {
                    ResetEvent(ovs[i].hEvent);
                    let ok = WinUsb_ReadIsochPipeAsap(
                        bh,
                        (i * xfer_len) as u32,
                        xfer_len as u32,
                        c as BOOL,
                        packets_per_xfer,
                        descs[i].as_mut_ptr(),
                        &ovs[i],
                    );
                    if ok != 0 || GetLastError() == ERROR_IO_PENDING {
                        return Ok(());
                    }
                    if !c {
                        break;
                    }
                    // "continue stream" refused (schedule gap): retry as a fresh ASAP start.
                }
                Err(last_err("WinUsb_ReadIsochPipeAsap"))
            };

            let mut result = Ok(());
            for i in 0..n_xfers {
                if let Err(e) = submit(i, i > 0, &mut ovs, &mut descs) {
                    result = Err(e);
                    break;
                }
            }
            let mut i = 0usize;
            let mut running = result.is_ok();
            let mut consecutive_fail = 0u32;
            while running {
                let mut n = 0u32;
                stats.transfers += 1;
                if WinUsb_GetOverlappedResult(self.usb, &ovs[i], &mut n, 1) == 0 {
                    // A late/missed transfer (e.g. error 87 after a scheduling gap) is not fatal:
                    // drop its data and restart the ASAP schedule with this transfer.
                    let code = GetLastError();
                    stats.xfer_errors += 1;
                    consecutive_fail += 1;
                    if consecutive_fail > 2 * n_xfers as u32 {
                        result = Err(format!("isochronous transfers keep failing (last Win32 error {code})"));
                        break;
                    }
                    if let Err(e) = submit(i, false, &mut ovs, &mut descs) {
                        result = Err(e);
                        break;
                    }
                    i = (i + 1) % n_xfers;
                    continue;
                }
                consecutive_fail = 0;
                for d in &descs[i] {
                    stats.packets += 1;
                    if d.Status != 0 {
                        stats.packet_errors += 1;
                        continue;
                    }
                    if d.Length == 0 {
                        stats.empty_packets += 1;
                        continue;
                    }
                    stats.bytes += d.Length as u64;
                    let start = i * xfer_len + d.Offset as usize;
                    let pkt = &buf[start..start + d.Length as usize];
                    if !on_packet(pkt) {
                        running = false;
                        break;
                    }
                }
                if running {
                    if let Err(e) = submit(i, true, &mut ovs, &mut descs) {
                        result = Err(e);
                        break;
                    }
                    i = (i + 1) % n_xfers;
                }
            }

            WinUsb_AbortPipe(self.usb, pipe);
            for (k, ov) in ovs.iter().enumerate() {
                let mut n = 0u32;
                let _ = k;
                WinUsb_GetOverlappedResult(self.usb, ov, &mut n, 1);
                CloseHandle(ov.hEvent);
            }
            WinUsb_UnregisterIsochBuffer(bh);
            result.map(|_| stats)
        }
    }
}

#[derive(Default, Debug)]
pub struct IsoStats {
    pub transfers: u64,
    pub xfer_errors: u64,
    pub packets: u64,
    pub packet_errors: u64,
    pub empty_packets: u64,
    pub bytes: u64,
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            WinUsb_Free(self.usb);
            CloseHandle(self.file);
        }
    }
}
