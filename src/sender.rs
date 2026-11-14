//! Sends to the strip on its own thread, so USB writes (about 1 ms per 64-byte
//! report) never hold up screen capture.
//!
//! * screen frames and rhythm levels: only the newest one is kept (stale ones are replaced);
//! * commands (colour, effect, speed, brightness...): queued in order, never dropped,
//!   and they discard a pending frame/level so an old frame can't overwrite a new mode.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::device::Device;

#[derive(Clone, Debug)]
pub enum Cmd {
    SectionLed(Vec<[u8; 5]>),
    LedEffect { group: u8, index: u8 },
    DynamicSpeed(u8),
    SoundSensitivity(u8),
    Brightness(u8),
}

#[derive(Default)]
struct Mailbox {
    frame: Vec<[u8; 5]>,
    has_frame: bool,
    rhythm: Option<(u8, u8)>,
    commands: VecDeque<Cmd>,
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

    /// Queues a screen frame, replacing one that has not been sent yet.
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

    /// Queues a "Computer Rhythm" step, replacing one that has not been sent yet.
    pub fn rhythm(&self, index: u8, level: u8) {
        let mut g = self.inner.m.lock().unwrap();
        g.rhythm = Some((index, level));
        self.inner.cv.notify_all();
    }

    /// Queues a command; pending frames and rhythm steps are dropped.
    pub fn command(&self, cmd: Cmd) {
        let mut g = self.inner.m.lock().unwrap();
        g.has_frame = false;
        g.rhythm = None;
        g.commands.push_back(cmd);
        self.inner.cv.notify_all();
    }

    /// Drops everything queued, turns the lights off and waits (bounded) until done.
    pub fn turn_off(&self, timeout: Duration) {
        let g = {
            let mut g = self.inner.m.lock().unwrap();
            g.has_frame = false;
            g.rhythm = None;
            g.commands.clear();
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

enum Job {
    Off,
    Command(Cmd),
    Frame,
    Rhythm(u8, u8),
    Stop,
}

fn run_command(dev: &mut Device, cmd: &Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::SectionLed(segs) => dev.set_section_led(segs),
        Cmd::LedEffect { group, index } => dev.set_led_effect(*group, *index),
        Cmd::DynamicSpeed(v) => dev.set_dynamic_speed(*v),
        Cmd::SoundSensitivity(v) => dev.set_sound_sensitivity(*v),
        Cmd::Brightness(v) => dev.set_brightness(*v),
    }
}

fn worker(mut dev: Device, inner: Arc<Inner>) {
    let mut frame: Vec<[u8; 5]> = Vec::new();
    loop {
        let job = {
            let mut g = inner.m.lock().unwrap();
            while !g.stop && !g.off_requested && g.commands.is_empty() && !g.has_frame && g.rhythm.is_none() {
                g = inner.cv.wait(g).unwrap();
            }
            if g.stop {
                Job::Stop
            } else if std::mem::take(&mut g.off_requested) {
                Job::Off
            } else if let Some(c) = g.commands.pop_front() {
                Job::Command(c)
            } else if g.has_frame {
                std::mem::swap(&mut frame, &mut g.frame);
                g.has_frame = false;
                Job::Frame
            } else {
                let (i, l) = g.rhythm.take().unwrap();
                Job::Rhythm(i, l)
            }
        };
        let t = Instant::now();
        let result = match &job {
            Job::Stop => return,
            Job::Off => {
                for _ in 0..3 {
                    let _ = dev.turn_off();
                    std::thread::sleep(Duration::from_millis(20));
                }
                inner.m.lock().unwrap().off_done += 1;
                inner.cv.notify_all();
                continue;
            }
            Job::Command(c) => {
                let r = run_command(&mut dev, c);
                // The firmware needs a moment between configuration commands (SyncLight waits 20-80 ms).
                std::thread::sleep(Duration::from_millis(30));
                r
            }
            Job::Frame => dev.send_sync(&frame),
            Job::Rhythm(i, l) => dev.computer_rhythm(*i, *l),
        };
        let el = t.elapsed();
        let mut g = inner.m.lock().unwrap();
        match result {
            Ok(()) => {
                if matches!(job, Job::Frame) {
                    g.stats.sent += 1;
                    g.stats.time += el;
                    g.stats.time_max = g.stats.time_max.max(el);
                }
            }
            Err(e) => {
                g.error = Some(format!("{e:#}"));
                inner.cv.notify_all();
                return;
            }
        }
    }
}
