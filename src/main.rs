//! Console tool: diagnostics and benchmarking for the sync engine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use synclight::capture::{self, Capturer};
use synclight::color;
use synclight::config::Settings;
use synclight::device::Device;
use synclight::engine::{self, Options, Shared, State};
use synclight::layout::{Edge, Layout};
use synclight::logger;

const HELP: &str = "\
synclight-rs — консольная утилита для лент Robobloq (диагностика)
Для повседневной работы используйте synclight-tray.exe (иконка в трее).

  synclight-rs [ОПЦИИ]

Режимы:
  --dry-run         захват и расчёт цветов без отправки на ленту
  --info            только прочитать информацию о ленте
  --test            тестовая раскраска сторон (проверка направления)
  --test-leds       каждый светодиод своим цветом (проверка покадровой адресации)
  --leds N          для --test-leds: только первые N светодиодов по отдельности

Настройки (по умолчанию — как у вашей ленты, файл настроек трея не читается):
  --fps N           частота обновления ленты (по умолчанию 60)
  --smooth MS       сглаживание, мс (по умолчанию 60; 0 = выкл)
  --display N       номер дисплея (по умолчанию 0)
  --edges 3|4       сколько сторон монитора занимает лента (по умолчанию 3)
  --forward         лента идёт по часовой стрелке от левого нижнего угла
  --horizontal N    светодиодов на верхней стороне
  --vertical N      светодиодов на каждой боковой стороне
  --wire RGB        порядок цветов в ленте
  --no-vivid        не усиливать почти чистые цвета
  --per-led         своя зона экрана у каждого светодиода (вместо сетки 16x9)
  --gain K          усиление яркости, например 1.3 (по умолчанию 1.0)
  --duration S      остановиться через S секунд
";

struct Args {
    dry_run: bool,
    info: bool,
    test: bool,
    test_leds: bool,
    leds: Option<usize>,
    duration: Option<f32>,
    settings: Settings,
}

fn value<T: std::str::FromStr>(it: &mut impl Iterator<Item = String>, name: &str) -> Result<T> {
    let v = it.next().ok_or_else(|| anyhow!("{name}: нужно значение"))?;
    v.parse().map_err(|_| anyhow!("{name}: неверное значение '{v}'"))
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        dry_run: false,
        info: false,
        test: false,
        test_leds: false,
        leds: None,
        duration: None,
        settings: Settings::default(),
    };
    let s = &mut a.settings;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => a.dry_run = true,
            "--info" => a.info = true,
            "--test" => a.test = true,
            "--test-leds" => a.test_leds = true,
            "--leds" => a.leds = Some(value(&mut it, "--leds")?),
            "--duration" => a.duration = Some(value(&mut it, "--duration")?),
            "--fps" => s.fps = value(&mut it, "--fps")?,
            "--smooth" => s.smooth_ms = value(&mut it, "--smooth")?,
            "--display" => s.display = value(&mut it, "--display")?,
            "--edges" => s.edges = value(&mut it, "--edges")?,
            "--forward" => s.reverse = false,
            "--horizontal" => s.horizontal = Some(value(&mut it, "--horizontal")?),
            "--vertical" => s.vertical = Some(value(&mut it, "--vertical")?),
            "--wire" => s.wire = value(&mut it, "--wire")?,
            "--no-vivid" => s.vivid = false,
            "--per-led" => s.per_led = true,
            "--gain" => s.gain = value(&mut it, "--gain")?,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("неизвестный аргумент: {other} (см. --help)"),
        }
    }
    if !(1..=240).contains(&s.fps) {
        bail!("--fps должен быть от 1 до 240");
    }
    if s.edges != 3 && s.edges != 4 {
        bail!("--edges может быть 3 или 4");
    }
    if !(0.1..=4.0).contains(&s.gain) {
        bail!("--gain должен быть от 0.1 до 4");
    }
    color::Pipeline::wire_map(&s.wire)?;
    Ok(a)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Ошибка: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    capture::init_dpi();
    logger::init(None, true);

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).context("Ctrl+C handler")?;
    }

    if args.info || args.test || args.test_leds {
        if engine::synclight_running() {
            bail!("запущен SyncLight — закройте его, чтобы не мешать ему управлять лентой");
        }
        let mut dev = Device::open()?;
        let i = dev.read_info()?;
        println!(
            "Лента: модель {} | светодиодов {} | монитор {}\" | прошивка {}.{}.{} | uuid {}",
            i.id, i.lamps, i.display_size, i.version[0], i.version[1], i.version[2], i.uuid
        );
        if args.info {
            return Ok(());
        }
        let hold = Duration::from_secs_f32(args.duration.unwrap_or(8.0));
        if args.test_leds {
            return test_leds(&mut dev, i.lamps as usize, args.leds, &running, hold);
        }
        let cap = Capturer::new(args.settings.display)?;
        let (lay, desc) = engine::build_layout(Some(&i), cap.src_w, cap.src_h, &args.settings)?;
        println!("Раскладка: {desc}");
        return test_pattern(&mut dev, &lay, &running, hold);
    }

    let shared = Shared::new(args.settings.clone());
    let worker = {
        let s = shared.clone();
        let opts = Options { dry_run: args.dry_run, yield_to_synclight: true, publish_status: false };
        std::thread::spawn(move || engine::run(s, opts))
    };
    println!(
        "Работаю. Ctrl+C — выход{}.",
        if args.dry_run { " (dry-run: на ленту ничего не отправляется)" } else { "" }
    );

    let deadline = args.duration.map(|s| Instant::now() + Duration::from_secs_f32(s));
    let mut next_print = Instant::now() + Duration::from_secs(2);
    while running.load(Ordering::SeqCst) && deadline.map_or(true, |d| Instant::now() < d) {
        std::thread::sleep(Duration::from_millis(100));
        if Instant::now() >= next_print {
            next_print += Duration::from_secs(2);
            let st = shared.status();
            if st.state == State::Running {
                print_status(&st, args.dry_run);
            }
        }
    }
    shared.request_stop();
    let _ = worker.join();
    Ok(())
}

fn print_status(st: &engine::Status, dry: bool) {
    let step = (st.preview.len() / 6).max(1);
    let preview: Vec<String> = st
        .preview
        .iter()
        .step_by(step)
        .map(|c| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]))
        .collect();
    println!(
        "кадров {:5.1}/с | захват держит поток {:.2} мс за шаг (макс {:.2}) | GPU отдаёт за {:.2} мс (макс {:.2}){} | расчёт {:.3} мс | {}{}",
        st.fps,
        st.grab_ms,
        st.grab_max_ms,
        st.latency_ms,
        st.latency_max_ms,
        if st.gpu_skipped > 0 { format!(", пропущено {}", st.gpu_skipped) } else { String::new() },
        st.proc_ms,
        if dry {
            String::new()
        } else {
            format!(
                "лента {:4.1}/с, USB {:.2} мс (макс {:.2}), заменено {} | ",
                st.sends, st.hid_ms, st.hid_max_ms, st.replaced
            )
        },
        preview.join(" ")
    );
}

fn test_pattern(d: &mut Device, lay: &Layout, running: &AtomicBool, hold: Duration) -> Result<()> {
    let color = |e: Edge| match e {
        Edge::Right => [255, 0, 0],
        Edge::Top => [0, 255, 0],
        Edge::Left => [0, 0, 255],
        Edge::Bottom => [85, 85, 85],
    };
    let segs: Vec<[u8; 5]> = lay
        .segments
        .iter()
        .map(|s| {
            let c = color::limit_power(color(s.edge));
            [s.start, c[0], c[1], c[2], s.end]
        })
        .collect();
    println!(
        "Тест: справа должен быть КРАСНЫЙ, сверху ЗЕЛЁНЫЙ, слева СИНИЙ (снизу серый). {:.0} с...",
        hold.as_secs_f32()
    );
    let end = Instant::now() + hold;
    while running.load(Ordering::SeqCst) && Instant::now() < end {
        d.send_sync(&segs)?;
        std::thread::sleep(Duration::from_millis(250));
    }
    for _ in 0..3 {
        let _ = d.turn_off();
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("Тест окончен, подсветка выключена.");
    Ok(())
}

/// Per-LED addressing test: a rainbow along the strip with every second LED off,
/// one segment per LED (the strip must accept a frame of `lamps` segments).
fn test_leds(d: &mut Device, lamps: usize, single: Option<usize>, running: &AtomicBool, hold: Duration) -> Result<()> {
    let single = single.unwrap_or(lamps).min(lamps);
    let hue = |t: f32| {
        let h = t * 6.0;
        let f = |k: f32| ((((k + h) % 6.0) - 3.0).abs() - 1.0).clamp(0.0, 1.0);
        [(f(0.0) * 255.0) as u8, (f(4.0) * 255.0) as u8, (f(2.0) * 255.0) as u8]
    };
    let mut segs: Vec<[u8; 5]> = Vec::with_capacity(single + 1);
    for led in 1..=single {
        let c = if led % 2 == 1 { color::limit_power(hue((led - 1) as f32 / lamps as f32)) } else { [0, 0, 0] };
        segs.push([led as u8, c[0], c[1], c[2], led as u8]);
    }
    if single < lamps {
        segs.push([(single + 1) as u8, 40, 40, 40, lamps as u8]);
    }
    let bytes = 7 + segs.len() * 5;
    println!(
        "Тест: {} сегментов, кадр {} байт ({} USB-пакетов). Ожидается радуга вдоль ленты, каждый второй светодиод выключен{}. {:.0} с...",
        segs.len(),
        bytes,
        bytes.div_ceil(64),
        if single < lamps { format!(", после {single}-го — ровный серый") } else { String::new() },
        hold.as_secs_f32()
    );
    let end = Instant::now() + hold;
    while running.load(Ordering::SeqCst) && Instant::now() < end {
        d.send_sync(&segs)?;
        std::thread::sleep(Duration::from_millis(250));
    }
    for _ in 0..3 {
        let _ = d.turn_off();
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("Тест окончен, подсветка выключена.");
    Ok(())
}
