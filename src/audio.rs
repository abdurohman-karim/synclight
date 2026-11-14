//! Sound level of the default Windows output device, for "Computer Rhythm".
//!
//! Same source as the original SyncLight: the endpoint peak meter
//! (`IAudioMeterInformation::GetPeakValue`, the bar you see in the volume mixer).
//! Nothing is recorded; reading it costs microseconds.

use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

pub struct PeakMeter {
    meter: Option<IAudioMeterInformation>,
    opened_at: Instant,
    next_try: Instant,
}

/// The default output may change (headphones plugged in): re-open it this often.
const REOPEN_EVERY: Duration = Duration::from_secs(5);

impl PeakMeter {
    pub fn new() -> Self {
        unsafe {
            // The engine thread has no other COM users; MTA is fine for the endpoint API.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let now = Instant::now();
        PeakMeter { meter: None, opened_at: now, next_try: now }
    }

    fn open() -> windows::core::Result<IAudioMeterInformation> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            device.Activate(CLSCTX_ALL, None)
        }
    }

    /// Peak sample level since the last call, 0.0..=1.0; None if no output device.
    pub fn peak(&mut self) -> Option<f32> {
        let now = Instant::now();
        if self.meter.is_some() && now.duration_since(self.opened_at) >= REOPEN_EVERY {
            self.meter = None;
            self.next_try = now;
        }
        if self.meter.is_none() && now >= self.next_try {
            match Self::open() {
                Ok(m) => {
                    self.meter = Some(m);
                    self.opened_at = now;
                }
                Err(_) => self.next_try = now + Duration::from_secs(2),
            }
        }
        let value = unsafe { self.meter.as_ref()?.GetPeakValue() };
        match value {
            Ok(v) => Some(v.clamp(0.0, 1.0)),
            Err(_) => {
                self.meter = None;
                self.next_try = now + Duration::from_secs(1);
                None
            }
        }
    }
}

impl Default for PeakMeter {
    fn default() -> Self {
        Self::new()
    }
}
