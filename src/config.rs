//! Settings file, re-read by the camera about once a second.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::isp::{Filter, IspParams};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub mirror: bool,
    pub filter: Filter,
    pub strength: f32,
}

impl Default for Settings {
    fn default() -> Self {
        let p = IspParams::default();
        Self { mirror: p.mirror, filter: p.filter, strength: p.strength }
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    PathBuf::from(base).join("QCVCam").join("config.txt")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        if let Ok(text) = std::fs::read_to_string(path()) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("");
                let Some((k, v)) = line.split_once('=') else { continue };
                match k.trim().to_ascii_lowercase().as_str() {
                    "mirror" => s.mirror = parse_bool(v).unwrap_or(s.mirror),
                    "filter" => s.filter = Filter::parse(v).unwrap_or(s.filter),
                    "strength" => s.strength = v.trim().parse::<f32>().map(|x| x.clamp(0.0, 1.0)).unwrap_or(s.strength),
                    _ => {}
                }
            }
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = format!(
            "# QuickCam Messenger virtual camera settings (applied live)\n\
             # mirror   = on | off\n\
             # filter   = none | vintage | noir\n\
             # strength = 0.0 .. 1.0\n\
             mirror = {}\nfilter = {}\nstrength = {:.2}\n",
            if self.mirror { "on" } else { "off" },
            self.filter.name(),
            self.strength
        );
        let p = path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, text)
    }

    pub fn apply(&self, p: &mut IspParams) {
        p.mirror = self.mirror;
        p.filter = self.filter;
        p.strength = self.strength;
    }

    /// Updates `key` from a user string; returns an error message for bad input.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key.to_ascii_lowercase().as_str() {
            "mirror" | "flip" => self.mirror = parse_bool(value).ok_or("mirror must be on or off")?,
            "filter" | "look" => self.filter = Filter::parse(value).ok_or("filter must be none, vintage or noir")?,
            "strength" => {
                self.strength = value.parse::<f32>().map_err(|_| "strength must be a number 0..1")?.clamp(0.0, 1.0)
            }
            _ => return Err(format!("unknown setting '{key}' (use mirror, filter or strength)")),
        }
        Ok(())
    }
}

/// Cheap change detector: stats the file at most once per second.
pub struct Watcher {
    last_check: Instant,
    mtime: Option<SystemTime>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self { last_check: Instant::now() - Duration::from_secs(10), mtime: None }
    }
}

impl Watcher {
    /// Returns new settings when the file changed (and on the first call).
    pub fn poll(&mut self) -> Option<Settings> {
        if self.last_check.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.last_check = Instant::now();
        let m = std::fs::metadata(path()).and_then(|m| m.modified()).ok();
        if m == self.mtime && self.mtime.is_some() {
            return None;
        }
        self.mtime = m;
        Some(Settings::load())
    }
}
