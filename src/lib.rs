//! qccam – Logitech QuickCam Messenger (046d:08f6) support for Windows via WinUSB.
//! Built as an rlib (used by the qcsnap/qcvcam tools) and as qccam.dll,
//! the Media Foundation virtual camera source loaded by Windows Frame Server.

pub mod log;
pub mod winusb;
pub mod st6422;
pub mod image;
pub mod isp;
pub mod config;
pub mod capture;
pub mod vcam;
