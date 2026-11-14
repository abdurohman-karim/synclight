//! Sends frames to the strip on its own thread, so USB writes (about 1 ms per
//! 64-byte report) never hold up screen capture. Only the newest frame is kept:
//! if the strip is slower than the screen, stale frames are replaced, not queued.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::device::Device;

#[derive(Default)]
struct Mailbox {
    frame: Vec<[u8; 5]>,
    has_frame: bool,
    off_requested: bool,
    off_done: u64,
    stop: bool,
    error: Option<String>,
    stats: SendStats,
}

/// Sender timings accumulated since the last `take_stats`.
#[derive(Clone, Copy, Default, Debug)]
pub struct SendStats {
    pub sent: u32,
    /// Frames replaced by a newer one before they could be sent.
    pub replaced: u32,
    pub time: Duration,
    pub time_max: Duration,
}

struct Inner {
    m: Mutex<Mailbox>,
    cv: Condvar,
}

pub struct Sender {
    inner: Arc<Inner>,
    handle: Option<JoinHandle<()>>,
}

impl Sender {
    pub fn start(dev: Device) -> Sender {
        let inner = Arc::new(Inner { m: Mutex::new(Mailbox::default()), cv: Condvar::new() });
        let worker_inner = inner.clone();
        let handle = std::thread::Builder::new()
            .name("hid-sender".into())
            .spawn(move || worker(dev, worker_inner))
            .expect("hid sender thread");
        Sender { inner, handle: Some(handle) }
    }

    /// Queues a frame, replacing one that has not been sent yet.
    pub fn post(&self, segs: &[[u8; 5]]) {
        let mut g = self.inner.m.lock().unwrap();
        if g.has_frame {
            g.stats.replaced += 1;
        }
        g.frame.clear();
        g.frame.extend_from_slice(segs);
        g.has_frame = true;
        self.inner.cv.notify_all();
    }

    /// Drops any queued frame, turns the lights off and waits (bounded) until done.
    pub fn turn_off(&self, timeout: Duration) {
        let g = {
            let mut g = self.inner.m.lock().unwrap();
            g.has_frame = false;
            g.off_requested = true;
            g
        };
        let target = g.off_done + 1;
        self.inner.cv.notify_all();
        let _ = self
            .inner
            .cv
            .wait_timeout_while(g, timeout, |g| g.off_done < target && g.error.is_none() && !g.stop);
    }

    /// Error that stopped the sender (strip unplugged etc.), if any.
    pub fn take_error(&self) -> Option<String> {
        self.inner.m.lock().unwrap().error.take()
    }

    pub fn take_stats(&self) -> SendStats {
        std::mem::take(&mut self.inner.m.lock().unwrap().stats)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.inner.m.lock().unwrap().stop = true;
        self.inner.cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn worker(mut dev: Device, inner: Arc<Inner>) {
    let mut frame: Vec<[u8; 5]> = Vec::new();
    loop {
        let (send, off, stop) = {
            let mut g = inner.m.lock().unwrap();
            while !g.has_frame && !g.off_requested && !g.stop {
                g = inner.cv.wait(g).unwrap();
            }
            let send = g.has_frame;
            if send {
                std::mem::swap(&mut frame, &mut g.frame);
                g.has_frame = false;
            }
            (send, std::mem::take(&mut g.off_requested), g.stop)
        };
        if send && !stop {
            let t = Instant::now();
            let r = dev.send_sync(&frame);
            let el = t.elapsed();
            let mut g = inner.m.lock().unwrap();
            match r {
                Ok(()) => {
                    g.stats.sent += 1;
                    g.stats.time += el;
                    g.stats.time_max = g.stats.time_max.max(el);
                }
                Err(e) => {
                    g.error = Some(format!("{e:#}"));
                    inner.cv.notify_all();
                    return;
                }
            }
        }
        if off {
            for _ in 0..3 {
                let _ = dev.turn_off();
                std::thread::sleep(Duration::from_millis(20));
            }
            inner.m.lock().unwrap().off_done += 1;
            inner.cv.notify_all();
        }
        if stop {
            return;
        }
    }
}
