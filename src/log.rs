//! File logger (Frame Server has no console).

use std::io::Write;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

pub fn log(msg: impl AsRef<str>) {
    let _g = LOCK.lock();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let line = format!("[{secs:.3}] pid {} {}\n", std::process::id(), msg.as_ref());
    if std::env::var_os("QCVCAM_STDERR").is_some() {
        eprint!("{line}");
    }
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let path = std::path::Path::new(&base).join("QCVCam").join("log.txt");
    // Keep the log bounded: rotate to log.old.txt past 1 MB.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::rename(&path, path.with_file_name("log.old.txt"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! vlog {
    ($($t:tt)*) => { $crate::log::log(format!($($t)*)) };
}
