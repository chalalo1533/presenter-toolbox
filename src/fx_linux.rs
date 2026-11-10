//! Linux (X11) counterpart of `fx.rs`: spotlight, click ripples, keystrokes drawn as keycaps
//! and "find my mouse", all drawn over the desktop while you keep working.
//!
//! One thread owns the state and three click-through windows the window manager ignores:
//!  * `spot`:   a dark see-through window with a round hole cut out around the cursor;
//!  * `cursor`: ripples, the find-mouse ring and the spotlight's edge;
//!  * `badge`:  the keys / status label at the bottom of the screen.
//! Mouse and keyboard activity arrives from the X event thread in `linux.rs` (XInput2 raw
//! events, selected only while a feature needs them). See-through windows need a compositing
//! window manager; without one the ripples and keys still show, with hard edges.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use crate::soft::{Canvas, Mask, TextCache};
use crate::win::{self, X, cursor_pos};

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

#[derive(Clone, Copy, Debug)]
pub enum InputEv {
    Click { x: i32, y: i32, btn: u8 },
    Key { ks0: u32, ks1: u32, ctrl: bool, alt: bool, shift: bool, win: bool },
    Wheel(f32),
    Motion(i32),
}

enum FxMsg {
    Cmd(usize),
    Input(InputEv),
    Status(String),
    ClearStatus,
}

const RIPPLE_SECS: f32 = 0.5;
const FIND_SECS: f32 = 1.1;
const BADGE_SECS: f32 = 2.0;
const CAP_H: f32 = 50.0;
const FRAME: Duration = Duration::from_millis(15);

static TX: OnceLock<mpsc::Sender<FxMsg>> = OnceLock::new();
static CONFIG: Mutex<Option<FxConfig>> = Mutex::new(None);
pub static SPOT_ON: AtomicBool = AtomicBool::new(false);
pub static TUTORIAL_ON: AtomicBool = AtomicBool::new(false);
static STATUS_SHOWN: AtomicBool = AtomicBool::new(false);

pub fn start(cfg: FxConfig) {
    *CONFIG.lock().unwrap() = Some(cfg);
    let (tx, rx) = mpsc::channel();
    if TX.set(tx).is_ok() {
        std::thread::spawn(move || fx_thread(rx));
    }
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
    if let Some(tx) = TX.get() {
        let _ = tx.send(FxMsg::Cmd(cmd as usize));
    }
}

/// Mouse / keyboard activity seen by the X event thread.
pub fn input(ev: InputEv) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(FxMsg::Input(ev));
    }
}

/// Shows a short text label at the bottom of the screen.
pub fn status(text: &str) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(FxMsg::Status(text.to_string()));
    }
}

/// Takes a status label off the screen right away (before a screen capture, so the label
/// does not end up in the picture). Returns true if there was one to remove.
pub fn clear_status() -> bool {
    let shown = STATUS_SHOWN.swap(false, Ordering::SeqCst);
    if shown {
        if let Some(tx) = TX.get() {
            let _ = tx.send(FxMsg::ClearStatus);
        }
    }
    shown
}

fn config() -> FxConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Keys (X11 keysyms)

fn is_modifier(ks: u32) -> bool {
    matches!(ks, 0xFFE1..=0xFFEE | 0xFE01..=0xFE13 | 0xFF7E | 0xFF7F | 0xFF14 | 0)
}

/// Keys that type something (letters, digits, punctuation, space).
fn is_printable(ks: u32) -> bool {
    matches!(ks, 0x20..=0x7E | 0xA0..=0xFF | 0xFFB0..=0xFFB9 | 0x0100_0000..=0x0110_FFFF)
}

fn key_char(ks: u32) -> Option<char> {
    match ks {
        0x20..=0x7E | 0xA0..=0xFF => char::from_u32(ks),
        0xFFB0..=0xFFB9 => char::from_digit(ks - 0xFFB0, 10),
        0x0100_0000..=0x0110_FFFF => char::from_u32(ks - 0x0100_0000),
        _ => None,
    }
}

fn key_name(ks: u32) -> String {
    match ks {
        0xFF08 => "Retroceso".into(),
        0xFF09 | 0xFE20 => "Tab".into(),
        0xFF0D | 0xFF8D => "Enter".into(),
        0xFF13 => "Pausa".into(),
        0xFF1B => "Esc".into(),
        0x20 => "Espacio".into(),
        0xFF61 => "ImprPant".into(),
        0xFF67 => "Menú".into(),
        0xFF63 => "Insert".into(),
        0xFFFF => "Supr".into(),
        0xFF50 => "Inicio".into(),
        0xFF57 => "Fin".into(),
        0xFF55 => "RePág".into(),
        0xFF56 => "AvPág".into(),
        0xFF51 => "←".into(),
        0xFF52 => "↑".into(),
        0xFF53 => "→".into(),
        0xFF54 => "↓".into(),
        0xFFBE..=0xFFD5 => format!("F{}", ks - 0xFFBD),
        0xFFB0..=0xFFB9 => format!("Num {}", ks - 0xFFB0),
        0xFFAA => "Num *".into(),
        0xFFAB => "Num +".into(),
        0xFFAD => "Num -".into(),
        0xFFAF => "Num /".into(),
        _ => match key_char(ks) {
            Some(c) => c.to_uppercase().collect(),
            None => format!("0x{ks:X}"),
        },
    }
}

// ---------------------------------------------------------------------------
// State

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

#[derive(Default)]
struct Shake {
    last_x: i32,
    dir: i32,
    travel: i32,
    revs: VecDeque<Instant>,
    last: Option<Instant>,
}

/// One click-through window whose picture is replaced as a whole every frame.
struct Layer {
    win: Window,
    gc: Gcontext,
    pix: Pixmap,
    size: (i32, i32),
    shown: bool,
    argb: bool,
}

impl Layer {
    fn new(x: &X, argb: bool) -> Layer {
        let win = x.popup_window(argb, -10, -10, 1, 1, 0, EventMask::NO_EVENT);
        x.click_through(win);
        let _ = x.conn.change_property32(
            PropMode::REPLACE,
            win,
            x.atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[x.atoms._NET_WM_WINDOW_TYPE_DND],
        );
        let gc = x.conn.generate_id().unwrap_or(0);
        let _ = x.conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0));
        Layer { win, gc, pix: 0, size: (0, 0), shown: false, argb }
    }

    /// Shows `buf` (premultiplied BGRA, w×h) at screen position (sx, sy).
    fn present(&mut self, x: &X, sx: i32, sy: i32, w: i32, h: i32, buf: &mut [u8]) {
        if w <= 0 || h <= 0 || self.win == 0 {
            return;
        }
        // the compositor may have been switched on or off since the window was made
        if !self.shown && self.argb != x.composited() {
            let _ = x.conn.destroy_window(self.win);
            if self.pix != 0 {
                let _ = x.conn.free_pixmap(self.pix);
            }
            *self = Layer::new(x, !self.argb);
        }
        let depth = if self.argb { 32 } else { x.root_depth };
        if self.size != (w, h) {
            if self.pix != 0 {
                let _ = x.conn.free_pixmap(self.pix);
            }
            self.pix = x.conn.generate_id().unwrap_or(0);
            let _ = x.conn.create_pixmap(depth, self.pix, self.win, w as u16, h as u16);
            self.size = (w, h);
        }
        if !self.argb {
            // no compositor: cut the window to the shape of what is drawn, with solid colours
            let mut rects = Vec::new();
            for y in 0..h {
                let mut run: Option<i32> = None;
                for xx in 0..=w {
                    let on = xx < w && {
                        let p = &mut buf[((y * w + xx) * 4) as usize..((y * w + xx) * 4 + 4) as usize];
                        let a = p[3] as u32;
                        if a >= 96 {
                            for c in &mut p[..3] {
                                *c = (*c as u32 * 255 / a).min(255) as u8;
                            }
                        }
                        a >= 96
                    };
                    match (run, on) {
                        (None, true) => run = Some(xx),
                        (Some(s), false) => {
                            rects.push(Rectangle { x: s as i16, y: y as i16, width: (xx - s) as u16, height: 1 });
                            run = None;
                        }
                        _ => {}
                    }
                }
            }
            let _ = x.conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::YX_BANDED, self.win, 0, 0, &rects);
        }
        // draw off-screen, then swap the whole picture in at once: no half-drawn frames
        x.put_image(self.pix, self.gc, depth, w, h, buf);
        let _ = x.conn.change_window_attributes(self.win, &ChangeWindowAttributesAux::new().background_pixmap(self.pix));
        let _ = x.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new().x(sx).y(sy).width(w as u32).height(h as u32).stack_mode(StackMode::ABOVE),
        );
        let _ = x.conn.clear_area(false, self.win, 0, 0, 0, 0);
        if !self.shown {
            let _ = x.conn.map_window(self.win);
            self.shown = true;
        }
    }

    fn hide(&mut self, x: &X) {
        if self.shown {
            let _ = x.conn.unmap_window(self.win);
            self.shown = false;
        }
    }
}

struct State {
    x: &'static X,
    cfg: FxConfig,
    spot: Window,
    spot_on: bool,
    spot_r: f32,
    spot_key: (i32, i32, i32),
    spot_frames: u32,
    root: (i32, i32),
    tutorial: bool,
    ripples: Vec<Ripple>,
    find_t0: Option<Instant>,
    badge: Option<Badge>,
    shake: Shake,
    text: TextCache,
    cursor_layer: Layer,
    badge_layer: Layer,
    wanted: (bool, bool),
}

fn fx_thread(rx: mpsc::Receiver<FxMsg>) {
    let Some(x) = win::x() else {
        // no X server: swallow messages so senders never block or error
        for _ in rx {}
        return;
    };
    let argb = x.composited();
    let cfg = config();
    let mut st = State {
        x,
        spot_r: cfg.spot_radius,
        cfg,
        spot: 0,
        spot_on: false,
        spot_key: (i32::MIN, 0, 0),
        spot_frames: 0,
        root: x.root_size(),
        tutorial: false,
        ripples: Vec::new(),
        find_t0: None,
        badge: None,
        shake: Shake::default(),
        text: TextCache::default(),
        cursor_layer: Layer::new(x, argb),
        badge_layer: Layer::new(x, argb),
        wanted: (false, false),
    };
    st.apply_hooks();
    let _ = x.conn.flush();
    let mut last_frame = Instant::now() - FRAME;
    loop {
        let msg = if st.active() {
            let wait = FRAME.saturating_sub(last_frame.elapsed());
            match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };
        if let Some(m) = msg {
            st.on_msg(m);
            while let Ok(m) = rx.try_recv() {
                st.on_msg(m);
            }
        }
        if last_frame.elapsed() >= FRAME || !st.active() {
            last_frame = Instant::now();
            st.tick();
        }
    }
}

impl State {
    fn active(&self) -> bool {
        self.spot_on || !self.ripples.is_empty() || self.find_t0.is_some() || self.badge.is_some()
    }

    fn on_msg(&mut self, m: FxMsg) {
        match m {
            FxMsg::Cmd(c) => self.on_cmd(c),
            FxMsg::Input(ev) => self.on_input(ev),
            FxMsg::Status(t) => self.status(&t),
            FxMsg::ClearStatus => {
                if self.badge.as_ref().is_some_and(|b| matches!(b.kind, BadgeKind::Status(_))) {
                    self.badge = None;
                }
            }
        }
    }

    fn on_cmd(&mut self, cmd: usize) {
        match cmd {
            1 => {
                let on = !self.spot_on;
                if on && !self.x.composited() {
                    self.status("El foco necesita los efectos de escritorio (compositor) activados");
                } else {
                    self.set_spot(on);
                    self.status(if on { "Foco activado  ·  Ctrl+Mayús+rueda: tamaño" } else { "Foco desactivado" });
                }
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
    }

    fn status(&mut self, text: &str) {
        let now = Instant::now();
        self.badge = Some(Badge { kind: BadgeKind::Status(text.into()), t0: now, shown: now, pressed: now, count: 1 });
    }

    /// Tells the X event thread which input we need (nothing is watched while all is off).
    fn apply_hooks(&mut self) {
        let raw = (self.tutorial && (self.cfg.show_clicks || self.cfg.show_keys)) || self.cfg.find_on_shake;
        let wanted = (raw, self.spot_on);
        if wanted != self.wanted {
            self.wanted = wanted;
            win::want_input(raw, self.spot_on);
        }
    }

    fn set_spot(&mut self, on: bool) {
        let x = self.x;
        self.spot_on = on;
        SPOT_ON.store(on, Ordering::SeqCst);
        if on {
            self.root = x.root_size();
            let alpha = (self.cfg.spot_dim.clamp(0.05, 0.95) * 255.0) as u32;
            if self.spot == 0 {
                self.spot = x.popup_window(true, 0, 0, self.root.0, self.root.1, alpha << 24, EventMask::NO_EVENT);
                x.click_through(self.spot);
                let _ = x.conn.change_property32(
                    PropMode::REPLACE,
                    self.spot,
                    x.atoms._NET_WM_WINDOW_TYPE,
                    AtomEnum::ATOM,
                    &[x.atoms._NET_WM_WINDOW_TYPE_DND],
                );
            }
            // premultiplied black: only the alpha byte is set
            let _ = x.conn.change_window_attributes(self.spot, &ChangeWindowAttributesAux::new().background_pixel(alpha << 24));
            let _ = x.conn.configure_window(
                self.spot,
                &ConfigureWindowAux::new().x(0).y(0).width(self.root.0 as u32).height(self.root.1 as u32).stack_mode(StackMode::ABOVE),
            );
            self.spot_key = (i32::MIN, 0, 0);
            self.update_spot();
            let _ = x.conn.map_window(self.spot);
            let _ = x.conn.clear_area(false, self.spot, 0, 0, 0, 0);
        } else if self.spot != 0 {
            let _ = x.conn.unmap_window(self.spot);
        }
    }

    /// Cuts the hole around the cursor (only when the cursor or size changed).
    fn update_spot(&mut self) {
        if !self.spot_on || self.spot == 0 {
            return;
        }
        let x = self.x;
        // other windows may have been raised over the dark layer; put it back on top now and then
        self.spot_frames += 1;
        if self.spot_frames % 20 == 0 {
            let _ = x.conn.configure_window(self.spot, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
        let (cx, cy) = cursor_pos();
        let r = self.spot_r.round() as i32;
        if self.spot_key == (cx, cy, r) {
            return;
        }
        self.spot_key = (cx, cy, r);
        let (w, h) = self.root;
        let rect = |x0: i32, y0: i32, x1: i32, y1: i32| Rectangle {
            x: x0.clamp(0, w) as i16,
            y: y0.clamp(0, h) as i16,
            width: (x1.clamp(0, w) - x0.clamp(0, w)).max(0) as u16,
            height: (y1.clamp(0, h) - y0.clamp(0, h)).max(0) as u16,
        };
        let mut rects = Vec::with_capacity(2 * r as usize + 4);
        rects.push(rect(0, 0, w, cy - r));
        for y in (cy - r).max(0)..(cy + r).min(h) {
            let dy = y as f32 + 0.5 - cy as f32;
            let half = ((r * r) as f32 - dy * dy).max(0.0).sqrt().round() as i32;
            rects.push(rect(0, y, cx - half, y + 1));
            rects.push(rect(cx + half, y, w, y + 1));
        }
        rects.push(rect(0, cy + r, w, h));
        rects.retain(|r| r.width > 0 && r.height > 0);
        let _ = x.conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::YX_BANDED, self.spot, 0, 0, &rects);
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
                    2 => [255, 70, 60],
                    _ => [70, 160, 255],
                };
                self.ripples.push(Ripple { x: x as f32, y: y as f32, t0: now, rgb });
                if self.ripples.len() > 24 {
                    self.ripples.remove(0);
                }
            }
            InputEv::Motion(x) => {
                if self.cfg.find_on_shake && self.shake_detect(x, now) {
                    self.find_t0 = Some(now);
                }
            }
            InputEv::Wheel(d) => {
                if self.spot_on {
                    self.spot_r = (self.spot_r * 1.12f32.powf(d)).clamp(40.0, 900.0);
                }
            }
            InputEv::Key { ks0, ks1, ctrl, alt, shift, win } => {
                if self.tutorial && self.cfg.show_keys {
                    self.on_key(ks0, ks1, ctrl, alt, shift, win, now);
                }
            }
        }
    }

    /// A real "shake": at least 5 quick direction changes of 60+ px within 0.9 s.
    fn shake_detect(&mut self, x: i32, now: Instant) -> bool {
        let sh = &mut self.shake;
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
    }

    #[allow(clippy::too_many_arguments)]
    fn on_key(&mut self, ks0: u32, ks1: u32, ctrl: bool, alt: bool, shift: bool, winkey: bool, now: Instant) {
        if is_modifier(ks0) {
            return;
        }
        let plain = !ctrl && !alt && !winkey;
        let fresh = self.badge.as_ref().is_some_and(|b| now - b.t0 < Duration::from_millis(1600));

        if plain && is_printable(ks0) {
            if self.cfg.keys_only_combos {
                return; // never show what is being typed (passwords!)
            }
            let ch = key_char(if shift { ks1 } else { ks0 }).or_else(|| key_char(ks0)).unwrap_or('?');
            if let Some(b) = &mut self.badge {
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
            self.badge = Some(Badge { kind: BadgeKind::Typed(ch.to_string()), t0: now, shown: now, pressed: now, count: 1 });
            return;
        }
        if plain && ks0 == 0xFF08 && fresh {
            if let Some(b) = &mut self.badge {
                if let BadgeKind::Typed(t) = &mut b.kind {
                    t.pop();
                    b.t0 = now;
                    return;
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
            parts.push("Super".into());
        }
        parts.push(key_name(ks0));
        let kind = BadgeKind::Keys(parts);
        if let Some(b) = &mut self.badge {
            if b.kind == kind && fresh {
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
        let _ = self.x.conn.flush();
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
        Some(badge_layout(&mut self.text, &kind, count, fade_in * fade_out, press))
    }

    fn render(&mut self) {
        let x = self.x;
        let now = Instant::now();
        let age = |t0: Instant| (now - t0).as_secs_f32();
        self.ripples.retain(|r| age(r.t0) < RIPPLE_SECS);
        if self.find_t0.is_some_and(|t| age(t) > FIND_SECS) {
            self.find_t0 = None;
        }
        if self.badge.as_ref().is_some_and(|b| age(b.t0) > BADGE_SECS) {
            self.badge = None;
        }
        STATUS_SHOWN.store(self.badge.as_ref().is_some_and(|b| matches!(b.kind, BadgeKind::Status(_))), Ordering::SeqCst);
        let (cx, cy) = cursor_pos();

        // -- layer 1: everything that happens around the cursor
        let mut bb: Option<(i32, i32, i32, i32)> = None;
        let mut add = |x0: i32, y0: i32, x1: i32, y1: i32| {
            bb = Some(match bb {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        };
        for r in &self.ripples {
            let (rx, ry) = (r.x as i32, r.y as i32);
            add(rx - 52, ry - 52, rx + 52, ry + 52);
        }
        if self.find_t0.is_some() {
            add(cx - 250, cy - 250, cx + 250, cy + 250);
        }
        if self.spot_on {
            let r = self.spot_r as i32 + 6;
            add(cx - r, cy - r, cx + r, cy + r);
        }
        match bb {
            Some((x0, y0, x1, y1)) if x1 > x0 && y1 > y0 && (x1 - x0) as i64 * (y1 - y0) as i64 <= 4000 * 4000 => {
                let (w, h) = (x1 - x0, y1 - y0);
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let mut cv = Canvas { w, h, px: &mut buf };
                let (ox, oy) = (x0 as f32, y0 as f32);
                if self.spot_on {
                    cv.ring(cx as f32 - ox, cy as f32 - oy, self.spot_r, 2.0, [255, 255, 255], 0.55);
                }
                for r in &self.ripples {
                    let t = (age(r.t0) / RIPPLE_SECS).min(1.0);
                    draw_ripple(&mut cv, r.x - ox, r.y - oy, t, r.rgb);
                }
                if let Some(t0) = self.find_t0 {
                    let t = (age(t0) / FIND_SECS).min(1.0);
                    draw_find(&mut cv, cx as f32 - ox, cy as f32 - oy, t);
                }
                self.cursor_layer.present(x, x0, y0, w, h, &mut buf);
            }
            _ => self.cursor_layer.hide(x),
        }

        // -- layer 2: keys / status label at the bottom of the screen
        match self.layout_badge(now) {
            Some(bd) => {
                let mon = x.monitor_at(cx, cy);
                let (w, h) = (bd.w.ceil() as i32 + 1, bd.h.ceil() as i32 + 1);
                let bx = mon.x + (mon.w - w) / 2;
                let by = mon.y + mon.h - h - 90;
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let mut cv = Canvas { w, h, px: &mut buf };
                draw_badge(&mut cv, &bd, 0.0, 0.0);
                self.badge_layer.present(x, bx, by, w, h, &mut buf);
            }
            None => self.badge_layer.hide(x),
        }
    }
}

fn draw_ripple(cv: &mut Canvas, x: f32, y: f32, t: f32, rgb: [u8; 3]) {
    let e = 1.0 - (1.0 - t) * (1.0 - t);
    let rad = 8.0 + 36.0 * e;
    let a = 1.0 - t;
    cv.disc(x, y, 7.0 * (1.0 - t) + 2.0, rgb, 0.55 * a);
    cv.ring(x, y, rad + 2.0, 1.5, [0, 0, 0], 0.35 * a);
    cv.ring(x, y, rad, 4.0 * (1.0 - t) + 1.5, rgb, a);
}

fn draw_find(cv: &mut Canvas, x: f32, y: f32, t: f32) {
    let (rad, a) = if t < 0.7 {
        let p = t / 0.7;
        (230.0 - 205.0 * (1.0 - (1.0 - p).powi(3)), 1.0)
    } else {
        let p = (t - 0.7) / 0.3;
        (25.0 + 12.0 * p, 1.0 - p)
    };
    cv.ring(x, y, rad, 9.0, [0, 0, 0], 0.45 * a);
    cv.ring(x, y, rad, 5.0, [0, 210, 255], a);
}

fn badge_layout(text: &mut TextCache, kind: &BadgeKind, count: u32, alpha: f32, press: f32) -> BadgeDraw {
    let pad = 14.0;
    let mut items = Vec::new();
    let mut x = pad;
    let h = match kind {
        BadgeKind::Keys(parts) => {
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    let m = text.get("+", 26, true);
                    items.push(Item::Plus { x: x + 8.0, m: m.clone() });
                    x += m.w as f32 + 16.0;
                }
                let m = text.get(p, 22, true);
                let w = (m.w as f32 + 30.0).max(CAP_H);
                items.push(Item::Cap { x, w, m });
                x += w;
            }
            if count > 1 {
                let m = text.get(&format!("×{count}"), 20, true);
                let w = m.w as f32 + 20.0;
                x += 12.0;
                items.push(Item::Count { x, w, m });
                x += w;
            }
            CAP_H + 5.0 + pad * 2.0
        }
        BadgeKind::Typed(t) => {
            let m = text.get(if t.is_empty() { " " } else { t }, 28, true);
            let hh = m.h as f32 + pad * 2.0;
            x += 4.0;
            items.push(Item::Label { x, m: m.clone() });
            x += m.w as f32 + 4.0;
            hh
        }
        BadgeKind::Status(t) => {
            let m = text.get(t, 20, true);
            let hh = m.h as f32 + pad * 2.0;
            x += 4.0;
            items.push(Item::Label { x, m: m.clone() });
            x += m.w as f32 + 4.0;
            hh
        }
    };
    BadgeDraw { w: x + pad, h, items, alpha, press }
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
// Sample pictures (no X server needed): `--fx-sample` and `--showcase`

fn sample_badge(parts: &[&str], count: u32) -> BadgeDraw {
    let mut text = TextCache::default();
    let kind = BadgeKind::Keys(parts.iter().map(|s| s.to_string()).collect());
    badge_layout(&mut text, &kind, count, 1.0, 0.0)
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
    draw_ripple(&mut cv, sx + sr * 0.35, sy + sr * 0.15, 0.45, [255, 196, 0]);
    draw_ripple(&mut cv, sx - sr * 0.45, sy - sr * 0.3, 0.75, [255, 70, 60]);
    let bd = sample_badge(&["Ctrl", "Mayús", "S"], 2);
    let bx = (w as f32 - bd.w) / 2.0;
    let by = h as f32 - bd.h - 90.0;
    draw_badge(&mut cv, &bd, bx, by);
    let rgba = crate::soft::to_rgba_over(&px, [0, 0, 0]);
    image::save_buffer(path, &rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8).is_ok()
}

/// Self-test: sample keycaps, a ripple and the find-mouse ring on a grey "desktop".
pub fn render_sample(path: &std::path::Path) -> bool {
    let (w, h) = (900i32, 260i32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let mut cv = Canvas { w, h, px: &mut px };
    let bd = sample_badge(&["Ctrl", "Mayús", "P"], 3);
    draw_badge(&mut cv, &bd, 40.0, 150.0);
    let arrows = sample_badge(&["Alt", "←"], 1);
    draw_badge(&mut cv, &arrows, 480.0, 150.0);
    draw_ripple(&mut cv, 120.0, 70.0, 0.4, [255, 196, 0]);
    draw_find(&mut cv, 260.0, 70.0, 0.69);
    let rgba = crate::soft::to_rgba_over(&px, [96, 92, 88]);
    image::save_buffer(path, &rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8).is_ok()
}
