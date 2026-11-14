//! "Start with Windows": HKCU\...\Run value pointing at the tray executable.

use windows_registry::CURRENT_USER;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "SyncLightRS";

fn command() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    format!("\"{}\"", exe.display())
}

pub fn is_enabled() -> bool {
    CURRENT_USER
        .open(RUN)
        .and_then(|k| k.get_string(NAME))
        .map(|v| v.eq_ignore_ascii_case(&command()))
        .unwrap_or(false)
}

pub fn set(on: bool) -> windows::core::Result<()> {
    let key = CURRENT_USER.create(RUN)?;
    if on {
        key.set_string(NAME, &command())
    } else {
        match key.remove_value(NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
            Err(e) => Err(e),
        }
    }
}
