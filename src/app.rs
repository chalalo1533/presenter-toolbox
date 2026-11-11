use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, CursorIcon, Event, FontId, Key, Pos2, Rect, Shape,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::settings::{HOTKEY_SLOTS, Hotkey, Hotkeys, SMOOTHING_NAMES, Settings};
use crate::win::{self, Action, Msg, ScreenRect};

const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 32.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Hidden,
    Options,
    Zoom,
    Draw,
    Break,
    Snip,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Zoom,
    Live,
    Draw,
    Type,
    Break,
    Shot,
    Snip,
    Spot,
    Tutorial,
    Gif,
    About,
}

#[derive(Clone, Debug)]
enum Kind {
    Free(Vec<Pos2>),
    Line(Pos2, Pos2),
    Rect(Pos2, Pos2),
    Ellipse(Pos2, Pos2),
    Arrow(Pos2, Pos2),
    Text { pos: Pos2, text: String, size: f32, right: bool },
}

#[derive(Clone, Debug)]
struct Annot {
    kind: Kind,
    color: Color32,
    /// width in image pixels
    width: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum SnipTarget {
    Copy,
    Save,
    Gif,
}

#[derive(Clone, Copy, PartialEq)]
enum ShotKind {
    Copy,
    Save,
    File,
}

pub struct ZoomApp {
    s: Settings,
    hwnd: isize,
    rx: mpsc::Receiver<Msg>,
    mode: Mode,
    tab: Tab,

    // captured screen
    tex: Option<TextureHandle>,
    img: Option<Arc<ColorImage>>,
    img_size: Vec2,
    monitor: ScreenRect,

    // view
    log_zoom: f32,
    zoom_target: f32,
    origin: Vec2,
    anchor_img: Pos2,
    anchor_px: Pos2,
    snap_origin: bool,
    exiting: bool,
    draw_from_zoom: bool,

    // drawing
    annots: Vec<Annot>,
    drawing: Option<(Pos2, Vec<Pos2>)>,
    pen_color: Color32,
    highlighter: bool,
    board: Option<Color32>,
    typing: Option<Annot>,
    typing_started_frame: u64,

    // break
    break_end: Instant,
    break_beeped: bool,

    // snip
    snip_start: Option<Pos2>,
    snip_target: SnipTarget,

    // showing / screenshots
    show_stage: u8,
    show_deadline: Instant,
    shot_rx: mpsc::Receiver<Arc<ColorImage>>,
    shot_tx: mpsc::Sender<Arc<ColorImage>>,
    shot_pending: Option<ShotKind>,
    screenshot_path: Option<PathBuf>,
    frames: u64,
    toast: Option<(String, Instant)>,
    last_frame_ms: f32,

    // shortcut editor: which slot is waiting for a new combination
    capturing: Option<usize>,
    hk_msg: Option<String>,
    /// whether the "it is still running" notice has already been shown this run
    told_where_it_went: bool,
}

/// The native window id: an HWND on Windows, an X11 window on Linux.
fn hwnd_of(cc: &eframe::CreationContext<'_>) -> isize {
    match cc.window_handle().map(|h| h.as_raw()) {
        #[cfg(windows)]
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        #[cfg(target_os = "linux")]
        Ok(RawWindowHandle::Xlib(h)) => h.window as isize,
        #[cfg(target_os = "linux")]
        Ok(RawWindowHandle::Xcb(h)) => h.window.get() as isize,
        _ => 0,
    }
}

fn c32(c: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

impl ZoomApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        s: Settings,
        screenshot_path: Option<PathBuf>,
        start_hidden: bool,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_visuals(egui::Visuals::light());
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let hwnd = hwnd_of(cc);
        let (tx, rx) = mpsc::channel();
        win::start(ctx.clone(), tx, hwnd, screenshot_path.is_none(), s.live_level, win::defs_from(&s.hotkeys));
        let (shot_tx, shot_rx) = mpsc::channel();
        if screenshot_path.is_none() {
            crate::fx::start(fx_config(&s));
        }
        let pen_color = c32(s.pen_color);
        Self {
            hwnd,
            rx,
            mode: if start_hidden { Mode::Hidden } else { Mode::Options },
            tab: Tab::Zoom,
            tex: None,
            img: None,
            img_size: vec2(1.0, 1.0),
            monitor: ScreenRect::default(),
            log_zoom: 0.0,
            zoom_target: 1.0,
            origin: Vec2::ZERO,
            anchor_img: Pos2::ZERO,
            anchor_px: Pos2::ZERO,
            snap_origin: true,
            exiting: false,
            draw_from_zoom: false,
            annots: Vec::new(),
            drawing: None,
            pen_color,
            highlighter: false,
            board: None,
            typing: None,
            typing_started_frame: 0,
            break_end: Instant::now(),
            break_beeped: false,
            snip_start: None,
            snip_target: SnipTarget::Copy,
            show_stage: 0,
            show_deadline: Instant::now(),
            shot_rx,
            shot_tx,
            shot_pending: None,
            screenshot_path,
            frames: 0,
            toast: None,
            last_frame_ms: 0.0,
            capturing: None,
            hk_msg: None,
            told_where_it_went: false,
            s,
        }
    }

    fn zoom(&self) -> f32 {
        self.log_zoom.exp()
    }

    // -----------------------------------------------------------------------
    // messages from the hotkey thread

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match (msg.action, msg.capture) {
                (Action::Exit, _) => self.quit(),
                (Action::Options, _) => {
                    if matches!(self.mode, Mode::Hidden | Mode::Options) {
                        self.open_options(ctx);
                    }
                }
                (Action::Screenshot, Some(cap)) => {
                    if copy_image(&cap.image) {
                        win::notify(
                            "Captura copiada",
                            &format!("{} × {} px en el portapapeles. Pégala con Ctrl+V.", cap.rect.w, cap.rect.h),
                        );
                    } else {
                        win::notify("Captura", "No se pudo copiar al portapapeles.");
                    }
                }
                (Action::Screenshot, None) => {
                    if matches!(self.mode, Mode::Zoom | Mode::Draw) {
                        self.request_window_shot(ctx, ShotKind::Copy);
                    }
                }
                (Action::GifDone, _) => {
                    if let Some(tmp) = msg.file {
                        save_gif(&tmp);
                    }
                }
                (a, Some(cap)) => self.begin(ctx, a, cap),
                (a, None) => self.toggle(a),
            }
        }
        while let Ok(img) = self.shot_rx.try_recv() {
            self.on_window_shot(ctx, img);
        }
        if let Some(sh) = win::shared_state() {
            if sh.live_error.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.toast("El zoom en vivo no está disponible en este equipo");
            }
        }
    }

    fn quit(&mut self) {
        self.s.save();
        win::remove_tray();
        std::process::exit(0);
    }

    fn open_options(&mut self, ctx: &egui::Context) {
        self.mode = Mode::Options;
        let scale = win::dpi_scale_for(self.hwnd).max(ctx.pixels_per_point());
        win::place_centered(self.hwnd, (680.0 * scale) as i32, (580.0 * scale) as i32);
        win::activate(self.hwnd);
        ctx.request_repaint();
    }

    // -----------------------------------------------------------------------
    // shortcut editor

    fn start_capture(&mut self, slot: usize) {
        self.capturing = Some(slot);
        self.hk_msg = None;
        // otherwise pressing e.g. Ctrl+1 here would start a zoom instead of being recorded
        win::suspend_hotkeys(true);
    }

    fn end_capture(&mut self) {
        self.capturing = None;
        win::set_hotkeys(win::defs_from(&self.s.hotkeys));
    }

    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.capturing else { return };
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            let Event::Key { key, physical_key, pressed: true, modifiers, .. } = ev else { continue };
            let key = physical_key.unwrap_or(key);
            if key == Key::Escape {
                self.hk_msg = None;
                self.end_capture();
                return;
            }
            let Some(vk) = key_to_vk(key) else { continue };
            let is_f = (0x70..=0x87).contains(&vk);
            if !(modifiers.ctrl || modifiers.alt) && !is_f {
                self.hk_msg = Some("Combina la tecla con Ctrl o Alt (o usa F1–F24).".into());
                continue;
            }
            let hk = Hotkey { ctrl: modifiers.ctrl, shift: modifiers.shift, alt: modifiers.alt, vk };
            if let Some(j) = (0..HOTKEY_SLOTS.len()).find(|&j| j != slot && self.s.hotkeys.get(j) == hk) {
                self.hk_msg = Some(format!("{} ya se usa para «{}».", hk.label(), HOTKEY_SLOTS[j].1));
                continue;
            }
            *self.s.hotkeys.get_mut(slot) = hk;
            self.hk_msg = None;
            self.end_capture();
            self.s.save();
            return;
        }
    }

    fn hotkey_button(&mut self, ui: &mut egui::Ui, slot: usize, accent: Color32) {
        let capturing = self.capturing == Some(slot);
        let (text, fg, bg) = if capturing {
            ("Pulsa la combinación…".to_string(), Color32::WHITE, accent)
        } else {
            (self.s.hotkeys.get(slot).label(), accent, Color32::WHITE)
        };
        let b = egui::Button::new(egui::RichText::new(text).monospace().color(fg))
            .fill(bg)
            .stroke(Stroke::new(1.0, Color32::from_gray(190)))
            .corner_radius(4);
        let r = ui.add(b).on_hover_text("Haz clic y pulsa la nueva combinación (Esc cancela)");
        if r.clicked() {
            if capturing {
                self.end_capture();
            } else {
                self.start_capture(slot);
            }
        }
    }

    fn heading(&mut self, ui: &mut egui::Ui, title: &str, slot: Option<usize>, fixed: &str, accent: Color32) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).size(20.0).strong().color(Color32::from_gray(20)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(slot) = slot {
                    self.hotkey_button(ui, slot, accent);
                } else if !fixed.is_empty() {
                    egui::Frame::new()
                        .fill(Color32::WHITE)
                        .stroke(Stroke::new(1.0, Color32::from_gray(190)))
                        .corner_radius(4)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| ui.label(egui::RichText::new(fixed).monospace().color(accent)));
                }
            });
        });
        if let Some(slot) = slot {
            self.hotkey_status(ui, slot);
        }
    }

    /// Error / hint line under a shortcut button.
    fn hotkey_status(&self, ui: &mut egui::Ui, slot: usize) {
        let red = Color32::from_rgb(190, 40, 30);
        if self.capturing == Some(slot) {
            let m = self.hk_msg.clone().unwrap_or_else(|| "Esperando… (Esc para cancelar)".into());
            ui.label(egui::RichText::new(m).small().color(if self.hk_msg.is_some() { red } else { Color32::from_gray(90) }));
        } else if let Some(sh) = win::shared_state() {
            if let Some((_, false)) = sh.hotkeys.lock().unwrap().get(slot) {
                ui.label(egui::RichText::new("⚠ Otro programa ya usa esta combinación; elige otra.").small().color(red));
            }
        }
    }

    fn hide_options(&mut self) {
        if self.capturing.is_some() {
            self.end_capture();
        }
        self.s.save();
        self.mode = Mode::Hidden;
        win::hide(self.hwnd);
        // Without a tray icon (GNOME removed the system tray) the program would seem to be
        // gone, so the first time it hides it says where to find it again.
        if !self.told_where_it_went && !win::has_tray() {
            self.told_where_it_went = true;
            let key = self.s.hotkeys.options;
            let how = if key.vk == 0 {
                "Vuelve a abrirlo desde el menú de aplicaciones para ver las opciones.".to_string()
            } else {
                format!("Pulsa {} para volver a las opciones.", key.label())
            };
            win::notify("Presenter Toolbox sigue activo", &how);
        }
    }

    fn begin(&mut self, ctx: &egui::Context, a: Action, cap: win::Capture) {
        let opts = if self.s.smooth_image { TextureOptions::LINEAR } else { TextureOptions::NEAREST };
        let size = cap.image.size;
        let img = Arc::new(cap.image);
        // one upload; every frame after this is just a textured quad on the GPU
        self.tex = Some(ctx.load_texture("screen", ColorImage::clone(&img), opts));
        self.img = Some(img);
        self.img_size = vec2(size[0] as f32, size[1] as f32);
        self.monitor = cap.rect;
        self.annots.clear();
        self.drawing = None;
        self.typing = None;
        self.board = None;
        self.exiting = false;
        self.snap_origin = true;
        self.highlighter = false;
        self.pen_color = c32(self.s.pen_color);
        let (cx, cy) = win::cursor_pos();
        let cur = pos2((cx - cap.rect.x) as f32, (cy - cap.rect.y) as f32);
        self.anchor_px = cur;
        self.anchor_img = cur;
        match a {
            Action::Zoom => {
                self.mode = Mode::Zoom;
                self.zoom_target = self.s.zoom_level.clamp(MIN_ZOOM, MAX_ZOOM);
                self.log_zoom = if self.s.animate { 0.0 } else { self.zoom_target.ln() };
            }
            Action::Draw => {
                self.mode = Mode::Draw;
                self.draw_from_zoom = false;
                self.zoom_target = 1.0;
                self.log_zoom = 0.0;
            }
            Action::Break => {
                self.mode = Mode::Break;
                self.break_end = Instant::now() + Duration::from_secs(self.s.break_minutes as u64 * 60);
                self.break_beeped = false;
            }
            Action::Snip | Action::SnipSave | Action::Gif => {
                self.mode = Mode::Snip;
                self.snip_start = None;
                self.snip_target = match a {
                    Action::SnipSave => SnipTarget::Save,
                    Action::Gif => SnipTarget::Gif,
                    _ => SnipTarget::Copy,
                };
            }
            _ => {}
        }
        // Resize while still hidden, paint one frame, then reveal: no flash of old content.
        win::place_overlay(self.hwnd, cap.rect, false);
        self.show_stage = 1;
        self.show_deadline = Instant::now() + Duration::from_millis(160);
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        ctx.request_repaint();
    }

    fn reveal(&mut self) {
        self.show_stage = 0;
        win::place_overlay(self.hwnd, self.monitor, true);
        win::activate(self.hwnd);
    }

    fn toggle(&mut self, a: Action) {
        match (a, self.mode) {
            (Action::Zoom, Mode::Zoom | Mode::Draw) => self.exit_all(),
            (Action::Draw, Mode::Zoom) => self.enter_draw(),
            (Action::Draw, Mode::Draw) => self.exit_all(),
            (Action::Break, Mode::Break) => self.hide_overlay(),
            (_, Mode::Hidden | Mode::Options) => {
                // the capture was dropped somewhere; make sure we are not stuck
                if let Some(sh) = win::shared_state() {
                    sh.overlay_active.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    fn exit_all(&mut self) {
        self.commit_typing();
        if self.s.animate && self.zoom() > 1.01 {
            self.exiting = true;
            self.zoom_target = 1.0;
            self.mode = Mode::Zoom;
            self.anchor_px = self.cursor_px();
        } else {
            self.hide_overlay();
        }
    }

    fn hide_overlay(&mut self) {
        win::hide(self.hwnd);
        self.mode = Mode::Hidden;
        self.exiting = false;
        self.tex = None;
        self.img = None;
        self.show_stage = 0;
        if let Some(sh) = win::shared_state() {
            sh.overlay_active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn cursor_px(&self) -> Pos2 {
        let (cx, cy) = win::cursor_pos();
        pos2((cx - self.monitor.x) as f32, (cy - self.monitor.y) as f32)
    }

    fn enter_draw(&mut self) {
        let cur = self.cursor_px();
        self.anchor_img = pos2(self.origin.x, self.origin.y) + cur.to_vec2() / self.zoom();
        self.anchor_px = cur;
        self.draw_from_zoom = true;
        self.mode = Mode::Draw;
        self.zoom_target = self.zoom();
        self.log_zoom = self.zoom_target.ln();
    }

    fn toast(&mut self, t: &str) {
        self.toast = Some((t.to_string(), Instant::now()));
    }

    /// Test mode (`--screenshot out.png --demo zoom|draw|break|snip`): uses a made-up
    /// desktop picture instead of the real screen, so nothing private ends up in the file.
    fn start_demo(&mut self, ctx: &egui::Context) {
        let args: Vec<String> = std::env::args().collect();
        let Some(demo) = args.iter().position(|a| a == "--demo").and_then(|i| args.get(i + 1)).cloned() else {
            return;
        };
        let rect = win::monitor_at_cursor();
        let image = demo_desktop(rect.w.max(64) as usize, rect.h.max(64) as usize);
        let action = match demo.as_str() {
            "draw" => Action::Draw,
            "break" => Action::Break,
            "snip" => Action::Snip,
            _ => Action::Zoom,
        };
        self.begin(ctx, action, win::Capture { rect, image });
        if action == Action::Zoom || action == Action::Draw {
            let w = rect.w as f32;
            let h = rect.h as f32;
            let c = pos2(w * 0.5, h * 0.5);
            self.anchor_px = c;
            self.anchor_img = c;
            let red = Color32::from_rgb(230, 30, 30);
            self.annots.push(Annot {
                kind: Kind::Ellipse(c + vec2(-w * 0.12, -h * 0.08), c + vec2(w * 0.12, h * 0.08)),
                color: red,
                width: 3.0,
            });
            self.annots.push(Annot {
                kind: Kind::Arrow(c + vec2(-w * 0.3, h * 0.2), c + vec2(-w * 0.13, h * 0.06)),
                color: Color32::from_rgb(30, 90, 230),
                width: 3.0,
            });
            let wave: Vec<Pos2> = (0..80)
                .map(|i| c + vec2(-w * 0.1 + i as f32 * w * 0.0025, h * 0.12 + (i as f32 * 0.25).sin() * 6.0))
                .collect();
            self.annots.push(Annot { kind: Kind::Free(wave), color: Color32::from_rgba_unmultiplied(250, 220, 0, 90), width: 9.0 });
            self.annots.push(Annot {
                kind: Kind::Text { pos: c + vec2(-w * 0.3, h * 0.22), text: "¡Mira aquí!".into(), size: 16.0, right: false },
                color: Color32::from_rgb(30, 90, 230),
                width: 1.0,
            });
            if action == Action::Zoom {
                self.zoom_target = 2.0;
            }
        }
        if action == Action::Snip {
            let c = pos2(rect.w as f32 * 0.3, rect.h as f32 * 0.3) / ctx.pixels_per_point();
            self.snip_start = Some(c);
        }
    }

    // -----------------------------------------------------------------------
    // copy / save the picture on screen

    fn request_window_shot(&mut self, ctx: &egui::Context, kind: ShotKind) {
        let tx = self.shot_tx.clone();
        let c = ctx.clone();
        self.shot_pending = Some(kind);
        ctx.request_screenshot(move |img| {
            let _ = tx.send(img);
            c.request_repaint();
        });
        ctx.request_repaint();
    }

    fn on_window_shot(&mut self, ctx: &egui::Context, img: Arc<ColorImage>) {
        let Some(kind) = self.shot_pending.take() else { return };
        match kind {
            ShotKind::Copy => {
                if copy_image(&img) {
                    self.toast("Copiado al portapapeles");
                } else {
                    self.toast("No se pudo copiar");
                }
            }
            ShotKind::Save => {
                if let Some(p) = win::save_dialog(self.hwnd, "ZoomIt.png") {
                    if save_png(&img, &p) {
                        self.toast("Imagen guardada");
                    } else {
                        self.toast("No se pudo guardar");
                    }
                }
                win::activate(self.hwnd);
            }
            ShotKind::File => {
                if let Some(p) = self.screenshot_path.clone() {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    save_png(&img, &p);
                }
                std::process::exit(0);
            }
        }
        ctx.request_repaint();
    }

    // -----------------------------------------------------------------------
    // zoom + draw

    fn view_size(&self) -> Vec2 {
        self.img_size / self.zoom()
    }

    fn zoom_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ctx.content_rect();
        let ppp = ctx.pixels_per_point();
        let dt = ctx.input(|i| i.unstable_dt).clamp(0.001, 0.05);
        let cur_px = self.cursor_px();
        let cur_pts = rect.min + cur_px.to_vec2() / ppp;

        self.zoom_input(&ctx, cur_px);
        if self.mode == Mode::Hidden {
            return;
        }

        // --- animate zoom in log space (constant perceived speed, 60-240 Hz)
        let target_lz = self.zoom_target.ln();
        if self.s.animate {
            self.log_zoom += (target_lz - self.log_zoom) * (1.0 - (-dt * 13.0).exp());
            if (target_lz - self.log_zoom).abs() < 0.0015 {
                self.log_zoom = target_lz;
            }
        } else {
            self.log_zoom = target_lz;
        }
        let z = self.zoom();
        let view = self.view_size();

        // --- where to look
        let mut target = if self.mode == Mode::Zoom && self.typing.is_none() {
            // proportional panning: the screen edge is reached when the cursor reaches the edge
            cur_px.to_vec2() * (1.0 - 1.0 / z)
        } else {
            self.anchor_img.to_vec2() - self.anchor_px.to_vec2() / z
        };
        let max = (self.img_size - view).max(Vec2::ZERO);
        target = target.clamp(Vec2::ZERO, max);
        if self.snap_origin || !self.s.animate {
            self.origin = target;
            self.snap_origin = false;
        } else {
            self.origin += (target - self.origin) * (1.0 - (-dt * 30.0).exp());
            if (target - self.origin).length() < 0.05 {
                self.origin = target;
            }
        }
        let settled = self.log_zoom == target_lz && self.origin == target;

        if self.exiting && z <= 1.004 {
            self.hide_overlay();
            return;
        }

        // --- paint
        let painter = ui.painter();
        let origin = self.origin;
        let scale = z / ppp; // image px -> points
        let to_screen = |p: Pos2| rect.min + (p.to_vec2() - origin) * scale;
        if let Some(b) = self.board {
            painter.rect_filled(rect, 0.0, b);
        } else if let Some(tex) = &self.tex {
            let uv = Rect::from_min_size(
                pos2(origin.x / self.img_size.x, origin.y / self.img_size.y),
                vec2(view.x / self.img_size.x, view.y / self.img_size.y),
            );
            painter.image(tex.id(), rect, uv, Color32::WHITE);
        }
        for a in &self.annots {
            paint_annot(painter, a, &to_screen, scale);
        }
        if let Some((start, pts)) = &self.drawing {
            if let Some(a) = self.shape_from_drag(&ctx, *start, pts) {
                paint_annot(painter, &a, &to_screen, scale);
            }
        }
        if let Some(t) = &self.typing {
            paint_annot(painter, t, &to_screen, scale);
            // blinking caret
            if let Kind::Text { pos, text, size, right } = &t.kind {
                let font = FontId::proportional(size * scale);
                let last = text.rsplit('\n').next().unwrap_or("");
                let lines = text.split('\n').count().max(1) as f32;
                let lw = painter.layout_no_wrap(last.to_string(), font.clone(), t.color).size().x;
                let row_h = painter.layout_no_wrap("Ag".into(), font, t.color).size().y;
                let p = to_screen(*pos);
                let x = if *right { p.x } else { p.x + lw };
                let y = p.y + (lines - 1.0) * row_h;
                if (ctx.input(|i| i.time) * 2.0) as i64 % 2 == 0 {
                    painter.line_segment([pos2(x + 1.0, y), pos2(x + 1.0, y + row_h)], Stroke::new(2.0, t.color));
                }
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        let hide_hud = self.shot_pending.is_some();
        if self.mode == Mode::Draw && self.typing.is_none() && !hide_hud {
            ctx.set_cursor_icon(CursorIcon::None);
            let w = self.effective_width_px() / ppp;
            let col = self.current_color();
            painter.circle_stroke(cur_pts, (w / 2.0).max(2.0), Stroke::new(1.5, col));
            painter.circle_filled(cur_pts, 1.5, col);
        } else if self.typing.is_some() {
            ctx.set_cursor_icon(CursorIcon::Text);
        }
        if !hide_hud {
            self.paint_toast(ui, rect);
        }

        if !settled || self.drawing.is_some() {
            ctx.request_repaint();
        }
    }

    fn current_color(&self) -> Color32 {
        if self.highlighter {
            let [r, g, b, _] = self.pen_color.to_array();
            Color32::from_rgba_unmultiplied(r, g, b, 90)
        } else {
            self.pen_color
        }
    }

    /// Pen width in screen pixels.
    fn effective_width_px(&self) -> f32 {
        if self.highlighter { self.s.pen_width * 3.0 } else { self.s.pen_width }
    }

    fn to_img(&self, px: Pos2) -> Pos2 {
        pos2(self.origin.x, self.origin.y) + px.to_vec2() / self.zoom()
    }

    fn shape_from_drag(&self, ctx: &egui::Context, start: Pos2, pts: &[Pos2]) -> Option<Annot> {
        let end = *pts.last()?;
        let (m, tab) = ctx.input(|i| (i.modifiers, i.key_down(Key::Tab)));
        let kind = if m.ctrl && m.shift {
            Kind::Arrow(start, end)
        } else if m.ctrl {
            Kind::Rect(start, end)
        } else if tab {
            Kind::Ellipse(start, end)
        } else if m.shift {
            Kind::Line(start, end)
        } else {
            Kind::Free(smooth_stroke(pts, self.s.stroke_smoothing))
        };
        Some(Annot { kind, color: self.current_color(), width: self.effective_width_px() / self.zoom() })
    }

    fn set_zoom_steps(&mut self, steps: f32, cur_px: Pos2) {
        if steps == 0.0 {
            return;
        }
        // keep the pixel under the cursor fixed while zooming in draw mode
        self.anchor_img = self.to_img(cur_px);
        self.anchor_px = cur_px;
        self.zoom_target = (self.zoom_target * 1.25f32.powf(steps)).clamp(MIN_ZOOM, MAX_ZOOM);
    }

    fn commit_typing(&mut self) {
        if let Some(t) = self.typing.take() {
            if let Kind::Text { text, .. } = &t.kind {
                if !text.trim().is_empty() {
                    self.annots.push(t);
                }
            }
        }
    }

    fn start_typing(&mut self, cur_px: Pos2, right: bool) {
        self.commit_typing();
        if self.mode == Mode::Zoom {
            self.enter_draw();
        }
        let z = self.zoom();
        self.typing = Some(Annot {
            kind: Kind::Text { pos: self.to_img(cur_px), text: String::new(), size: self.s.font_size / z, right },
            color: self.pen_color,
            width: 1.0,
        });
        self.typing_started_frame = self.frames;
    }

    fn zoom_input(&mut self, ctx: &egui::Context, cur_px: Pos2) {
        let events = ctx.input(|i| i.events.clone());
        let (pressed, down, released, secondary, mods) = ctx.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.secondary_pressed(),
                i.modifiers,
            )
        });
        if self.exiting {
            return;
        }

        for ev in &events {
            match ev {
                Event::MouseWheel { unit, delta, modifiers, .. } => {
                    let steps = match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Page => delta.y * 3.0,
                        egui::MouseWheelUnit::Point => delta.y / 50.0,
                    };
                    if let Some(t) = &mut self.typing {
                        if let Kind::Text { size, .. } = &mut t.kind {
                            *size = (*size * 1.12f32.powf(steps)).clamp(2.0, 2000.0);
                        }
                    } else if modifiers.ctrl && self.mode == Mode::Draw {
                        self.s.pen_width = (self.s.pen_width + steps * 2.0).clamp(1.0, 80.0);
                    } else {
                        self.set_zoom_steps(steps, cur_px);
                    }
                }
                Event::Copy => self.request_window_shot(ctx, ShotKind::Copy),
                Event::Text(t) if self.typing.is_some() => {
                    if self.frames != self.typing_started_frame {
                        if let Some(Annot { kind: Kind::Text { text, .. }, .. }) = &mut self.typing {
                            text.push_str(t);
                        }
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => {
                    self.on_key(ctx, *key, *modifiers, cur_px);
                    if self.mode == Mode::Hidden {
                        return;
                    }
                }
                _ => {}
            }
        }

        // mouse
        if secondary {
            if self.typing.is_some() {
                self.commit_typing();
            } else if self.mode == Mode::Draw && self.draw_from_zoom {
                self.drawing = None;
                self.mode = Mode::Zoom;
            } else {
                self.exit_all();
                return;
            }
        }
        if pressed {
            if self.typing.is_some() {
                self.commit_typing();
            } else {
                if self.mode == Mode::Zoom {
                    self.enter_draw();
                }
                let p = self.to_img(cur_px);
                self.drawing = Some((p, vec![p]));
            }
        }
        if let Some((_, pts)) = &mut self.drawing {
            if down {
                let p = pos2(self.origin.x, self.origin.y) + cur_px.to_vec2() / self.log_zoom.exp();
                if pts.last().is_none_or(|l| l.distance(p) * self.log_zoom.exp() > 0.8) {
                    pts.push(p);
                }
            }
        }
        if released || (!down && self.drawing.is_some()) {
            if let Some((start, pts)) = self.drawing.take() {
                if let Some(a) = self.shape_from_drag(ctx, start, &pts) {
                    self.annots.push(a);
                }
            }
        }
        let _ = mods;
    }

    fn on_key(&mut self, ctx: &egui::Context, key: Key, m: egui::Modifiers, cur_px: Pos2) {
        if self.typing.is_some() {
            if let Some(Annot { kind: Kind::Text { text, .. }, .. }) = &mut self.typing {
                match key {
                    Key::Escape => self.commit_typing(),
                    Key::Enter => text.push('\n'),
                    Key::Backspace => {
                        text.pop();
                    }
                    _ => {}
                }
            }
            return;
        }
        match key {
            Key::Escape => self.exit_all(),
            Key::ArrowUp if m.ctrl && self.mode == Mode::Draw => {
                self.s.pen_width = (self.s.pen_width + 2.0).clamp(1.0, 80.0)
            }
            Key::ArrowDown if m.ctrl && self.mode == Mode::Draw => {
                self.s.pen_width = (self.s.pen_width - 2.0).clamp(1.0, 80.0)
            }
            Key::ArrowUp => self.set_zoom_steps(1.0, cur_px),
            Key::ArrowDown => self.set_zoom_steps(-1.0, cur_px),
            Key::S if m.ctrl => self.request_window_shot(ctx, ShotKind::Save),
            Key::S => {
                let n = SMOOTHING_NAMES.len() as u32;
                self.s.stroke_smoothing = if m.shift {
                    (self.s.stroke_smoothing + n - 1) % n
                } else {
                    (self.s.stroke_smoothing + 1) % n
                };
                let name = SMOOTHING_NAMES[self.s.stroke_smoothing.min(n - 1) as usize];
                self.toast(&format!("Suavizado del trazo: {name}"));
            }
            Key::Z if m.ctrl => {
                self.annots.pop();
            }
            Key::T => self.start_typing(cur_px, m.shift),
            Key::E if !m.ctrl => {
                self.annots.clear();
            }
            Key::W if !m.ctrl => {
                self.board = if self.board == Some(Color32::WHITE) { None } else { Some(Color32::WHITE) };
                if self.mode == Mode::Zoom {
                    self.enter_draw();
                }
            }
            Key::K if !m.ctrl => {
                self.board = if self.board == Some(Color32::BLACK) { None } else { Some(Color32::BLACK) };
                if self.mode == Mode::Zoom {
                    self.enter_draw();
                }
            }
            Key::R | Key::G | Key::B | Key::O | Key::Y | Key::P if !m.ctrl => {
                self.pen_color = match key {
                    Key::R => Color32::from_rgb(230, 30, 30),
                    Key::G => Color32::from_rgb(20, 170, 50),
                    Key::B => Color32::from_rgb(30, 90, 230),
                    Key::O => Color32::from_rgb(250, 140, 0),
                    Key::Y => Color32::from_rgb(250, 220, 0),
                    _ => Color32::from_rgb(235, 60, 190),
                };
                self.highlighter = m.shift;
                self.s.pen_color = self.pen_color.to_array();
                if self.mode == Mode::Zoom {
                    self.enter_draw();
                }
            }
            _ => {}
        }
    }

    fn paint_toast(&mut self, ui: &egui::Ui, rect: Rect) {
        let mut text = None;
        if let Some((t, at)) = &self.toast {
            if at.elapsed() < Duration::from_millis(1800) {
                text = Some(t.clone());
            }
        }
        match text {
            Some(t) => {
                let p = ui.painter();
                let font = FontId::proportional(18.0);
                let g = p.layout_no_wrap(t, font, Color32::WHITE);
                let r = Rect::from_center_size(pos2(rect.center().x, rect.max.y - 60.0), g.size() + vec2(28.0, 16.0));
                p.rect_filled(r, CornerRadius::same(8), Color32::from_black_alpha(200));
                p.galley(r.min + vec2(14.0, 8.0), g, Color32::WHITE);
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
            None => self.toast = None,
        }
    }

    // -----------------------------------------------------------------------
    // break timer

    fn break_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ctx.content_rect();
        let events = ctx.input(|i| i.events.clone());
        for ev in &events {
            match ev {
                Event::Key { key, pressed: true, .. } => match key {
                    Key::Escape => {
                        self.hide_overlay();
                        return;
                    }
                    Key::ArrowUp => self.adjust_break(60),
                    Key::ArrowDown => self.adjust_break(-60),
                    Key::R => self.s.break_color = [230, 30, 30, 255],
                    Key::G => self.s.break_color = [20, 170, 50, 255],
                    Key::B => self.s.break_color = [30, 90, 230, 255],
                    Key::Y => self.s.break_color = [250, 220, 0, 255],
                    Key::W => self.s.break_color = [255, 255, 255, 255],
                    Key::K => self.s.break_color = [0, 0, 0, 255],
                    _ => {}
                },
                Event::MouseWheel { delta, .. } => {
                    if delta.y > 0.0 {
                        self.adjust_break(60)
                    } else if delta.y < 0.0 {
                        self.adjust_break(-60)
                    }
                }
                _ => {}
            }
        }
        if ctx.input(|i| i.pointer.secondary_pressed()) {
            self.hide_overlay();
            return;
        }

        let painter = ui.painter();
        if let Some(tex) = &self.tex {
            let v = (255.0 * (1.0 - self.s.break_dim.clamp(0.0, 1.0))) as u8;
            painter.image(
                tex.id(),
                rect,
                Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                Color32::from_rgb(v, v, v),
            );
        } else {
            painter.rect_filled(rect, 0.0, Color32::from_gray(30));
        }
        let now = Instant::now();
        let (secs, over) = if now < self.break_end {
            ((self.break_end - now).as_secs_f32().ceil() as u64, false)
        } else {
            ((now - self.break_end).as_secs(), true)
        };
        if over && !self.break_beeped {
            self.break_beeped = true;
            if self.s.break_sound {
                win::beep();
            }
        }
        let col = c32(self.s.break_color);
        let text = if over && !self.s.break_show_elapsed {
            "0:00".to_string()
        } else {
            format!("{}{}:{:02}", if over { "-" } else { "" }, secs / 60, secs % 60)
        };
        let size = (rect.height() * 0.24).max(40.0);
        painter.text(
            rect.center() + vec2(3.0, 3.0),
            Align2::CENTER_CENTER,
            &text,
            FontId::monospace(size),
            Color32::from_black_alpha(120),
        );
        painter.text(rect.center(), Align2::CENTER_CENTER, &text, FontId::monospace(size), col);
        if over {
            painter.text(
                rect.center() + vec2(0.0, size * 0.75),
                Align2::CENTER_CENTER,
                "¡Se acabó el descanso!",
                FontId::proportional(size * 0.22),
                col,
            );
        }
        painter.text(
            pos2(rect.center().x, rect.max.y - 40.0),
            Align2::CENTER_CENTER,
            "↑ / ↓ o rueda: ±1 minuto     R G B Y W K: color     Esc: salir",
            FontId::proportional(16.0),
            Color32::from_white_alpha(200),
        );
        // redraw exactly when the next second ticks over
        let frac = if over {
            1.0 - (now - self.break_end).as_secs_f32().fract()
        } else {
            (self.break_end - now).as_secs_f32().fract()
        };
        ctx.request_repaint_after(Duration::from_secs_f32(frac.max(0.01) + 0.005));
    }

    fn adjust_break(&mut self, secs: i64) {
        let now = Instant::now();
        let remaining = self.break_end.saturating_duration_since(now).as_secs() as i64;
        let next = (remaining + secs).max(60);
        self.break_end = now + Duration::from_secs(next as u64);
        self.break_beeped = false;
        self.s.break_minutes = ((next + 30) / 60).clamp(1, 600) as u32;
    }

    // -----------------------------------------------------------------------
    // snip

    fn snip_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ctx.content_rect();
        let ppp = ctx.pixels_per_point();
        let cur = rect.min + self.cursor_px().to_vec2() / ppp;
        let (esc, pressed, released, secondary) = ctx.input(|i| {
            (
                i.key_pressed(Key::Escape),
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.secondary_pressed(),
            )
        });
        if esc || secondary {
            self.hide_overlay();
            return;
        }
        if pressed {
            self.snip_start = Some(cur);
        }
        let painter = ui.painter();
        if let Some(tex) = &self.tex {
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
        }
        let dim = Color32::from_black_alpha(110);
        if let Some(start) = self.snip_start {
            let sel = Rect::from_two_pos(start, cur);
            // darken around the selection
            painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.max.x, sel.min.y)), 0.0, dim);
            painter.rect_filled(Rect::from_min_max(pos2(rect.min.x, sel.max.y), rect.max), 0.0, dim);
            painter.rect_filled(Rect::from_min_max(pos2(rect.min.x, sel.min.y), pos2(sel.min.x, sel.max.y)), 0.0, dim);
            painter.rect_filled(Rect::from_min_max(pos2(sel.max.x, sel.min.y), pos2(rect.max.x, sel.max.y)), 0.0, dim);
            painter.rect_stroke(sel, 0.0, Stroke::new(1.5, Color32::from_rgb(0, 150, 255)), StrokeKind::Outside);
            let label = format!("{} × {}", (sel.width() * ppp).round(), (sel.height() * ppp).round());
            painter.text(sel.left_bottom() + vec2(0.0, 6.0), Align2::LEFT_TOP, label, FontId::proportional(14.0), Color32::WHITE);
            if released {
                self.finish_snip(sel, rect, ppp);
                return;
            }
        } else {
            painter.rect_filled(rect, 0.0, dim);
            painter.text(
                pos2(rect.center().x, rect.min.y + 40.0),
                Align2::CENTER_CENTER,
                match self.snip_target {
                    SnipTarget::Save => "Arrastra para guardar una zona  ·  Esc: cancelar",
                    SnipTarget::Gif => "Arrastra para elegir la zona a grabar en GIF  ·  Esc: cancelar",
                    SnipTarget::Copy => "Arrastra para copiar una zona  ·  Esc: cancelar",
                },
                FontId::proportional(18.0),
                Color32::WHITE,
            );
        }
        let guide = Stroke::new(1.0, Color32::from_white_alpha(140));
        painter.line_segment([pos2(rect.min.x, cur.y), pos2(rect.max.x, cur.y)], guide);
        painter.line_segment([pos2(cur.x, rect.min.y), pos2(cur.x, rect.max.y)], guide);
        ctx.set_cursor_icon(CursorIcon::Crosshair);
    }

    fn finish_snip(&mut self, sel: Rect, rect: Rect, ppp: f32) {
        let Some(img) = self.img.clone() else {
            self.hide_overlay();
            return;
        };
        let x0 = (((sel.min.x - rect.min.x) * ppp).round().max(0.0)) as usize;
        let y0 = (((sel.min.y - rect.min.y) * ppp).round().max(0.0)) as usize;
        let x1 = (((sel.max.x - rect.min.x) * ppp).round() as usize).min(img.size[0]);
        let y1 = (((sel.max.y - rect.min.y) * ppp).round() as usize).min(img.size[1]);
        if x1 <= x0 + 2 || y1 <= y0 + 2 {
            self.snip_start = None;
            return;
        }
        let w = x1 - x0;
        if self.snip_target == SnipTarget::Gif {
            let r = ScreenRect {
                x: self.monitor.x + x0 as i32,
                y: self.monitor.y + y0 as i32,
                w: w as i32,
                h: (y1 - y0) as i32,
            };
            self.hide_overlay();
            crate::gif_rec::start(r, self.s.gif_fps);
            return;
        }
        let mut pixels = Vec::with_capacity(w * (y1 - y0));
        for y in y0..y1 {
            pixels.extend_from_slice(&img.pixels[y * img.size[0] + x0..y * img.size[0] + x1]);
        }
        let crop = ColorImage::new([w, y1 - y0], pixels);
        self.hide_overlay();
        if self.snip_target == SnipTarget::Save {
            if let Some(p) = win::save_dialog(0, "Recorte.png") {
                save_png(&crop, &p);
            }
        } else {
            copy_image(&crop);
        }
    }

    // -----------------------------------------------------------------------
    // options window

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.capture_hotkey(&ctx);
        crate::fx::set_config(fx_config(&self.s));
        let rect = ctx.content_rect();
        let accent = Color32::from_rgb(0, 95, 184);
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::from_rgb(243, 243, 243));
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::from_gray(170)), StrokeKind::Inside);

        // title bar
        let title = Rect::from_min_size(rect.min, vec2(rect.width(), 36.0));
        let resp = ui.interact(title, egui::Id::unique("titlebar"), egui::Sense::click_and_drag());
        if resp.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        painter.rect_filled(title, 0.0, Color32::WHITE);
        draw_icon(painter, pos2(title.min.x + 20.0, title.center().y), 9.0);
        painter.text(
            pos2(title.min.x + 38.0, title.center().y),
            Align2::LEFT_CENTER,
            "Presenter Toolbox — Opciones",
            FontId::proportional(14.0),
            Color32::from_gray(30),
        );
        let close = Rect::from_min_max(pos2(title.max.x - 46.0, title.min.y), title.max);
        let cr = ui.interact(close, egui::Id::unique("close"), egui::Sense::click());
        if cr.hovered() {
            painter.rect_filled(close, 0.0, Color32::from_rgb(196, 43, 28));
        }
        painter.text(
            close.center(),
            Align2::CENTER_CENTER,
            "✕",
            FontId::proportional(14.0),
            if cr.hovered() { Color32::WHITE } else { Color32::from_gray(40) },
        );
        if cr.clicked() {
            self.hide_options();
            return;
        }

        let body = Rect::from_min_max(pos2(rect.min.x, title.max.y), rect.max);
        let side = Rect::from_min_max(body.min, pos2(body.min.x + 170.0, body.max.y));
        let content = Rect::from_min_max(pos2(side.max.x + 1.0, body.min.y), body.max).shrink2(vec2(22.0, 16.0));
        ui.painter().rect_filled(side, 0.0, Color32::from_rgb(232, 236, 241));

        let mut side_ui = ui.new_child(egui::UiBuilder::new().max_rect(side.shrink(10.0)));
        side_ui.spacing_mut().item_spacing.y = 4.0;
        let tabs = [
            (Tab::Zoom, "🔍  Zoom"),
            (Tab::Live, "🎥  Zoom en vivo"),
            (Tab::Draw, "✏  Dibujar"),
            (Tab::Type, "🔤  Texto"),
            (Tab::Break, "⏱  Descanso"),
            (Tab::Shot, "📷  Captura"),
            (Tab::Snip, "✂  Recorte"),
            (Tab::Spot, "🔦  Foco"),
            (Tab::Tutorial, "🖱  Clics y teclas"),
            (Tab::Gif, "🎬  Grabar GIF"),
            (Tab::About, "ℹ  Acerca de"),
        ];
        for (t, label) in tabs {
            let selected = self.tab == t;
            let (r, resp) = side_ui.allocate_exact_size(vec2(150.0, 32.0), egui::Sense::click());
            let fill = if selected {
                Color32::WHITE
            } else if resp.hovered() {
                Color32::from_rgb(222, 228, 236)
            } else {
                Color32::TRANSPARENT
            };
            side_ui.painter().rect_filled(r, CornerRadius::same(5), fill);
            if selected {
                side_ui.painter().rect_filled(
                    Rect::from_min_size(r.min + vec2(0.0, 8.0), vec2(3.0, 16.0)),
                    CornerRadius::same(2),
                    accent,
                );
            }
            side_ui.painter().text(
                r.left_center() + vec2(12.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(14.0),
                Color32::from_gray(25),
            );
            if resp.clicked() {
                self.tab = t;
            }
        }

        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(content));
        c.spacing_mut().item_spacing.y = 8.0;
        let changed_live = self.tab_content(&mut c, accent);
        if changed_live {
            if let Some(sh) = win::shared_state() {
                sh.live_level.store(self.s.live_level.to_bits(), std::sync::atomic::Ordering::SeqCst);
            }
        }

        // bottom buttons
        let btn_y = rect.max.y - 50.0;
        let mut b = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(
            pos2(content.min.x, btn_y),
            pos2(content.max.x, rect.max.y - 12.0),
        )));
        b.horizontal(|ui| {
            ui.checkbox(&mut self.s.show_options_on_start, "Mostrar al iniciar");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_sized([96.0, 30.0], egui::Button::new("Salir")).clicked() {
                    self.quit();
                }
                let ok = egui::Button::new(egui::RichText::new("Aceptar").color(Color32::WHITE)).fill(accent);
                if ui.add_sized([96.0, 30.0], ok).clicked() {
                    self.hide_options();
                }
            });
        });
    }

    fn tab_content(&mut self, ui: &mut egui::Ui, accent: Color32) -> bool {
        let mut live_changed = false;
        match self.tab {
            Tab::Zoom => {
                self.heading(ui, "Zoom", Some(0), "", accent);
                ui.label("Congela la pantalla y la amplía. Mueve el ratón para recorrerla; rueda o ↑/↓ para acercar o alejar. Clic izquierdo para dibujar encima; Esc o clic derecho para salir. Ctrl+C copia y Ctrl+S guarda lo que ves.");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Nivel de zoom inicial:");
                    ui.add(egui::Slider::new(&mut self.s.zoom_level, 1.25..=8.0).step_by(0.25).suffix("×"));
                });
                ui.checkbox(&mut self.s.animate, "Animar el zoom (transición suave)");
                ui.checkbox(&mut self.s.smooth_image, "Suavizar la imagen ampliada");
                ui.add_space(6.0);
                perf_note(ui, "Rendimiento: la imagen se sube una sola vez a la tarjeta gráfica; acercar, alejar y moverse sólo cambia la parte visible, a la frecuencia de tu pantalla.");
                ui.add_space(6.0);
                if ui.button("Probar zoom ahora").clicked() {
                    self.hide_options();
                    win::post_action(Action::Zoom);
                }
            }
            Tab::Live => {
                self.heading(ui, "Zoom en vivo", Some(3), "", accent);
                ui.label("Amplía la pantalla sin congelarla: los vídeos siguen reproduciéndose y puedes seguir usando los programas. Ctrl+↑/↓ cambia el nivel; vuelve a pulsar el atajo para salir.");
                ui.horizontal(|ui| {
                    ui.label("Nivel inicial:");
                    live_changed |= ui
                        .add(egui::Slider::new(&mut self.s.live_level, 1.25..=8.0).step_by(0.25).suffix("×"))
                        .changed();
                });
                match win::unavailable(Action::LiveZoom) {
                    Some(why) => unavailable_note(ui, why),
                    None => perf_note(ui, "Rendimiento: el ampliado lo hace el compositor de Windows en la GPU, sincronizado con cada fotograma de la pantalla."),
                }
                ui.label(egui::RichText::new("Funciona en la pantalla principal.").small().color(Color32::from_gray(90)));
            }
            Tab::Draw => {
                self.heading(ui, "Dibujar", Some(1), "", accent);
                ui.label("Dibuja sobre la pantalla sin ampliarla (o haz clic estando en zoom).");
                egui::Grid::new("draw_keys").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for (k, d) in [
                        ("R G B O Y P", "Color del lápiz (con Mayús: resaltador)"),
                        ("Mayús + arrastrar", "Línea recta"),
                        ("Ctrl + arrastrar", "Rectángulo"),
                        ("Tab + arrastrar", "Elipse"),
                        ("Ctrl+Mayús + arrastrar", "Flecha"),
                        ("Ctrl + rueda / ↑↓", "Grosor del lápiz"),
                        ("S / Mayús+S", "Más / menos suavizado del trazo"),
                        ("E", "Borrar todo"),
                        ("W / K", "Pizarra blanca / negra"),
                        ("Ctrl+Z", "Deshacer"),
                        ("Clic derecho", "Volver al zoom / salir"),
                    ] {
                        ui.label(egui::RichText::new(k).monospace().color(accent));
                        ui.label(d);
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Color:");
                    let mut col = c32(self.s.pen_color);
                    if ui.color_edit_button_srgba(&mut col).changed() {
                        self.s.pen_color = col.to_array();
                    }
                    ui.label("  Grosor:");
                    ui.add(egui::Slider::new(&mut self.s.pen_width, 1.0..=40.0).suffix(" px"));
                });
                ui.horizontal(|ui| {
                    ui.label("Suavizado del trazo:");
                    let cur = SMOOTHING_NAMES[(self.s.stroke_smoothing as usize).min(SMOOTHING_NAMES.len() - 1)];
                    egui::ComboBox::from_id_salt("stroke_smoothing").selected_text(cur).show_ui(ui, |ui| {
                        for (i, name) in SMOOTHING_NAMES.iter().enumerate() {
                            ui.selectable_value(&mut self.s.stroke_smoothing, i as u32, *name);
                        }
                    });
                    ui.label(egui::RichText::new("quita el temblor de la mano").small().color(Color32::from_gray(90)));
                });
            }
            Tab::Type => {
                self.heading(ui, "Texto", None, "T", accent);
                ui.label("En modo dibujo pulsa T para escribir donde está el ratón (Mayús+T: alineado a la derecha). La rueda cambia el tamaño, Intro hace un salto de línea y Esc termina.");
                ui.horizontal(|ui| {
                    ui.label("Tamaño de letra:");
                    ui.add(egui::Slider::new(&mut self.s.font_size, 10.0..=120.0).suffix(" px"));
                });
                let col = c32(self.s.pen_color);
                egui::Frame::new().fill(Color32::WHITE).inner_margin(10).corner_radius(4).show(ui, |ui| {
                    ui.label(egui::RichText::new("Ejemplo de texto").size(self.s.font_size.min(48.0)).color(col));
                });
            }
            Tab::Break => {
                self.heading(ui, "Temporizador de descanso", Some(2), "", accent);
                ui.label("Muestra una cuenta atrás a pantalla completa. ↑/↓ o la rueda ajustan los minutos; Esc la cierra.");
                ui.horizontal(|ui| {
                    ui.label("Minutos:");
                    ui.add(egui::DragValue::new(&mut self.s.break_minutes).range(1..=600));
                });
                ui.checkbox(&mut self.s.break_show_elapsed, "Mostrar el tiempo pasado al terminar");
                ui.checkbox(&mut self.s.break_sound, "Sonido al terminar");
                ui.horizontal(|ui| {
                    ui.label("Oscurecer el fondo:");
                    ui.add(egui::Slider::new(&mut self.s.break_dim, 0.0..=1.0).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                });
                ui.horizontal(|ui| {
                    ui.label("Color del reloj:");
                    let mut col = c32(self.s.break_color);
                    if ui.color_edit_button_srgba(&mut col).changed() {
                        self.s.break_color = col.to_array();
                    }
                });
            }
            Tab::Snip => {
                self.heading(ui, "Recorte", Some(5), "", accent);
                ui.label("Arrastra para elegir una zona de la pantalla y se copia al portapapeles.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Recorte guardado en archivo PNG:");
                    self.hotkey_button(ui, 6, accent);
                });
                self.hotkey_status(ui, 6);
            }
            Tab::Shot => {
                self.heading(ui, "Captura de pantalla", Some(4), "", accent);
                ui.label("Copia al instante la pantalla donde está el ratón al portapapeles, sin abrir nada. Después la pegas con Ctrl+V en Word, WhatsApp, Paint, un correo…");
                ui.label("Si estás en zoom o dibujando, copia exactamente lo que ves, con tus anotaciones.");
                ui.add_space(4.0);
                perf_note(ui, "Aparece un aviso del sistema confirmando que la captura quedó copiada.");
            }
            Tab::Spot => {
                self.heading(ui, "Foco", Some(7), "", accent);
                if let Some(why) = win::unavailable(Action::Spotlight) {
                    unavailable_note(ui, why);
                }
                ui.label("Oscurece toda la pantalla menos un círculo alrededor del mouse, para que todos miren donde tú quieres. Puedes seguir haciendo clic y trabajando normalmente. Vuelve a pulsar el atajo para apagarlo.");
                ui.horizontal(|ui| {
                    ui.label("Tamaño del círculo:");
                    ui.add(egui::Slider::new(&mut self.s.spot_radius, 60.0..=500.0).suffix(" px"));
                });
                ui.horizontal(|ui| {
                    ui.label("Oscurecer:");
                    ui.add(egui::Slider::new(&mut self.s.spot_dim, 0.2..=0.9).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                });
                perf_note(ui, "Mientras está activo, Ctrl+Mayús+rueda del mouse agranda o achica el círculo.");
                if ui.button("Probar ahora").clicked() {
                    self.hide_options();
                    win::post_action(Action::Spotlight);
                }
            }
            Tab::Tutorial => {
                self.heading(ui, "Clics y teclas", Some(8), "", accent);
                if let Some(why) = win::unavailable(Action::Tutorial) {
                    unavailable_note(ui, why);
                }
                ui.label("Modo tutorial: cada clic deja una onda de color y las teclas que pulsas aparecen abajo en la pantalla. Ideal para clases y grabaciones.");
                ui.checkbox(&mut self.s.show_clicks, "Mostrar los clics (amarillo: izquierdo · rojo: derecho · azul: central)");
                ui.checkbox(&mut self.s.show_keys, "Mostrar las teclas");
                ui.add_enabled_ui(self.s.show_keys, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.checkbox(&mut self.s.keys_only_combos, "Sólo combinaciones (Ctrl, Alt, Win) y teclas especiales");
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(46.0);
                        ui.label(egui::RichText::new("Recomendado: así nunca se ve lo que escribes, como contraseñas.").small().color(Color32::from_gray(90)));
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Encontrar el mouse").size(16.0).strong().color(Color32::from_gray(20)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.hotkey_button(ui, 9, accent);
                    });
                });
                self.hotkey_status(ui, 9);
                ui.label("Un anillo se cierra sobre el cursor para que lo encuentres al tiro, ideal con el proyector o pantallas grandes.");
                ui.checkbox(&mut self.s.shake_to_find, "También al sacudir el mouse de lado a lado");
            }
            Tab::Gif => {
                self.heading(ui, "Grabar GIF", Some(10), "", accent);
                if let Some(why) = win::unavailable(Action::Gif) {
                    unavailable_note(ui, why);
                }
                ui.label("Pulsa el atajo y arrastra para elegir la zona: empieza a grabar y un borde rojo la marca. Vuelve a pulsar el atajo para terminar y elige dónde guardar el .gif.");
                ui.horizontal(|ui| {
                    ui.label("Fotogramas por segundo:");
                    ui.add(egui::Slider::new(&mut self.s.gif_fps, 5..=25));
                });
                ui.label(egui::RichText::new(format!("Máximo {} segundos. Menos fotogramas = archivo más liviano.", crate::gif_rec::MAX_SECS)).small().color(Color32::from_gray(90)));
                perf_note(ui, "Con «Clics y teclas» activado, los clics y atajos salen en la grabación: perfecto para mostrar cómo se hace algo.");
            }
            Tab::About => {
                self.heading(ui, "Acerca de", None, "", accent);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label("By ");
                    ui.label(egui::RichText::new("@chalalo").strong().color(accent));
                    ui.label(", basado en el trabajo del gran ");
                    ui.hyperlink_to("Mark Russinovich", "https://learn.microsoft.com/sysinternals/downloads/zoomit");
                    ui.label(".");
                });
                ui.label("Presenter Toolbox: zoom, anotación, foco, clics y teclas en pantalla y grabación GIF para clases y presentaciones. Acelerado por GPU.");
                if win::has_tray() {
                    ui.label("Vive en la bandeja del sistema (junto al reloj). Clic en el icono para el menú.");
                } else {
                    ui.label(format!(
                        "Tu escritorio no tiene bandeja del sistema, así que el programa queda en segundo plano. \
                         Pulsa {} o vuelve a abrirlo desde el menú de aplicaciones para volver aquí.",
                        self.s.hotkeys.options.label()
                    ));
                }
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Atajos de teclado").strong());
                let keys = win::shared_state().map(|s| s.hotkeys.lock().unwrap().clone()).unwrap_or_default();
                // use whatever room is left above the footer instead of a fixed height,
                // so the whole list fits when the window is tall enough
                let list_h = (ui.available_height() - 96.0).max(120.0);
                egui::ScrollArea::vertical().max_height(list_h).show(ui, |ui| {
                    for (name, ok) in keys {
                        ui.horizontal(|ui| {
                            if ok {
                                ui.colored_label(Color32::from_rgb(20, 140, 50), "✔");
                            } else {
                                ui.colored_label(Color32::from_rgb(200, 40, 30), "✖");
                            }
                            ui.label(name);
                            if !ok {
                                ui.label(egui::RichText::new("(lo usa otro programa)").small());
                            }
                        });
                    }
                });
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Para cambiar un atajo, haz clic en su recuadro en la pestaña correspondiente.").small().color(Color32::from_gray(90)));
                if ui.button("Restaurar atajos predeterminados").clicked() {
                    self.s.hotkeys = Hotkeys::default();
                    self.hk_msg = None;
                    self.end_capture();
                    self.s.save();
                }
            }
        }
        live_changed
    }
}

fn unavailable_note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(255, 244, 214))
        .stroke(Stroke::new(1.0, Color32::from_rgb(230, 190, 110)))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(format!("⚠ {text}")).color(Color32::from_rgb(110, 70, 0)));
        });
}

fn perf_note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(228, 240, 252))
        .stroke(Stroke::new(1.0, Color32::from_rgb(170, 200, 235)))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(format!("⚡ {text}")).color(Color32::from_rgb(20, 60, 110)));
        });
}

fn fx_config(s: &Settings) -> crate::fx::FxConfig {
    crate::fx::FxConfig {
        spot_radius: s.spot_radius,
        spot_dim: s.spot_dim,
        show_clicks: s.show_clicks,
        show_keys: s.show_keys,
        keys_only_combos: s.keys_only_combos,
        find_on_shake: s.shake_to_find,
    }
}

/// Asks where to keep a finished recording and moves it there.
fn save_gif(tmp: &std::path::Path) {
    let size_mb = std::fs::metadata(tmp).map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0);
    match win::save_dialog(0, "Grabacion.gif") {
        Some(dest) => {
            let ok = std::fs::rename(tmp, &dest).is_ok() || std::fs::copy(tmp, &dest).is_ok();
            let _ = std::fs::remove_file(tmp);
            if ok {
                win::notify("GIF guardado", &format!("{} ({size_mb:.1} MB)", dest.display()));
            } else {
                win::notify("Grabar GIF", "No se pudo guardar el archivo.");
            }
        }
        None => {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

/// Maps an egui key to a Windows virtual-key code for RegisterHotKey.
fn key_to_vk(k: Key) -> Option<u32> {
    let n = k.name();
    if n.chars().count() == 1 {
        let c = n.chars().next()?.to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(num) = n.strip_prefix('F').and_then(|s| s.parse::<u32>().ok()) {
        if (1..=24).contains(&num) {
            return Some(0x6F + num);
        }
    }
    Some(match k {
        Key::Space => 0x20,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        _ => return None,
    })
}

/// Evens out the shake of a free-hand stroke. Level 0 leaves it untouched; higher levels
/// average over more neighbouring points, then round the corners (Chaikin). The first and
/// last points stay exactly where the pen went down and came up.
fn smooth_stroke(pts: &[Pos2], level: u32) -> Vec<Pos2> {
    if level == 0 || pts.len() < 3 {
        return pts.to_vec();
    }
    let radius = [0usize, 2, 4, 7, 11][level.min(4) as usize];
    let n = pts.len();
    let mut out: Vec<Pos2> = (0..n)
        .map(|i| {
            let r = radius.min(i).min(n - 1 - i);
            let mut s = Vec2::ZERO;
            for p in &pts[i - r..=i + r] {
                s += p.to_vec2();
            }
            (s / (2 * r + 1) as f32).to_pos2()
        })
        .collect();
    for _ in 0..2 {
        if out.len() < 3 {
            break;
        }
        let mut next = Vec::with_capacity(out.len() * 2);
        next.push(out[0]);
        for w in out.windows(2) {
            next.push(w[0].lerp(w[1], 0.25));
            next.push(w[0].lerp(w[1], 0.75));
        }
        next.push(*out.last().unwrap());
        out = next;
    }
    out
}

fn draw_icon(p: &egui::Painter, c: Pos2, r: f32) {
    p.line_segment(
        [c + vec2(r * 0.7, r * 0.7), c + vec2(r * 1.5, r * 1.5)],
        Stroke::new(r * 0.35, Color32::from_gray(50)),
    );
    p.circle(c, r, Color32::from_rgb(170, 215, 255), Stroke::new(r * 0.25, Color32::from_rgb(20, 70, 160)));
}

fn paint_annot(painter: &egui::Painter, a: &Annot, xf: &dyn Fn(Pos2) -> Pos2, scale: f32) {
    let w = (a.width * scale).max(0.75);
    let stroke = Stroke::new(w, a.color);
    let caps = |p: Pos2| painter.circle_filled(p, w / 2.0, a.color);
    match &a.kind {
        Kind::Free(pts) => {
            let pts: Vec<Pos2> = pts.iter().map(|p| xf(*p)).collect();
            if pts.len() == 1 {
                caps(pts[0]);
            } else {
                if a.color.a() == 255 {
                    caps(pts[0]);
                    caps(*pts.last().unwrap());
                }
                painter.add(Shape::line(pts, stroke));
            }
        }
        Kind::Line(p, q) => {
            painter.line_segment([xf(*p), xf(*q)], stroke);
            if a.color.a() == 255 {
                caps(xf(*p));
                caps(xf(*q));
            }
        }
        Kind::Rect(p, q) => {
            painter.rect_stroke(Rect::from_two_pos(xf(*p), xf(*q)), 0.0, stroke, StrokeKind::Middle);
        }
        Kind::Ellipse(p, q) => {
            let r = Rect::from_two_pos(xf(*p), xf(*q));
            let n = 96;
            let pts: Vec<Pos2> = (0..n)
                .map(|i| {
                    let t = i as f32 / n as f32 * std::f32::consts::TAU;
                    r.center() + vec2(t.cos() * r.width() / 2.0, t.sin() * r.height() / 2.0)
                })
                .collect();
            painter.add(Shape::closed_line(pts, stroke));
        }
        Kind::Arrow(p, q) => {
            let (p, q) = (xf(*p), xf(*q));
            painter.line_segment([p, q], stroke);
            let dir = (q - p).normalized();
            if dir.is_finite() && (q - p).length() > 1.0 {
                let len = (w * 4.0).max(12.0);
                let n = vec2(-dir.y, dir.x);
                let a1 = q - dir * len + n * len * 0.5;
                let a2 = q - dir * len - n * len * 0.5;
                painter.add(Shape::convex_polygon(vec![q, a1, a2], a.color, Stroke::NONE));
            }
        }
        Kind::Text { pos, text, size, right } => {
            let align = if *right { Align2::RIGHT_TOP } else { Align2::LEFT_TOP };
            if !text.is_empty() {
                painter.text(xf(*pos), align, text, FontId::proportional(size * scale), a.color);
            }
        }
    }
}

/// A made-up desktop: wallpaper gradient, a couple of windows and a taskbar.
pub fn demo_desktop(w: usize, h: usize) -> ColorImage {
    let mut px = vec![Color32::BLACK; w * h];
    for y in 0..h {
        for x in 0..w {
            let t = y as f32 / h as f32;
            let u = x as f32 / w as f32;
            px[y * w + x] = Color32::from_rgb((20.0 + 60.0 * u) as u8, (60.0 + 80.0 * t) as u8, (140.0 + 90.0 * (1.0 - t)) as u8);
        }
    }
    let mut fill = |x0: f32, y0: f32, x1: f32, y1: f32, c: Color32| {
        let (x0, y0) = ((x0 * w as f32) as usize, (y0 * h as f32) as usize);
        let (x1, y1) = (((x1 * w as f32) as usize).min(w), ((y1 * h as f32) as usize).min(h));
        for y in y0..y1 {
            for x in x0..x1 {
                px[y * w + x] = c;
            }
        }
    };
    // a document window
    fill(0.18, 0.16, 0.72, 0.82, Color32::from_rgb(250, 250, 250));
    fill(0.18, 0.16, 0.72, 0.20, Color32::from_rgb(225, 230, 238));
    for i in 0..14 {
        let y = 0.25 + i as f32 * 0.035;
        let len = 0.30 + ((i * 37) % 17) as f32 * 0.012;
        fill(0.21, y, 0.21 + len, y + 0.012, Color32::from_rgb(90, 95, 105));
    }
    fill(0.42, 0.42, 0.58, 0.56, Color32::from_rgb(255, 196, 64));
    // a chart window
    fill(0.62, 0.30, 0.92, 0.70, Color32::from_rgb(40, 44, 52));
    for (i, hgt) in [0.1f32, 0.18, 0.14, 0.25, 0.3, 0.22].iter().enumerate() {
        let x = 0.65 + i as f32 * 0.043;
        fill(x, 0.66 - hgt, x + 0.03, 0.66, Color32::from_rgb(80, 200, 140));
    }
    // taskbar
    fill(0.0, 0.95, 1.0, 1.0, Color32::from_rgb(32, 32, 36));
    for i in 0..6 {
        let x = 0.40 + i as f32 * 0.035;
        fill(x, 0.958, x + 0.022, 0.992, Color32::from_rgb(90 + i * 20, 140, 220 - i * 15));
    }
    ColorImage::new([w, h], px)
}

fn image_rgba(img: &ColorImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.pixels.len() * 4);
    for p in &img.pixels {
        let [r, g, b, _] = p.to_array();
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

fn copy_image(img: &ColorImage) -> bool {
    let data = arboard::ImageData {
        width: img.size[0],
        height: img.size[1],
        bytes: std::borrow::Cow::Owned(image_rgba(img)),
    };
    #[cfg(windows)]
    {
        arboard::Clipboard::new().and_then(|mut c| c.set_image(data)).is_ok()
    }
    // On X11 the copied picture lives inside the program that copied it, so the clipboard
    // object has to stay alive for as long as the app runs.
    #[cfg(not(windows))]
    {
        static CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);
        let mut guard = CLIPBOARD.lock().unwrap();
        if guard.is_none() {
            *guard = arboard::Clipboard::new().ok();
        }
        guard.as_mut().is_some_and(|c| c.set_image(data).is_ok())
    }
}

fn save_png(img: &ColorImage, path: &std::path::Path) -> bool {
    image::save_buffer(path, &image_rgba(img), img.size[0] as u32, img.size[1] as u32, image::ExtendedColorType::Rgba8)
        .is_ok()
}

impl eframe::App for ZoomApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        if self.show_stage > 0 && Instant::now() >= self.show_deadline {
            self.reveal();
        }
        if self.show_stage > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        self.frames += 1;
        let ctx = ui.ctx().clone();

        // keep the overlay glued to its monitor (e.g. after a DPI change)
        if matches!(self.mode, Mode::Zoom | Mode::Draw | Mode::Break | Mode::Snip)
            && self.show_stage == 0
            && self.frames % 30 == 0
            && win::window_rect(self.hwnd) != self.monitor
        {
            win::place_overlay(self.hwnd, self.monitor, true);
        }

        match self.mode {
            Mode::Hidden | Mode::Options => self.options_ui(ui),
            Mode::Zoom | Mode::Draw => self.zoom_ui(ui),
            Mode::Break => self.break_ui(ui),
            Mode::Snip => self.snip_ui(ui),
        }

        match self.show_stage {
            1 => {
                self.show_stage = 2;
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                ctx.request_repaint();
            }
            2 => self.reveal(),
            _ => {}
        }

        if self.screenshot_path.is_some() {
            if self.frames == 3 {
                self.start_demo(&ctx);
            }
            if self.frames == 120 {
                self.request_window_shot(&ctx, ShotKind::File);
            }
            ctx.request_repaint();
        }
        self.last_frame_ms = t0.elapsed().as_secs_f32() * 1000.0;
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn on_exit(&mut self) {
        self.s.save();
        win::remove_tray();
    }
}
