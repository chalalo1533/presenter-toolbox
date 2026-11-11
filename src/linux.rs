//! Linux (X11) counterpart of `win.rs`. It is compiled under the same module name (`win`)
//! so the rest of the app does not care which system it runs on.
//!
//! Global hotkeys are passive key grabs on the root window, the tray icon speaks the classic
//! XEmbed "system tray" protocol (XFCE, Cinnamon, MATE, KDE…), its menu is a small window we
//! draw ourselves, and screen capture is a plain `GetImage` of the root window.
//! "Live zoom" has no X11 equivalent of the Windows magnifier API and is not available here.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::settings::{HOTKEY_SLOTS, Hotkey, Hotkeys};
use crate::soft::{Canvas, text_mask};

#[path = "linux_gnome.rs"]
mod gnome;
pub use gnome::{action_from_name, detect_session, wayland};

static INITIAL_ACTION: Mutex<Option<Action>> = Mutex::new(None);

/// `--do <action>` on the command line: what to do once running (or to pass on to the copy
/// that already is).
pub fn set_initial_action(a: Option<Action>) {
    *INITIAL_ACTION.lock().unwrap() = a;
}

pub fn initial_action() -> Option<Action> {
    *INITIAL_ACTION.lock().unwrap()
}

/// Why a feature cannot be used on this system, if that is the case.
pub fn unavailable(a: Action) -> Option<&'static str> {
    match a {
        Action::LiveZoom | Action::LiveIn | Action::LiveOut => Some(
            "El zoom en vivo todavía no está disponible en Linux. Usa el zoom normal o el zoom propio de tu escritorio.",
        ),
        Action::Spotlight | Action::Tutorial | Action::FindMouse | Action::Gif if wayland() => Some(
            "No disponible en sesiones Wayland (el escritorio normal de Ubuntu, Debian o Fedora): ahí el sistema no deja que una aplicación vea el mouse y el teclado de las demás ni dibuje sobre ellas. Funciona en sesiones X11.",
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Types shared with the UI (same shapes as on Windows)

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
    pub overlay_active: AtomicBool,
    pub live_level: AtomicU32,
    pub hotkeys: Mutex<Vec<(String, bool)>>,
    pub hotkey_defs: Mutex<Vec<HotkeyDef>>,
    pub live_error: AtomicBool,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

pub fn shared_state() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}

// ---------------------------------------------------------------------------
// The X11 connection (one for the whole program; requests are thread-safe)

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
        _NET_ACTIVE_WINDOW,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DND,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_NAME,
        UTF8_STRING,
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_VISUAL,
        _XEMBED_INFO,
        MANAGER,
        _PRESENTER_TOOLBOX_INSTANCE,
        _PRESENTER_TOOLBOX_CMD,
    }
}

pub struct X {
    pub conn: RustConnection,
    pub root: Window,
    pub root_depth: u8,
    pub root_visual: Visualid,
    /// true when pixels are laid out B,G,R,X in memory (the usual case)
    pub bgr: bool,
    /// a 32-bit visual + colormap for windows with per-pixel transparency
    pub argb: Option<(Visualid, Colormap)>,
    /// hidden window: owns the single-instance selection and receives our internal messages
    pub cmd_win: Window,
    pub atoms: Atoms,
    tray_sel: Atom,
    cm_sel: Atom,
    min_keycode: u8,
    keymap: Mutex<(usize, Vec<u32>)>,
    monitors: Mutex<(Option<Instant>, Vec<ScreenRect>)>,
}

static XCTX: OnceLock<Option<X>> = OnceLock::new();

/// The shared X11 connection, or None when there is no X server to talk to.
pub fn x() -> Option<&'static X> {
    XCTX.get_or_init(|| connect().ok()).as_ref()
}

fn connect() -> Result<X, Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let setup = conn.setup().clone();
    let screen = &setup.roots[screen_num];
    let root = screen.root;
    let atoms = Atoms::new(&conn)?.reply()?;
    let tray_sel = conn.intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?.reply()?.atom;
    let cm_sel = conn.intern_atom(false, format!("_NET_WM_CM_S{screen_num}").as_bytes())?.reply()?.atom;

    let mut bgr = true;
    let mut argb_visual = None;
    for d in &screen.allowed_depths {
        for v in &d.visuals {
            if v.visual_id == screen.root_visual {
                bgr = v.red_mask == 0x00ff_0000;
            }
            if d.depth == 32 && v.class == VisualClass::TRUE_COLOR && v.red_mask == 0x00ff_0000 && argb_visual.is_none()
            {
                argb_visual = Some(v.visual_id);
            }
        }
    }
    let argb = match argb_visual {
        Some(v) => {
            let cmap = conn.generate_id()?;
            conn.create_colormap(ColormapAlloc::NONE, cmap, root, v)?;
            Some((v, cmap))
        }
        None => None,
    };

    let cmd_win = conn.generate_id()?;
    conn.create_window(
        0,
        cmd_win,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    conn.change_property8(PropMode::REPLACE, cmd_win, atoms._NET_WM_NAME, atoms.UTF8_STRING, b"Presenter Toolbox")?;
    // MANAGER messages on the root tell us when a system tray appears
    conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?;
    let _ = conn.extension_information(shape::X11_EXTENSION_NAME);
    conn.flush()?;

    let x = X {
        conn,
        root,
        root_depth: screen.root_depth,
        root_visual: screen.root_visual,
        bgr,
        argb,
        cmd_win,
        atoms,
        tray_sel,
        cm_sel,
        min_keycode: setup.min_keycode,
        keymap: Mutex::new((1, Vec::new())),
        monitors: Mutex::new((None, Vec::new())),
    };
    x.load_keymap(setup.max_keycode);
    Ok(x)
}

impl X {
    fn load_keymap(&self, max_keycode: u8) {
        let count = max_keycode - self.min_keycode + 1;
        if let Ok(r) = self.conn.get_keyboard_mapping(self.min_keycode, count).map(|c| c.reply()) {
            if let Ok(r) = r {
                *self.keymap.lock().unwrap() = (r.keysyms_per_keycode.max(1) as usize, r.keysyms);
            }
        }
    }

    /// Unshifted and shifted keysym of a key.
    pub fn keysyms(&self, keycode: u8) -> (u32, u32) {
        let km = self.keymap.lock().unwrap();
        let i = (keycode.saturating_sub(self.min_keycode)) as usize * km.0;
        let a = km.1.get(i).copied().unwrap_or(0);
        let b = if km.0 > 1 { km.1.get(i + 1).copied().unwrap_or(0) } else { 0 };
        (a, if b == 0 { a } else { b })
    }

    fn keycode_of(&self, keysym: u32) -> Option<u8> {
        let km = self.keymap.lock().unwrap();
        for col in 0..km.0.min(2) {
            for (row, syms) in km.1.chunks(km.0).enumerate() {
                if syms.get(col) == Some(&keysym) {
                    return Some(self.min_keycode + row as u8);
                }
            }
        }
        None
    }

    /// True when a compositing manager is running (needed for see-through windows).
    pub fn composited(&self) -> bool {
        self.argb.is_some()
            && self.conn.get_selection_owner(self.cm_sel).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.owner != 0)
    }

    pub fn root_size(&self) -> (i32, i32) {
        match self.conn.get_geometry(self.root).ok().and_then(|c| c.reply().ok()) {
            Some(g) => (g.width as i32, g.height as i32),
            None => (1920, 1080),
        }
    }

    fn monitors(&self) -> Vec<ScreenRect> {
        let mut m = self.monitors.lock().unwrap();
        if m.0.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            let mut list = Vec::new();
            if let Some(r) = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok()) {
                for mon in r.monitors {
                    if mon.width > 0 && mon.height > 0 {
                        list.push(ScreenRect { x: mon.x as i32, y: mon.y as i32, w: mon.width as i32, h: mon.height as i32 });
                    }
                }
            }
            if list.is_empty() {
                let (w, h) = self.root_size();
                list.push(ScreenRect { x: 0, y: 0, w, h });
            }
            *m = (Some(Instant::now()), list);
        }
        m.1.clone()
    }

    pub fn monitor_at(&self, px: i32, py: i32) -> ScreenRect {
        let list = self.monitors();
        list.iter()
            .copied()
            .find(|m| px >= m.x && py >= m.y && px < m.x + m.w && py < m.y + m.h)
            .unwrap_or(list[0])
    }

    /// Sends a 32-bit image (our premultiplied BGRA layout) to a window or pixmap, in as many
    /// requests as the server's size limit needs.
    pub fn put_image(&self, drawable: Drawable, gc: Gcontext, depth: u8, w: i32, h: i32, data: &[u8]) {
        if w <= 0 || h <= 0 {
            return;
        }
        let swapped;
        let data = if self.bgr {
            data
        } else {
            let mut v = data.to_vec();
            for p in v.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
            swapped = v;
            &swapped[..]
        };
        let row = (w * 4) as usize;
        let max = self.conn.maximum_request_bytes().saturating_sub(64).max(row);
        let rows_per = (max / row).max(1);
        let mut y = 0usize;
        while y < h as usize {
            let n = rows_per.min(h as usize - y);
            let _ = self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                gc,
                w as u16,
                n as u16,
                0,
                y as i16,
                0,
                depth,
                &data[y * row..(y + n) * row],
            );
            y += n;
        }
    }

    /// Lets every click go through a window to whatever is underneath.
    pub fn click_through(&self, win: Window) {
        let _ = self.conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[]);
    }

    /// A borderless window the window manager leaves alone (menus, overlays).
    pub fn popup_window(&self, argb: bool, x: i32, y: i32, w: i32, h: i32, background: u32, events: EventMask) -> Window {
        let Ok(win) = self.conn.generate_id() else { return 0 };
        let mut aux = CreateWindowAux::new()
            .override_redirect(1)
            .border_pixel(0)
            .background_pixel(background)
            .event_mask(events);
        let (depth, visual) = match (argb, self.argb) {
            (true, Some((visual, cmap))) => {
                aux = aux.colormap(cmap);
                (32, visual)
            }
            _ => (self.root_depth, self.root_visual),
        };
        let _ = self.conn.create_window(
            depth,
            win,
            self.root,
            x as i16,
            y as i16,
            w.clamp(1, 32000) as u16,
            h.clamp(1, 32000) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        );
        let _ = self.conn.change_property8(PropMode::REPLACE, win, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, b"Presenter Toolbox");
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"presenter_toolbox\0presenter_toolbox\0",
        );
        win
    }

    fn send_cmd(&self, to: Window, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, to, self.atoms._PRESENTER_TOOLBOX_CMD, data);
        let _ = self.conn.send_event(false, to, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    fn root_message(&self, window: Window, type_: Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, window, type_, data);
        let _ = self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        );
    }

    fn is_mapped(&self, win: Window) -> bool {
        self.conn
            .get_window_attributes(win)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|a| a.map_state != MapState::UNMAPPED)
    }

    /// WM_NORMAL_HINTS: where we want the window and whether its size is fixed.
    fn size_hints(&self, win: Window, r: ScreenRect, fixed: bool) {
        let mut v = [0u32; 18];
        v[0] = 1 | 2 | if fixed { 16 | 32 } else { 0 }; // USPosition | USSize | PMinSize | PMaxSize
        v[1] = r.x as u32;
        v[2] = r.y as u32;
        v[3] = r.w as u32;
        v[4] = r.h as u32;
        if fixed {
            v[5] = r.w as u32;
            v[6] = r.h as u32;
            v[7] = r.w as u32;
            v[8] = r.h as u32;
        }
        let _ = self.conn.change_property32(PropMode::REPLACE, win, AtomEnum::WM_NORMAL_HINTS, AtomEnum::WM_SIZE_HINTS, &v);
    }

    fn set_fullscreen(&self, win: Window, on: bool) {
        let a = &self.atoms;
        if self.is_mapped(win) {
            self.root_message(win, a._NET_WM_STATE, [on as u32, a._NET_WM_STATE_FULLSCREEN, 0, 1, 0]);
        } else if on {
            let _ = self.conn.change_property32(PropMode::REPLACE, win, a._NET_WM_STATE, AtomEnum::ATOM, &[a._NET_WM_STATE_FULLSCREEN]);
        } else {
            let _ = self.conn.delete_property(win, a._NET_WM_STATE);
        }
    }

    fn move_resize(&self, win: Window, r: ScreenRect, raise: bool) {
        let mut aux = ConfigureWindowAux::new().x(r.x).y(r.y).width(r.w.max(1) as u32).height(r.h.max(1) as u32);
        if raise {
            aux = aux.stack_mode(StackMode::ABOVE);
        }
        let _ = self.conn.configure_window(win, &aux);
    }
}

// ---------------------------------------------------------------------------
// Screen helpers

pub fn cursor_pos() -> (i32, i32) {
    x().and_then(|x| x.conn.query_pointer(x.root).ok()?.reply().ok())
        .map(|r| (r.root_x as i32, r.root_y as i32))
        .unwrap_or((0, 0))
}

pub fn monitor_at_cursor() -> ScreenRect {
    let (cx, cy) = cursor_pos();
    match x() {
        Some(x) => x.monitor_at(cx, cy),
        None => ScreenRect { x: 0, y: 0, w: 1920, h: 1080 },
    }
}

/// Grabs a rectangle of the screen as the compositor shows it.
pub fn capture(rect: ScreenRect) -> Option<ColorImage> {
    let x = x()?;
    let (rw, rh) = x.root_size();
    if wayland() {
        return capture_wayland(rect, rw, rh);
    }
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = (rect.x + rect.w).min(rw);
    let y1 = (rect.y + rect.h).min(rh);
    if x1 <= x0 || y1 <= y0 || rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let img = x
        .conn
        .get_image(ImageFormat::Z_PIXMAP, x.root, x0 as i16, y0 as i16, w as u16, h as u16, !0)
        .ok()?
        .reply()
        .ok()?;
    let bpp = img.data.len() / (w * h).max(1);
    if bpp != 4 {
        return None; // 16-bit and other exotic screens are not supported
    }
    // the requested rectangle may hang over the screen edge: pad it with black
    let (fw, fh) = (rect.w as usize, rect.h as usize);
    let (ox, oy) = ((x0 - rect.x) as usize, (y0 - rect.y) as usize);
    let mut pixels = vec![Color32::BLACK; fw * fh];
    let (ri, bi) = if x.bgr { (2, 0) } else { (0, 2) };
    for y in 0..h {
        let src = &img.data[y * w * 4..(y + 1) * w * 4];
        let dst = &mut pixels[(y + oy) * fw + ox..(y + oy) * fw + ox + w];
        for (d, p) in dst.iter_mut().zip(src.chunks_exact(4)) {
            *d = Color32::from_rgb(p[ri], p[1], p[bi]);
        }
    }
    Some(ColorImage::new([fw, fh], pixels))
}

/// Wayland: the portal hands over the whole desktop; cut out the rectangle we were asked for.
/// With display scaling the picture is larger than the X11 coordinates we work in, so it is
/// brought to the size the caller expects.
fn capture_wayland(rect: ScreenRect, rw: i32, rh: i32) -> Option<ColorImage> {
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    let full = gnome::capture_desktop()?;
    let (fw, fh) = (full.width() as f32, full.height() as f32);
    let (sx, sy) = (fw / rw.max(1) as f32, fh / rh.max(1) as f32);
    let cx = ((rect.x.max(0) as f32) * sx).round() as u32;
    let cy = ((rect.y.max(0) as f32) * sy).round() as u32;
    let cw = ((rect.w as f32 * sx).round() as u32).min(full.width().saturating_sub(cx)).max(1);
    let ch = ((rect.h as f32 * sy).round() as u32).min(full.height().saturating_sub(cy)).max(1);
    let mut part = image::imageops::crop_imm(&full, cx, cy, cw, ch).to_image();
    if (cw, ch) != (rect.w as u32, rect.h as u32) {
        part = image::imageops::resize(&part, rect.w as u32, rect.h as u32, image::imageops::FilterType::Triangle);
    }
    let pixels = part.pixels().map(|p| Color32::from_rgb(p[0], p[1], p[2])).collect();
    Some(ColorImage::new([rect.w as usize, rect.h as usize], pixels))
}

// ---------------------------------------------------------------------------
// Placement of the app's own window (done with X11 directly, behind winit's back,
// just like the Windows version does with Win32)

pub fn place_overlay(hwnd: isize, r: ScreenRect, show: bool) {
    let Some(x) = x() else { return };
    let win = hwnd as Window;
    if win == 0 {
        return;
    }
    // without min/max sizes, or some window managers refuse to make the window full screen
    x.size_hints(win, r, false);
    let mapped = x.is_mapped(win);
    x.set_fullscreen(win, true);
    x.move_resize(win, r, show);
    if show {
        if !mapped {
            let _ = x.conn.map_window(win);
            let _ = x.conn.flush();
            x.root_message(win, x.atoms._NET_WM_STATE, [1, x.atoms._NET_WM_STATE_FULLSCREEN, 0, 1, 0]);
            x.move_resize(win, r, true);
        }
    }
    let _ = x.conn.flush();
}

pub fn place_centered(hwnd: isize, w: i32, h: i32) {
    let Some(x) = x() else { return };
    let win = hwnd as Window;
    if win == 0 {
        return;
    }
    let m = monitor_at_cursor();
    let r = ScreenRect { x: m.x + (m.w - w) / 2, y: m.y + (m.h - h) / 2, w, h };
    x.set_fullscreen(win, false);
    x.size_hints(win, r, true);
    x.move_resize(win, r, true);
    if !x.is_mapped(win) {
        let _ = x.conn.map_window(win);
        let _ = x.conn.flush();
        x.move_resize(win, r, true);
    }
    let _ = x.conn.flush();
}

pub fn activate(hwnd: isize) {
    let Some(x) = x() else { return };
    let win = hwnd as Window;
    if win == 0 {
        return;
    }
    // source 2 ("pager") gets past focus-stealing prevention, like a taskbar click would
    x.root_message(win, x.atoms._NET_ACTIVE_WINDOW, [2, 0, 0, 0, 0]);
    let _ = x.conn.set_input_focus(InputFocus::PARENT, win, 0u32);
    let _ = x.conn.flush();
}

pub fn hide(hwnd: isize) {
    let Some(x) = x() else { return };
    let win = hwnd as Window;
    if win == 0 {
        return;
    }
    let _ = x.conn.unmap_window(win);
    // the synthetic event is how a window tells the window manager it is really gone
    let ev = UnmapNotifyEvent { response_type: UNMAP_NOTIFY_EVENT, sequence: 0, event: x.root, window: win, from_configure: false };
    let _ = x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
    let _ = x.conn.flush();
}

pub fn window_rect(hwnd: isize) -> ScreenRect {
    let win = hwnd as Window;
    let get = || {
        let x = x()?;
        let g = x.conn.get_geometry(win).ok()?.reply().ok()?;
        let t = x.conn.translate_coordinates(win, x.root, 0, 0).ok()?.reply().ok()?;
        Some(ScreenRect { x: t.dst_x as i32, y: t.dst_y as i32, w: g.width as i32, h: g.height as i32 })
    };
    get().unwrap_or_default()
}

pub fn dpi_scale_for(_hwnd: isize) -> f32 {
    1.0
}

pub fn beep() {
    if let Some(x) = x() {
        let _ = x.conn.bell(0);
        let _ = x.conn.flush();
    }
}

/// Asks the hotkey thread to do something (e.g. start a zoom from the options window).
pub fn post_action(a: Action) {
    if let Some(x) = x() {
        x.send_cmd(x.cmd_win, [CMD_ACTION, a as u32, 1, 0, 0]);
    }
}

fn post_action_now(a: Action) {
    if let Some(x) = x() {
        x.send_cmd(x.cmd_win, [CMD_ACTION, a as u32, 0, 0, 0]);
    }
}

/// Returns false if another copy is already running (and tells it to show its options).
pub fn single_instance() -> bool {
    let Some(x) = x() else { return true };
    let sel = x.atoms._PRESENTER_TOOLBOX_INSTANCE;
    let owner = x.conn.get_selection_owner(sel).ok().and_then(|c| c.reply().ok()).map(|r| r.owner).unwrap_or(0);
    if owner != 0 && owner != x.cmd_win {
        let action = initial_action().unwrap_or(Action::Options);
        x.send_cmd(owner, [CMD_ACTION, action as u32, 0, 0, 0]);
        return false;
    }
    let _ = x.conn.set_selection_owner(x.cmd_win, sel, 0u32);
    let _ = x.conn.flush();
    true
}

fn user_dir(kind: &str, fallback: &str) -> std::path::PathBuf {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()));
    if let Ok(out) = std::process::Command::new("xdg-user-dir").arg(kind).output() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !p.is_empty() && std::path::Path::new(&p).is_dir() {
            return p.into();
        }
    }
    let p = home.join(fallback);
    if p.is_dir() { p } else { home }
}

/// Asks where to save a file with the desktop's own dialog (zenity or kdialog). If neither
/// is installed the file goes to the Pictures folder with a dated name.
pub fn save_dialog(_owner: isize, default_name: &str) -> Option<std::path::PathBuf> {
    let gif = default_name.to_ascii_lowercase().ends_with(".gif");
    let ext = if gif { "gif" } else { "png" };
    let dir = user_dir("PICTURES", "Pictures");
    let suggested = dir.join(default_name);
    let with_ext = |p: std::path::PathBuf| {
        if p.extension().is_none() { p.with_extension(ext) } else { p }
    };
    // plain ASCII on purpose: zenity rejects accented arguments when the locale is not UTF-8
    let filter = if gif { "GIF | *.gif" } else { "PNG | *.png" };
    // exit code 0 = a file was chosen, 1 = cancelled; anything else means the program is
    // missing or did not understand an option, so the next one is tried
    let ask = |cmd: &mut std::process::Command| -> Option<Option<std::path::PathBuf>> {
        let out = cmd.stderr(std::process::Stdio::null()).output().ok()?;
        match out.status.code() {
            Some(0) => {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if p.is_empty() { None } else { Some(Some(with_ext(p.into()))) }
            }
            Some(1) => Some(None),
            _ => None,
        }
    };
    // zenity 3 needs --confirm-overwrite; zenity 4 always asks and rejects the option
    for confirm in [true, false] {
        let mut cmd = std::process::Command::new("zenity");
        cmd.args(["--file-selection", "--save", "--title", "Guardar como"]);
        if confirm {
            cmd.arg("--confirm-overwrite");
        }
        cmd.arg("--filename").arg(&suggested).arg("--file-filter").arg(filter);
        if let Some(answer) = ask(&mut cmd) {
            return answer;
        }
    }
    let mut cmd = std::process::Command::new("kdialog");
    cmd.arg("--getsavefilename").arg(&suggested).arg(format!("*.{ext}"));
    if let Some(answer) = ask(&mut cmd) {
        return answer;
    }
    // no dialog program: pick a name that cannot overwrite anything
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let stem = std::path::Path::new(default_name).file_stem().and_then(|s| s.to_str()).unwrap_or("Captura").to_string();
    let path = dir.join(format!("{stem}-{secs}.{ext}"));
    notify("Archivo guardado", &path.display().to_string());
    Some(path)
}

/// Shows a desktop notification (falls back to our own on-screen label).
pub fn notify(title: &str, text: &str) {
    let spawned = std::process::Command::new("notify-send")
        .args(["-a", "Presenter Toolbox", "-i", "zoom-in", "-t", "4000", title, text])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(_) => crate::fx::status(&format!("{title}: {text}")),
    }
}

pub fn check_live() -> String {
    "linux: el zoom en vivo no está disponible".into()
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

// ---------------------------------------------------------------------------
// Hotkey + tray thread

const CMD_ACTION: u32 = 1;
const CMD_REREGISTER: u32 = 2;
const CMD_SUSPEND: u32 = 3;
const CMD_INPUT: u32 = 4;
const CMD_QUIT_TRAY: u32 = 5;

pub fn start(
    ctx: egui::Context,
    tx: mpsc::Sender<Msg>,
    _app_hwnd: isize,
    tray: bool,
    live_level: f32,
    defs: Vec<HotkeyDef>,
) {
    let sh = Arc::new(Shared {
        ctx,
        tx,
        overlay_active: AtomicBool::new(false),
        live_level: AtomicU32::new(live_level.to_bits()),
        hotkeys: Mutex::new(Vec::new()),
        hotkey_defs: Mutex::new(defs),
        live_error: AtomicBool::new(false),
    });
    let _ = SHARED.set(sh);
    if x().is_some() {
        std::thread::spawn(move || event_thread(tray));
        // started by a shortcut (`--do zoom`): do it as soon as the event thread is up
        if let Some(a) = initial_action().filter(|a| *a != Action::Options) {
            post_action_now(a);
        }
    } else if let Some(sh) = SHARED.get() {
        let defs = sh.hotkey_defs.lock().unwrap().clone();
        *sh.hotkeys.lock().unwrap() = defs.iter().map(|d| (format!("{}  {}", d.key.label(), d.name), false)).collect();
    }
}

/// True once an icon is actually sitting in a panel. GNOME has no system tray at all,
/// so there the program has to tell the user another way how to get back to the options.
static TRAY_DOCKED: AtomicBool = AtomicBool::new(false);

pub fn has_tray() -> bool {
    TRAY_DOCKED.load(Ordering::SeqCst)
}

pub fn remove_tray() {
    TRAY_DOCKED.store(false, Ordering::SeqCst);
    if wayland() {
        gnome::clear_shortcuts();
    }
    if let Some(x) = x() {
        x.send_cmd(x.cmd_win, [CMD_QUIT_TRAY, 0, 0, 0, 0]);
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Replaces the global shortcuts (also resumes them after `suspend_hotkeys(true)`).
pub fn set_hotkeys(defs: Vec<HotkeyDef>) {
    if let Some(sh) = SHARED.get() {
        *sh.hotkey_defs.lock().unwrap() = defs;
        if let Some(x) = x() {
            x.send_cmd(x.cmd_win, [CMD_REREGISTER, 0, 0, 0, 0]);
        }
    }
}

/// Turns the global shortcuts off while the user is typing a new combination.
pub fn suspend_hotkeys(on: bool) {
    if let Some(x) = x() {
        x.send_cmd(x.cmd_win, [CMD_SUSPEND, on as u32, 0, 0, 0]);
    }
}

/// Called by the effects thread: which global input it currently needs to see.
pub fn want_input(raw: bool, wheel: bool) {
    if let Some(x) = x() {
        x.send_cmd(x.cmd_win, [CMD_INPUT, raw as u32, wheel as u32, 0, 0]);
    }
}

/// Windows virtual-key code (what the settings store) → X11 keysym.
fn vk_to_keysym(vk: u32) -> Option<u32> {
    Some(match vk {
        0x30..=0x39 => vk,
        0x41..=0x5A => vk + 0x20,
        0x70..=0x87 => 0xFFBE + (vk - 0x70),
        0x20 => 0x20,
        0x2D => 0xFF63,
        0x2E => 0xFFFF,
        0x24 => 0xFF50,
        0x23 => 0xFF57,
        0x21 => 0xFF55,
        0x22 => 0xFF56,
        0x25 => 0xFF51,
        0x26 => 0xFF52,
        0x27 => 0xFF53,
        0x28 => 0xFF54,
        _ => return None,
    })
}

const MOD_SHIFT: u16 = 1;
const MOD_LOCK: u16 = 2;
const MOD_CTRL: u16 = 4;
const MOD_ALT: u16 = 8;
const MOD_NUM: u16 = 16;
const MOD_SUPER: u16 = 64;
/// Caps Lock and Num Lock must not stop a shortcut from firing.
const LOCK_VARIANTS: [u16; 4] = [0, MOD_LOCK, MOD_NUM, MOD_LOCK | MOD_NUM];

struct Tray {
    win: Window,
    gc: Gcontext,
    argb: bool,
    w: i32,
    h: i32,
}

struct MenuItem {
    action: Option<Action>,
    label: String,
    key: String,
    checked: bool,
    y0: i32,
    y1: i32,
}

struct Menu {
    win: Window,
    gc: Gcontext,
    items: Vec<MenuItem>,
    w: i32,
    h: i32,
    hover: Option<usize>,
    opened: Instant,
    pressed_inside: bool,
}

struct Events {
    x: &'static X,
    grabs: Vec<(u8, u16, i32)>,
    last_release: (u8, u32),
    tray_wanted: bool,
    tray: Option<Tray>,
    menu: Option<Menu>,
    xi_ready: bool,
    raw_on: bool,
    wheel_on: bool,
    last_motion: Instant,
}

fn event_thread(tray: bool) {
    let Some(x) = x() else { return };
    let mut ev = Events {
        x,
        grabs: Vec::new(),
        last_release: (0, 0),
        tray_wanted: tray,
        tray: None,
        menu: None,
        xi_ready: false,
        raw_on: false,
        wheel_on: false,
        last_motion: Instant::now(),
    };
    ev.xi_ready = x
        .conn
        .xinput_xi_query_version(2, 2)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| v.major_version >= 2);
    ev.register_all();
    if tray {
        ev.dock_tray();
    }
    let _ = x.conn.flush();
    loop {
        let Ok(event) = x.conn.wait_for_event() else { break };
        ev.handle(event);
        while let Ok(Some(event)) = x.conn.poll_for_event() {
            ev.handle(event);
        }
        let _ = x.conn.flush();
    }
}

impl Events {
    fn ungrab_all(&mut self) {
        for (code, mods, _) in self.grabs.drain(..) {
            for extra in LOCK_VARIANTS {
                let _ = self.x.conn.ungrab_key(code, self.x.root, ModMask::from(mods | extra));
            }
        }
    }

    /// (Re)registers every shortcut and records which ones the X server accepted.
    fn register_all(&mut self) {
        let Some(sh) = SHARED.get() else { return };
        self.ungrab_all();
        let defs = sh.hotkey_defs.lock().unwrap().clone();
        if wayland() {
            // key grabs only reach X11 windows there; the desktop itself runs the shortcuts
            *sh.hotkeys.lock().unwrap() = gnome::apply_shortcuts(&defs);
            sh.ctx.request_repaint();
            return;
        }
        let mut status = Vec::new();
        for d in defs {
            if d.key.vk == 0 {
                status.push((format!("Sin asignar  {}", d.name), true));
                continue;
            }
            let label = format!("{}  {}", d.key.label(), d.name);
            let Some(code) = vk_to_keysym(d.key.vk).and_then(|ks| self.x.keycode_of(ks)) else {
                status.push((label, false));
                continue;
            };
            let mut mods = 0;
            if d.key.ctrl {
                mods |= MOD_CTRL;
            }
            if d.key.shift {
                mods |= MOD_SHIFT;
            }
            if d.key.alt {
                mods |= MOD_ALT;
            }
            let mut ok = true;
            for extra in LOCK_VARIANTS {
                let grabbed = self
                    .x
                    .conn
                    .grab_key(false, self.x.root, ModMask::from(mods | extra), code, GrabMode::ASYNC, GrabMode::ASYNC)
                    .ok()
                    .is_some_and(|c| c.check().is_ok());
                ok &= grabbed;
            }
            if ok {
                self.grabs.push((code, mods, d.id));
            } else {
                // another program owns this combination: release whatever part we did get
                for extra in LOCK_VARIANTS {
                    let _ = self.x.conn.ungrab_key(code, self.x.root, ModMask::from(mods | extra));
                }
            }
            status.push((label, ok));
        }
        *sh.hotkeys.lock().unwrap() = status;
        sh.ctx.request_repaint();
    }

    fn set_input(&mut self, raw: bool, wheel: bool) {
        let x = self.x;
        if raw != self.raw_on && self.xi_ready {
            let bits = if raw {
                xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_BUTTON_PRESS | xinput::XIEventMask::RAW_MOTION
            } else {
                xinput::XIEventMask::from(0u32)
            };
            // device 1 = "all master devices": every keyboard and mouse, wherever the focus is
            let _ = x.conn.xinput_xi_select_events(x.root, &[xinput::EventMask { deviceid: 1, mask: vec![bits] }]);
            self.raw_on = raw;
        }
        if wheel != self.wheel_on {
            for button in [ButtonIndex::M4, ButtonIndex::M5] {
                for extra in LOCK_VARIANTS {
                    let mods = ModMask::from(MOD_CTRL | MOD_SHIFT | extra);
                    if wheel {
                        let _ = x.conn.grab_button(
                            false,
                            x.root,
                            EventMask::BUTTON_PRESS,
                            GrabMode::ASYNC,
                            GrabMode::ASYNC,
                            x11rb::NONE,
                            x11rb::NONE,
                            button,
                            mods,
                        );
                    } else {
                        let _ = x.conn.ungrab_button(button, x.root, mods);
                    }
                }
            }
            self.wheel_on = wheel;
        }
    }

    fn handle(&mut self, event: Event) {
        let x = self.x;
        match event {
            Event::KeyPress(e) => {
                if self.menu.is_some() {
                    self.close_menu();
                    return;
                }
                // auto-repeat arrives as release+press with the same timestamp: not a new press
                if self.last_release == (e.detail, e.time) {
                    return;
                }
                let state = u16::from(e.state) & (MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_SUPER);
                if let Some(&(_, _, id)) = self.grabs.iter().find(|g| g.0 == e.detail && g.1 == state) {
                    if let Some(a) = Action::from_id(id as usize) {
                        trigger(a, false);
                    }
                }
            }
            Event::KeyRelease(e) => self.last_release = (e.detail, e.time),
            Event::ButtonPress(e) => {
                if self.menu.as_ref().is_some_and(|m| m.win == e.event) {
                    let inside = self.menu_item_at(e.event_x as i32, e.event_y as i32).is_some();
                    if inside {
                        if let Some(m) = &mut self.menu {
                            m.pressed_inside = true;
                        }
                    } else {
                        self.close_menu();
                    }
                } else if self.tray.as_ref().is_some_and(|t| t.win == e.event) {
                    match e.detail {
                        1 => trigger(Action::Options, false),
                        3 => self.open_menu(e.root_x as i32, e.root_y as i32),
                        _ => {}
                    }
                } else if e.event == x.root && (e.detail == 4 || e.detail == 5) {
                    crate::fx::input(crate::fx::InputEv::Wheel(if e.detail == 4 { 1.0 } else { -1.0 }));
                }
            }
            Event::ButtonRelease(e) => {
                if self.menu.as_ref().is_some_and(|m| m.win == e.event) {
                    let hit = self.menu_item_at(e.event_x as i32, e.event_y as i32);
                    let armed = self.menu.as_ref().is_some_and(|m| m.pressed_inside || m.opened.elapsed() > Duration::from_millis(350));
                    if let (Some(i), true) = (hit, armed) {
                        let action = self.menu.as_ref().and_then(|m| m.items[i].action);
                        self.close_menu();
                        if let Some(a) = action {
                            trigger(a, true);
                        }
                    }
                }
            }
            Event::MotionNotify(e) => {
                if self.menu.as_ref().is_some_and(|m| m.win == e.event) {
                    let hit = self.menu_item_at(e.event_x as i32, e.event_y as i32);
                    if self.menu.as_ref().is_some_and(|m| m.hover != hit) {
                        if let Some(m) = &mut self.menu {
                            m.hover = hit;
                        }
                        self.draw_menu();
                    }
                }
            }
            Event::Expose(e) => {
                if self.menu.as_ref().is_some_and(|m| m.win == e.window) {
                    self.draw_menu();
                } else if self.tray.as_ref().is_some_and(|t| t.win == e.window) && e.count == 0 {
                    self.draw_tray();
                }
            }
            Event::ConfigureNotify(e) => {
                if let Some(t) = &mut self.tray {
                    if t.win == e.window && (t.w, t.h) != (e.width as i32, e.height as i32) {
                        t.w = e.width as i32;
                        t.h = e.height as i32;
                        self.draw_tray();
                    }
                }
            }
            Event::DestroyNotify(e) => {
                if self.tray.as_ref().is_some_and(|t| t.win == e.window) {
                    self.tray = None; // the panel went away; we dock again when a new one shows up
                    TRAY_DOCKED.store(false, Ordering::SeqCst);
                }
            }
            Event::MappingNotify(_) => {
                x.load_keymap(x.conn.setup().max_keycode);
                self.register_all();
            }
            Event::ClientMessage(e) => {
                let d = e.data.as_data32();
                if e.type_ == x.atoms._PRESENTER_TOOLBOX_CMD {
                    match d[0] {
                        CMD_ACTION => {
                            if let Some(a) = Action::from_id(d[1] as usize) {
                                trigger(a, d[2] != 0);
                            }
                        }
                        CMD_REREGISTER => self.register_all(),
                        CMD_SUSPEND => {
                            if d[1] != 0 {
                                self.ungrab_all();
                                if wayland() {
                                    gnome::clear_shortcuts();
                                }
                            } else {
                                self.register_all();
                            }
                        }
                        CMD_INPUT => self.set_input(d[1] != 0, d[2] != 0),
                        CMD_QUIT_TRAY => {
                            self.close_menu();
                            if let Some(t) = self.tray.take() {
                                let _ = x.conn.destroy_window(t.win);
                            }
                            self.tray_wanted = false;
                        }
                        _ => {}
                    }
                } else if e.type_ == x.atoms.MANAGER && d[1] == x.tray_sel && self.tray_wanted && self.tray.is_none() {
                    self.dock_tray();
                }
            }
            Event::XinputRawKeyPress(e) => {
                let code = e.detail as u8;
                let (ks0, ks1) = x.keysyms(code);
                let mask = x
                    .conn
                    .query_pointer(x.root)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| u16::from(r.mask))
                    .unwrap_or(0);
                crate::fx::input(crate::fx::InputEv::Key {
                    ks0,
                    ks1,
                    ctrl: mask & MOD_CTRL != 0,
                    alt: mask & MOD_ALT != 0,
                    shift: mask & MOD_SHIFT != 0,
                    win: mask & MOD_SUPER != 0,
                });
            }
            Event::XinputRawButtonPress(e) => {
                if (1..=3).contains(&e.detail) {
                    let (px, py) = cursor_pos();
                    crate::fx::input(crate::fx::InputEv::Click { x: px, y: py, btn: (e.detail - 1) as u8 });
                }
            }
            Event::XinputRawMotion(_) => {
                // a fast mouse sends hundreds of these per second; sampling is plenty
                if self.last_motion.elapsed() >= Duration::from_millis(8) {
                    self.last_motion = Instant::now();
                    crate::fx::input(crate::fx::InputEv::Motion(cursor_pos().0));
                }
            }
            _ => {}
        }
    }

    // -- tray icon (XEmbed system tray protocol) ----------------------------

    fn dock_tray(&mut self) {
        let x = self.x;
        let manager = x.conn.get_selection_owner(x.tray_sel).ok().and_then(|c| c.reply().ok()).map(|r| r.owner).unwrap_or(0);
        if manager == 0 {
            return;
        }
        // panels that want see-through icons announce a 32-bit visual
        let wants_argb = x
            .conn
            .get_property(false, manager, x.atoms._NET_SYSTEM_TRAY_VISUAL, AtomEnum::VISUALID, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .is_some_and(|v| x.argb.is_some_and(|(av, _)| av == v));
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY;
        let Ok(win) = x.conn.generate_id() else { return };
        let created = if wants_argb {
            let (visual, cmap) = x.argb.unwrap();
            x.conn.create_window(
                32,
                win,
                x.root,
                -64,
                -64,
                24,
                24,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &CreateWindowAux::new().border_pixel(0).background_pixel(0).colormap(cmap).event_mask(events),
            )
        } else {
            x.conn.create_window(
                x.root_depth,
                win,
                x.root,
                -64,
                -64,
                24,
                24,
                0,
                WindowClass::INPUT_OUTPUT,
                x.root_visual,
                // 1 = ParentRelative: show the panel's own background behind the icon
                &CreateWindowAux::new().background_pixmap(1u32).event_mask(events),
            )
        };
        if created.is_err() {
            return;
        }
        let _ = x.conn.change_property8(PropMode::REPLACE, win, x.atoms._NET_WM_NAME, x.atoms.UTF8_STRING, b"Presenter Toolbox");
        let _ = x.conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"presenter_toolbox\0presenter_toolbox\0");
        let _ = x.conn.change_property32(PropMode::REPLACE, win, x.atoms._XEMBED_INFO, x.atoms._XEMBED_INFO, &[0, 1]);
        let Ok(gc) = x.conn.generate_id() else { return };
        let _ = x.conn.create_gc(gc, win, &CreateGCAux::new());
        // opcode 0 = SYSTEM_TRAY_REQUEST_DOCK
        let ev = ClientMessageEvent::new(32, manager, x.atoms._NET_SYSTEM_TRAY_OPCODE, [0u32, 0, win, 0, 0]);
        let _ = x.conn.send_event(false, manager, EventMask::NO_EVENT, ev);
        self.tray = Some(Tray { win, gc, argb: wants_argb, w: 24, h: 24 });
        TRAY_DOCKED.store(true, Ordering::SeqCst);
    }

    fn draw_tray(&mut self) {
        let x = self.x;
        let Some(t) = &self.tray else { return };
        let (w, h) = (t.w.max(1), t.h.max(1));
        let s = w.min(h);
        let icon = icon_rgba(s as usize);
        let (ox, oy) = ((w - s) / 2, (h - s) / 2);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut rects = Vec::new();
        for y in 0..s {
            let mut run: Option<i32> = None;
            for xx in 0..=s {
                let a = if xx < s { icon[((y * s + xx) * 4 + 3) as usize] } else { 0 };
                let i = ((y * s + xx) * 4) as usize;
                if xx < s {
                    let o = (((y + oy) * w + xx + ox) * 4) as usize;
                    let (r, g, b) = (icon[i] as u32, icon[i + 1] as u32, icon[i + 2] as u32);
                    if t.argb {
                        let af = a as u32;
                        buf[o..o + 4].copy_from_slice(&[(b * af / 255) as u8, (g * af / 255) as u8, (r * af / 255) as u8, a]);
                    } else {
                        buf[o..o + 4].copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
                    }
                }
                // without transparency the icon's outline is cut out of the window instead
                match (run, a >= 110) {
                    (None, true) => run = Some(xx),
                    (Some(start), false) => {
                        rects.push(Rectangle { x: (start + ox) as i16, y: (y + oy) as i16, width: (xx - start) as u16, height: 1 });
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        if t.argb {
            x.put_image(t.win, t.gc, 32, w, h, &buf);
        } else {
            let _ = x.conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::UNSORTED, t.win, 0, 0, &rects);
            x.put_image(t.win, t.gc, x.root_depth, w, h, &buf);
        }
    }

    // -- the tray menu: a small window we draw ourselves ---------------------

    fn menu_item_at(&self, px: i32, py: i32) -> Option<usize> {
        let m = self.menu.as_ref()?;
        if px < 0 || px >= m.w {
            return None;
        }
        m.items.iter().position(|it| it.action.is_some() && py >= it.y0 && py < it.y1)
    }

    fn open_menu(&mut self, px: i32, py: i32) {
        self.close_menu();
        let x = self.x;
        let Some(sh) = SHARED.get() else { return };
        let defs = sh.hotkey_defs.lock().unwrap().clone();
        let key_of = |a: Action| defs.iter().find(|d| d.id == a as i32).map(|d| d.key.label()).unwrap_or_default();
        let entries = [
            (Action::Zoom, "Zoom"),
            (Action::Draw, "Dibujar"),
            (Action::Break, "Temporizador de descanso"),
            (Action::Screenshot, "Captura al portapapeles"),
            (Action::Snip, "Recorte"),
            (Action::Spotlight, "Foco"),
            (Action::Tutorial, "Clics y teclas"),
            (Action::FindMouse, "Encontrar el mouse"),
            (Action::Gif, if crate::gif_rec::recording() { "Detener la grabación GIF" } else { "Grabar GIF" }),
        ];
        const ROW: i32 = 30;
        const SEP: i32 = 9;
        let mut items = Vec::new();
        let mut y = 4;
        let mut push = |action: Option<Action>, label: &str, key: String, checked: bool, y: &mut i32| {
            let hgt = if action.is_some() { ROW } else { SEP };
            items.push(MenuItem { action, label: label.into(), key, checked, y0: *y, y1: *y + hgt });
            *y += hgt;
        };
        push(Some(Action::Options), "Opciones…", String::new(), false, &mut y);
        push(None, "", String::new(), false, &mut y);
        for (a, label) in entries {
            let on = match a {
                Action::Spotlight => crate::fx::SPOT_ON.load(Ordering::SeqCst),
                Action::Tutorial => crate::fx::TUTORIAL_ON.load(Ordering::SeqCst),
                _ => false,
            };
            push(Some(a), label, key_of(a), on, &mut y);
        }
        push(None, "", String::new(), false, &mut y);
        push(Some(Action::Exit), "Salir", String::new(), false, &mut y);
        let h = y + 4;
        let label_w = items.iter().map(|it| text_mask(&it.label, 15, false).0).max().unwrap_or(0);
        let key_w = items.iter().map(|it| if it.key.is_empty() { 0 } else { text_mask(&it.key, 14, false).0 }).max().unwrap_or(0);
        let w = 34 + label_w + 28 + key_w + 14;

        let mon = x.monitor_at(px, py);
        let mx = (px + 2).min(mon.x + mon.w - w).max(mon.x);
        let my = if py + 2 + h > mon.y + mon.h { (py - 2 - h).max(mon.y) } else { py + 2 };
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let win = x.popup_window(false, mx, my, w, h, 0x00f9_f9f9, events);
        if win == 0 {
            return;
        }
        let _ = x.conn.change_property32(PropMode::REPLACE, win, x.atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, &[x.atoms._NET_WM_WINDOW_TYPE_POPUP_MENU]);
        let Ok(gc) = x.conn.generate_id() else { return };
        let _ = x.conn.create_gc(gc, win, &CreateGCAux::new());
        let _ = x.conn.map_window(win);
        let _ = x.conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        // while the menu is open every click and key comes to it, so a click elsewhere closes it
        let _ = x.conn.grab_pointer(
            false,
            win,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            0u32,
        );
        let _ = x.conn.grab_keyboard(false, win, 0u32, GrabMode::ASYNC, GrabMode::ASYNC);
        self.menu = Some(Menu { win, gc, items, w, h, hover: None, opened: Instant::now(), pressed_inside: false });
        self.draw_menu();
    }

    fn close_menu(&mut self) {
        if let Some(m) = self.menu.take() {
            let _ = self.x.conn.ungrab_pointer(0u32);
            let _ = self.x.conn.ungrab_keyboard(0u32);
            let _ = self.x.conn.destroy_window(m.win);
            let _ = self.x.conn.flush();
        }
    }

    fn draw_menu(&mut self) {
        let x = self.x;
        let Some(m) = &self.menu else { return };
        let mut buf = vec![0u8; (m.w * m.h * 4) as usize];
        let mut cv = Canvas { w: m.w, h: m.h, px: &mut buf };
        cv.fill(0, 0, m.w, m.h, [170, 170, 170]);
        cv.fill(1, 1, m.w - 1, m.h - 1, [249, 249, 249]);
        for (i, it) in m.items.iter().enumerate() {
            if it.action.is_none() {
                let y = (it.y0 + it.y1) / 2;
                cv.fill(10, y, m.w - 10, y + 1, [214, 214, 214]);
                continue;
            }
            if m.hover == Some(i) {
                cv.rrect(4.0, it.y0 as f32, (m.w - 4) as f32, it.y1 as f32, 4.0, [221, 236, 250], 1.0);
            }
            if it.checked {
                cv.disc(17.0, (it.y0 + it.y1) as f32 / 2.0, 4.5, [0, 95, 184], 1.0);
            }
            let bold = it.action == Some(Action::Options);
            let (tw, th, px) = text_mask(&it.label, 15, bold);
            let label = crate::soft::Mask { w: tw, h: th, px };
            cv.mask(32, it.y0 + (it.y1 - it.y0 - th) / 2, &label, [28, 28, 32], 1.0);
            if !it.key.is_empty() {
                let (kw, kh, px) = text_mask(&it.key, 14, false);
                let key = crate::soft::Mask { w: kw, h: kh, px };
                cv.mask(m.w - 14 - kw, it.y0 + (it.y1 - it.y0 - kh) / 2, &key, [110, 110, 118], 1.0);
            }
        }
        x.put_image(m.win, m.gc, x.root_depth, m.w, m.h, &buf);
    }
}

fn send(a: Action, capture: Option<Capture>) {
    let Some(sh) = SHARED.get() else { return };
    let _ = sh.tx.send(Msg { action: a, capture, file: None });
    sh.ctx.request_repaint();
}

/// Our own status label must not appear in the picture we are about to take.
fn settle_before_capture() {
    if crate::fx::clear_status() {
        std::thread::sleep(Duration::from_millis(70));
    }
}

fn trigger(a: Action, delayed: bool) {
    let Some(sh) = SHARED.get() else { return };
    match a {
        Action::Options | Action::Exit => send(a, None),
        Action::GifDone => {}
        _ if unavailable(a).is_some() => {
            let title = match a {
                Action::Spotlight => "Foco",
                Action::Tutorial => "Clics y teclas",
                Action::FindMouse => "Encontrar el mouse",
                Action::Gif => "Grabar GIF",
                _ => "Zoom en vivo",
            };
            notify(title, unavailable(a).unwrap_or_default());
        }
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
                std::thread::sleep(Duration::from_millis(250));
            }
            settle_before_capture();
            let rect = monitor_at_cursor();
            if let Some(image) = capture(rect) {
                send(a, Some(Capture { rect, image }));
            }
        }
        _ => {
            if sh.overlay_active.load(Ordering::SeqCst) {
                send(a, None);
                return;
            }
            if delayed {
                // let a menu or the options window disappear first (compositors fade them out)
                std::thread::sleep(Duration::from_millis(250));
            }
            settle_before_capture();
            let rect = monitor_at_cursor();
            if let Some(image) = capture(rect) {
                sh.overlay_active.store(true, Ordering::SeqCst);
                send(a, Some(Capture { rect, image }));
            }
        }
    }
}
