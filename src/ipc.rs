//! Live status shared between the tray process (engine) and the settings window.
//!
//! The engine publishes a small fixed-size block into named shared memory; the
//! window maps it read-only. A sequence counter (seqlock) protects against torn
//! reads: it is odd while the writer is in the middle of an update.
//! Settings go the other way through config.ini plus a `WM_RELOAD` message.

use std::mem::size_of;
use std::sync::atomic::{fence, AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, FILE_MAP_READ,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_APP};

pub const MAX_LEDS: usize = 254;
const MAGIC: u32 = 0x5352_4C53; // "SLRS"
const NAME: PCWSTR = w!("Local\\synclight-rs-status");

/// Window class of the tray's hidden window.
pub const TRAY_CLASS: PCWSTR = w!("SyncLightRSTray");
/// Sent to the tray window: "config.ini changed, apply it now".
pub const WM_RELOAD: u32 = WM_APP + 2;

/// Engine state codes stored in [`StatusBlock::state`].
pub mod state {
    pub const STARTING: u8 = 0;
    pub const OFF: u8 = 1;
    pub const RUNNING: u8 = 2;
    pub const NO_DEVICE: u8 = 3;
    pub const NO_CAPTURE: u8 = 4;
    pub const PAUSED: u8 = 5;
    pub const ERROR: u8 = 6;
}

#[repr(C)]
pub struct StatusBlock {
    pub magic: u32,
    pub seq: AtomicU32,
    /// Bumped on every update; the window uses it to tell a live engine from a dead one.
    pub heartbeat: u32,
    /// Bumped whenever LED colours change.
    pub frame: u32,
    pub state: u8,
    pub has_device: u8,
    pub lamps: u8,
    pub display_size: u8,
    pub firmware: [u8; 4],
    pub model: [u8; 8],
    pub edges: u8,
    pub reverse: u8,
    pub per_led: u8,
    pub zones: u8,
    /// Active mode: 0 sync, 1 static, 2 effect, 3 rhythm.
    pub mode: u8,
    /// Last sound level sent for "Computer Rhythm", 0..=100.
    pub audio_level: u8,
    pub _pad: [u8; 2],
    pub horizontal: u16,
    pub vertical: u16,
    pub led_count: u16,
    pub summary_len: u16,
    pub summary: [u8; 200],
    pub screen_len: u16,
    pub screen: [u8; 62],
    pub fps: f32,
    pub sends: f32,
    pub grab_ms: f32,
    pub latency_ms: f32,
    pub hid_ms: f32,
    /// Colour of every LED in strip order (before the USB power limit, for display).
    pub leds: [[u8; 3]; MAX_LEDS],
}

fn put_str(dst: &mut [u8], len: &mut u16, s: &str) {
    let mut n = s.len().min(dst.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    *len = n as u16;
}

impl StatusBlock {
    pub fn set_summary(&mut self, s: &str) {
        put_str(&mut self.summary, &mut self.summary_len, s);
    }

    pub fn set_screen(&mut self, s: &str) {
        put_str(&mut self.screen, &mut self.screen_len, s);
    }

    pub fn summary(&self) -> String {
        String::from_utf8_lossy(&self.summary[..self.summary_len as usize]).into_owned()
    }

    pub fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen[..self.screen_len as usize]).into_owned()
    }

    pub fn model(&self) -> String {
        let n = self.model.iter().position(|&b| b == 0).unwrap_or(self.model.len());
        String::from_utf8_lossy(&self.model[..n]).into_owned()
    }
}

/// Engine side: creates the shared block and updates it.
pub struct StatusWriter {
    map: HANDLE,
    view: *mut StatusBlock,
}

// The view is only touched from the engine thread that owns the writer.
unsafe impl Send for StatusWriter {}

impl StatusWriter {
    pub fn create() -> Option<Self> {
        let size = size_of::<StatusBlock>();
        unsafe {
            let map = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, size as u32, NAME).ok()?;
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size).Value as *mut StatusBlock;
            if view.is_null() {
                let _ = CloseHandle(map);
                return None;
            }
            std::ptr::write_bytes(view as *mut u8, 0, size);
            (*view).magic = MAGIC;
            Some(StatusWriter { map, view })
        }
    }

    pub fn update(&self, f: impl FnOnce(&mut StatusBlock)) {
        unsafe {
            let b = &mut *self.view;
            b.seq.fetch_add(1, Ordering::AcqRel); // odd: writing
            fence(Ordering::Release);
            f(b);
            b.heartbeat = b.heartbeat.wrapping_add(1);
            fence(Ordering::Release);
            b.seq.fetch_add(1, Ordering::Release); // even: consistent
        }
    }
}

impl Drop for StatusWriter {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut _ });
            let _ = CloseHandle(self.map);
        }
    }
}

/// Window side: read-only view of the block.
pub struct StatusReader {
    map: HANDLE,
    view: *const StatusBlock,
}

impl StatusReader {
    pub fn open() -> Option<Self> {
        let size = size_of::<StatusBlock>();
        unsafe {
            let map = OpenFileMappingW(FILE_MAP_READ.0, false, NAME).ok()?;
            let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, size).Value as *const StatusBlock;
            if view.is_null() {
                let _ = CloseHandle(map);
                return None;
            }
            Some(StatusReader { map, view })
        }
    }

    /// A consistent copy of the block, or None if the writer kept it busy.
    pub fn read(&self) -> Option<Box<StatusBlock>> {
        unsafe {
            for _ in 0..16 {
                let s1 = (*self.view).seq.load(Ordering::Acquire);
                if s1 & 1 == 1 {
                    std::hint::spin_loop();
                    continue;
                }
                let mut out = Box::<StatusBlock>::new_uninit();
                std::ptr::copy_nonoverlapping(self.view, out.as_mut_ptr(), 1);
                fence(Ordering::Acquire);
                if (*self.view).seq.load(Ordering::Relaxed) == s1 {
                    let b = out.assume_init();
                    return (b.magic == MAGIC).then_some(b);
                }
            }
            None
        }
    }
}

impl Drop for StatusReader {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut _ });
            let _ = CloseHandle(self.map);
        }
    }
}

/// Asks the running tray process to re-read config.ini right away.
pub fn notify_tray_reload() -> bool {
    unsafe {
        match FindWindowW(TRAY_CLASS, PCWSTR::null()) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                PostMessageW(Some(hwnd), WM_RELOAD, Default::default(), Default::default()).is_ok()
            }
            _ => false,
        }
    }
}
