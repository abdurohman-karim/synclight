//! Minimal event log: one line per event, local time, optional echo to stderr.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_SIZE: u64 = 512 * 1024;

static SINK: Mutex<Option<File>> = Mutex::new(None);
static ECHO: AtomicBool = AtomicBool::new(false);

pub fn init(path: Option<PathBuf>, echo: bool) {
    ECHO.store(echo, Ordering::Relaxed);
    if let Some(p) = path {
        let too_big = std::fs::metadata(&p).map(|m| m.len() > MAX_SIZE).unwrap_or(false);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(!too_big)
            .truncate(too_big)
            .open(&p);
        if let Ok(f) = file {
            *SINK.lock().unwrap() = Some(f);
        }
    }
}

pub fn info(msg: impl AsRef<str>) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond,
        msg.as_ref()
    );
    if ECHO.load(Ordering::Relaxed) {
        eprint!("{line}");
    }
    if let Some(f) = SINK.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}
