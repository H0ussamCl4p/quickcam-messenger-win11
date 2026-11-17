# quickcam-messenger-win11

![Windows 11](https://img.shields.io/badge/Windows-11-0078D6?logo=windows&logoColor=white)
![Rust](https://img.shields.io/badge/Rust-stable-CE422B?logo=rust&logoColor=white)
![USB](https://img.shields.io/badge/USB-046d%3A08f6-6E4AFF)
![License](https://img.shields.io/badge/license-GPL--2.0-2EA44F)

Driver and virtual camera for the Logitech QuickCam Messenger (`046d:08f6`) on Windows 11.
Logitech never shipped a driver for Windows 8 or later.

The camera is driven from user mode through WinUSB. `qccam.dll` is a Media Foundation
source registered as a Windows 11 virtual camera, so any app can use it.
Output is 640x480 at 10 fps, the sensor's maximum.

## Install

1. Plug in the camera. In Device Manager, right-click **Camera** → Update driver →
   Browse my computer → Let me pick → Universal Serial Bus devices → **WinUsb Device**.
2. Build:
   ```
   cargo build --release
   ```
3. From an admin PowerShell in the repo folder:
   ```
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```
4. Pick **QuickCam Messenger (Rust)** in your camera app.

To remove it, run `uninstall.ps1` the same way.

## Settings

Changes apply while the camera is running.

```
qcvcam set mirror on|off
qcvcam set filter none|vintage|noir
qcvcam set strength 0..1
qcvcam config
```

The settings are stored in `C:\ProgramData\QCVCam\config.txt`.

## Tools

- `qcsnap info`: device info
- `qcsnap snap`: save a still to `captures/`
- `qcvcam test`: capture test without installing
- `qcvcam live`: test through the Windows camera stack
- `qcvcam preview`: render each filter
- `qcbench`, `qcprobe`: frame rate and register experiments

## Notes

- 10 fps is a hardware limit. Exposure is capped at about 100 ms and gain covers the rest, so more light means less noise.
- Log: `C:\ProgramData\QCVCam\log.txt`
- A blue screen with a moving bar means the camera isn't connected or isn't using WinUSB.

## License

GPL-2.0-or-later. The camera protocol is ported from the Linux `gspca/stv06xx` driver.
This project isn't affiliated with Logitech.
