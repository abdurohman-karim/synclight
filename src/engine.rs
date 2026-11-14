//! Background sync engine: capture -> zones -> smoothing -> strip.
//!
//! Runs on its own thread and recovers by itself: the strip can be unplugged and
//! plugged back, the display mode can change, the screen can be locked. While the
//! original SyncLight is running, the engine releases the strip and waits.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use crate::capture::{Capturer, Grab};
use crate::color::Pipeline;
use crate::config::Settings;
use crate::device::{Device, DeviceInfo};
use crate::layout::{self, Edge, Layout, LayoutConfig, PixRect};
use crate::ipc::{self, StatusBlock, StatusWriter};
use crate::logger;
use crate::sender::Sender;

const KEEPALIVE: Duration = Duration::from_millis(500);
const RETRY: Duration = Duration::from_secs(2);
const CAPTURE_LOST_OFF: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Starting,
    Off,
    Running,
    NoDevice,
    NoCapture(String),
    PausedForSyncLight,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: State,
    pub device: Option<DeviceInfo>,
    pub screen: String,
    pub layout: String,
    /// Downscaled frames delivered per second.
    pub fps: f32,
    /// Frames written to the strip per second.
    pub sends: f32,
    /// Time the engine thread was held by capture, per frame / worst single call.
    pub grab_ms: f32,
    pub grab_max_ms: f32,
    /// GPU submit -> pixels on CPU, average / worst.
    pub latency_ms: f32,
    pub latency_max_ms: f32,
    /// Frames dropped because the GPU was still busy with earlier ones.
    pub gpu_skipped: u32,
    pub proc_ms: f32,
    /// USB write time per frame (sender thread), average / worst.
    pub hid_ms: f32,
    pub hid_max_ms: f32,
    /// Frames replaced by newer ones before the strip took them.
    pub replaced: u32,
    pub preview: Vec<[u8; 3]>,
    /// Incremented on every update.
    pub seq: u64,
}

impl Status {
    fn new() -> Self {
        Status {
            state: State::Starting,
            device: None,
            screen: String::new(),
            layout: String::new(),
            fps: 0.0,
            sends: 0.0,
            grab_ms: 0.0,
            grab_max_ms: 0.0,
            latency_ms: 0.0,
            latency_max_ms: 0.0,
            gpu_skipped: 0,
            proc_ms: 0.0,
            hid_ms: 0.0,
            hid_max_ms: 0.0,
            replaced: 0,
            preview: Vec::new(),
            seq: 0,
        }
    }

    /// One-line human readable state.
    pub fn summary(&self) -> String {
        match &self.state {
            State::Starting => "Запуск...".into(),
            State::Off => "Синхронизация выключена".into(),
            State::Running => format!("Работает: {:.0} кадров экрана/с, {:.0} отправок/с", self.fps, self.sends),
            State::NoDevice => "Лента не подключена".into(),
            State::NoCapture(e) => format!("Нет захвата экрана: {e}"),
            State::PausedForSyncLight => "Пауза: запущен SyncLight".into(),
            State::Error(e) => format!("Ошибка: {e}"),
        }
    }
}

pub struct Shared {
    settings: Mutex<Settings>,
    rev: AtomicU64,
    status: Mutex<Status>,
    stop: AtomicBool,
}

impl Shared {
    pub fn new(settings: Settings) -> Arc<Self> {
        Arc::new(Shared {
            settings: Mutex::new(settings),
            rev: AtomicU64::new(1),
            status: Mutex::new(Status::new()),
            stop: AtomicBool::new(false),
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Applies a change and returns the new settings.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Settings {
        let mut s = self.settings.lock().unwrap();
        f(&mut s);
        *s = s.clone().sanitize();
        self.rev.fetch_add(1, Ordering::SeqCst);
        s.clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

pub struct Options {
    /// Capture and compute only, never touch the strip.
    pub dry_run: bool,
    /// Release the strip while the original SyncLight is running.
    pub yield_to_synclight: bool,
    /// Publish live status (LED colours, stats) for the settings window.
    pub publish_status: bool,
}

fn state_code(s: &State) -> u8 {
    match s {
        State::Starting => ipc::state::STARTING,
        State::Off => ipc::state::OFF,
        State::Running => ipc::state::RUNNING,
        State::NoDevice => ipc::state::NO_DEVICE,
        State::NoCapture(_) => ipc::state::NO_CAPTURE,
        State::PausedForSyncLight => ipc::state::PAUSED,
        State::Error(_) => ipc::state::ERROR,
    }
}

/// Builds the LED layout for a device and screen size.
pub fn build_layout(info: Option<&DeviceInfo>, w: u32, h: u32, s: &Settings) -> Result<(Layout, String)> {
    let model = info.map(|i| i.id.as_str()).unwrap_or("000501");
    let lamps = info.map(|i| i.lamps as usize).unwrap_or(65);
    let size = info.map(|i| i.display_size).unwrap_or(27);
    let (mut hz, mut vt) = layout::edge_lamps(model, size, lamps, s.edges, w, h);
    if let Some(x) = s.horizontal {
        hz = x;
    }
    if let Some(x) = s.vertical {
        vt = x;
    }
    let (cols, rows) = layout::grid(model, lamps, w, h);
    let lay = Layout::new(&LayoutConfig {
        cols,
        rows,
        horizontal: hz,
        vertical: vt,
        edges: s.edges,
        reverse: s.reverse,
        per_led: s.per_led,
    })?;
    let zones = if s.per_led {
        format!("своя зона у каждого светодиода ({} зон)", lay.segments.len())
    } else {
        format!("сетка {cols}x{rows} ({} зон)", lay.segments.len())
    };
    let mut desc = format!("{zones}, сверху {hz}, по бокам по {vt}, всего {} светодиодов", lay.total_leds);
    if lay.total_leds != lamps {
        desc += &format!(" (внимание: лента сообщает {lamps})");
    }
    Ok((lay, desc))
}

pub fn synclight_running() -> bool {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case("SyncLight.exe") {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

#[derive(Clone, PartialEq)]
struct LayoutKey {
    w: u32,
    h: u32,
    model: String,
    lamps: u8,
    size: u8,
    edges: u8,
    reverse: bool,
    horizontal: Option<usize>,
    vertical: Option<usize>,
    per_led: bool,
}

/// Engine-side counters; capture and USB timings come from their own components.
struct Stats {
    since: Instant,
    ticks: u32,
    posted: u32,
    proc: Duration,
}

impl Stats {
    fn new() -> Self {
        Stats { since: Instant::now(), ticks: 0, posted: 0, proc: Duration::ZERO }
    }
}

struct Engine {
    shared: Arc<Shared>,
    opts: Options,
    settings: Settings,
    rev: u64,
    pipe: Pipeline,

    /// Strip connection; USB writes happen on the sender thread.
    out: Option<Sender>,
    info: Option<DeviceInfo>,
    cap: Option<Capturer>,
    lay: Option<(LayoutKey, Layout)>,

    /// Per segment: screen region and inner fallback region, in downscaled pixels.
    regions: Vec<(PixRect, Option<PixRect>)>,
    /// Per segment: latest captured colour (target) and smoothed colour (current).
    target: Vec<[f32; 3]>,
    current: Vec<[f32; 3]>,
    /// Scratch buffer for inner-band colours (letterbox fallback).
    inner: Vec<[f32; 3]>,
    segs: Vec<[u8; 5]>,
    last_sent: Vec<[u8; 5]>,
    /// Per LED: colour as it should look, for the settings window preview.
    preview: Vec<[u8; 3]>,
    ipc: Option<StatusWriter>,
    have_frame: bool,
    idle: bool,
    lights_on: bool,

    next_dev_try: Instant,
    next_cap_try: Instant,
    next_guard: Instant,
    guard_hit: bool,
    capture_lost_since: Option<Instant>,
    last_error: String,
    last_tick: Instant,
    last_send: Instant,
    stats: Stats,
}

pub fn run(shared: Arc<Shared>, opts: Options) {
    let settings = shared.settings();
    let now = Instant::now();
    let ipc = if opts.publish_status { StatusWriter::create() } else { None };
    let mut e = Engine {
        pipe: pipeline(&settings),
        rev: shared.rev.load(Ordering::SeqCst),
        settings,
        shared,
        opts,
        ipc,
        preview: Vec::new(),
        out: None,
        info: None,
        cap: None,
        lay: None,
        regions: Vec::new(),
        target: Vec::new(),
        current: Vec::new(),
        inner: Vec::new(),
        segs: Vec::new(),
        last_sent: Vec::new(),
        have_frame: false,
        idle: true,
        lights_on: false,
        next_dev_try: now,
        next_cap_try: now,
        next_guard: now,
        guard_hit: false,
        capture_lost_since: None,
        last_error: String::new(),
        last_tick: now,
        last_send: now,
        stats: Stats::new(),
    };
    e.run();
}

fn pipeline(s: &Settings) -> Pipeline {
    Pipeline {
        wire: Pipeline::wire_map(&s.wire).unwrap_or([0, 1, 2]),
        black: 20,
        vivid: s.vivid,
        gain: s.gain,
    }
}

impl Engine {
    fn run(&mut self) {
        logger::info("движок запущен");
        while !self.shared.stopping() {
            self.reload_settings();

            if self.opts.yield_to_synclight && self.synclight_guard() {
                self.set_state(State::PausedForSyncLight);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            if !self.settings.enabled {
                self.lights_off();
                self.cap = None; // release the duplication while idle
                self.set_state(State::Off);
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            if !self.ensure_device() || !self.ensure_capture() || !self.ensure_layout() {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            self.tick();
        }
        self.lights_off();
        logger::info("движок остановлен");
    }

    fn set_state(&mut self, state: State) {
        let summary = {
            let mut st = self.shared.status.lock().unwrap();
            if st.state != state {
                let mut tmp = st.clone();
                tmp.state = state.clone();
                logger::info(format!("состояние: {}", tmp.summary()));
                st.state = state.clone();
            }
            st.seq += 1;
            st.summary()
        };
        let running = state == State::Running;
        self.publish(|b| {
            b.state = state_code(&state);
            b.set_summary(&summary);
            if !running {
                b.leds = [[0; 3]; ipc::MAX_LEDS];
            }
        });
    }

    /// Updates the shared status block for the settings window (if enabled).
    fn publish(&self, f: impl FnOnce(&mut StatusBlock)) {
        if let Some(w) = self.ipc.as_ref() {
            w.update(f);
        }
    }

    fn log_error_once(&mut self, msg: String) {
        if msg != self.last_error {
            logger::info(&msg);
            self.last_error = msg;
        }
    }

    fn reload_settings(&mut self) {
        let rev = self.shared.rev.load(Ordering::SeqCst);
        if rev == self.rev {
            return;
        }
        self.rev = rev;
        let new = self.shared.settings();
        if new.display != self.settings.display {
            self.cap = None;
        }
        self.pipe = pipeline(&new);
        self.settings = new;
        self.idle = false; // re-evaluate colours right away
    }

    /// True while SyncLight.exe is running (checked every 3 s).
    fn synclight_guard(&mut self) -> bool {
        if Instant::now() >= self.next_guard {
            self.next_guard = Instant::now() + Duration::from_secs(3);
            let hit = synclight_running();
            if hit && !self.guard_hit {
                logger::info("обнаружен SyncLight: отдаю ему ленту");
                // SyncLight drives the strip now: close our handle without touching the lights.
                self.out = None;
                self.lights_on = false;
                self.cap = None;
            }
            self.guard_hit = hit;
        }
        self.guard_hit
    }

    fn ensure_device(&mut self) -> bool {
        if self.opts.dry_run || self.out.is_some() {
            return true;
        }
        if Instant::now() < self.next_dev_try {
            return false;
        }
        self.next_dev_try = Instant::now() + RETRY;
        let mut dev = match Device::open() {
            Ok(d) => d,
            Err(e) => {
                self.log_error_once(format!("лента: {e:#}"));
                self.set_state(State::NoDevice);
                return false;
            }
        };
        let info = match dev.read_info() {
            Ok(i) => i,
            Err(e) => {
                self.log_error_once(format!("лента: {e:#}"));
                self.set_state(State::NoDevice);
                return false;
            }
        };
        if (info.version[0], info.version[1], info.version[2]) <= (1, 0, 2) {
            self.next_dev_try = Instant::now() + Duration::from_secs(30);
            self.set_state(State::Error(format!(
                "прошивка {}.{}.{} не поддерживается",
                info.version[0], info.version[1], info.version[2]
            )));
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
        let _ = dev.disable_open_url();
        logger::info(format!(
            "лента подключена: модель {}, {} светодиодов, монитор {}\", прошивка {}.{}.{}",
            info.id, info.lamps, info.display_size, info.version[0], info.version[1], info.version[2]
        ));
        self.last_error.clear();
        self.shared.status.lock().unwrap().device = Some(info.clone());
        self.publish(|b| {
            b.has_device = 1;
            b.lamps = info.lamps;
            b.display_size = info.display_size;
            b.firmware = [info.version[0], info.version[1], info.version[2], 0];
            b.model = [0; 8];
            let id = info.id.as_bytes();
            b.model[..id.len().min(8)].copy_from_slice(&id[..id.len().min(8)]);
        });
        self.info = Some(info);
        self.out = Some(Sender::start(dev));
        self.lights_on = false;
        true
    }

    fn release_device(&mut self, reason: &str) {
        if self.out.take().is_some() {
            logger::info(format!("лента отключена: {reason}"));
        }
        self.lights_on = false;
        self.next_dev_try = Instant::now() + Duration::from_secs(1);
        self.shared.status.lock().unwrap().device = None;
        self.publish(|b| b.has_device = 0);
    }

    fn ensure_capture(&mut self) -> bool {
        if self.cap.is_some() {
            return true;
        }
        if Instant::now() < self.next_cap_try {
            return false;
        }
        self.next_cap_try = Instant::now() + RETRY;
        match Capturer::new(self.settings.display) {
            Ok(c) => {
                let screen = format!("{} {}x{}", c.name, c.src_w, c.src_h);
                logger::info(format!(
                    "захват экрана: {screen}, на видеокарте уменьшается до {}x{}, ожидание GPU: {}",
                    c.small_w,
                    c.small_h,
                    if c.uses_fence() { "fence-событие" } else { "опрос" }
                ));
                self.publish(|b| b.set_screen(&screen));
                self.shared.status.lock().unwrap().screen = screen;
                self.cap = Some(c);
                self.capture_lost_since = None;
                true
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.log_error_once(format!("захват экрана: {msg}"));
                self.set_state(State::NoCapture(msg));
                false
            }
        }
    }

    fn ensure_layout(&mut self) -> bool {
        let cap = self.cap.as_ref().unwrap();
        let s = &self.settings;
        let key = LayoutKey {
            w: cap.src_w,
            h: cap.src_h,
            model: self.info.as_ref().map(|i| i.id.clone()).unwrap_or_default(),
            lamps: self.info.as_ref().map(|i| i.lamps).unwrap_or(0),
            size: self.info.as_ref().map(|i| i.display_size).unwrap_or(0),
            edges: s.edges,
            reverse: s.reverse,
            horizontal: s.horizontal,
            vertical: s.vertical,
            per_led: s.per_led,
        };
        if matches!(&self.lay, Some((k, _)) if *k == key) {
            return true;
        }
        match build_layout(self.info.as_ref(), cap.src_w, cap.src_h, s) {
            Ok((lay, desc)) => {
                logger::info(format!("раскладка: {desc}"));
                self.shared.status.lock().unwrap().layout = desc;
                let (w, h) = (cap.small_w as usize, cap.small_h as usize);
                self.regions = lay
                    .segments
                    .iter()
                    .map(|s| (s.rect.to_pixels(w, h), s.alt.map(|a| a.to_pixels(w, h))))
                    .collect();
                let n = lay.segments.len();
                self.target = vec![[0.0; 3]; n];
                self.current = vec![[0.0; 3]; n];
                self.inner = vec![[0.0; 3]; n];
                self.segs = Vec::with_capacity(n);
                self.preview = vec![[0; 3]; lay.total_leds];
                self.last_sent.clear();
                self.have_frame = false;
                self.idle = false;
                let leds_on = |edge: Edge| -> u16 {
                    lay.segments.iter().filter(|s| s.edge == edge).map(|s| (s.end - s.start + 1) as u16).sum()
                };
                let (top, side) = (leds_on(Edge::Top), leds_on(Edge::Left));
                let (edges, reverse, per_led, total) = (lay.edges, s.reverse, lay.per_led, lay.total_leds);
                self.publish(|b| {
                    b.horizontal = top;
                    b.vertical = side;
                    b.edges = edges;
                    b.reverse = reverse as u8;
                    b.per_led = per_led as u8;
                    b.zones = n.min(255) as u8;
                    b.led_count = total.min(ipc::MAX_LEDS) as u16;
                });
                self.lay = Some((key, lay));
                true
            }
            Err(e) => {
                self.set_state(State::Error(format!("раскладка: {e:#}")));
                false
            }
        }
    }

    fn lights_off(&mut self) {
        if !self.lights_on {
            return;
        }
        if let Some(o) = self.out.as_ref() {
            o.turn_off(Duration::from_millis(500));
        }
        self.lights_on = false;
        self.last_sent.clear();
    }

    fn tick(&mut self) {
        let tick = Instant::now();
        let dt = (tick - self.last_tick).as_secs_f32().min(0.5);
        self.last_tick = tick;
        let period = Duration::from_secs_f64(1.0 / self.settings.fps as f64);

        // The sender thread stops on the first USB error (strip unplugged).
        if let Some(err) = self.out.as_ref().and_then(|o| o.take_error()) {
            self.release_device(&err);
            return;
        }

        // --- capture (never blocks longer than a couple of ms) ---
        let timeout = if self.idle { 100 } else { 0 };
        let res = {
            let cap = self.cap.as_mut().unwrap();
            cap.grab(timeout).map(|g| (matches!(g, Grab::New), cap.is_live(), cap.has_pending()))
        };
        let (new_frame, live, pending) = match res {
            Ok(r) => r,
            Err(e) => {
                logger::info(format!("ошибка захвата: {e:#}; перезапускаю захват"));
                self.cap = None;
                self.next_cap_try = Instant::now() + Duration::from_secs(1);
                return;
            }
        };
        if live {
            self.capture_lost_since = None;
        } else {
            // Lock screen / UAC: don't leave stale colours on the wall.
            let since = *self.capture_lost_since.get_or_insert(tick);
            if since.elapsed() >= CAPTURE_LOST_OFF {
                self.lights_off();
            }
            self.idle = true;
            return;
        }

        let p0 = Instant::now();
        let (_, lay) = self.lay.as_ref().unwrap();
        let cap = self.cap.as_ref().unwrap();
        // After a layout change on a still screen, reuse the last captured frame.
        if new_frame || (!self.have_frame && cap.has_frame()) {
            for (i, (outer, inner)) in self.regions.iter().enumerate() {
                self.target[i] = cap.rect_average(outer);
                if let Some(r) = inner {
                    self.inner[i] = cap.rect_average(r);
                }
            }
            // Letterboxed video: when the whole outer band of the top (bottom) edge is
            // black, take colours from the next band inward, as SyncLight does.
            for edge in [Edge::Top, Edge::Bottom] {
                let band = || lay.segments.iter().enumerate().filter(move |(_, s)| s.edge == edge && s.alt.is_some());
                let present = band().next().is_some();
                let black = band().all(|(i, _)| self.target[i].iter().all(|&x| x < 10.0));
                if present && black {
                    for (i, _) in band() {
                        self.target[i] = self.inner[i];
                    }
                }
            }
            if !self.have_frame {
                self.current.copy_from_slice(&self.target);
                self.have_frame = true;
            }
        }

        let mut max_delta = 0f32;
        if self.have_frame {
            // --- smoothing (exponential, frame-rate independent) ---
            let tau = self.settings.smooth_ms / 1000.0;
            let alpha = if tau <= 0.0 { 1.0 } else { 1.0 - (-dt / tau).exp() };
            for (c, t) in self.current.iter_mut().zip(&self.target) {
                for k in 0..3 {
                    let d = t[k] - c[k];
                    c[k] += alpha * d;
                    max_delta = max_delta.max(d.abs());
                }
            }

            // --- colours per segment; neighbours with the same colour share one segment
            // (the frame format takes LED ranges), which keeps USB packets small ---
            self.segs.clear();
            for (s, cur) in lay.segments.iter().zip(&self.current) {
                let look = self.pipe.look(*cur);
                for led in s.start..=s.end {
                    if let Some(p) = self.preview.get_mut(led as usize - 1) {
                        *p = look;
                    }
                }
                let c = self.pipe.to_strip(look);
                if let Some(last) = self.segs.last_mut() {
                    if last[1..4] == c[..] && last[4] as usize + 1 == s.start as usize {
                        last[4] = s.end;
                        continue;
                    }
                }
                self.segs.push([s.start, c[0], c[1], c[2], s.end]);
            }
            self.stats.proc += p0.elapsed();
            self.stats.ticks += 1;

            // --- hand over to the sender thread ---
            if self.segs != self.last_sent || self.last_send.elapsed() >= KEEPALIVE {
                if let Some(o) = self.out.as_ref() {
                    o.post(&self.segs);
                }
                self.lights_on = !self.opts.dry_run;
                self.stats.posted += 1;
                self.last_sent.clone_from(&self.segs);
                self.last_send = Instant::now();
                if let Some(w) = self.ipc.as_ref() {
                    let preview = &self.preview;
                    w.update(|b| {
                        let n = preview.len().min(ipc::MAX_LEDS);
                        b.leds[..n].copy_from_slice(&preview[..n]);
                        b.frame = b.frame.wrapping_add(1);
                    });
                }
            }
        }

        // Idle only when nothing is moving: no new image, nothing on the GPU, colours settled.
        self.idle = !new_frame && !pending && max_delta < 0.5;
        self.publish_stats();
        if !self.idle {
            if let Some(rest) = period.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }

    fn publish_stats(&mut self) {
        let el = self.stats.since.elapsed();
        if el < Duration::from_secs(1) {
            return;
        }
        let s = el.as_secs_f32();
        let ms = |d: Duration| d.as_secs_f32() * 1000.0;
        let per = |d: Duration, n: u32| if n == 0 { 0.0 } else { ms(d) / n as f32 };
        let cs = self.cap.as_mut().map(|c| c.take_stats()).unwrap_or_default();
        let ss = self.out.as_ref().map(|o| o.take_stats()).unwrap_or_default();
        let st = &self.stats;
        let preview = self.segs.iter().map(|c| [c[1], c[2], c[3]]).collect();
        {
            let mut out = self.shared.status.lock().unwrap();
            out.fps = cs.frames as f32 / s;
            out.sends = if self.out.is_some() { ss.sent as f32 } else { st.posted as f32 } / s;
            out.grab_ms = per(cs.busy, cs.calls);
            out.grab_max_ms = ms(cs.busy_max);
            out.latency_ms = per(cs.latency, cs.frames);
            out.latency_max_ms = ms(cs.latency_max);
            out.gpu_skipped = cs.skipped;
            out.proc_ms = per(st.proc, st.ticks);
            out.hid_ms = per(ss.time, ss.sent);
            out.hid_max_ms = ms(ss.time_max);
            out.replaced = ss.replaced;
            out.preview = preview;
        }
        let (fps, sends) = (cs.frames as f32 / s, if self.out.is_some() { ss.sent as f32 } else { st.posted as f32 } / s);
        let (grab, lat, hid) = (per(cs.busy, cs.calls), per(cs.latency, cs.frames), per(ss.time, ss.sent));
        self.publish(|b| {
            b.fps = fps;
            b.sends = sends;
            b.grab_ms = grab;
            b.latency_ms = lat;
            b.hid_ms = hid;
        });
        self.stats = Stats::new();
        self.set_state(State::Running);
    }
}
