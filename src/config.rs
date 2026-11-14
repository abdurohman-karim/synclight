//! Settings stored in %APPDATA%\synclight-rs\config.ini (simple `key = value`).

use std::path::PathBuf;

/// What the strip shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Colours follow the screen edges.
    Sync,
    /// One solid colour.
    Static,
    /// Built-in animated effect of the strip firmware (SyncLight "Dynamic").
    Effect,
    /// Effect driven by sound (SyncLight "Rhythm").
    Rhythm,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Sync, Mode::Static, Mode::Effect, Mode::Rhythm];

    pub fn key(self) -> &'static str {
        match self {
            Mode::Sync => "sync",
            Mode::Static => "static",
            Mode::Effect => "effect",
            Mode::Rhythm => "rhythm",
        }
    }

    fn from_key(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.key() == s)
    }
}

/// Where the sound for rhythm effects comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmSource {
    /// Windows output level ("Computer Rhythm"), sent by this app 25 times per second.
    Computer,
    /// The strip's own microphone ("Controller Rhythm").
    Device,
}

/// Number of built-in effects per group in the strip firmware.
pub const EFFECTS: u8 = 7;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub mode: Mode,
    pub gain: f32,
    pub smooth_ms: f32,
    pub fps: u32,
    pub display: usize,
    pub edges: u8,
    pub reverse: bool,
    pub horizontal: Option<usize>,
    pub vertical: Option<usize>,
    pub wire: String,
    pub vivid: bool,
    /// One screen zone per LED instead of SyncLight's 16x9 grid.
    pub per_led: bool,
    /// Colour for `Mode::Static`, sRGB.
    pub color: [u8; 3],
    /// Built-in effect for `Mode::Effect`, 0..EFFECTS.
    pub effect: u8,
    /// Effect speed, 0 (slow) ..= 100 (fast).
    pub effect_speed: u8,
    /// Rhythm effect, 0..EFFECTS.
    pub rhythm: u8,
    pub rhythm_source: RhythmSource,
    /// 0..=100. Device: microphone sensitivity. Computer: level boost (50 = as is).
    pub rhythm_sensitivity: u8,
    /// Strip's own (hardware) brightness, percent 2..=100.
    pub strip_brightness: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            mode: Mode::Sync,
            gain: 1.0,
            smooth_ms: 60.0,
            fps: 60,
            display: 0,
            edges: 3,
            reverse: true,
            horizontal: None,
            vertical: None,
            wire: "RGB".into(),
            vivid: true,
            per_led: false,
            color: [0xff, 0x00, 0xf7],
            effect: 0,
            effect_speed: 50,
            rhythm: 0,
            rhythm_source: RhythmSource::Computer,
            rhythm_sensitivity: 50,
            strip_brightness: 100,
        }
    }
}

impl Settings {
    /// Brings out-of-range values back into safe limits.
    pub fn sanitize(mut self) -> Self {
        self.gain = if self.gain.is_finite() { self.gain.clamp(0.1, 4.0) } else { 1.0 };
        self.smooth_ms = if self.smooth_ms.is_finite() { self.smooth_ms.clamp(0.0, 2000.0) } else { 60.0 };
        self.fps = self.fps.clamp(1, 240);
        if self.edges != 4 {
            self.edges = 3;
        }
        if crate::color::Pipeline::wire_map(&self.wire).is_err() {
            self.wire = "RGB".into();
        }
        self.effect = self.effect.min(EFFECTS - 1);
        self.rhythm = self.rhythm.min(EFFECTS - 1);
        self.effect_speed = self.effect_speed.min(100);
        self.rhythm_sensitivity = self.rhythm_sensitivity.min(100);
        self.strip_brightness = self.strip_brightness.clamp(2, 100);
        self
    }

    /// Value for the strip's brightness command (SyncLight sends 5..=255).
    pub fn strip_brightness_raw(&self) -> u8 {
        ((self.strip_brightness as u32 * 255 + 50) / 100).clamp(5, 255) as u8
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("synclight-rs")
}

pub fn path() -> PathBuf {
    dir().join("config.ini")
}

pub fn log_path() -> PathBuf {
    dir().join("log.txt")
}

fn flag(v: &str) -> Option<bool> {
    match v {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn parse_hex(v: &str) -> Option<[u8; 3]> {
    let h = v.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([c(0)?, c(2)?, c(4)?])
}

pub fn hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

/// Parses config text. Keys that are missing keep the values from `base`.
fn parse_onto(text: &str, mut s: Settings) -> Settings {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        let n = |v: &str| v.parse::<usize>().ok().filter(|&x| x > 0);
        match k.as_str() {
            "enabled" => s.enabled = flag(v).unwrap_or(s.enabled),
            "mode" => s.mode = Mode::from_key(&v.to_ascii_lowercase()).unwrap_or(s.mode),
            "gain" => s.gain = v.replace(',', ".").parse().unwrap_or(s.gain),
            "smooth_ms" => s.smooth_ms = v.replace(',', ".").parse().unwrap_or(s.smooth_ms),
            "fps" => s.fps = v.parse().unwrap_or(s.fps),
            "display" => s.display = v.parse().unwrap_or(s.display),
            "edges" => s.edges = v.parse().unwrap_or(s.edges),
            "reverse" => s.reverse = flag(v).unwrap_or(s.reverse),
            "horizontal" => s.horizontal = n(v),
            "vertical" => s.vertical = n(v),
            "wire" => s.wire = v.to_ascii_uppercase(),
            "vivid" => s.vivid = flag(v).unwrap_or(s.vivid),
            "per_led" => s.per_led = flag(v).unwrap_or(s.per_led),
            "color" => s.color = parse_hex(v).unwrap_or(s.color),
            "effect" => s.effect = v.parse().unwrap_or(s.effect),
            "effect_speed" => s.effect_speed = v.parse().unwrap_or(s.effect_speed),
            "rhythm" => s.rhythm = v.parse().unwrap_or(s.rhythm),
            "rhythm_source" => {
                s.rhythm_source = match v.to_ascii_lowercase().as_str() {
                    "device" | "mic" | "microphone" => RhythmSource::Device,
                    "computer" | "pc" => RhythmSource::Computer,
                    _ => s.rhythm_source,
                }
            }
            "rhythm_sensitivity" => s.rhythm_sensitivity = v.parse().unwrap_or(s.rhythm_sensitivity),
            "strip_brightness" => s.strip_brightness = v.parse().unwrap_or(s.strip_brightness),
            _ => {}
        }
    }
    s.sanitize()
}

pub fn parse(text: &str) -> Settings {
    parse_onto(text, Settings::default())
}

/// Defaults taken from the original SyncLight settings (first run only):
/// static colour, strip brightness, effect speed, microphone sensitivity, rhythm source.
fn synclight_defaults() -> Settings {
    let mut s = Settings::default();
    let Some(appdata) = std::env::var_os("APPDATA") else { return s };
    let Ok(text) = std::fs::read_to_string(PathBuf::from(appdata).join("quiklight-desktop").join("config.json")) else {
        return s;
    };
    // Tiny scanner for `"key": value` in SyncLight's JSON (first device wins).
    let value_after = |key: &str, from: usize| -> Option<(String, usize)> {
        let k = format!("\"{key}\"");
        let i = text[from..].find(&k)? + from + k.len();
        let rest = text[i..].trim_start().strip_prefix(':')?.trim_start();
        let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
        Some((rest[..end].trim().trim_matches('"').to_string(), i))
    };
    if let Some(c) = value_after("hue", 0).and_then(|(v, _)| parse_hex(&v)) {
        s.color = c;
    }
    if let Some((_, at)) = value_after("brightnessColor", 0) {
        if let Some(a) = value_after("a", at).and_then(|(v, _)| v.parse::<f32>().ok()) {
            s.strip_brightness = (a * 100.0).round().clamp(2.0, 100.0) as u8;
        }
    }
    if let Some(v) = value_after("dynamicSpeed", 0).and_then(|(v, _)| v.parse().ok()) {
        s.effect_speed = v;
    }
    if let Some(v) = value_after("pickupSensitivity", 0).and_then(|(v, _)| v.parse().ok()) {
        s.rhythm_sensitivity = v;
    }
    if let Some((v, _)) = value_after("rhythmType", 0) {
        s.rhythm_source = if v == "2" { RhythmSource::Device } else { RhythmSource::Computer };
    }
    s.sanitize()
}

pub fn load() -> Settings {
    match std::fs::read_to_string(path()) {
        Ok(t) => parse_onto(&t, synclight_defaults()),
        Err(_) => synclight_defaults(),
    }
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    let b = |x: bool| if x { 1 } else { 0 };
    let text = format!(
        "# Настройки synclight-rs. После сохранения файла программа подхватит их сама.\n\
         \n\
         # Подсветка включена (1/0)\n\
         enabled = {}\n\
         # Режим: sync (по экрану), static (один цвет), effect (эффект ленты), rhythm (под музыку)\n\
         mode = {}\n\
         \n\
         # --- Синхронизация с экраном ---\n\
         # Усиление яркости: 1.0 = как на экране, 1.3 = ярче\n\
         gain = {:.2}\n\
         # Сглаживание, мс: меньше = быстрее реакция, больше = плавнее\n\
         smooth_ms = {:.0}\n\
         # Частота обновления ленты, кадров в секунду (1..240)\n\
         fps = {}\n\
         # Номер дисплея (0 = первый)\n\
         display = {}\n\
         # Сколько сторон монитора занимает лента: 3 или 4\n\
         edges = {}\n\
         # Направление: 1 = против часовой от правого нижнего угла, 0 = по часовой от левого нижнего\n\
         reverse = {}\n\
         # Светодиодов сверху и на каждой боковой стороне (0 = взять из таблицы модели)\n\
         horizontal = {}\n\
         vertical = {}\n\
         # Порядок цветов в ленте (RGB, GRB, ...)\n\
         wire = {}\n\
         # Усиливать почти чистые цвета, как оригинальный SyncLight (1/0)\n\
         vivid = {}\n\
         # Зоны экрана: 1 = своя зона у каждого светодиода, 0 = сетка 16x9 как в SyncLight\n\
         per_led = {}\n\
         \n\
         # --- Другие режимы ---\n\
         # Цвет для режима static\n\
         color = {}\n\
         # Эффект ленты для режима effect: 0..6, скорость 0..100\n\
         effect = {}\n\
         effect_speed = {}\n\
         # Эффект под музыку: 0..6; источник звука: computer (звук Windows) или device (микрофон ленты)\n\
         rhythm = {}\n\
         rhythm_source = {}\n\
         # Чувствительность к звуку, 0..100\n\
         rhythm_sensitivity = {}\n\
         # Яркость самой ленты, процентов (2..100)\n\
         strip_brightness = {}\n",
        b(s.enabled),
        s.mode.key(),
        s.gain,
        s.smooth_ms,
        s.fps,
        s.display,
        s.edges,
        b(s.reverse),
        s.horizontal.unwrap_or(0),
        s.vertical.unwrap_or(0),
        s.wire,
        b(s.vivid),
        b(s.per_led),
        hex(s.color),
        s.effect,
        s.effect_speed,
        s.rhythm,
        if s.rhythm_source == RhythmSource::Device { "device" } else { "computer" },
        s.rhythm_sensitivity,
        s.strip_brightness,
    );
    std::fs::write(path(), text)
}
