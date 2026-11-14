//! Settings stored in %APPDATA%\synclight-rs\config.ini (simple `key = value`).

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
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
        self
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

pub fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
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
            _ => {}
        }
    }
    s.sanitize()
}

pub fn load() -> Settings {
    std::fs::read_to_string(path()).map(|t| parse(&t)).unwrap_or_default()
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    let b = |x: bool| if x { 1 } else { 0 };
    let text = format!(
        "# Настройки synclight-rs. После сохранения файла программа подхватит их сама.\n\
         \n\
         # Синхронизация с экраном включена (1/0)\n\
         enabled = {}\n\
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
         per_led = {}\n",
        b(s.enabled),
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
    );
    std::fs::write(path(), text)
}
