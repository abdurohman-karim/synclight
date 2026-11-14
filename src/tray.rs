//! synclight-tray: background app with a notification-area icon.
//!
//! Click on the icon opens the settings window (a separate process, `--ui`);
//! right click shows a short menu. Settings live in %APPDATA%\synclight-rs\config.ini:
//! the window tells this process to re-read them with `WM_RELOAD`, manual edits are
//! picked up by a timer.
//! `synclight-tray.exe --autostart on|off` sets autostart and exits.

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::SystemTime;

use synclight::config::{self, Settings};
use synclight::engine::{self, Options, Shared, State};
use synclight::{autostart, capture, icon, ipc, logger, ui};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

const ID_OPEN: usize = 100;
const ID_TOGGLE: usize = 101;
const ID_EXIT: usize = 102;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct App {
    hwnd: HWND,
    shared: Arc<Shared>,
    engine: Option<JoinHandle<()>>,
    icon_on: HICON,
    icon_off: HICON,
    icon_active: bool,
    icon_added: bool,
    tip: String,
    cfg_mtime: Option<SystemTime>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Short borrow of the app state. Never call anything that pumps messages inside `f`.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

/// Brings an already open settings window to the front. Returns false if there is none.
fn raise_ui_window() -> bool {
    unsafe {
        match FindWindowW(PCWSTR::null(), &HSTRING::from(ui::WINDOW_TITLE)) {
            Ok(h) if !h.is_invalid() => {
                if IsIconic(h).as_bool() {
                    let _ = ShowWindow(h, SW_RESTORE);
                }
                let _ = SetForegroundWindow(h);
                true
            }
            _ => false,
        }
    }
}

fn open_ui() {
    if raise_ui_window() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Err(e) = std::process::Command::new(exe).arg("--ui").spawn() {
            logger::info(format!("не удалось открыть окно настроек: {e}"));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--autostart") => {
            let on = args.get(1).map(String::as_str) != Some("off");
            std::process::exit(if autostart::set(on).is_ok() { 0 } else { 1 });
        }
        Some("--ui") => {
            unsafe {
                let mutex = CreateMutexW(None, true, w!("Local\\synclight-rs-ui"));
                if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
                    raise_ui_window();
                    return;
                }
            }
            if let Err(e) = ui::run() {
                unsafe {
                    MessageBoxW(
                        None,
                        &HSTRING::from(format!("Не удалось открыть окно настроек:\n{e}")),
                        w!("SyncLight RS"),
                        MB_ICONERROR | MB_OK,
                    );
                }
            }
            return;
        }
        _ => {}
    }

    unsafe {
        // One background instance per user session.
        let mutex = CreateMutexW(None, true, w!("Local\\synclight-rs-tray"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
    }

    capture::init_dpi();
    let _ = std::fs::create_dir_all(config::dir());
    logger::init(Some(config::log_path()), false);
    logger::info(format!("synclight-tray {} запущен", env!("CARGO_PKG_VERSION")));

    if !config::path().exists() {
        let _ = config::save(&Settings::default());
    }
    let settings = config::load();
    let shared = Shared::new(settings);
    let engine = {
        let s = shared.clone();
        std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                engine::run(s, Options { dry_run: false, yield_to_synclight: true, publish_status: true })
            })
            .expect("engine thread")
    };

    unsafe {
        let hinst = GetModuleHandleW(None).expect("module handle");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: ipc::TRAY_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
        // A hidden top-level window (not message-only) so it receives WM_ENDSESSION.
        // Its title differs from the settings window so FindWindow can tell them apart.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            ipc::TRAY_CLASS,
            w!("SyncLight RS service"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("window");

        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let app = App {
            hwnd,
            shared,
            engine: Some(engine),
            icon_on: make_icon(size, true),
            icon_off: make_icon(size, false),
            icon_active: false,
            icon_added: false,
            tip: "SyncLight RS".into(),
            cfg_mtime: config_mtime(),
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        update_icon(true);
        SetTimer(Some(hwnd), TIMER_ID, 1000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    shutdown();
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(config::path()).and_then(|m| m.modified()).ok()
}

/// Stops the engine (it turns the strip off) and removes the icon. Safe to call twice.
fn shutdown() {
    let parts = with_app(|a| (a.engine.take(), a.shared.clone(), a.hwnd, a.icon_added));
    if let Some((engine, shared, hwnd, added)) = parts {
        shared.request_stop();
        if let Some(h) = engine {
            let _ = h.join();
        }
        if added {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            with_app(|a| a.icon_added = false);
        }
        logger::info("synclight-tray завершён");
    }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..wide.len()].copy_from_slice(&wide);
    nid.szTip[wide.len()] = 0;
}

/// Adds the icon or refreshes its picture and tooltip from the engine status.
fn update_icon(force_add: bool) {
    let Some((hwnd, status, icon_on, icon_off, added, prev_tip, prev_active)) =
        with_app(|a| (a.hwnd, a.shared.status(), a.icon_on, a.icon_off, a.icon_added, a.tip.clone(), a.icon_active))
    else {
        return;
    };
    let active = status.state == State::Running;
    let mut tip = format!("SyncLight RS\n{}", status.summary());
    if let Some(d) = &status.device {
        tip += &format!("\nЛента {}, {} светодиодов", d.id, d.lamps);
    }
    if !force_add && added && tip == prev_tip && active == prev_active {
        return;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: if active { icon_on } else { icon_off },
        ..Default::default()
    };
    set_tip(&mut nid, &tip);
    let ok = unsafe {
        if force_add || !added {
            Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
        } else {
            Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
        }
    };
    with_app(|a| {
        a.icon_added = ok;
        a.tip = tip;
        a.icon_active = active;
    });
}

fn reload_config() {
    let m = config_mtime();
    if let Some(shared) = with_app(|a| {
        a.cfg_mtime = m;
        a.shared.clone()
    }) {
        let new = config::load();
        shared.update_settings(|s| *s = new);
    }
    update_icon(false);
}

fn toggle_enabled() {
    if let Some(shared) = with_app(|a| a.shared.clone()) {
        let s = shared.update_settings(|s| s.enabled = !s.enabled);
        if let Err(e) = config::save(&s) {
            logger::info(format!("не удалось сохранить настройки: {e}"));
        }
        let m = config_mtime();
        with_app(|a| a.cfg_mtime = m);
    }
    update_icon(false);
}

fn on_timer() {
    // Pick up manual edits of config.ini.
    let m = config_mtime();
    let changed = with_app(|a| m.is_some() && m != a.cfg_mtime).unwrap_or(false);
    if changed {
        reload_config();
        logger::info("настройки перечитаны из файла");
    }
    update_icon(false);
}

fn add_item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING | flags, id, &HSTRING::from(text));
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(shared) = with_app(|a| a.shared.clone()) else { return };
    let enabled = shared.settings().enabled;
    let st = shared.status();

    let Ok(menu) = CreatePopupMenu() else { return };
    add_item(menu, MF_GRAYED, 0, &st.summary());
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add_item(menu, MENU_ITEM_FLAGS(0), ID_OPEN, "Открыть SyncLight RS");
    let _ = SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
    add_item(menu, if enabled { MF_CHECKED } else { MF_UNCHECKED }, ID_TOGGLE, "Подсветка включена");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add_item(menu, MENU_ITEM_FLAGS(0), ID_EXIT, "Выход");

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_OPEN => open_ui(),
        ID_TOGGLE => toggle_enabled(),
        ID_EXIT => {
            shutdown();
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => open_ui(),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        ipc::WM_RELOAD => {
            reload_config();
            LRESULT(0)
        }
        WM_TIMER => {
            on_timer();
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wp.0 != 0 {
                shutdown();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            // Explorer restarted: the icon must be added again.
            update_icon(true);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Tray icon from the shared drawing (RGBA -> BGRA for GDI).
fn make_icon(size: i32, active: bool) -> HICON {
    let mut px = icon::rgba(size as u32, active);
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    let n = size as usize;
    let mask = vec![0u8; n.div_ceil(16) * 2 * n];
    unsafe { CreateIcon(None, size, size, 1, 32, mask.as_ptr(), px.as_ptr()).unwrap_or_default() }
}
