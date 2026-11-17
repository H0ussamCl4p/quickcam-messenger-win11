//! ST6422 protocol, ported from Linux gspca/stv06xx.

use crate::winusb::{Device, Result};

pub const VID: u16 = 0x046D;
pub const PID: u16 = 0x08F6;

const REQ: u8 = 0x04;
const ISO_ENABLE: u16 = 0x1440;
const ISO_SIZE_L: u16 = 0x15c1;
const COMMIT: u16 = 0x143f;

#[derive(Clone, Copy, Debug)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
    /// lines the camera actually sends (first 4 are garbage)
    pub sent_lines: usize,
    pub res_reg: u8,
    pub packet_size: u16,
}

pub const MODE_SMALL: Mode = Mode { width: 162, height: 120, sent_lines: 124, res_reg: 0x0f, packet_size: 300 };
pub const MODE_LARGE: Mode = Mode { width: 324, height: 240, sent_lines: 248, res_reg: 0x02, packet_size: 847 };

/// Bridge register write: vendor OUT, bRequest 4, wValue = register, 1 or 2 data bytes (LE).
pub fn write_reg(dev: &Device, reg: u16, val: u16) -> Result<()> {
    let bytes = val.to_le_bytes();
    let data = if val > 0xff { &bytes[..] } else { &bytes[..1] };
    dev.vendor_write(REQ, reg, data)
}

pub fn read_reg(dev: &Device, reg: u16) -> Result<u8> {
    let v = dev.vendor_read(REQ, reg, 1)?;
    v.first().copied().ok_or_else(|| format!("short read of register 0x{reg:04x}"))
}

const INIT: &[(u16, u16)] = &[
    (0x1440, 0x00), // capture off
    (0x1436, 0x00),
    (0x1432, 0x03), // brightness
    (0x143a, 0xf9), // contrast
    (0x0509, 0x38), // R gain
    (0x050a, 0x38), // G gain
    (0x050b, 0x38), // B gain
    (0x050c, 0x2a),
    (0x050d, 0x01),
    (0x1431, 0x00),
    (0x1433, 0x34),
    (0x1438, 0x18),
    (0x1439, 0x00),
    (0x143b, 0x05),
    (0x143c, 0x00),
    (0x143e, 0x01), // exposure high
    (0x143d, 0x00), // exposure low
    (0x1442, 0xe2),
    (0x1500, 0xd0),
    (0x1500, 0xd0),
    (0x1500, 0x50),
    (0x1501, 0xaf),
    (0x1502, 0xc2),
    (0x1503, 0x45),
    (0x1505, 0x02),
    (0x150e, 0x8e),
    (0x150f, 0x37),
    (0x15c0, 0x00),
    (0x15c3, 0x08),
    (0x143f, 0x01), // commit
];

pub fn init(dev: &Device) -> Result<()> {
    std::thread::sleep(std::time::Duration::from_millis(250));
    for &(r, v) in INIT {
        write_reg(dev, r, v).map_err(|e| format!("init write 0x{r:04x}=0x{v:02x}: {e}"))?;
    }
    Ok(())
}

pub struct Controls {
    pub exposure: u16, // 0..1023
    pub gain: u8,      // 0..255
    pub brightness: u8, // 0..31
}

pub fn apply_controls(dev: &Device, c: &Controls) -> Result<()> {
    write_reg(dev, 0x1432, c.brightness.min(31) as u16)?;
    for r in [0x0509, 0x050a, 0x050b] {
        write_reg(dev, r, c.gain as u16)?;
    }
    write_reg(dev, 0x050c, 0x2a)?;
    write_reg(dev, 0x050d, 0x01)?;
    let e = c.exposure.min(1023);
    write_reg(dev, 0x143d, e & 0xff)?;
    write_reg(dev, 0x143e, e >> 8)?;
    write_reg(dev, COMMIT, 1)
}

/// `packet_size` is what we tell the camera to send per isochronous packet.
pub fn start(dev: &Device, mode: &Mode, packet_size: u16) -> Result<()> {
    write_reg(dev, ISO_SIZE_L, packet_size)?;
    write_reg(dev, 0x1505, mode.res_reg as u16)?;
    write_reg(dev, COMMIT, 1)?;
    write_reg(dev, ISO_ENABLE, 1)
}

pub fn stop(dev: &Device) -> Result<()> {
    write_reg(dev, ISO_ENABLE, 0)
}

/// Reassembles frames from isochronous packets.
pub struct FrameAssembler {
    mode: Mode,
    cur: Option<Vec<u8>>,
    to_skip: usize,
    pub sof_seen: u64,
    pub eof_seen: u64,
    pub bad_frames: u64,
    pub chunk_ids: std::collections::BTreeMap<u16, u64>,
}

impl FrameAssembler {
    pub fn new(mode: Mode) -> Self {
        Self { mode, cur: None, to_skip: 0, sof_seen: 0, eof_seen: 0, bad_frames: 0, chunk_ids: Default::default() }
    }

    /// Feed one isochronous packet; returns a complete raw Bayer frame (width*height bytes) if one finished.
    pub fn push(&mut self, mut data: &[u8]) -> Option<Vec<u8>> {
        let mut out = None;
        while data.len() >= 4 {
            let id = u16::from_be_bytes([data[0], data[1]]);
            let clen = u16::from_be_bytes([data[2], data[3]]) as usize;
            data = &data[4..];
            *self.chunk_ids.entry(id).or_default() += 1;
            if data.len() < clen {
                // truncated chunk -> this frame is broken
                if self.cur.take().is_some() {
                    self.bad_frames += 1;
                }
                return out;
            }
            let chunk = &data[..clen];
            match id {
                _ if id & 0xff00 == 0x0200 || id == 0x4200 => {
                    if let Some(f) = self.cur.as_mut() {
                        let skip = self.to_skip.min(chunk.len());
                        self.to_skip -= skip;
                        f.extend_from_slice(&chunk[skip..]);
                    }
                }
                0x8001 | 0x8005 | 0xc001 | 0xc005 => {
                    self.sof_seen += 1;
                    self.cur = Some(Vec::with_capacity(self.mode.width * self.mode.sent_lines));
                    self.to_skip = self.mode.width * 4;
                }
                0x8002 | 0x8006 | 0xc002 => {
                    self.eof_seen += 1;
                    if let Some(f) = self.cur.take() {
                        let need = self.mode.width * self.mode.height;
                        if f.len() >= need {
                            let mut f = f;
                            f.truncate(need);
                            out = Some(f);
                        } else {
                            self.bad_frames += 1;
                        }
                    }
                }
                _ => {}
            }
            data = &data[clen..];
        }
        out
    }
}
