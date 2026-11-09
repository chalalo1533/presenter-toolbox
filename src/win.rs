//! Everything that talks to Windows directly: global hotkeys, the tray icon,
//! fast screen capture, window placement and the GPU (DWM) live zoom.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Magnification::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::settings::{HOTKEY_SLOTS, Hotkey, Hotkeys};

pub const TRAY_CLASS: &str = "ZoomItRsTrayWindow";
const WM_TRAY: u32 = WM_APP + 1;
const WM_ACTION: u32 = WM_APP + 2;
const WM_REREGISTER: u32 = WM_APP + 3;
const WM_SUSPEND: u32 = WM_APP + 4;

/// One global shortcut as handed to the hotkey thread.
#[derive(Clone, Debug)]
pub struct HotkeyDef {
    pub id: i32,
    pub key: Hotkey,
    pub name: &'static str,
}

pub fn defs_from(h: &Hotkeys) -> Vec<HotkeyDef> {
    HOTKEY_SLOTS
        .iter()
        .enumerate()
        .map(|(i, (id, name))| HotkeyDef { id: *id, key: h.get(i), name })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Zoom = 1,
    Draw = 2,
    Break = 3,
    LiveZoom = 4,
    Screenshot = 5,
    Snip = 6,
    SnipSave = 7,
    Spotlight = 8,
    Tutorial = 9,
    FindMouse = 12,
    Gif = 13,
    Options = 20,
    Exit = 21,
    LiveIn = 30,
    LiveOut = 31,
    /// a GIF finished encoding (Msg.file holds the temporary file)
    GifDone = 40,
}

impl Action {
    fn from_id(id: usize) -> Option<Action> {
        Some(match id {
            1 => Action::Zoom,
            2 => Action::Draw,
            3 => Action::Break,
            4 => Action::LiveZoom,
            5 => Action::Screenshot,
            6 => Action::Snip,
            7 => Action::SnipSave,
            8 => Action::Spotlight,
            9 => Action::Tutorial,
            12 => Action::FindMouse,
            13 => Action::Gif,
            20 => Action::Options,
            21 => Action::Exit,
            30 => Action::LiveIn,
            31 => Action::LiveOut,
            _ => return None,
        })
    }
}

pub struct Capture {
    pub rect: ScreenRect,
    pub image: ColorImage,
}

pub struct Msg {
    pub action: Action,
    pub capture: Option<Capture>,
    pub file: Option<std::path::PathBuf>,
}

/// Called by the GIF recorder when the file is ready to be saved.
pub fn gif_done(path: std::path::PathBuf) {
    if let Some(sh) = SHARED.get() {
        let _ = sh.tx.send(Msg { action: Action::GifDone, capture: None, file: Some(path) });
        sh.ctx.request_repaint();
    }
}

/// The current shortcut for an action, e.g. "Ctrl+9".
pub fn hotkey_label(a: Action) -> String {
    SHARED
        .get()
        .and_then(|sh| sh.hotkey_defs.lock().unwrap().iter().find(|d| d.id == a as i32).map(|d| d.key.label()))
        .unwrap_or_default()
}

/// State shared between the UI thread and the hotkey/tray thread.
pub struct Shared {
    pub ctx: egui::Context,
    pub tx: mpsc::Sender<Msg>,
    #[allow(dead_code)]
    pub app_hwnd: AtomicIsize,
    pub tray_hwnd: AtomicIsize,
    pub overlay_active: AtomicBool,
    pub live_running: AtomicBool,
    pub live_target: AtomicU32,
    pub live_level: AtomicU32,
    pub hotkeys: Mutex<Vec<(String, bool)>>,
    pub hotkey_defs: Mutex<Vec<HotkeyDef>>,
    pub live_error: AtomicBool,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get().expect("shared state")
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Screen helpers

pub fn cursor_pos() -> (i32, i32) {
    unsafe {
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        (p.x, p.y)
    }
}

pub fn monitor_at_cursor() -> ScreenRect {
    unsafe {
        let (x, y) = cursor_pos();
        let hm = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hm, &mut mi) != 0 {
            let r = mi.rcMonitor;
            ScreenRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
        } else {
            ScreenRect { x: 0, y: 0, w: GetSystemMetrics(SM_CXSCREEN), h: GetSystemMetrics(SM_CYSCREEN) }
        }
    }
}

/// Grabs one monitor straight into a DIB section (one GPU→CPU copy, no scaling).
pub fn capture(rect: ScreenRect) -> Option<ColorImage> {
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = rect.w;
        bmi.bmiHeader.biHeight = -rect.h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
            return None;
        }
        let old = SelectObject(mem, dib);
        let ok = BitBlt(mem, 0, 0, rect.w, rect.h, screen, rect.x, rect.y, SRCCOPY | CAPTUREBLT);
        GdiFlush();
        let n = (rect.w as usize) * (rect.h as usize);
        let src = std::slice::from_raw_parts(bits as *const u8, n * 4);
        let mut pixels = Vec::with_capacity(n);
        pixels.extend(src.chunks_exact(4).map(|p| Color32::from_rgb(p[2], p[1], p[0])));
        SelectObject(mem, old);
        DeleteObject(dib);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        if ok == 0 {
            return None;
        }
        Some(ColorImage::new([rect.w as usize, rect.h as usize], pixels))
    }
}

// ---------------------------------------------------------------------------
// Window placement (done with Win32 so it happens instantly, without a flash)

pub fn place_overlay(hwnd: isize, r: ScreenRect, show: bool) {
    unsafe {
        let flags = SWP_NOACTIVATE | if show { SWP_SHOWWINDOW } else { 0 };
        SetWindowPos(hwnd as HWND, HWND_TOPMOST, r.x, r.y, r.w, r.h, flags);
    }
}

pub fn place_centered(hwnd: isize, w: i32, h: i32) {
    unsafe {
        let m = monitor_at_cursor();
        let x = m.x + (m.w - w) / 2;
        let y = m.y + (m.h - h) / 2;
        SetWindowPos(hwnd as HWND, HWND_NOTOPMOST, x, y, w, h, SWP_SHOWWINDOW);
    }
}

pub fn activate(hwnd: isize) {
    unsafe {
        let h = hwnd as HWND;
        SetForegroundWindow(h);
        if GetForegroundWindow() != h {
            // Pressing a key releases the foreground lock for us.
            keybd_event(VK_MENU as u8, 0, 0, 0);
            SetForegroundWindow(h);
            keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        SetFocus(h);
    }
}

pub fn hide(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_HIDE);
    }
}

pub fn window_rect(hwnd: isize) -> ScreenRect {
    unsafe {
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd as HWND, &mut r);
        ScreenRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
    }
}

pub fn dpi_scale_for(hwnd: isize) -> f32 {
    unsafe {
        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd as HWND);
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }
}

pub fn beep() {
    unsafe {
        MessageBeep(MB_ICONASTERISK);
    }
}

/// Asks the hotkey thread to do something (e.g. start a zoom from the options window).
pub fn post_action(a: Action) {
    if let Some(sh) = SHARED.get() {
        let h = sh.tray_hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                PostMessageW(h as HWND, WM_ACTION, a as usize, 1);
            }
        }
    }
}

/// Returns false if another copy is already running (and tells it to show its options).
pub fn single_instance() -> bool {
    unsafe {
        let name = wide("ZoomItRs_SingleInstance_Mutex");
        let _m = CreateMutexW(null(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let cls = wide(TRAY_CLASS);
            let other = FindWindowW(cls.as_ptr(), null());
            if !other.is_null() {
                PostMessageW(other, WM_ACTION, Action::Options as usize, 0);
            }
            return false;
        }
        true
    }
}

pub fn save_dialog(owner: isize, default_name: &str) -> Option<std::path::PathBuf> {
    unsafe {
        let mut buf = [0u16; 1024];
        for (i, c) in default_name.encode_utf16().enumerate().take(1000) {
            buf[i] = c;
        }
        let gif = default_name.to_ascii_lowercase().ends_with(".gif");
        let filter: Vec<u16> = if gif { "Animación GIF (*.gif)\0*.gif\0\0" } else { "Imagen PNG (*.png)\0*.png\0\0" }
            .encode_utf16()
            .collect();
        let ext = wide(if gif { "gif" } else { "png" });
        let mut ofn: OPENFILENAMEW = zeroed();
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner as HWND;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = buf.as_mut_ptr();
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrDefExt = ext.as_ptr();
        ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        if GetSaveFileNameW(&mut ofn) == 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len])))
    }
}

// ---------------------------------------------------------------------------
// Icon (a small magnifying glass drawn in code)

pub fn icon_rgba(size: usize) -> Vec<u8> {
    let mut px = vec![0u8; size * size * 4];
    let s = size as f32;
    let (cx, cy, r) = (s * 0.42, s * 0.42, s * 0.30);
    let ring = s * 0.075;
    let color_at = |fx: f32, fy: f32| -> Option<[f32; 3]> {
        let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
        if d < r - ring {
            return Some([170.0, 215.0, 255.0]);
        }
        if d < r + ring {
            return Some([20.0, 70.0, 160.0]);
        }
        // handle: a thick, round-ended segment from the lens towards the corner
        let (ax, ay, bx, by) = (cx + r * 0.75, cy + r * 0.75, s * 0.86, s * 0.86);
        let (vx, vy) = (bx - ax, by - ay);
        let t = (((fx - ax) * vx + (fy - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
        let dd = ((fx - (ax + vx * t)).powi(2) + (fy - (ay + vy * t)).powi(2)).sqrt();
        if dd < s * 0.09 {
            return Some([40.0, 40.0, 48.0]);
        }
        None
    };
    // 4x4 supersampling for smooth (anti-aliased) edges at every size
    const N: usize = 4;
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f32; 3];
            let mut hits = 0usize;
            for sy in 0..N {
                for sx in 0..N {
                    let fx = x as f32 + (sx as f32 + 0.5) / N as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / N as f32;
                    if let Some(c) = color_at(fx, fy) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let i = (y * size + x) * 4;
                let h = hits as f32;
                px[i] = (acc[0] / h) as u8;
                px[i + 1] = (acc[1] / h) as u8;
                px[i + 2] = (acc[2] / h) as u8;
                px[i + 3] = (255 * hits / (N * N)) as u8;
            }
        }
    }
    px
}

unsafe fn make_hicon(size: usize) -> HICON {
    unsafe {
        let rgba = icon_rgba(size);
        let mut bgra = rgba.clone();
        for p in bgra.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        let color = CreateBitmap(size as i32, size as i32, 1, 32, bgra.as_ptr() as *const c_void);
        let mask_bits = vec![0u8; size * size / 8 + size * 4];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr() as *const c_void);
        let mut ii: ICONINFO = zeroed();
        ii.fIcon = 1;
        ii.hbmColor = color;
        ii.hbmMask = mask;
        let icon = CreateIconIndirect(&ii);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

// ---------------------------------------------------------------------------
// Hotkey + tray thread

pub fn start(
    ctx: egui::Context,
    tx: mpsc::Sender<Msg>,
    app_hwnd: isize,
    tray: bool,
    live_level: f32,
    defs: Vec<HotkeyDef>,
) {
    let sh = Arc::new(Shared {
        ctx,
        tx,
        app_hwnd: AtomicIsize::new(app_hwnd),
        tray_hwnd: AtomicIsize::new(0),
        overlay_active: AtomicBool::new(false),
        live_running: AtomicBool::new(false),
        live_target: AtomicU32::new(live_level.to_bits()),
        live_level: AtomicU32::new(live_level.to_bits()),
        hotkeys: Mutex::new(Vec::new()),
        hotkey_defs: Mutex::new(defs),
        live_error: AtomicBool::new(false),
    });
    let _ = SHARED.set(sh);
    std::thread::spawn(move || unsafe { hotkey_thread(tray) });
}

pub fn shared_state() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}

static TRAY_ADDED: AtomicBool = AtomicBool::new(false);

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid
    }
}

pub fn remove_tray() {
    if !TRAY_ADDED.swap(false, Ordering::SeqCst) {
        return;
    }
    if let Some(sh) = SHARED.get() {
        let h = sh.tray_hwnd.load(Ordering::SeqCst);
        unsafe {
            let nid = tray_data(h as HWND);
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
    stop_live_now();
}

unsafe fn hotkey_thread(tray: bool) {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let cls = wide(TRAY_CLASS);
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        let title = wide("Presenter Toolbox");
        let hwnd = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let sh = shared();
        sh.tray_hwnd.store(hwnd as isize, Ordering::SeqCst);

        register_all(hwnd);

        if tray {
            let mut nid = tray_data(hwnd);
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY;
            nid.hIcon = make_hicon(32);
            let tip = "Presenter Toolbox — clic para ver los atajos";
            for (i, c) in tip.encode_utf16().enumerate().take(120) {
                nid.szTip[i] = c;
            }
            if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
                TRAY_ADDED.store(true, Ordering::SeqCst);
            }
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn unregister_all(hwnd: HWND) {
    unsafe {
        for (id, _) in HOTKEY_SLOTS {
            UnregisterHotKey(hwnd, id);
        }
    }
}

/// (Re)registers every shortcut from `hotkey_defs` and records which ones Windows accepted.
unsafe fn register_all(hwnd: HWND) {
    unsafe {
        let sh = shared();
        unregister_all(hwnd);
        let defs = sh.hotkey_defs.lock().unwrap().clone();
        let mut status = Vec::new();
        for d in defs {
            if d.key.vk == 0 {
                status.push((format!("Sin asignar  {}", d.name), true));
                continue;
            }
            let mut m = MOD_NOREPEAT;
            if d.key.ctrl {
                m |= MOD_CONTROL;
            }
            if d.key.shift {
                m |= MOD_SHIFT;
            }
            if d.key.alt {
                m |= MOD_ALT;
            }
            let ok = RegisterHotKey(hwnd, d.id, m, d.key.vk) != 0;
            status.push((format!("{}  {}", d.key.label(), d.name), ok));
        }
        *sh.hotkeys.lock().unwrap() = status;
        sh.ctx.request_repaint();
    }
}

/// Replaces the global shortcuts (also resumes them after `suspend_hotkeys(true)`).
pub fn set_hotkeys(defs: Vec<HotkeyDef>) {
    if let Some(sh) = SHARED.get() {
        *sh.hotkey_defs.lock().unwrap() = defs;
        let h = sh.tray_hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                PostMessageW(h as HWND, WM_REREGISTER, 0, 0);
            }
        }
    }
}

/// Turns the global shortcuts off while the user is typing a new combination.
pub fn suspend_hotkeys(on: bool) {
    if let Some(sh) = SHARED.get() {
        let h = sh.tray_hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                PostMessageW(h as HWND, WM_SUSPEND, on as usize, 0);
            }
        }
    }
}

/// Shows a small Windows notification from the tray icon.
pub fn notify(title: &str, text: &str) {
    if !TRAY_ADDED.load(Ordering::SeqCst) {
        beep();
        return;
    }
    let Some(sh) = SHARED.get() else { return };
    unsafe {
        let mut nid = tray_data(sh.tray_hwnd.load(Ordering::SeqCst) as HWND);
        nid.uFlags = NIF_INFO;
        for (i, c) in text.encode_utf16().enumerate().take(250) {
            nid.szInfo[i] = c;
        }
        for (i, c) in title.encode_utf16().enumerate().take(60) {
            nid.szInfoTitle[i] = c;
        }
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_REREGISTER => {
                register_all(hwnd);
                0
            }
            WM_SUSPEND => {
                if wp != 0 {
                    unregister_all(hwnd);
                } else {
                    register_all(hwnd);
                }
                0
            }
            WM_HOTKEY => {
                if let Some(a) = Action::from_id(wp) {
                    trigger(a, false);
                } else if wp == 10 {
                    trigger(Action::LiveIn, false);
                } else if wp == 11 {
                    trigger(Action::LiveOut, false);
                }
                0
            }
            WM_ACTION => {
                if let Some(a) = Action::from_id(wp) {
                    trigger(a, lp != 0);
                }
                0
            }
            WM_TRAY => {
                let ev = (lp as u32) & 0xffff;
                if ev == WM_LBUTTONDBLCLK {
                    trigger(Action::Options, false);
                } else if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP {
                    show_menu(hwnd);
                }
                0
            }
            WM_COMMAND => {
                let id = wp & 0xffff;
                if let Some(a) = Action::from_id(id) {
                    trigger(a, true);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        let t = wide("Opciones…");
        AppendMenuW(menu, MF_STRING, Action::Options as usize, t.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let defs = shared().hotkey_defs.lock().unwrap().clone();
        let key_of = |a: Action| {
            defs.iter().find(|d| d.id == a as i32).map(|d| d.key.label()).unwrap_or_default()
        };
        let items = [
            (Action::Zoom, "Zoom"),
            (Action::Draw, "Dibujar"),
            (Action::Break, "Temporizador de descanso"),
            (Action::LiveZoom, "Zoom en vivo"),
            (Action::Screenshot, "Captura al portapapeles"),
            (Action::Snip, "Recorte"),
            (Action::Spotlight, "Foco"),
            (Action::Tutorial, "Clics y teclas"),
            (Action::FindMouse, "Encontrar el mouse"),
            (
                Action::Gif,
                if crate::gif_rec::recording() { "Detener la grabación GIF" } else { "Grabar GIF" },
            ),
        ];
        for (a, text) in items {
            let t = wide(&format!("{text}\t{}", key_of(a)));
            let on = match a {
                Action::Spotlight => crate::fx::SPOT_ON.load(Ordering::SeqCst),
                Action::Tutorial => crate::fx::TUTORIAL_ON.load(Ordering::SeqCst),
                _ => false,
            };
            AppendMenuW(menu, MF_STRING | if on { MF_CHECKED } else { 0 }, a as usize, t.as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let t = wide("Salir");
        AppendMenuW(menu, MF_STRING, Action::Exit as usize, t.as_ptr());
        SetMenuDefaultItem(menu, Action::Options as u32, 0);
        let (x, y) = cursor_pos();
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

fn send(a: Action, capture: Option<Capture>) {
    let sh = shared();
    let _ = sh.tx.send(Msg { action: a, capture, file: None });
    sh.ctx.request_repaint();
}

fn trigger(a: Action, delayed: bool) {
    let sh = shared();
    match a {
        Action::Options | Action::Exit => send(a, None),
        Action::GifDone => {}
        Action::LiveZoom => toggle_live(),
        Action::Spotlight => crate::fx::post(crate::fx::Cmd::Spot),
        Action::Tutorial => crate::fx::post(crate::fx::Cmd::Tutorial),
        Action::FindMouse => crate::fx::post(crate::fx::Cmd::Find),
        Action::Gif if crate::gif_rec::recording() => crate::gif_rec::stop(),
        Action::Screenshot => {
            if sh.overlay_active.load(Ordering::SeqCst) {
                // zooming or drawing: the app copies exactly what is on screen
                send(a, None);
                return;
            }
            if delayed {
                std::thread::sleep(Duration::from_millis(180));
            }
            let rect = monitor_at_cursor();
            if let Some(image) = capture(rect) {
                send(a, Some(Capture { rect, image }));
            }
        }
        Action::LiveIn | Action::LiveOut => {
            let cur = f32::from_bits(sh.live_target.load(Ordering::SeqCst));
            let next = if a == Action::LiveIn { cur * 1.25 } else { cur / 1.25 }.clamp(1.0, 16.0);
            sh.live_target.store(next.to_bits(), Ordering::SeqCst);
        }
        _ => {
            if sh.overlay_active.load(Ordering::SeqCst) {
                send(a, None);
                return;
            }
            if sh.live_running.load(Ordering::SeqCst) {
                stop_live_now();
                std::thread::sleep(Duration::from_millis(40));
            }
            if delayed {
                // let a menu or the options window disappear first
                std::thread::sleep(Duration::from_millis(180));
            }
            let rect = monitor_at_cursor();
            if let Some(image) = capture(rect) {
                sh.overlay_active.store(true, Ordering::SeqCst);
                send(a, Some(Capture { rect, image }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Live zoom: the desktop keeps running; Windows' compositor scales it on the GPU.

static LIVE_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn toggle_live() {
    let sh = shared();
    if sh.live_running.load(Ordering::SeqCst) {
        sh.live_running.store(false, Ordering::SeqCst);
        unregister_live_keys();
        return;
    }
    if let Some(h) = LIVE_THREAD.lock().unwrap().take() {
        let _ = h.join();
    }
    let level = f32::from_bits(sh.live_level.load(Ordering::SeqCst));
    sh.live_target.store(level.to_bits(), Ordering::SeqCst);
    sh.live_running.store(true, Ordering::SeqCst);
    unsafe {
        let h = sh.tray_hwnd.load(Ordering::SeqCst) as HWND;
        RegisterHotKey(h, 10, MOD_CONTROL, VK_UP as u32);
        RegisterHotKey(h, 11, MOD_CONTROL, VK_DOWN as u32);
    }
    let handle = std::thread::spawn(|| unsafe { live_thread() });
    *LIVE_THREAD.lock().unwrap() = Some(handle);
}

fn unregister_live_keys() {
    let sh = shared();
    unsafe {
        let h = sh.tray_hwnd.load(Ordering::SeqCst) as HWND;
        UnregisterHotKey(h, 10);
        UnregisterHotKey(h, 11);
    }
}

fn stop_live_now() {
    let Some(sh) = SHARED.get() else { return };
    if sh.live_running.swap(false, Ordering::SeqCst) {
        unregister_live_keys();
    }
    if let Some(h) = LIVE_THREAD.lock().unwrap().take() {
        let _ = h.join();
    }
}

/// Checks (without visibly zooming) that the GPU magnifier can be used here.
pub fn check_live() -> String {
    unsafe {
        let init = MagInitialize() != 0;
        let set = init && MagSetFullscreenTransform(1.0, 0, 0) != 0;
        if init {
            MagUninitialize();
        }
        format!("init={init} transform={set}")
    }
}

unsafe fn live_thread() {
    unsafe {
        let sh = shared();
        if MagInitialize() == 0 {
            sh.live_running.store(false, Ordering::SeqCst);
            sh.live_error.store(true, Ordering::SeqCst);
            sh.ctx.request_repaint();
            return;
        }
        let mut lz: f32 = 0.0; // ln(zoom)
        let mut last = Instant::now();

        loop {
            let running = sh.live_running.load(Ordering::SeqCst);
            let target = if running { f32::from_bits(sh.live_target.load(Ordering::SeqCst)) } else { 1.0 };
            let now = Instant::now();
            let dt = (now - last).as_secs_f32().min(0.05);
            last = now;
            lz += (target.ln() - lz) * (1.0 - (-dt * 14.0).exp());
            if !running && lz < 0.003 {
                break;
            }
            let z = lz.exp();
            let w = GetSystemMetrics(SM_CXSCREEN) as f32;
            let h = GetSystemMetrics(SM_CYSCREEN) as f32;
            let (cx, cy) = cursor_pos();
            let (cx, cy) = ((cx as f32).clamp(0.0, w), (cy as f32).clamp(0.0, h));
            // proportional mapping: the pixel under the cursor stays under the cursor,
            // so clicks land exactly where you see them.
            let tx = (cx * (1.0 - 1.0 / z)).clamp(0.0, w - w / z);
            let ty = (cy * (1.0 - 1.0 / z)).clamp(0.0, h - h / z);
            let ox = tx;
            let oy = ty;
            if MagSetFullscreenTransform(z, ox.round() as i32, oy.round() as i32) == 0 {
                sh.live_error.store(true, Ordering::SeqCst);
                sh.live_running.store(false, Ordering::SeqCst);
                sh.ctx.request_repaint();
                break;
            }
            // wait for the next compositor frame: smooth, tear-free, no busy loop
            if DwmFlush() < 0 {
                std::thread::sleep(Duration::from_millis(8));
            }
        }
        MagSetFullscreenTransform(1.0, 0, 0);
        MagUninitialize();
    }
}
