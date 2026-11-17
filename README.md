# QuickCam Messenger on Windows 10/11

Use the old **Logitech QuickCam Messenger** (USB ID `046d:08f6`, ST6422 chip) as a normal webcam
on modern Windows, even though Logitech never released a Windows 8/10/11 driver for it.

It works in the Windows Camera app, Teams, Zoom, Discord, OBS and browsers, at 640x480 and 10 fps
(the sensor's hardware maximum). Frames reach the app about 3 ms after capture.

**How it works:** a user-mode USB driver written in Rust talks to the camera through Microsoft's
built-in **WinUSB** driver. A **Media Foundation** media source (`qccam.dll`) is registered as a
**Windows 11 virtual camera**, so the system Frame Server shares it with every app.
No kernel driver, no test signing, no third-party driver installers.

```
camera ──USB 1.1 isochronous──> WinUSB ──> qccam.dll (protocol, auto-exposure, image processing)
       ──> Windows Camera Frame Server ──> Camera / Teams / Zoom / OBS / browser
```

## Features
- **Auto-exposure** that keeps the full 10 fps: exposure is capped and gain covers the rest, with 50 Hz anti-flicker steps.
- **Image processing:** bilinear demosaic, flicker-free white balance, auto-levels, gamma, saturation, sharpening, 640x480 NV12 output.
- **Looks:** `none`, `vintage` (warm faded film, vignette, grain) and `noir` (black & white). There's also a **horizontal mirror**. All of them change live.
- **Low latency:** 8 ms USB batches, frames stamped with their capture time and delivered as soon as they're ready.
- **Robust:** recovers from unplugging and USB stream gaps, and a crash in the DLL can't take down the Windows camera service.

## Requirements
- **Windows 11.** The virtual camera API (`MFCreateVirtualCamera`) needs Windows 11. On Windows 10 only the `qcsnap` stills tool works.
- **Rust** (stable, MSVC toolchain): <https://rustup.rs>
- **Admin rights,** once, for driver assignment and installation.

## Install

### 1. Give the camera the WinUSB driver (one time)
Plug in the camera, then open **Device Manager**:
1. Under **Other devices**, right-click **Camera** (shown with a yellow ⚠), then choose **Update driver**.
2. **Browse my computer for drivers** → **Let me pick from a list of available drivers on my computer**.
3. Pick **Universal Serial Bus devices** (or **Show All Devices**) → **Next**.
4. Manufacturer **WinUsb Device**, model **WinUsb Device** → **Next**, and accept the warning.

The camera now shows as *WinUsb Device*. This uses Microsoft's own signed driver. Zadig
(<https://zadig.akeo.ie>, choose **WinUSB** for `046D 08F6`) works too.

### 2. Build and test
```powershell
git clone https://github.com/H0ussamCl4p/quickcam-messenger-win11
cd quickcam-messenger-win11
cargo build --release
.\target\release\qcsnap.exe info       # the camera answers
.\target\release\qcvcam.exe test       # 8 s capture test, saves captures\vcam_test.png
```

### 3. Install the virtual camera (admin PowerShell, in the repo folder)
```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```
Open the Camera app (or any video app) and pick **"QuickCam Messenger (Rust)"**.

To check it through the real Windows camera stack:
```powershell
.\target\release\qcvcam.exe live 20
```

### Uninstall
```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1    # admin
```
To give the camera back to Windows, open Device Manager → the camera → **Uninstall device** (tick "delete the driver"), then unplug it.

## Settings (applied live, no admin needed)
```powershell
.\target\release\qcvcam.exe set mirror on          # on | off
.\target\release\qcvcam.exe set filter vintage     # none | vintage | noir
.\target\release\qcvcam.exe set strength 0.6       # 0..1, filter intensity
.\target\release\qcvcam.exe config                 # show current settings
.\target\release\qcvcam.exe preview                # render every look -> captures\look_*.png
```
Settings are stored in `C:\ProgramData\QCVCam\config.txt`, and the camera picks up changes within about a second.
Note that Teams and Zoom already mirror *your own* preview. With `mirror on`, other people also see the image flipped.

## Tools
| Tool | What it does |
|---|---|
| `qcsnap info` / `qcsnap snap` | Device info / save still photos (`--exposure`, `--gain`, `--all`, `--small`) |
| `qcvcam test` / `source` / `live` | Capture test / run the media source in-process / test through Frame Server |
| `qcvcam install` / `remove` | Register / remove the virtual camera (called by the scripts) |
| `qcbench` / `qcprobe` | Frame-rate, latency and register experiments |

## Performance
- **Frame rate:** the sensor's maximum is **10 fps** (about 95 ms to read out a frame). No known register changes it, and the 162x120 mode isn't faster either.
- **Exposure:** longer exposure than about 100 ms lowers the frame rate, so auto-exposure caps it and uses gain instead. Good lighting therefore means less noise.
- **Processing:** about 1–3 ms per frame. Frames reach apps about 3 ms after the last USB packet.

## Troubleshooting
- **Log:** `C:\ProgramData\QCVCam\log.txt`
- **Dark blue screen with a moving bar:** the virtual camera is running but the physical camera isn't delivering. Check that it's plugged in and still shows as *WinUsb Device*.
- **`access denied` from the tools:** another app (or the virtual camera) is using the camera. Close the app; the camera is released about 8 s later.
- **Camera not listed in apps:** run `install.ps1` again from an **admin** PowerShell.

## Source layout
| File | Role |
|---|---|
| `src/winusb.rs` | WinUSB wrapper: device lookup, control transfers, isochronous streaming |
| `src/st6422.rs` | Camera protocol: registers, startup sequence, frame reassembly |
| `src/isp.rs` | Image processor and auto-exposure |
| `src/capture.rs` | Shared capture thread, latency and robustness logic |
| `src/config.rs` | Live settings file |
| `src/vcam.rs` | COM / Media Foundation media source (`qccam.dll`) |
| `src/bin/qcvcam.rs` | Virtual camera install, tests and settings |
| `src/main.rs` | `qcsnap` stills tool |

## Credits and license
The camera protocol is ported from the Linux kernel driver `drivers/media/usb/gspca/stv06xx`
(Erik Andrén, Hans de Goede and contributors), which is GPL-licensed. This project is therefore released under the
**GNU General Public License v2.0 or later**. See [LICENSE](LICENSE).

Not affiliated with or endorsed by Logitech. "Logitech" and "QuickCam" are trademarks of their respective owners.
