//! Screen capture via DXGI Desktop Duplication.
//!
//! The original SyncLight copies the whole screen through GDI `BitBlt` on the CPU
//! every 20 ms. Here the GPU does the heavy lifting: the frame is copied into a
//! mip-mapped texture, `GenerateMips` box-filters it down (e.g. 3840x2160 -> 240x135),
//! and only that tiny level is read back. When the screen does not change,
//! `AcquireNextFrame` simply times out and no work is done at all.
//!
//! Read-back is pipelined through a small ring of staging textures and polled with
//! `D3D11_MAP_FLAG_DO_NOT_WAIT`: when the GPU is busy (video, games) the capture
//! thread waits at most `MAX_WAIT` and picks the frame up on a later call instead
//! of blocking inside `Map`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::layout::PixRect;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateWaitableTimerExW, SetWaitableTimer, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Smallest side of the read-back image. 2160p -> mip 4 -> 240x135.
const MIN_SMALL_SIDE: u32 = 128;
/// Frames that may be in flight on the GPU at once.
const RING: usize = 3;
/// Longest the caller is held waiting for the GPU in one `grab` call.
const MAX_WAIT: Duration = Duration::from_micros(2000);
const POLL_STEP: Duration = Duration::from_micros(250);

pub fn init_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub enum Grab {
    /// A new downscaled frame is in the internal buffer.
    New,
    /// Nothing new yet (screen unchanged, GPU still busy, or capture unavailable).
    Unchanged,
}

/// Capture timings accumulated since the last `take_stats`.
#[derive(Clone, Copy, Default, Debug)]
pub struct CapStats {
    /// Frames whose read-back completed.
    pub frames: u32,
    /// `grab` calls (each one is a tick of the engine loop).
    pub calls: u32,
    /// Frames skipped because the GPU still had `RING` frames in flight.
    pub skipped: u32,
    /// Time the caller was held inside `grab` (not counting waiting for a new image).
    pub busy: Duration,
    pub busy_max: Duration,
    /// From submitting GPU work to having the pixels on the CPU.
    pub latency: Duration,
    pub latency_max: Duration,
}

struct Textures {
    mip: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    staging: Vec<ID3D11Texture2D>,
}

/// GPU fence + event: the GPU wakes us exactly when a read-back is ready,
/// and a high-resolution timer bounds the wait (plain wait timeouts can be ~15 ms).
struct Fence {
    fence: ID3D11Fence,
    ctx4: ID3D11DeviceContext4,
    event: HANDLE,
    timer: HANDLE,
    value: u64,
}

impl Fence {
    fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Option<Fence> {
        unsafe {
            let dev5: ID3D11Device5 = device.cast().ok()?;
            let ctx4: ID3D11DeviceContext4 = ctx.cast().ok()?;
            let mut fence: Option<ID3D11Fence> = None;
            dev5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence).ok()?;
            let fence = fence?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).ok()?;
            let timer = match CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            ) {
                Ok(t) => t,
                Err(_) => {
                    let _ = CloseHandle(event);
                    return None;
                }
            };
            Some(Fence { fence, ctx4, event, timer, value: 0 })
        }
    }

    fn signal(&mut self) -> u64 {
        self.value += 1;
        unsafe {
            let _ = self.ctx4.Signal(&self.fence, self.value);
        }
        self.value
    }

    fn done(&self, value: u64) -> bool {
        unsafe { self.fence.GetCompletedValue() >= value }
    }

    /// Waits until `value` is reached or `budget` elapses. Returns true if reached.
    fn wait(&self, value: u64, budget: Duration) -> bool {
        if self.done(value) {
            return true;
        }
        unsafe {
            if self.fence.SetEventOnCompletion(value, self.event).is_err() {
                return false;
            }
            // Relative due time in 100 ns units (negative = relative).
            let due = -((budget.as_nanos() / 100).max(1) as i64);
            if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_err() {
                return self.done(value);
            }
            WaitForMultipleObjects(&[self.event, self.timer], false, INFINITE);
        }
        self.done(value)
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.timer);
        }
    }
}

pub struct Capturer {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    tex: Option<Textures>,
    fence: Option<Fence>,
    /// Staging slots with GPU work in flight, oldest first: (slot, submit time, fence value).
    pending: VecDeque<(usize, Instant, u64)>,
    next_slot: usize,
    /// Tightly packed BGRA pixels of the downscaled frame.
    buf: Vec<u8>,
    frame_ready: bool,
    stats: CapStats,
    pub name: String,
    pub src_w: u32,
    pub src_h: u32,
    pub small_w: u32,
    pub small_h: u32,
    pub level: u32,
}

impl Capturer {
    /// Opens the `display_index`-th display attached to the desktop.
    pub fn new(display_index: usize) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let mut found = None;
            let mut seen = 0usize;
            let mut a = 0;
            'outer: while let Ok(adapter) = factory.EnumAdapters1(a) {
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    let desc = output.GetDesc()?;
                    if desc.AttachedToDesktop.as_bool() {
                        if seen == display_index {
                            found = Some((adapter.clone(), output, desc));
                            break 'outer;
                        }
                        seen += 1;
                    }
                    o += 1;
                }
                a += 1;
            }
            let (adapter, output, desc) = found
                .ok_or_else(|| anyhow!("дисплей #{display_index} не найден (всего {seen})"))?;
            let name = String::from_utf16_lossy(&desc.DeviceName)
                .trim_end_matches('\0')
                .to_string();

            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;

            let (device, ctx) = (device.unwrap(), ctx.unwrap());
            let fence = Fence::new(&device, &ctx);
            let mut cap = Capturer {
                device,
                ctx,
                output: output.cast()?,
                dup: None,
                tex: None,
                fence,
                pending: VecDeque::with_capacity(RING),
                next_slot: 0,
                buf: Vec::new(),
                frame_ready: false,
                stats: CapStats::default(),
                name,
                src_w: 0,
                src_h: 0,
                small_w: 0,
                small_h: 0,
                level: 0,
            };
            cap.reopen().context("не удалось начать захват экрана")?;
            Ok(cap)
        }
    }

    /// (Re)creates the duplication interface; recreates textures if the mode changed.
    fn reopen(&mut self) -> Result<()> {
        self.dup = None;
        unsafe {
            let dup = self.output.DuplicateOutput(&self.device).context("DuplicateOutput")?;
            let dd = dup.GetDesc();
            let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            if w == 0 || h == 0 {
                bail!("нулевой размер экрана");
            }
            if self.tex.is_none() || w != self.src_w || h != self.src_h {
                self.create_textures(w, h)?;
            }
            self.dup = Some(dup);
        }
        Ok(())
    }

    fn create_textures(&mut self, w: u32, h: u32) -> Result<()> {
        let mut level = 0;
        while (w >> (level + 1)) >= MIN_SMALL_SIDE && (h >> (level + 1)) >= MIN_SMALL_SIDE {
            level += 1;
        }
        let (sw, sh) = ((w >> level).max(1), (h >> level).max(1));
        let sample = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
        let mip_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: level + 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: sample,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: sw,
            Height: sh,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: sample,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        unsafe {
            let mut mip = None;
            self.device.CreateTexture2D(&mip_desc, None, Some(&mut mip))?;
            let mip = mip.unwrap();
            let mut srv = None;
            self.device.CreateShaderResourceView(&mip, None, Some(&mut srv))?;
            let mut staging = Vec::with_capacity(RING);
            for _ in 0..RING {
                let mut t = None;
                self.device.CreateTexture2D(&staging_desc, None, Some(&mut t))?;
                staging.push(t.unwrap());
            }
            self.tex = Some(Textures { mip, srv: srv.unwrap(), staging });
        }
        self.src_w = w;
        self.src_h = h;
        self.small_w = sw;
        self.small_h = sh;
        self.level = level;
        self.buf = vec![0; (sw * sh * 4) as usize];
        self.frame_ready = false;
        self.pending.clear();
        self.next_slot = 0;
        Ok(())
    }

    /// False while the desktop cannot be captured (lock screen, UAC prompt, mode switch).
    pub fn is_live(&self) -> bool {
        self.dup.is_some()
    }

    /// True when the GPU fence is used for waiting (otherwise: polling fallback).
    pub fn uses_fence(&self) -> bool {
        self.fence.is_some()
    }

    /// GPU work is in flight; the caller should come back soon instead of idling.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A frame has been captured at least once since the textures were created.
    pub fn has_frame(&self) -> bool {
        self.frame_ready
    }

    pub fn take_stats(&mut self) -> CapStats {
        std::mem::take(&mut self.stats)
    }

    /// Waits up to `timeout_ms` for a new desktop image, queues it for downscaling
    /// and returns `New` when a downscaled frame has arrived in the buffer.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        if self.dup.is_none() {
            // Access was lost (mode change, UAC/secure desktop, fullscreen switch). Retry.
            if self.reopen().is_err() {
                std::thread::sleep(Duration::from_millis(timeout_ms.max(100) as u64));
                return Ok(Grab::Unchanged);
            }
        }
        // Don't block for a new image while older frames are still on the GPU.
        let timeout = if self.pending.is_empty() { timeout_ms } else { 0 };
        let dup = self.dup.as_ref().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let acquired = match unsafe { dup.AcquireNextFrame(timeout, &mut info, &mut res) } {
            Ok(()) => true,
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => false,
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                self.pending.clear();
                return Ok(Grab::Unchanged);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        };
        let busy = Instant::now();

        if acquired {
            // LastPresentTime == 0 means only the mouse pointer moved.
            let has_image = info.LastPresentTime != 0 && res.is_some();
            let submit = has_image && self.pending.len() < RING;
            if has_image && !submit {
                self.stats.skipped += 1;
            }
            let copied = if submit { self.copy_full_frame(res.unwrap()) } else { Ok(()) };
            unsafe {
                let _ = self.dup.as_ref().unwrap().ReleaseFrame();
            }
            copied?;
            if submit {
                self.submit_downscale();
            }
        }

        let got = self.poll_readback()?;
        let held = busy.elapsed();
        self.stats.calls += 1;
        self.stats.busy += held;
        self.stats.busy_max = self.stats.busy_max.max(held);
        Ok(if got { Grab::New } else { Grab::Unchanged })
    }

    fn copy_full_frame(&self, res: IDXGIResource) -> Result<()> {
        let tex = self.tex.as_ref().unwrap();
        let frame: ID3D11Texture2D = res.cast()?;
        unsafe {
            self.ctx.CopySubresourceRegion(&tex.mip, 0, 0, 0, 0, &frame, 0, None);
        }
        Ok(())
    }

    fn submit_downscale(&mut self) {
        let tex = self.tex.as_ref().unwrap();
        let slot = self.next_slot;
        unsafe {
            self.ctx.GenerateMips(&tex.srv);
            self.ctx.CopySubresourceRegion(&tex.staging[slot], 0, 0, 0, 0, &tex.mip, self.level, None);
        }
        let value = self.fence.as_mut().map(|f| f.signal()).unwrap_or(0);
        unsafe {
            // Kick the GPU now so the work is done by the time we look.
            self.ctx.Flush();
        }
        self.pending.push_back((slot, Instant::now(), value));
        self.next_slot = (slot + 1) % RING;
    }

    /// Reads back every finished frame (the newest one wins). Waits at most `MAX_WAIT`
    /// for the oldest one if nothing has finished yet.
    fn poll_readback(&mut self) -> Result<bool> {
        let deadline = Instant::now() + MAX_WAIT;
        let mut got = false;
        while let Some(&(slot, submitted, value)) = self.pending.front() {
            if let Some(f) = self.fence.as_ref() {
                // Already have a fresh frame: take only what is finished, don't wait.
                let budget = if got { Duration::ZERO } else { deadline.saturating_duration_since(Instant::now()) };
                let ready = if budget.is_zero() { f.done(value) } else { f.wait(value, budget) };
                if !ready {
                    break;
                }
            }
            let staging = self.tex.as_ref().unwrap().staging[slot].clone();
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            let r = unsafe {
                self.ctx.Map(&staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut m))
            };
            match r {
                Ok(()) => {
                    let row = (self.small_w * 4) as usize;
                    unsafe {
                        let src = m.pData as *const u8;
                        for y in 0..self.small_h as usize {
                            let line = std::slice::from_raw_parts(src.add(y * m.RowPitch as usize), row);
                            self.buf[y * row..(y + 1) * row].copy_from_slice(line);
                        }
                        self.ctx.Unmap(&staging, 0);
                    }
                    self.pending.pop_front();
                    let lat = submitted.elapsed();
                    self.stats.frames += 1;
                    self.stats.latency += lat;
                    self.stats.latency_max = self.stats.latency_max.max(lat);
                    self.frame_ready = true;
                    got = true;
                }
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    // GPU executes in order: if the oldest isn't done, newer ones aren't either.
                    if got || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(POLL_STEP);
                }
                Err(e) => return Err(e).context("Map"),
            }
        }
        Ok(got)
    }

    /// Average RGB over a region of the last downscaled frame.
    pub fn rect_average(&self, r: &PixRect) -> [f32; 3] {
        let w = self.small_w as usize;
        let (mut red, mut g, mut b) = (0u32, 0u32, 0u32);
        for y in r.y0..r.y1 {
            let line = &self.buf[(y * w + r.x0) * 4..(y * w + r.x1) * 4];
            for px in line.chunks_exact(4) {
                b += px[0] as u32;
                g += px[1] as u32;
                red += px[2] as u32;
            }
        }
        let n = ((r.y1 - r.y0) * (r.x1 - r.x0)) as f32;
        [red as f32 / n, g as f32 / n, b as f32 / n]
    }
}
