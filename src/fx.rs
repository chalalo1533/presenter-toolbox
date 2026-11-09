//! Live effects drawn over the desktop while you keep working: spotlight, click ripples,
//! keystrokes (drawn as real keycaps) and "find my mouse".
//!
//! Two threads:
//!  * the HOOK thread only owns the low-level mouse/keyboard hooks. Its callbacks do almost
//!    nothing (queue an event, post a message) so the system cursor never lags and Windows
//!    never drops the hook for being slow;
//!  * the EFFECTS thread owns the windows and does all drawing:
//!      - `spot`: a dark, semi-transparent window with a round hole around the cursor;
//!      - `fx`:   a per-pixel-alpha window, sized each frame to just what is animating.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win::{cursor_pos, monitor_at_cursor, wide};

#[derive(Clone, Debug, PartialEq)]
pub struct FxConfig {
    pub spot_radius: f32,
    pub spot_dim: f32,
    pub show_clicks: bool,
    pub show_keys: bool,
    pub keys_only_combos: bool,
    pub find_on_shake: bool,
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            spot_radius: 150.0,
            spot_dim: 0.6,
            show_clicks: true,
            show_keys: true,
            keys_only_combos: true,
            find_on_shake: false,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Cmd {
    Spot = 1,
    Tutorial = 2,
    Find = 3,
    Config = 4,
}

const WM_FX: u32 = WM_APP + 20;
const WM_FX_INPUT: u32 = WM_APP + 21;
const WM_HOOKS: u32 = WM_APP + 30;
const TIMER_ID: usize = 1;
const RIPPLE_SECS: f32 = 0.5;
const FIND_SECS: f32 = 1.1;
const BADGE_SECS: f32 = 2.0;
const CAP_H: f32 = 50.0;

static FX_HWND: AtomicIsize = AtomicIsize::new(0);
static HOOK_TID: AtomicU32 = AtomicU32::new(0);
static CONFIG: Mutex<Option<FxConfig>> = Mutex::new(None);
pub static SPOT_ON: AtomicBool = AtomicBool::new(false);
pub static TUTORIAL_ON: AtomicBool = AtomicBool::new(false);
// what the hook thread should currently report
static CLICKS_ON: AtomicBool = AtomicBool::new(false);
static KEYS_ON: AtomicBool = AtomicBool::new(false);
static SHAKE_ON: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
enum InputEv {
    Click { x: i32, y: i32, btn: u8 },
    Key { vk: u32, ctrl: bool, alt: bool, shift: bool, win: bool },
    Wheel(f32),
    Find,
}

static INPUT: Mutex<VecDeque<InputEv>> = Mutex::new(VecDeque::new());

pub fn start(cfg: FxConfig) {
    *CONFIG.lock().unwrap() = Some(cfg);
    std::thread::spawn(|| unsafe { hook_thread() });
    std::thread::spawn(|| unsafe { fx_thread() });
}

/// Pushes new settings to the effects thread (cheap when nothing changed).
pub fn set_config(cfg: FxConfig) {
    let mut c = CONFIG.lock().unwrap();
    if c.as_ref() != Some(&cfg) {
        *c = Some(cfg);
        drop(c);
        post(Cmd::Config);
    }
}

pub fn post(cmd: Cmd) {
    let h = FX_HWND.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            PostMessageW(h as HWND, WM_FX, cmd as usize, 0);
        }
    }
}

fn config() -> FxConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

fn push_input(ev: InputEv) {
    let was_empty = {
        let mut q = INPUT.lock().unwrap();
        let e = q.is_empty();
        if q.len() < 256 {
            q.push_back(ev);
        }
        e
    };
    if was_empty {
        let h = FX_HWND.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                PostMessageW(h as HWND, WM_FX_INPUT, 0, 0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hook thread

#[derive(Default)]
struct Shake {
    last_x: i32,
    dir: i32,
    travel: i32,
    revs: VecDeque<Instant>,
    last: Option<Instant>,
}

thread_local! {
    static SHAKE: RefCell<Shake> = RefCell::new(Shake::default());
}

/// A real "shake": at least 5 quick direction changes of 60+ px within 0.9 s.
fn shake_detect(x: i32) -> bool {
    SHAKE.with(|c| {
        let Ok(mut sh) = c.try_borrow_mut() else { return false };
        let now = Instant::now();
        let dx = x - sh.last_x;
        sh.last_x = x;
        if dx == 0 || dx.abs() > 500 {
            return false;
        }
        let dir = dx.signum();
        if dir != sh.dir {
            if sh.travel >= 60 {
                sh.revs.push_back(now);
            } else {
                sh.revs.clear(); // a small wobble breaks the chain
            }
            sh.dir = dir;
            sh.travel = 0;
        }
        sh.travel += dx.abs();
        while sh.revs.front().is_some_and(|t| now - *t > Duration::from_millis(900)) {
            sh.revs.pop_front();
        }
        if sh.revs.len() >= 5 && sh.last.is_none_or(|t| now - t > Duration::from_millis(1500)) {
            sh.last = Some(now);
            sh.revs.clear();
            return true;
        }
        false
    })
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let m = &*(lp as *const MSLLHOOKSTRUCT);
            let msg = wp as u32;
            match msg {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN if CLICKS_ON.load(Ordering::Relaxed) => {
                    let btn = match msg {
                        WM_LBUTTONDOWN => 0,
                        WM_RBUTTONDOWN => 1,
                        _ => 2,
                    };
                    push_input(InputEv::Click { x: m.pt.x, y: m.pt.y, btn });
                }
                WM_MOUSEMOVE if SHAKE_ON.load(Ordering::Relaxed) => {
                    if shake_detect(m.pt.x) {
                        push_input(InputEv::Find);
                    }
                }
                WM_MOUSEWHEEL
                    if SPOT_ON.load(Ordering::Relaxed) && key_down(VK_CONTROL) && key_down(VK_SHIFT) =>
                {
                    let delta = ((m.mouseData >> 16) as u16 as i16) as f32 / 120.0;
                    push_input(InputEv::Wheel(delta));
                    return 1; // swallowed: it only resizes the spotlight
                }
                _ => {}
            }
        }
        CallNextHookEx(null_mut(), code, wp, lp)
    }
}

unsafe extern "system" fn kb_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && (wp as u32 == WM_KEYDOWN || wp as u32 == WM_SYSKEYDOWN) && KEYS_ON.load(Ordering::Relaxed) {
            let k = &*(lp as *const KBDLLHOOKSTRUCT);
            if !is_modifier(k.vkCode) {
                push_input(InputEv::Key {
                    vk: k.vkCode,
                    ctrl: key_down(VK_CONTROL),
                    alt: key_down(VK_MENU),
                    shift: key_down(VK_SHIFT),
                    win: key_down(VK_LWIN) || key_down(VK_RWIN),
                });
            }
        }
        CallNextHookEx(null_mut(), code, wp, lp)
    }
}

unsafe fn hook_thread() {
    unsafe {
        let mut msg: MSG = zeroed();
        PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE); // create the message queue
        HOOK_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
        let hinst = GetModuleHandleW(null());
        let mut mh: HHOOK = null_mut();
        let mut kh: HHOOK = null_mut();
        let apply = |mh: &mut HHOOK, kh: &mut HHOOK| {
            let want_mouse =
                SPOT_ON.load(Ordering::SeqCst) || CLICKS_ON.load(Ordering::SeqCst) || SHAKE_ON.load(Ordering::SeqCst);
            let want_kb = KEYS_ON.load(Ordering::SeqCst);
            if want_mouse && mh.is_null() {
                *mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinst, 0);
            } else if !want_mouse && !mh.is_null() {
                UnhookWindowsHookEx(*mh);
                *mh = null_mut();
            }
            if want_kb && kh.is_null() {
                *kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), hinst, 0);
            } else if !want_kb && !kh.is_null() {
                UnhookWindowsHookEx(*kh);
                *kh = null_mut();
            }
        };
        apply(&mut mh, &mut kh);
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.hwnd.is_null() && msg.message == WM_HOOKS {
                apply(&mut mh, &mut kh);
            } else {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn refresh_hooks() {
    let tid = HOOK_TID.load(Ordering::SeqCst);
    if tid != 0 {
        unsafe {
            PostThreadMessageW(tid, WM_HOOKS, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Keys

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0x90 | 0x91 | 0xA0..=0xA5)
}

/// Keys that type something (letters, digits, punctuation, space).
fn is_printable(vk: u32) -> bool {
    matches!(vk, 0x20 | 0x30..=0x39 | 0x41..=0x5A | 0x60..=0x6F | 0xBA..=0xC0 | 0xDB..=0xDF | 0xE2)
}

fn key_char(vk: u32) -> Option<char> {
    let c = unsafe { MapVirtualKeyW(vk, 2) } & 0x7FFF;
    char::from_u32(c).filter(|c| *c != '\0' && !c.is_control())
}

fn key_name(vk: u32) -> String {
    match vk {
        0x08 => "⌫".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x13 => "Pausa".into(),
        0x1B => "Esc".into(),
        0x20 => "Espacio".into(),
        0x2C => "ImprPant".into(),
        0x5D => "Menú".into(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x21..=0x28 | 0x2D | 0x2E | 0x30..=0x39 | 0x41..=0x5A | 0x70..=0x87 => crate::settings::vk_name(vk),
        _ => key_char(vk).map(|c| c.to_string()).unwrap_or_else(|| format!("0x{vk:02X}")),
    }
}

// ---------------------------------------------------------------------------
// Effects thread state

struct Ripple {
    x: f32,
    y: f32,
    t0: Instant,
    rgb: [u8; 3],
}

#[derive(Clone, PartialEq)]
enum BadgeKind {
    Keys(Vec<String>),
    Typed(String),
    Status(String),
}

struct Badge {
    kind: BadgeKind,
    t0: Instant,
    shown: Instant,
    pressed: Instant,
    count: u32,
}

struct Mask {
    w: i32,
    h: i32,
    px: Vec<u8>,
}

enum Item {
    Cap { x: f32, w: f32, m: Rc<Mask> },
    Plus { x: f32, m: Rc<Mask> },
    Count { x: f32, w: f32, m: Rc<Mask> },
    Label { x: f32, m: Rc<Mask> },
}

struct BadgeDraw {
    w: f32,
    h: f32,
    items: Vec<Item>,
    alpha: f32,
    press: f32,
}

struct State {
    spot: HWND,
    fx: HWND,
    cfg: FxConfig,
    spot_on: bool,
    spot_r: f32,
    spot_key: (i32, i32, i32),
    vrect: (i32, i32, i32, i32),
    tutorial: bool,
    ripples: Vec<Ripple>,
    find_t0: Option<Instant>,
    badge: Option<Badge>,
    timer: bool,
    fx_shown: bool,
    masks: HashMap<(String, i32), Rc<Mask>>,
}

thread_local! {
    static ST: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `f` on the state unless it is already borrowed (re-entrant callbacks just skip).
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    ST.with(|c| match c.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

unsafe extern "system" fn plain_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn fx_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_FX => {
                with(|s| s.on_cmd(wp));
                0
            }
            WM_FX_INPUT => {
                let evs: Vec<InputEv> = INPUT.lock().unwrap().drain(..).collect();
                with(|s| {
                    for ev in evs {
                        s.on_input(ev);
                    }
                    s.ensure_timer();
                });
                0
            }
            WM_TIMER => {
                with(|s| s.tick());
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn fx_thread() {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let spot_cls = wide("PresenterToolboxSpot");
        let fx_cls = wide("PresenterToolboxFx");
        let title = wide("Presenter Toolbox efectos");

        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(plain_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = spot_cls.as_ptr();
        wc.hbrBackground = GetStockObject(BLACK_BRUSH) as HBRUSH;
        RegisterClassW(&wc);

        let mut wc2: WNDCLASSW = zeroed();
        wc2.lpfnWndProc = Some(fx_proc);
        wc2.hInstance = hinst;
        wc2.lpszClassName = fx_cls.as_ptr();
        RegisterClassW(&wc2);

        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let (vx, vy, vw, vh) = virtual_screen();
        let spot = CreateWindowExW(
            ex,
            spot_cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        SetLayeredWindowAttributes(spot, 0, 150, LWA_ALPHA);
        let fx = CreateWindowExW(
            ex,
            fx_cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let cfg = config();
        ST.with(|c| {
            *c.borrow_mut() = Some(State {
                spot,
                fx,
                spot_r: cfg.spot_radius,
                cfg,
                spot_on: false,
                spot_key: (i32::MIN, 0, 0),
                vrect: (vx, vy, vw, vh),
                tutorial: false,
                ripples: Vec::new(),
                find_t0: None,
                badge: None,
                timer: false,
                fx_shown: false,
                masks: HashMap::new(),
            })
        });
        FX_HWND.store(fx as isize, Ordering::SeqCst);
        with(|s| s.apply_hooks());

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl State {
    fn on_cmd(&mut self, cmd: usize) {
        match cmd {
            1 => {
                let on = !self.spot_on;
                self.set_spot(on);
                self.status(if on { "Foco activado  ·  Ctrl+Mayús+rueda: tamaño" } else { "Foco desactivado" });
            }
            2 => {
                self.tutorial = !self.tutorial;
                TUTORIAL_ON.store(self.tutorial, Ordering::SeqCst);
                self.status(if self.tutorial { "Clics y teclas: activado" } else { "Clics y teclas: desactivado" });
            }
            3 => self.find_t0 = Some(Instant::now()),
            _ => {
                self.cfg = config();
                if self.spot_on {
                    self.spot_r = self.cfg.spot_radius;
                    self.set_spot(true);
                }
            }
        }
        self.apply_hooks();
        self.ensure_timer();
    }

    fn status(&mut self, text: &str) {
        let now = Instant::now();
        self.badge = Some(Badge { kind: BadgeKind::Status(text.into()), t0: now, shown: now, pressed: now, count: 1 });
    }

    fn apply_hooks(&mut self) {
        CLICKS_ON.store(self.tutorial && self.cfg.show_clicks, Ordering::SeqCst);
        KEYS_ON.store(self.tutorial && self.cfg.show_keys, Ordering::SeqCst);
        SHAKE_ON.store(self.cfg.find_on_shake, Ordering::SeqCst);
        refresh_hooks();
    }

    fn ensure_timer(&mut self) {
        let active = self.spot_on || !self.ripples.is_empty() || self.find_t0.is_some() || self.badge.is_some();
        unsafe {
            if active && !self.timer {
                SetTimer(self.fx, TIMER_ID, 15, None);
                self.timer = true;
            } else if !active && self.timer {
                KillTimer(self.fx, TIMER_ID);
                self.timer = false;
                self.hide_fx();
            }
        }
    }

    fn set_spot(&mut self, on: bool) {
        self.spot_on = on;
        SPOT_ON.store(on, Ordering::SeqCst);
        unsafe {
            if on {
                let (vx, vy, vw, vh) = virtual_screen();
                self.vrect = (vx, vy, vw, vh);
                let alpha = (self.cfg.spot_dim.clamp(0.05, 0.95) * 255.0) as u8;
                SetLayeredWindowAttributes(self.spot, 0, alpha, LWA_ALPHA);
                SetWindowPos(self.spot, HWND_TOPMOST, vx, vy, vw, vh, SWP_NOACTIVATE);
                self.spot_key = (i32::MIN, 0, 0);
                self.update_spot();
                ShowWindow(self.spot, SW_SHOWNOACTIVATE);
                SetWindowPos(self.spot, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            } else {
                ShowWindow(self.spot, SW_HIDE);
            }
        }
    }

    /// Cuts the hole around the cursor (only when the cursor or size changed).
    fn update_spot(&mut self) {
        if !self.spot_on {
            return;
        }
        let (x, y) = cursor_pos();
        let r = self.spot_r.round() as i32;
        if self.spot_key == (x, y, r) {
            return;
        }
        self.spot_key = (x, y, r);
        let (vx, vy, vw, vh) = self.vrect;
        let (lx, ly) = (x - vx, y - vy);
        unsafe {
            let full = CreateRectRgn(0, 0, vw, vh);
            let hole = CreateEllipticRgn(lx - r, ly - r, lx + r + 1, ly + r + 1);
            CombineRgn(full, full, hole, RGN_DIFF);
            DeleteObject(hole);
            if SetWindowRgn(self.spot, full, 1) == 0 {
                DeleteObject(full);
            }
        }
    }

    fn on_input(&mut self, ev: InputEv) {
        let now = Instant::now();
        match ev {
            InputEv::Click { x, y, btn } => {
                if !(self.tutorial && self.cfg.show_clicks) {
                    return;
                }
                let rgb = match btn {
                    0 => [255, 196, 0],
                    1 => [255, 70, 60],
                    _ => [70, 160, 255],
                };
                self.ripples.push(Ripple { x: x as f32, y: y as f32, t0: now, rgb });
                if self.ripples.len() > 24 {
                    self.ripples.remove(0);
                }
            }
            InputEv::Find => {
                if self.cfg.find_on_shake {
                    self.find_t0 = Some(now);
                }
            }
            InputEv::Wheel(d) => {
                if self.spot_on {
                    self.spot_r = (self.spot_r * 1.12f32.powf(d)).clamp(40.0, 900.0);
                    self.update_spot();
                }
            }
            InputEv::Key { vk, ctrl, alt, shift, win } => {
                if self.tutorial && self.cfg.show_keys {
                    self.on_key(vk, ctrl, alt, shift, win, now);
                }
            }
        }
    }

    fn on_key(&mut self, vk: u32, ctrl: bool, alt: bool, shift: bool, winkey: bool, now: Instant) {
        let plain = !ctrl && !alt && !winkey;
        let recent = |b: &Badge| now - b.t0 < Duration::from_millis(1600);

        if plain && is_printable(vk) {
            if self.cfg.keys_only_combos {
                return; // never show what is being typed (passwords!)
            }
            let ch = match vk {
                0x20 => ' ',
                0x60..=0x69 => char::from_digit(vk - 0x60, 10).unwrap_or('?'),
                0x41..=0x5A if !shift => (vk as u8 as char).to_ascii_lowercase(),
                _ => key_char(vk).unwrap_or('?'),
            };
            if let Some(b) = &mut self.badge {
                let fresh = recent(b);
                if let BadgeKind::Typed(t) = &mut b.kind {
                    if fresh {
                        t.push(ch);
                        let n = t.chars().count();
                        if n > 32 {
                            *t = t.chars().skip(n - 32).collect();
                        }
                        b.t0 = now;
                        b.pressed = now;
                        return;
                    }
                }
            }
            self.badge =
                Some(Badge { kind: BadgeKind::Typed(ch.to_string()), t0: now, shown: now, pressed: now, count: 1 });
            return;
        }
        if plain && vk == 0x08 {
            if let Some(b) = &mut self.badge {
                if let BadgeKind::Typed(t) = &mut b.kind {
                    if now - b.t0 < Duration::from_millis(1600) {
                        t.pop();
                        b.t0 = now;
                        return;
                    }
                }
            }
        }
        let mut parts: Vec<String> = Vec::new();
        if ctrl {
            parts.push("Ctrl".into());
        }
        if alt {
            parts.push("Alt".into());
        }
        if shift {
            parts.push("Mayús".into());
        }
        if winkey {
            parts.push("Win".into());
        }
        parts.push(key_name(vk));
        let kind = BadgeKind::Keys(parts);
        if let Some(b) = &mut self.badge {
            if b.kind == kind && recent(b) {
                b.count += 1;
                b.t0 = now;
                b.pressed = now;
                return;
            }
        }
        self.badge = Some(Badge { kind, t0: now, shown: now, pressed: now, count: 1 });
    }

    fn tick(&mut self) {
        self.update_spot();
        self.render();
        self.ensure_timer();
    }

    fn hide_fx(&mut self) {
        if self.fx_shown {
            unsafe {
                ShowWindow(self.fx, SW_HIDE);
            }
            self.fx_shown = false;
        }
    }

    fn text(&mut self, s: &str, size: i32) -> Rc<Mask> {
        let key = (s.to_string(), size);
        if let Some(m) = self.masks.get(&key) {
            return m.clone();
        }
        if self.masks.len() > 128 {
            self.masks.clear();
        }
        let (w, h, px) = unsafe { text_mask(s, size) };
        let m = Rc::new(Mask { w, h, px });
        self.masks.insert(key, m.clone());
        m
    }

    fn layout_badge(&mut self, now: Instant) -> Option<BadgeDraw> {
        let b = self.badge.as_ref()?;
        let (kind, count) = (b.kind.clone(), b.count);
        let age = (now - b.t0).as_secs_f32();
        let shown = (now - b.shown).as_secs_f32();
        let pressed = (now - b.pressed).as_secs_f32();
        let fade_in = (shown / 0.08).min(1.0);
        let fade_out = if age > BADGE_SECS - 0.4 { ((BADGE_SECS - age) / 0.4).clamp(0.0, 1.0) } else { 1.0 };
        let press = if pressed < 0.14 { 3.5 * (1.0 - pressed / 0.14) } else { 0.0 };
        let pad = 14.0;
        let mut items = Vec::new();
        let mut x = pad;
        let h = match kind {
            BadgeKind::Keys(parts) => {
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        let m = self.text("+", 26);
                        items.push(Item::Plus { x: x + 8.0, m: m.clone() });
                        x += m.w as f32 + 16.0;
                    }
                    let m = self.text(p, 22);
                    let w = (m.w as f32 + 30.0).max(CAP_H);
                    items.push(Item::Cap { x, w, m });
                    x += w;
                }
                if count > 1 {
                    let m = self.text(&format!("×{count}"), 20);
                    let w = m.w as f32 + 20.0;
                    x += 12.0;
                    items.push(Item::Count { x, w, m });
                    x += w;
                }
                CAP_H + 5.0 + pad * 2.0
            }
            BadgeKind::Typed(t) => {
                let m = self.text(if t.is_empty() { " " } else { &t }, 28);
                let hh = m.h as f32 + pad * 2.0;
                x += 4.0;
                items.push(Item::Label { x, m: m.clone() });
                x += m.w as f32 + 4.0;
                hh
            }
            BadgeKind::Status(t) => {
                let m = self.text(&t, 20);
                let hh = m.h as f32 + pad * 2.0;
                x += 4.0;
                items.push(Item::Label { x, m: m.clone() });
                x += m.w as f32 + 4.0;
                hh
            }
        };
        Some(BadgeDraw { w: x + pad, h, items, alpha: fade_in * fade_out, press })
    }

    fn render(&mut self) {
        let now = Instant::now();
        let age = |t0: Instant| (now - t0).as_secs_f32();
        self.ripples.retain(|r| age(r.t0) < RIPPLE_SECS);
        if self.find_t0.is_some_and(|t| age(t) > FIND_SECS) {
            self.find_t0 = None;
        }
        if self.badge.as_ref().is_some_and(|b| age(b.t0) > BADGE_SECS) {
            self.badge = None;
        }

        let (cx, cy) = cursor_pos();
        let badge = self.layout_badge(now);
        let mut bb: Option<(i32, i32, i32, i32)> = None;
        let mut add = |x0: i32, y0: i32, x1: i32, y1: i32| {
            bb = Some(match bb {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        };
        for r in &self.ripples {
            let (x, y) = (r.x as i32, r.y as i32);
            add(x - 52, y - 52, x + 52, y + 52);
        }
        if self.find_t0.is_some() {
            add(cx - 250, cy - 250, cx + 250, cy + 250);
        }
        if self.spot_on {
            let r = self.spot_r as i32 + 6;
            add(cx - r, cy - r, cx + r, cy + r);
        }
        let badge_pos = badge.as_ref().map(|bd| {
            let mon = monitor_at_cursor();
            let bx = mon.x + (mon.w - bd.w as i32) / 2;
            let by = mon.y + mon.h - bd.h as i32 - 90;
            (bx, by)
        });
        if let (Some(bd), Some((bx, by))) = (&badge, badge_pos) {
            add(bx, by, bx + bd.w.ceil() as i32 + 1, by + bd.h.ceil() as i32 + 1);
        }
        let Some((x0, y0, x1, y1)) = bb else {
            self.hide_fx();
            return;
        };
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0 || h <= 0 || (w as i64) * (h as i64) > 5000 * 5000 {
            return;
        }

        unsafe {
            let screen = GetDC(null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                DeleteDC(mem);
                ReleaseDC(null_mut(), screen);
                return;
            }
            let old = SelectObject(mem, dib);
            let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            px.fill(0);
            let mut cv = Canvas { w, h, px };
            let (ox, oy) = (x0 as f32, y0 as f32);

            if self.spot_on {
                cv.ring(cx as f32 - ox, cy as f32 - oy, self.spot_r, 2.0, [255, 255, 255], 0.55);
            }
            for r in &self.ripples {
                let t = (age(r.t0) / RIPPLE_SECS).min(1.0);
                let e = 1.0 - (1.0 - t) * (1.0 - t);
                let rad = 8.0 + 36.0 * e;
                let a = 1.0 - t;
                let (x, y) = (r.x - ox, r.y - oy);
                cv.disc(x, y, 7.0 * (1.0 - t) + 2.0, r.rgb, 0.55 * a);
                cv.ring(x, y, rad + 2.0, 1.5, [0, 0, 0], 0.35 * a);
                cv.ring(x, y, rad, 4.0 * (1.0 - t) + 1.5, r.rgb, a);
            }
            if let Some(t0) = self.find_t0 {
                let t = (age(t0) / FIND_SECS).min(1.0);
                let (rad, a) = if t < 0.7 {
                    let p = t / 0.7;
                    (230.0 - 205.0 * (1.0 - (1.0 - p).powi(3)), 1.0)
                } else {
                    let p = (t - 0.7) / 0.3;
                    (25.0 + 12.0 * p, 1.0 - p)
                };
                let (x, y) = (cx as f32 - ox, cy as f32 - oy);
                cv.ring(x, y, rad, 9.0, [0, 0, 0], 0.45 * a);
                cv.ring(x, y, rad, 5.0, [0, 210, 255], a);
            }
            if let (Some(bd), Some((bx, by))) = (&badge, badge_pos) {
                draw_badge(&mut cv, bd, (bx - x0) as f32, (by - y0) as f32);
            }

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let dst = POINT { x: x0, y: y0 };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(self.fx, screen, &dst, &size, mem, &src, 0, &blend, ULW_ALPHA);
            SelectObject(mem, old);
            DeleteObject(dib);
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
            if !self.fx_shown {
                SetWindowPos(
                    self.fx,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                self.fx_shown = true;
            }
        }
    }
}

/// Keys drawn as real keycaps: a darker side, a light face with a top highlight,
/// and a little "press" when the combination is hit (again).
fn draw_badge(cv: &mut Canvas, bd: &BadgeDraw, ox: f32, oy: f32) {
    let a = bd.alpha;
    let pad = 14.0;
    cv.rrect(ox, oy, ox + bd.w, oy + bd.h, 16.0, [18, 18, 24], 0.62 * a);
    let top = oy + pad;
    for it in &bd.items {
        match it {
            Item::Cap { x, w, m } => {
                let (x, w, p) = (ox + x, *w, bd.press);
                // soft shadow, side, rim, face, highlight
                cv.rrect(x - 1.0, top + 3.0, x + w + 1.0, top + CAP_H + 7.0, 11.0, [0, 0, 0], 0.30 * a);
                cv.rrect(x, top + 4.0, x + w, top + CAP_H + 4.0, 10.0, [112, 114, 128], a);
                cv.rrect(x, top + p, x + w, top + CAP_H + p, 10.0, [176, 178, 192], a);
                cv.rrect(x + 1.5, top + p + 1.5, x + w - 1.5, top + CAP_H + p - 2.5, 9.0, [232, 233, 240], a);
                cv.rrect(x + 4.0, top + p + 3.0, x + w - 4.0, top + p + CAP_H * 0.52, 7.0, [251, 251, 254], 0.9 * a);
                let tx = (x + (w - m.w as f32) / 2.0).round() as i32;
                let ty = (top + p + (CAP_H - m.h as f32) / 2.0 - 1.0).round() as i32;
                cv.mask(tx, ty, m, [38, 40, 52], a);
            }
            Item::Plus { x, m } => {
                let ty = (top + (CAP_H - m.h as f32) / 2.0).round() as i32;
                cv.mask((ox + x).round() as i32, ty, m, [235, 236, 242], a);
            }
            Item::Count { x, w, m } => {
                let (x, w) = (ox + x, *w);
                let (y0, y1) = (top + CAP_H / 2.0 - 15.0, top + CAP_H / 2.0 + 15.0);
                cv.rrect(x, y0, x + w, y1, 15.0, [255, 140, 0], a);
                let tx = (x + (w - m.w as f32) / 2.0).round() as i32;
                let ty = (y0 + (30.0 - m.h as f32) / 2.0).round() as i32;
                cv.mask(tx, ty, m, [255, 255, 255], a);
            }
            Item::Label { x, m } => {
                cv.mask((ox + x).round() as i32, top.round() as i32, m, [255, 255, 255], a);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tiny anti-aliased software renderer into a premultiplied BGRA buffer

struct Canvas<'a> {
    w: i32,
    h: i32,
    px: &'a mut [u8],
}

impl Canvas<'_> {
    fn blend(&mut self, x: i32, y: i32, rgb: [u8; 3], a: f32) {
        if a <= 0.003 || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = a.min(1.0);
        let inv = 1.0 - a;
        let i = ((y * self.w + x) * 4) as usize;
        let p = &mut self.px[i..i + 4];
        p[0] = (rgb[2] as f32 * a + p[0] as f32 * inv) as u8;
        p[1] = (rgb[1] as f32 * a + p[1] as f32 * inv) as u8;
        p[2] = (rgb[0] as f32 * a + p[2] as f32 * inv) as u8;
        p[3] = (255.0 * a + p[3] as f32 * inv) as u8;
    }

    fn bounds(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (i32, i32, i32, i32) {
        (
            (x0.floor() as i32).max(0),
            (y0.floor() as i32).max(0),
            (x1.ceil() as i32).min(self.w - 1),
            (y1.ceil() as i32).min(self.h - 1),
        )
    }

    fn ring(&mut self, cx: f32, cy: f32, r: f32, th: f32, rgb: [u8; 3], a: f32) {
        let e = r + th + 1.0;
        let (x0, y0, x1, y1) = self.bounds(cx - e, cy - e, cx + e, cy + e);
        let inner = (r - th - 1.0).max(0.0);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dist = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                if dist < inner {
                    continue;
                }
                let cov = (th * 0.5 + 0.5 - (dist - r).abs()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, rgb, a * cov);
                }
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32, rgb: [u8; 3], a: f32) {
        let e = r + 1.0;
        let (x0, y0, x1, y1) = self.bounds(cx - e, cy - e, cx + e, cy + e);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, rgb, a * cov);
                }
            }
        }
    }

    fn rrect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, rad: f32, rgb: [u8; 3], a: f32) {
        let rad = rad.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let (hx, hy) = ((x1 - x0) / 2.0 - rad, (y1 - y0) / 2.0 - rad);
        let (bx0, by0, bx1, by1) = self.bounds(x0, y0, x1, y1);
        for y in by0..=by1 {
            for x in bx0..=bx1 {
                let qx = (x as f32 + 0.5 - cx).abs() - hx;
                let qy = (y as f32 + 0.5 - cy).abs() - hy;
                let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, rgb, a * cov);
                }
            }
        }
    }

    fn mask(&mut self, ox: i32, oy: i32, m: &Mask, rgb: [u8; 3], a: f32) {
        for y in 0..m.h {
            for x in 0..m.w {
                let v = m.px[(y * m.w + x) as usize];
                if v > 0 {
                    self.blend(ox + x, oy + y, rgb, a * v as f32 / 255.0);
                }
            }
        }
    }
}

/// Renders text with GDI (grey-scale anti-aliasing) and returns it as a coverage mask.
unsafe fn text_mask(text: &str, size: i32) -> (i32, i32, Vec<u8>) {
    unsafe {
        let w16: Vec<u16> = text.encode_utf16().collect();
        let face = wide("Segoe UI Semibold");
        let dc = CreateCompatibleDC(null_mut());
        let font = CreateFontW(-size, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 4, 0, face.as_ptr());
        let old_font = SelectObject(dc, font);
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, w16.as_ptr(), w16.len() as i32, &mut sz);
        let (w, h) = (sz.cx.max(1) + 4, sz.cy.max(1));
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            SelectObject(dc, old_font);
            DeleteObject(font);
            DeleteDC(dc);
            return (1, 1, vec![0]);
        }
        let old_bmp = SelectObject(dc, dib);
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        px.fill(0);
        SetTextColor(dc, 0x00FF_FFFF);
        SetBkMode(dc, 1);
        TextOutW(dc, 2, 0, w16.as_ptr(), w16.len() as i32);
        GdiFlush();
        let mask: Vec<u8> = px.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])).collect();
        SelectObject(dc, old_bmp);
        DeleteObject(dib);
        SelectObject(dc, old_font);
        DeleteObject(font);
        DeleteDC(dc);
        (w, h, mask)
    }
}

// ---------------------------------------------------------------------------
// Self-test: renders sample keycaps / ripples to a PNG without touching the desktop.

/// Keycaps badge built outside the live effects thread (for sample images).
fn sample_badge(parts: &[&str], count: u32) -> BadgeDraw {
    let text = |s: &str, size: i32| {
        let (w, h, px) = unsafe { text_mask(s, size) };
        Rc::new(Mask { w, h, px })
    };
    let pad = 14.0;
    let mut items = Vec::new();
    let mut x = pad;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            let m = text("+", 26);
            items.push(Item::Plus { x: x + 8.0, m: m.clone() });
            x += m.w as f32 + 16.0;
        }
        let m = text(p, 22);
        let cw = (m.w as f32 + 30.0).max(CAP_H);
        items.push(Item::Cap { x, w: cw, m });
        x += cw;
    }
    if count > 1 {
        let m = text(&format!("×{count}"), 20);
        let cw = m.w as f32 + 20.0;
        x += 12.0;
        items.push(Item::Count { x, w: cw, m });
        x += cw;
    }
    BadgeDraw { w: x + pad, h: CAP_H + 5.0 + pad * 2.0, items, alpha: 1.0, press: 0.0 }
}

/// Store screenshot: a made-up desktop with the spotlight, click ripples and keycaps on top.
pub fn render_showcase(path: &std::path::Path, desk: &eframe::egui::ColorImage) -> bool {
    let (w, h) = (desk.size[0] as i32, desk.size[1] as i32);
    let (sx, sy, sr) = (w as f32 * 0.50, h as f32 * 0.49, h as f32 * 0.21);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for (i, p) in desk.pixels.iter().enumerate() {
        let x = (i as i32 % w) as f32 + 0.5;
        let y = (i as i32 / w) as f32 + 0.5;
        // spotlight: dim everything outside the circle (soft 1.5 px edge)
        let d = (x - sx).hypot(y - sy);
        let inside = (sr + 0.75 - d).clamp(0.0, 1.5) / 1.5;
        let k = 0.38 + 0.62 * inside;
        let [r, g, b, _] = p.to_array();
        px.extend_from_slice(&[(b as f32 * k) as u8, (g as f32 * k) as u8, (r as f32 * k) as u8, 255]);
    }
    let mut cv = Canvas { w, h, px: &mut px };
    cv.ring(sx, sy, sr, 2.0, [255, 255, 255], 0.55);
    // a left click in progress and a right click fading out
    let (cx, cy) = (sx + sr * 0.35, sy + sr * 0.15);
    cv.disc(cx, cy, 6.0, [255, 196, 0], 0.55);
    cv.ring(cx, cy, 30.0, 1.5, [0, 0, 0], 0.35);
    cv.ring(cx, cy, 28.0, 3.5, [255, 196, 0], 1.0);
    let (rx, ry) = (sx - sr * 0.45, sy - sr * 0.3);
    cv.ring(rx, ry, 40.0, 1.5, [0, 0, 0], 0.2);
    cv.ring(rx, ry, 38.0, 2.0, [255, 70, 60], 0.55);
    let bd = sample_badge(&["Ctrl", "Mayús", "S"], 2);
    let bx = (w as f32 - bd.w) / 2.0;
    let by = h as f32 - bd.h - 90.0;
    draw_badge(&mut cv, &bd, bx, by);
    let mut rgba = Vec::with_capacity(px.len());
    for p in px.chunks_exact(4) {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    image::save_buffer(path, &rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8).is_ok()
}

pub fn render_sample(path: &std::path::Path) -> bool {
    let (w, h) = (900i32, 260i32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    // a mid-grey "desktop" so the semi-transparent parts are visible in the PNG
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[96, 92, 88, 255]);
    }
    let mut masks: HashMap<(String, i32), Rc<Mask>> = HashMap::new();
    let mut text = |s: &str, size: i32| {
        masks
            .entry((s.to_string(), size))
            .or_insert_with(|| {
                let (w, h, px) = unsafe { text_mask(s, size) };
                Rc::new(Mask { w, h, px })
            })
            .clone()
    };
    let mut items = Vec::new();
    let pad = 14.0;
    let mut x = pad;
    for (i, p) in ["Ctrl", "Mayús", "P"].iter().enumerate() {
        if i > 0 {
            let m = text("+", 26);
            items.push(Item::Plus { x: x + 8.0, m: m.clone() });
            x += m.w as f32 + 16.0;
        }
        let m = text(p, 22);
        let cw = (m.w as f32 + 30.0).max(CAP_H);
        items.push(Item::Cap { x, w: cw, m });
        x += cw;
    }
    let m = text("×3", 20);
    let cw = m.w as f32 + 20.0;
    x += 12.0;
    items.push(Item::Count { x, w: cw, m });
    x += cw;
    let bd = BadgeDraw { w: x + pad, h: CAP_H + 5.0 + pad * 2.0, items, alpha: 1.0, press: 0.0 };
    let mut cv = Canvas { w, h, px: &mut px };
    draw_badge(&mut cv, &bd, 40.0, 150.0);
    cv.disc(120.0, 70.0, 5.0, [255, 196, 0], 0.5);
    cv.ring(120.0, 70.0, 30.0, 1.5, [0, 0, 0], 0.35);
    cv.ring(120.0, 70.0, 28.0, 3.0, [255, 196, 0], 1.0);
    cv.ring(260.0, 70.0, 30.0, 9.0, [0, 0, 0], 0.45);
    cv.ring(260.0, 70.0, 30.0, 5.0, [0, 210, 255], 1.0);
    // BGRA premultiplied over opaque grey == plain RGB now
    let mut rgba = Vec::with_capacity(px.len());
    for p in px.chunks_exact(4) {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    image::save_buffer(path, &rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8).is_ok()
}
