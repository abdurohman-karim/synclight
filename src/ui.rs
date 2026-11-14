//! Settings window (egui). Runs as its own process (`synclight-tray.exe --ui`) and lives
//! only while the window is open. Live colours and stats come from the engine through
//! shared memory (`ipc`); settings are written to config.ini and the tray is told to
//! apply them right away, so every control takes effect immediately.

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, Ui,
};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::config::{self, Mode, RhythmSource, Settings};
use crate::ipc::{self, StatusBlock, StatusReader};
use crate::{autostart, icon};

pub const WINDOW_TITLE: &str = "SyncLight RS";

const BG: Color32 = Color32::from_rgb(0x0f, 0x10, 0x15);
const CARD: Color32 = Color32::from_rgb(0x17, 0x19, 0x21);
const CARD_HI: Color32 = Color32::from_rgb(0x23, 0x26, 0x31);
const BORDER: Color32 = Color32::from_rgb(0x25, 0x28, 0x34);
const TEXT: Color32 = Color32::from_rgb(0xe9, 0xeb, 0xf2);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x8f, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x5c, 0xff);
const ACCENT_SOFT: Color32 = Color32::from_rgb(0xb3, 0xa3, 0xff);
const GREEN: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
const AMBER: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
const RED: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);
const GREY: Color32 = Color32::from_rgb(0x6b, 0x70, 0x80);
const LED_OFF: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x38);

const FPS_CHOICES: [u32; 4] = [30, 60, 120, 144];
const SAVE_EVERY: Duration = Duration::from_millis(120);
const STALE_AFTER: Duration = Duration::from_secs(3);

pub fn run() -> Result<(), String> {
    let size = 64;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([880.0, 580.0])
            .with_icon(egui::IconData { rgba: icon::rgba(size, true), width: size, height: size }),
        ..Default::default()
    };
    eframe::run_native(WINDOW_TITLE, options, Box::new(|cc| Ok(Box::new(SettingsApp::new(cc)))))
        .map_err(|e| e.to_string())
}

struct SettingsApp {
    settings: Settings,
    cfg_mtime: Option<SystemTime>,
    next_cfg_check: Instant,
    dirty: bool,
    last_save: Instant,
    reader: Option<StatusReader>,
    next_reader_try: Instant,
    status: Option<Box<StatusBlock>>,
    heartbeat: u32,
    heartbeat_at: Option<Instant>,
    autostart: bool,
    notice: Option<String>,
    logo: egui::TextureHandle,
}

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(config::path()).and_then(|m| m.modified()).ok()
}

fn open_path(path: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Starts the background (tray) process.
fn start_tray() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

impl SettingsApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let logo_px = 96;
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([logo_px, logo_px], &icon::rgba(logo_px as u32, true)),
            egui::TextureOptions::LINEAR,
        );
        SettingsApp {
            settings: config::load(),
            cfg_mtime: config_mtime(),
            next_cfg_check: Instant::now(),
            dirty: false,
            last_save: Instant::now(),
            reader: None,
            next_reader_try: Instant::now(),
            status: None,
            heartbeat: 0,
            heartbeat_at: None,
            autostart: autostart::is_enabled(),
            notice: None,
            logo,
        }
    }

    fn connected(&self) -> bool {
        self.status.is_some() && self.heartbeat_at.is_some_and(|t| t.elapsed() < STALE_AFTER)
    }

    fn poll_status(&mut self) {
        if self.reader.is_none() && Instant::now() >= self.next_reader_try {
            self.reader = StatusReader::open();
            self.next_reader_try = Instant::now() + Duration::from_secs(1);
        }
        if let Some(r) = self.reader.as_ref() {
            if let Some(b) = r.read() {
                if b.heartbeat != self.heartbeat || self.heartbeat_at.is_none() {
                    self.heartbeat = b.heartbeat;
                    self.heartbeat_at = Some(Instant::now());
                }
                self.status = Some(b);
            }
        }
        // The tray exited while we kept the old mapping: drop it and look again later.
        if self.reader.is_some() && self.heartbeat_at.is_some_and(|t| t.elapsed() > STALE_AFTER * 2) {
            self.reader = None;
        }
    }

    /// Picks up changes made elsewhere (tray menu, manual edit of config.ini).
    fn poll_config(&mut self) {
        if Instant::now() < self.next_cfg_check {
            return;
        }
        self.next_cfg_check = Instant::now() + Duration::from_millis(700);
        let m = config_mtime();
        if !self.dirty && m.is_some() && m != self.cfg_mtime {
            self.cfg_mtime = m;
            self.settings = config::load();
        }
        self.autostart = autostart::is_enabled();
    }

    /// Saves at most every `SAVE_EVERY` (sliders fire on every frame while dragged).
    fn flush_save(&mut self) {
        if !self.dirty || self.last_save.elapsed() < SAVE_EVERY {
            return;
        }
        self.settings = self.settings.clone().sanitize();
        match config::save(&self.settings) {
            Ok(()) => {
                self.cfg_mtime = config_mtime();
                ipc::notify_tray_reload();
            }
            Err(e) => self.notice = Some(format!("Не удалось сохранить настройки: {e}")),
        }
        self.dirty = false;
        self.last_save = Instant::now();
    }

    fn device_line(&self) -> String {
        match self.status.as_ref() {
            Some(b) if self.connected() && b.has_device == 1 => {
                let mut s = format!(
                    "Лента {} · {} светодиодов · прошивка {}.{}.{}",
                    b.model(),
                    b.lamps,
                    b.firmware[0],
                    b.firmware[1],
                    b.firmware[2]
                );
                let screen = b.screen();
                if !screen.is_empty() {
                    s += &format!(" · {}", screen.trim_start_matches("\\\\.\\"));
                }
                s
            }
            Some(b) if self.connected() && b.state == ipc::state::PAUSED => {
                "Лентой управляет оригинальный SyncLight — закройте его, и подсветка продолжит работу".into()
            }
            Some(_) if self.connected() => "Лента не найдена — проверьте USB-кабель".into(),
            _ => "Фоновая служба не запущена".into(),
        }
    }

    fn status_badge(&self) -> (String, Color32) {
        if !self.connected() {
            return ("Служба остановлена".into(), RED);
        }
        let b = self.status.as_ref().unwrap();
        match b.state {
            ipc::state::RUNNING => ("Работает".into(), GREEN),
            ipc::state::OFF => ("Выключено".into(), GREY),
            ipc::state::NO_DEVICE => ("Лента не подключена".into(), AMBER),
            ipc::state::PAUSED => ("Пауза: запущен SyncLight".into(), AMBER),
            ipc::state::NO_CAPTURE => ("Нет захвата экрана".into(), AMBER),
            ipc::state::ERROR => ("Ошибка".into(), RED),
            _ => ("Запуск…".into(), GREY),
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), vec2(46.0, 46.0))));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("SyncLight RS").family(semibold()).size(24.0).color(TEXT));
                ui.label(RichText::new(self.device_line()).size(13.0).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = self.settings.enabled;
                if toggle(ui, &mut on, 1.3).changed() {
                    self.settings.enabled = on;
                    self.dirty = true;
                }
                ui.label(RichText::new(if on { "Подсветка вкл." } else { "Подсветка выкл." }).size(14.0).color(MUTED));
                ui.add_space(16.0);
                let (text, color) = self.status_badge();
                pill(ui, &text, color);
            });
        });
    }

    /// Mode the engine is actually running, or the selected one while offline.
    fn active_mode(&self) -> Mode {
        match self.status.as_ref().filter(|_| self.connected()).map(|b| b.mode) {
            Some(1) => Mode::Static,
            Some(2) => Mode::Effect,
            Some(3) => Mode::Rhythm,
            Some(_) => Mode::Sync,
            None => self.settings.mode,
        }
    }

    fn audio_level(&self) -> u8 {
        self.status.as_ref().filter(|_| self.connected()).map(|b| b.audio_level).unwrap_or(0)
    }

    fn preview_card(&mut self, ui: &mut Ui) {
        let connected = self.connected();
        let mode = self.active_mode();
        let computer = self.settings.rhythm_source == RhythmSource::Computer;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_title(ui, "Лента сейчас");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let info = match self.status.as_ref() {
                        Some(b) if connected => match mode {
                            Mode::Sync if b.zones > 0 => format!("{} зон · {} светодиодов", b.zones, b.led_count),
                            Mode::Static => format!("цвет {}", config::hex(self.settings.color)),
                            Mode::Effect => format!("эффект №{}", self.settings.effect + 1),
                            Mode::Rhythm => format!("музыка №{}", self.settings.rhythm + 1),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    };
                    ui.label(RichText::new(info).size(12.5).color(MUTED));
                });
            });
            let h = (ui.available_width() * 0.62).clamp(250.0, 400.0);
            let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), h), Sense::hover());
            self.paint_preview(&painter, resp.rect, resp.hover_pos(), mode);
            if !connected {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Фоновая служба не запущена — подсветка сейчас не работает.").color(AMBER));
                    if ui.add(button("Запустить")).clicked() {
                        start_tray();
                    }
                });
            } else {
                hint(
                    ui,
                    match mode {
                        Mode::Sync | Mode::Static => "Наведите на светодиод, чтобы увидеть его номер и цвет.",
                        Mode::Effect => "Эффект анимирует сама лента, поэтому здесь он не показывается.",
                        Mode::Rhythm if computer => "Лента реагирует на звук компьютера; точки показывают громкость.",
                        Mode::Rhythm => "Лента слушает свой микрофон, поэтому здесь эффект не показывается.",
                    },
                );
            }
        });
    }

    fn paint_preview(&self, p: &egui::Painter, area: Rect, hover: Option<Pos2>, mode: Mode) {
        let live = self.status.as_ref().filter(|_| self.connected());
        let (h, v, edges, reverse) = match live {
            Some(b) if b.horizontal > 0 && b.vertical > 0 => (b.horizontal as usize, b.vertical as usize, b.edges, b.reverse == 1),
            _ => (31, 17, self.settings.edges, self.settings.reverse),
        };

        // Monitor geometry (16:9), with room for LEDs and the stand.
        let margin = 30.0;
        let stand = 44.0;
        let mw = (area.width() - margin * 2.0).min((area.height() - margin - stand) * 16.0 / 9.0);
        let mh = mw * 9.0 / 16.0;
        let mon = Rect::from_min_size(pos2(area.center().x - mw / 2.0, area.top() + margin), vec2(mw, mh));
        let pos = led_positions(mon, h, v, edges, reverse, 13.0);
        let computer = self.settings.rhythm_source == RhythmSource::Computer;

        // Colour of every LED: what the strip shows (sync, static), or the sound level (computer rhythm).
        let raw: Vec<[u8; 3]> = (0..pos.len())
            .map(|i| match mode {
                Mode::Rhythm if computer => {
                    let lit = (self.audio_level() as f32 / 100.0 * pos.len() as f32).round() as usize;
                    if i < lit {
                        let c = lerp_color(ACCENT, Color32::from_rgb(0xff, 0x4d, 0xb8), i as f32 / pos.len() as f32);
                        [c.r(), c.g(), c.b()]
                    } else {
                        [0, 0, 0]
                    }
                }
                Mode::Sync | Mode::Static => live.and_then(|b| b.leds.get(i).copied()).unwrap_or([0, 0, 0]),
                _ => [0, 0, 0],
            })
            .collect();
        // A lit LED is bright even at a low value (40/255 visibly glows on the wall),
        // while the same value on a monitor looks black: lift dark values for display.
        let show = |x: u8| ((x as f32 / 255.0).powf(0.55) * 255.0).round() as u8;
        let colors: Vec<Option<Color32>> = raw
            .iter()
            .map(|c| (c.iter().any(|&x| x > 0)).then(|| Color32::from_rgb(show(c[0]), show(c[1]), show(c[2]))))
            .collect();

        // Average colour: soft light on the "wall" and a hint of it on the screen.
        let (mut sum, mut lit) = ([0u32; 3], 0u32);
        for c in colors.iter().flatten() {
            sum[0] += c.r() as u32;
            sum[1] += c.g() as u32;
            sum[2] += c.b() as u32;
            lit += 1;
        }
        let avg = if lit > 0 {
            Color32::from_rgb((sum[0] / lit) as u8, (sum[1] / lit) as u8, (sum[2] / lit) as u8)
        } else {
            Color32::BLACK
        };
        if lit > 0 {
            // Many faint, growing layers: reads as a soft halo rather than a box.
            for k in 0..14 {
                let e = 6.0 + k as f32 * 4.0;
                p.rect_filled(mon.expand(e), CornerRadius::same((12.0 + e) as u8), avg.gamma_multiply(0.018));
            }
        }

        // Stand.
        let stand_col = Color32::from_rgb(0x2a, 0x2d, 0x38);
        p.rect_filled(
            Rect::from_center_size(pos2(mon.center().x, mon.bottom() + 15.0), vec2(mw * 0.07, 26.0)),
            CornerRadius::same(2),
            stand_col,
        );
        p.rect_filled(
            Rect::from_center_size(pos2(mon.center().x, mon.bottom() + 30.0), vec2(mw * 0.3, 7.0)),
            CornerRadius::same(4),
            stand_col,
        );

        // Monitor.
        p.rect(mon, CornerRadius::same(10), Color32::from_rgb(0x0a, 0x0b, 0x0f), Stroke::new(2.0_f32, Color32::from_rgb(0x34, 0x38, 0x46)), StrokeKind::Inside);
        let screen = mon.shrink(8.0);
        p.rect_filled(screen, CornerRadius::same(4), Color32::from_rgb(0x12, 0x14, 0x1b));
        if lit > 0 {
            p.rect_filled(screen, CornerRadius::same(4), avg.gamma_multiply(0.12));
        }

        // What runs on the strip, for modes the preview can't draw.
        let caption = match mode {
            Mode::Effect => Some((format!("Эффект №{}", self.settings.effect + 1), "анимацию рисует сама лента".to_string())),
            Mode::Rhythm if computer => Some((format!("{}%", self.audio_level()), format!("громкость · музыка №{}", self.settings.rhythm + 1))),
            Mode::Rhythm => Some((format!("Музыка №{}", self.settings.rhythm + 1), "слушает микрофон ленты".to_string())),
            _ => None,
        };
        if let Some((big, small)) = caption.filter(|_| live.is_some()) {
            p.text(screen.center() - vec2(0.0, 10.0), Align2::CENTER_CENTER, big, FontId::new(30.0, semibold()), TEXT);
            p.text(screen.center() + vec2(0.0, 20.0), Align2::CENTER_CENTER, small, FontId::proportional(14.0), MUTED);
        }

        // LEDs: glow first, then the dots on top.
        let spacing = (mw / h.max(1) as f32).min(mh / v.max(1) as f32);
        let r = (spacing * 0.30).clamp(2.0, 6.0);
        for (at, c) in pos.iter().zip(&colors) {
            if let Some(c) = c {
                p.circle_filled(*at, r * 3.6, c.gamma_multiply(0.06));
                p.circle_filled(*at, r * 2.2, c.gamma_multiply(0.16));
            }
        }
        for (at, c) in pos.iter().zip(&colors) {
            match c {
                Some(c) => {
                    p.circle_filled(*at, r, *c);
                }
                None => {
                    p.circle_filled(*at, r * 0.8, LED_OFF);
                }
            }
        }

        // Where the strip starts.
        if let Some(first) = pos.first() {
            p.text(*first + vec2(0.0, 12.0), Align2::CENTER_TOP, "начало", FontId::proportional(11.5), MUTED);
        }

        // Hovered LED: ring + tooltip.
        if let Some(hp) = hover {
            let near = pos
                .iter()
                .enumerate()
                .map(|(i, at)| (i, *at, at.distance(hp)))
                .filter(|(_, _, d)| *d < 12.0)
                .min_by(|a, b| a.2.total_cmp(&b.2));
            if let Some((i, at, _)) = near {
                p.circle_stroke(at, r + 3.0, Stroke::new(1.5_f32, Color32::WHITE));
                let c = raw.get(i).copied().unwrap_or([0, 0, 0]);
                let text = if c == [0, 0, 0] {
                    format!("Светодиод №{} · выключен", i + 1)
                } else {
                    format!("Светодиод №{} · #{:02X}{:02X}{:02X}", i + 1, c[0], c[1], c[2])
                };
                let galley = p.layout_no_wrap(text, FontId::proportional(13.0), TEXT);
                let size = galley.size() + vec2(34.0, 14.0);
                let mut tip = Rect::from_min_size(hp + vec2(14.0, -size.y - 8.0), size);
                if tip.right() > area.right() {
                    tip = tip.translate(vec2(area.right() - tip.right(), 0.0));
                }
                if tip.top() < area.top() {
                    tip = tip.translate(vec2(0.0, area.top() - tip.top()));
                }
                p.rect(tip, CornerRadius::same(8), CARD_HI, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
                p.circle_filled(pos2(tip.left() + 13.0, tip.center().y), 5.0, Color32::from_rgb(c[0], c[1], c[2]));
                p.circle_stroke(pos2(tip.left() + 13.0, tip.center().y), 5.0, Stroke::new(1.0_f32, BORDER));
                p.galley(pos2(tip.left() + 25.0, tip.center().y - galley.size().y / 2.0), galley, TEXT);
            }
        }
    }

    fn stats_row(&self, ui: &mut Ui) {
        let mode = self.active_mode();
        let live = self.status.as_ref().filter(|b| self.connected() && b.state == ipc::state::RUNNING);
        let val = |f: &dyn Fn(&StatusBlock) -> String| live.map(|b| f(b)).unwrap_or_else(|| "—".into());
        let s = &self.settings;
        let tiles: [(&str, String); 4] = match mode {
            Mode::Sync => [
                ("Экран", val(&|b| format!("{:.0} к/с", b.fps))),
                ("Лента", val(&|b| format!("{:.0} к/с", b.sends))),
                ("USB", val(&|b| format!("{:.1} мс", b.hid_ms))),
                ("Видеокарта", val(&|b| format!("{:.1} мс", b.latency_ms))),
            ],
            Mode::Static => [
                ("Режим", "Цвет".into()),
                ("Цвет", config::hex(s.color)),
                ("Яркость ленты", format!("{}%", s.strip_brightness)),
                ("Захват экрана", "выкл.".into()),
            ],
            Mode::Effect => [
                ("Режим", "Эффект".into()),
                ("Эффект", format!("№{}", s.effect + 1)),
                ("Скорость", format!("{}%", s.effect_speed)),
                ("Яркость ленты", format!("{}%", s.strip_brightness)),
            ],
            Mode::Rhythm => [
                ("Режим", "Музыка".into()),
                ("Эффект", format!("№{}", s.rhythm + 1)),
                ("Источник", if s.rhythm_source == RhythmSource::Computer { "ПК".into() } else { "микрофон".into() }),
                (
                    "Громкость",
                    if s.rhythm_source == RhythmSource::Computer { val(&|b| format!("{}%", b.audio_level)) } else { "—".into() },
                ),
            ],
        };
        ui.columns(4, |cols| {
            for (col, (label, value)) in cols.iter_mut().zip(tiles) {
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(col, |ui| {
                        ui.set_width(ui.available_width());
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.add(egui::Label::new(RichText::new(label).size(12.0).color(MUTED)).truncate());
                            ui.add(egui::Label::new(RichText::new(value).family(semibold()).size(20.0).color(TEXT)).truncate());
                        });
                    });
            }
        });
    }

    fn settings_panel(&mut self, ui: &mut Ui) {
        let connected = self.connected();
        let level = self.audio_level();
        let s = &mut self.settings;
        let mut changed = false;

        if mode_tiles(ui, &mut s.mode, s.color) {
            changed = true;
        }
        ui.add_space(12.0);

        match s.mode {
            Mode::Sync => {
                card(ui, |ui| {
                    section_title(ui, "Зоны экрана");
                    let mut sel = s.per_led as usize;
                    if segmented(ui, "zones", &mut sel, &["Сетка 16×9", "Каждый светодиод"]) {
                        s.per_led = sel == 1;
                        changed = true;
                    }
                    hint(
                        ui,
                        if s.per_led {
                            "Своя зона у каждого светодиода: самая подробная картинка, вдвое больше данных по USB."
                        } else {
                            "Как в оригинальном SyncLight: 34 зоны по 1–2 светодиода."
                        },
                    );
                });
                ui.add_space(12.0);

                card(ui, |ui| {
                    section_title(ui, "Изображение");
                    ui.label(RichText::new("Режим синхронизации").size(15.0).color(TEXT));
                    let mut preset = if s.smooth_ms <= 40.0 {
                        0
                    } else if s.smooth_ms >= 100.0 {
                        1
                    } else {
                        usize::MAX
                    };
                    if segmented(ui, "preset", &mut preset, &["Игры", "Фильмы"]) {
                        s.smooth_ms = if preset == 0 { 20.0 } else { 150.0 };
                        changed = true;
                    }
                    hint(ui, "Игры — мгновенная реакция, Фильмы — мягкие переходы (как Game/Movie mode в SyncLight).");
                    ui.add_space(4.0);
                    let mut pct = s.gain * 100.0;
                    if slider_row(ui, "Яркость", format!("{pct:.0}%"), &mut pct, 50.0..=200.0, ("тусклее", "ярче")) {
                        s.gain = (pct / 5.0).round() * 5.0 / 100.0;
                        changed = true;
                    }
                    ui.add_space(4.0);
                    let mut sm = s.smooth_ms;
                    if slider_row(ui, "Плавность", format!("{sm:.0} мс"), &mut sm, 0.0..=300.0, ("мгновенно", "плавно")) {
                        s.smooth_ms = (sm / 5.0).round() * 5.0;
                        changed = true;
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Частота обновления, кадров/с").size(15.0).color(TEXT));
                    let mut idx = FPS_CHOICES.iter().position(|f| *f == s.fps).unwrap_or(usize::MAX);
                    if segmented(ui, "fps", &mut idx, &["30", "60", "120", "144"]) {
                        s.fps = FPS_CHOICES[idx];
                        changed = true;
                    }
                    ui.add_space(6.0);
                    if switch_row(ui, "Насыщенные цвета", "Почти чистые цвета становятся полностью чистыми, как в SyncLight.", &mut s.vivid) {
                        changed = true;
                    }
                });
                ui.add_space(12.0);

                card(ui, |ui| {
                    section_title(ui, "Установка ленты");
                    ui.label(RichText::new("Где начинается лента").size(15.0).color(TEXT));
                    let mut dir = s.reverse as usize;
                    if segmented(ui, "dir", &mut dir, &["Снизу слева", "Снизу справа"]) {
                        s.reverse = dir == 1;
                        changed = true;
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Сколько сторон занимает").size(15.0).color(TEXT));
                    let mut e = (s.edges == 4) as usize;
                    if segmented(ui, "edges", &mut e, &["3 стороны", "4 стороны"]) {
                        s.edges = if e == 1 { 4 } else { 3 };
                        changed = true;
                    }
                    hint(ui, "Проверка: цвета в предпросмотре слева должны совпадать с краями экрана.");
                });
            }
            Mode::Static => {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        section_title(ui, "Цвет");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(config::hex(s.color)).family(semibold()).size(14.0).color(ACCENT_SOFT));
                            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 10.0, Color32::from_rgb(s.color[0], s.color[1], s.color[2]));
                            ui.painter().circle_stroke(r.center(), 10.0, Stroke::new(1.0_f32, BORDER));
                        });
                    });
                    ui.add_space(4.0);
                    let mut hue = hue_of(s.color);
                    if hue_bar(ui, &mut hue) {
                        s.color = hsv(hue, 1.0, 1.0);
                        changed = true;
                    }
                    ui.add_space(8.0);
                    if let Some(c) = swatches(ui, s.color) {
                        s.color = c;
                        changed = true;
                    }
                    hint(ui, "Сумма каналов ограничивается, как в SyncLight: лента питается от USB, поэтому белый светит мягче чистых цветов.");
                });
            }
            Mode::Effect => {
                card(ui, |ui| {
                    section_title(ui, "Эффект ленты");
                    hint(ui, "Встроенные анимации прошивки — те же 7, что в разделе Dynamic у SyncLight.");
                    ui.add_space(4.0);
                    if effect_grid(ui, "effects", &mut s.effect) {
                        changed = true;
                    }
                    ui.add_space(8.0);
                    let mut sp = s.effect_speed as f32;
                    if slider_row(ui, "Скорость", format!("{sp:.0}%"), &mut sp, 0.0..=100.0, ("медленно", "быстро")) {
                        s.effect_speed = sp.round() as u8;
                        changed = true;
                    }
                });
            }
            Mode::Rhythm => {
                card(ui, |ui| {
                    section_title(ui, "Под музыку");
                    ui.label(RichText::new("Источник звука").size(15.0).color(TEXT));
                    let mut src = (s.rhythm_source == RhythmSource::Device) as usize;
                    if segmented(ui, "src", &mut src, &["Звук компьютера", "Микрофон ленты"]) {
                        s.rhythm_source = if src == 1 { RhythmSource::Device } else { RhythmSource::Computer };
                        changed = true;
                    }
                    hint(
                        ui,
                        if s.rhythm_source == RhythmSource::Computer {
                            "Громкость берётся из индикатора Windows и отправляется на ленту 25 раз в секунду."
                        } else {
                            "Лента сама слушает музыку своим микрофоном."
                        },
                    );
                    ui.add_space(6.0);
                    if effect_grid(ui, "rhythms", &mut s.rhythm) {
                        changed = true;
                    }
                    ui.add_space(8.0);
                    let mut sens = s.rhythm_sensitivity as f32;
                    if slider_row(ui, "Чувствительность", format!("{sens:.0}%"), &mut sens, 0.0..=100.0, ("тише", "громче")) {
                        s.rhythm_sensitivity = sens.round() as u8;
                        changed = true;
                    }
                    if s.rhythm_source == RhythmSource::Computer {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Уровень звука").size(15.0).color(TEXT));
                        level_meter(ui, if connected { level } else { 0 });
                    }
                });
            }
        }
        ui.add_space(12.0);

        card(ui, |ui| {
            section_title(ui, "Лента");
            let mut b = s.strip_brightness as f32;
            if slider_row(ui, "Яркость ленты", format!("{b:.0}%"), &mut b, 2.0..=100.0, ("тусклее", "ярче")) {
                s.strip_brightness = b.round() as u8;
                changed = true;
            }
            hint(ui, "Яркость самой ленты (второй ползунок в SyncLight).");
        });
        ui.add_space(12.0);

        card(ui, |ui| {
            section_title(ui, "Система");
            let mut auto = self.autostart;
            if switch_row(ui, "Запускать вместе с Windows", "Фоновая служба стартует при входе в систему.", &mut auto) {
                match autostart::set(auto) {
                    Ok(()) => self.autostart = auto,
                    Err(e) => self.notice = Some(format!("Не удалось изменить автозапуск: {e}")),
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(button("Журнал")).clicked() {
                    open_path(&config::log_path());
                }
                if ui.add(button("Файл настроек")).clicked() {
                    open_path(&config::path());
                }
                if !connected && ui.add(button("Запустить службу")).clicked() {
                    start_tray();
                }
            });
            if let Some(n) = self.notice.as_ref() {
                ui.label(RichText::new(n).size(12.5).color(RED));
            }
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("v{} · изменения применяются сразу", env!("CARGO_PKG_VERSION")))
                .size(12.0)
                .color(MUTED),
        );
        if changed {
            self.dirty = true;
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_status();
        self.poll_config();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(22)))
            .show(ctx, |ui| {
                self.header(ui);
                ui.add_space(16.0);
                let avail = ui.available_size();
                let left_w = (avail.x * 0.56).floor();
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(vec2(left_w, avail.y), Layout::top_down(Align::Min), |ui| {
                        self.preview_card(ui);
                        ui.add_space(12.0);
                        self.stats_row(ui);
                    });
                    ui.add_space(8.0);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), avail.y), Layout::top_down(Align::Min), |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            self.settings_panel(ui);
                        });
                    });
                });
            });
        self.flush_save();
        // Live preview: ~30 fps while the window is open.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

// ---------- layout of the LED dots around the monitor ----------

fn led_positions(m: Rect, h: usize, v: usize, edges: u8, reverse: bool, gap: f32) -> Vec<Pos2> {
    let along = |i: usize, n: usize| (i as f32 + 0.5) / n as f32;
    let left = |i: usize| pos2(m.left() - gap, m.top() + along(i, v) * m.height());
    let right = |i: usize| pos2(m.right() + gap, m.top() + along(i, v) * m.height());
    let top = |i: usize| pos2(m.left() + along(i, h) * m.width(), m.top() - gap);
    let bottom = |i: usize| pos2(m.left() + along(i, h) * m.width(), m.bottom() + gap);
    let mut out = Vec::with_capacity(2 * (h + v));
    if !reverse {
        out.extend((0..v).rev().map(left));
        out.extend((0..h).map(top));
        out.extend((0..v).map(right));
        if edges == 4 {
            out.extend((0..h).rev().map(bottom));
        }
    } else {
        out.extend((0..v).rev().map(right));
        out.extend((0..h).rev().map(top));
        out.extend((0..v).map(left));
        if edges == 4 {
            out.extend((0..h).map(bottom));
        }
    }
    out
}

// ---------- widgets ----------

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section_title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).family(semibold()).size(16.0).color(TEXT));
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(MUTED)).wrap());
}

fn button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(14.0).color(TEXT))
        .fill(CARD_HI)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(8))
        .min_size(vec2(0.0, 34.0))
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// iOS-style switch with animation.
fn toggle(ui: &mut Ui, on: &mut bool, scale: f32) -> egui::Response {
    let size = vec2(46.0, 26.0) * scale;
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let r = rect.height() / 2.0;
    p.rect_filled(rect, CornerRadius::same(r as u8), lerp_color(Color32::from_rgb(0x2c, 0x2f, 0x3b), ACCENT, t));
    let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
    p.circle_filled(pos2(x, rect.center().y), r - 3.5, Color32::WHITE);
    resp
}

/// Row of mutually exclusive options; returns true when the selection changed.
fn segmented(ui: &mut Ui, id: &str, selected: &mut usize, options: &[&str]) -> bool {
    let font = FontId::proportional(14.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|o| ui.painter().layout_no_wrap((*o).to_owned(), font.clone(), TEXT).size().x + 30.0)
        .collect();
    let (rect, _) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>() + 6.0, 36.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect_filled(rect, CornerRadius::same(10), BG);
    let mut changed = false;
    let mut x = rect.left() + 3.0;
    for (i, (text, w)) in options.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(*w, rect.height() - 6.0));
        let resp = ui.interact(r, ui.id().with(id).with(i), Sense::click());
        if resp.clicked() && *selected != i {
            *selected = i;
            changed = true;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let sel = *selected == i;
        let fill = if sel {
            ACCENT
        } else if resp.hovered() {
            CARD_HI
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(r, CornerRadius::same(8), fill);
        p.text(r.center(), Align2::CENTER_CENTER, *text, font.clone(), if sel { Color32::WHITE } else { MUTED });
        x += w;
    }
    changed
}

/// Title + value on one line, a full-width slider, and the two end labels below.
fn slider_row(
    ui: &mut Ui,
    title: &str,
    value_text: String,
    value: &mut f32,
    range: RangeInclusive<f32>,
    ends: (&str, &str),
) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(15.0).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value_text).family(semibold()).size(15.0).color(ACCENT_SOFT));
        });
    });
    ui.spacing_mut().slider_width = ui.available_width();
    let changed = ui.add(egui::Slider::new(value, range).show_value(false).trailing_fill(true)).changed();
    // End labels drawn right under the rail (a layout row would add a tall gap).
    ui.add_space(-6.0);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let font = FontId::proportional(11.5);
    ui.painter().text(row.left_center(), Align2::LEFT_CENTER, ends.0, font.clone(), MUTED);
    ui.painter().text(row.right_center(), Align2::RIGHT_CENTER, ends.1, font, MUTED);
    changed
}

/// Title and description on the left, switch on the right.
fn switch_row(ui: &mut Ui, title: &str, description: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 72.0).max(100.0);
        ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_max_width(w);
            ui.label(RichText::new(title).size(15.0).color(TEXT));
            hint(ui, description);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = toggle(ui, on, 1.0).changed();
        });
    });
    changed
}

/// Rounded status badge with a coloured dot.
fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(13.5), TEXT);
    let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 36.0, 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(15), color.gamma_multiply(0.14));
    p.rect_stroke(rect, CornerRadius::same(15), Stroke::new(1.0_f32, color.gamma_multiply(0.5)), StrokeKind::Inside);
    p.circle_filled(pos2(rect.left() + 15.0, rect.center().y), 4.0, color);
    p.galley(pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
}

// ---------- look ----------

fn add_font(fonts: &mut egui::FontDefinitions, name: &str, file: &str) -> bool {
    match std::fs::read(Path::new(r"C:\Windows\Fonts").join(file)) {
        Ok(bytes) => {
            fonts.font_data.insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            true
        }
        Err(_) => false,
    }
}

fn setup_style(ctx: &egui::Context) {
    // Segoe UI from Windows itself (not bundled); egui's built-in font is the fallback.
    let mut fonts = egui::FontDefinitions::default();
    if add_font(&mut fonts, "ui", "segoeui.ttf") {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".to_owned());
    }
    let mut semi = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if add_font(&mut fonts, "ui-semibold", "seguisb.ttf") {
        semi.insert(0, "ui-semibold".to_owned());
    }
    fonts.families.insert(semibold(), semi);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = BG;
    v.faint_bg_color = CARD;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(8);
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x2c, 0x2f, 0x3b);
    v.widgets.inactive.weak_bg_fill = CARD_HI;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.bg_fill = ACCENT_SOFT;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2c, 0x30, 0x3d);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    v.widgets.active.bg_fill = Color32::WHITE;
    v.widgets.active.weak_bg_fill = ACCENT;
    v.widgets.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);

    ctx.style_mut(|style| {
        style.visuals = v;
        style.spacing.item_spacing = vec2(10.0, 8.0);
        style.spacing.button_padding = vec2(14.0, 7.0);
        style.spacing.interact_size.y = 26.0;
        style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(15.0));
        style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
    });
}

// ---------- mode widgets ----------

/// Colour from hue (0..1), saturation and value.
fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let f = |n: f32| {
        let k = (n + h.rem_euclid(1.0) * 6.0) % 6.0;
        v - v * s * (k.min(4.0 - k).clamp(0.0, 1.0))
    };
    [(f(5.0) * 255.0).round() as u8, (f(3.0) * 255.0).round() as u8, (f(1.0) * 255.0).round() as u8]
}

fn hsv32(h: f32, s: f32, v: f32) -> Color32 {
    let c = hsv(h, s, v);
    Color32::from_rgb(c[0], c[1], c[2])
}

fn hue_of(c: [u8; 3]) -> f32 {
    let [r, g, b] = c.map(|x| x as f32 / 255.0);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    if d <= f32::EPSILON {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h / 6.0
}

/// Four big tiles: screen / colour / effects / music.
fn mode_tiles(ui: &mut Ui, mode: &mut Mode, color: [u8; 3]) -> bool {
    let labels = ["Экран", "Цвет", "Эффекты", "Музыка"];
    let gap = 8.0;
    let w = (ui.available_width() - gap * 3.0) / 4.0;
    let h = 76.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let p = ui.painter().clone();
    let mut changed = false;
    for (i, m) in Mode::ALL.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + i as f32 * (w + gap), rect.top()), vec2(w, h));
        let resp = ui.interact(r, ui.id().with("mode").with(i), Sense::click());
        if resp.clicked() && *mode != *m {
            *mode = *m;
            changed = true;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let sel = *mode == *m;
        let fill = if sel {
            ACCENT.gamma_multiply(0.25)
        } else if resp.hovered() {
            CARD_HI
        } else {
            CARD
        };
        let stroke = if sel { Stroke::new(1.5_f32, ACCENT) } else { Stroke::new(1.0_f32, BORDER) };
        p.rect(r, CornerRadius::same(14), fill, stroke, StrokeKind::Inside);
        let col = if sel { Color32::WHITE } else { MUTED };
        mode_icon(&p, *m, pos2(r.center().x, r.top() + 28.0), col, color);
        p.text(pos2(r.center().x, r.bottom() - 16.0), Align2::CENTER_CENTER, labels[i], FontId::proportional(14.0), if sel { TEXT } else { MUTED });
    }
    changed
}

fn mode_icon(p: &egui::Painter, m: Mode, c: Pos2, col: Color32, color: [u8; 3]) {
    let stroke = Stroke::new(2.0_f32, col);
    match m {
        Mode::Sync => {
            p.rect_stroke(Rect::from_center_size(c - vec2(0.0, 2.0), vec2(28.0, 18.0)), CornerRadius::same(3), stroke, StrokeKind::Inside);
            p.line_segment([pos2(c.x - 6.0, c.y + 11.0), pos2(c.x + 6.0, c.y + 11.0)], stroke);
            for (k, h) in [0.0f32, 0.3, 0.6].iter().enumerate() {
                p.circle_filled(pos2(c.x - 8.0 + k as f32 * 8.0, c.y - 15.0), 2.2, hsv32(*h, 0.7, 1.0));
            }
        }
        Mode::Static => {
            p.circle_filled(c, 11.0, Color32::from_rgb(color[0], color[1], color[2]));
            p.circle_stroke(c, 11.0, Stroke::new(1.5_f32, col));
        }
        Mode::Effect => {
            for k in 0..6 {
                let a = k as f32 / 6.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                p.circle_filled(c + vec2(a.cos(), a.sin()) * 9.0, 3.2, hsv32(k as f32 / 6.0, 0.75, 1.0));
            }
        }
        Mode::Rhythm => {
            for (k, hgt) in [8.0f32, 18.0, 12.0, 22.0, 10.0].iter().enumerate() {
                let x = c.x - 12.0 + k as f32 * 6.0;
                p.line_segment([pos2(x, c.y + 11.0), pos2(x, c.y + 11.0 - hgt)], Stroke::new(3.0_f32, col));
            }
        }
    }
}

/// Seven round buttons for the firmware effects.
fn effect_grid(ui: &mut Ui, id: &str, selected: &mut u8) -> bool {
    let n = config::EFFECTS as usize;
    let d = 44.0;
    let gap = 8.0;
    let per_row = (((ui.available_width() + gap) / (d + gap)).floor() as usize).clamp(1, n);
    let rows = n.div_ceil(per_row);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), rows as f32 * (d + gap) - gap), Sense::hover());
    let p = ui.painter().clone();
    let mut changed = false;
    for i in 0..n {
        let (row, col) = (i / per_row, i % per_row);
        let c = pos2(rect.left() + col as f32 * (d + gap) + d / 2.0, rect.top() + row as f32 * (d + gap) + d / 2.0);
        let r = Rect::from_center_size(c, vec2(d, d));
        let resp = ui.interact(r, ui.id().with(id).with(i), Sense::click());
        if resp.clicked() && *selected as usize != i {
            *selected = i as u8;
            changed = true;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let sel = *selected as usize == i;
        let tint = hsv32(i as f32 / n as f32, 0.6, 1.0);
        let fill = if sel {
            ACCENT
        } else if resp.hovered() {
            CARD_HI
        } else {
            BG
        };
        p.circle_filled(c, d / 2.0, fill);
        p.circle_stroke(c, d / 2.0 - 1.5, Stroke::new(2.0_f32, if sel { Color32::WHITE } else { tint.gamma_multiply(0.75) }));
        p.text(c, Align2::CENTER_CENTER, format!("{}", i + 1), FontId::new(19.0, semibold()), if sel { Color32::WHITE } else { TEXT });
    }
    changed
}

/// Rainbow bar; click or drag to pick a hue (0..1).
fn hue_bar(ui: &mut Ui, hue: &mut f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click_and_drag());
    let bar = rect.shrink2(vec2(12.0, 7.0));
    let p = ui.painter();
    let mut mesh = egui::Mesh::default();
    let steps = 48u32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = bar.left() + t * bar.width();
        let c = hsv32(t, 1.0, 1.0);
        mesh.colored_vertex(pos2(x, bar.top()), c);
        mesh.colored_vertex(pos2(x, bar.bottom()), c);
        if i > 0 {
            let b = 2 * (i - 1);
            mesh.add_triangle(b, b + 1, b + 2);
            mesh.add_triangle(b + 1, b + 2, b + 3);
        }
    }
    p.add(egui::Shape::mesh(mesh));
    p.circle_filled(pos2(bar.left(), bar.center().y), bar.height() / 2.0, hsv32(0.0, 1.0, 1.0));
    p.circle_filled(pos2(bar.right(), bar.center().y), bar.height() / 2.0, hsv32(1.0, 1.0, 1.0));
    let mut changed = false;
    if resp.is_pointer_button_down_on() || resp.dragged() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let h = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 0.999);
            if (h - *hue).abs() > 0.001 {
                *hue = h;
                changed = true;
            }
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let knob = pos2(bar.left() + *hue * bar.width(), bar.center().y);
    p.circle(knob, 12.0, hsv32(*hue, 1.0, 1.0), Stroke::new(3.0_f32, Color32::WHITE));
    changed
}

/// Quick colours; returns the clicked one.
fn swatches(ui: &mut Ui, current: [u8; 3]) -> Option<[u8; 3]> {
    const COLORS: [[u8; 3]; 10] = [
        [0xff, 0xff, 0xff],
        [0xff, 0xb4, 0x6b],
        [0xff, 0x00, 0x00],
        [0xff, 0x6a, 0x00],
        [0xff, 0xd0, 0x00],
        [0x00, 0xff, 0x40],
        [0x00, 0xe5, 0xff],
        [0x00, 0x40, 0xff],
        [0x8a, 0x2b, 0xe2],
        [0xff, 0x00, 0xf7],
    ];
    let d = 28.0;
    let gap = 10.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), d + 4.0), Sense::hover());
    let p = ui.painter().clone();
    let mut picked = None;
    for (i, c) in COLORS.iter().enumerate() {
        let center = pos2(rect.left() + 2.0 + d / 2.0 + i as f32 * (d + gap), rect.center().y);
        if center.x + d / 2.0 > rect.right() {
            break;
        }
        let r = Rect::from_center_size(center, vec2(d, d));
        let resp = ui.interact(r, ui.id().with("swatch").with(i), Sense::click());
        if resp.clicked() {
            picked = Some(*c);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        p.circle_filled(center, d / 2.0, Color32::from_rgb(c[0], c[1], c[2]));
        let ring = if *c == current {
            Stroke::new(2.5_f32, Color32::WHITE)
        } else if resp.hovered() {
            Stroke::new(1.5_f32, MUTED)
        } else {
            Stroke::new(1.0_f32, BORDER)
        };
        p.circle_stroke(center, d / 2.0 + 2.0, ring);
    }
    picked
}

/// Segmented level meter, 0..=100.
fn level_meter(ui: &mut Ui, level: u8) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    let p = ui.painter();
    let n = 28;
    let gap = 3.0;
    let w = (rect.width() - gap * (n - 1) as f32) / n as f32;
    let lit = (level as f32 / 100.0 * n as f32).round() as usize;
    for i in 0..n {
        let r = Rect::from_min_size(pos2(rect.left() + i as f32 * (w + gap), rect.top()), vec2(w, rect.height()));
        let t = i as f32 / (n - 1) as f32;
        let on = lerp_color(GREEN, if t < 0.7 { AMBER } else { RED }, (t / 0.7).min(1.0));
        p.rect_filled(r, CornerRadius::same(3), if i < lit { on } else { LED_OFF });
    }
}
