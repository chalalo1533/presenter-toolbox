use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A global keyboard shortcut. `vk` is a Windows virtual-key code (0 = not assigned).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub vk: u32,
}

impl Hotkey {
    pub const fn ctrl(vk: u32) -> Self {
        Self { ctrl: true, shift: false, alt: false, vk }
    }
    pub const fn ctrl_shift(vk: u32) -> Self {
        Self { ctrl: true, shift: true, alt: false, vk }
    }
    pub fn label(&self) -> String {
        if self.vk == 0 {
            return "Sin asignar".into();
        }
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Mayús+");
        }
        s.push_str(&vk_name(self.vk));
        s
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(|c| c.to_string()).unwrap_or_default(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x20 => "Espacio".into(),
        0x2D => "Insert".into(),
        0x2E => "Supr".into(),
        0x24 => "Inicio".into(),
        0x23 => "Fin".into(),
        0x21 => "RePág".into(),
        0x22 => "AvPág".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        _ => format!("0x{vk:02X}"),
    }
}

/// (hotkey id registered with Windows, name) — the index is the slot used by `Hotkeys::get`.
pub const HOTKEY_SLOTS: [(i32, &str); 12] = [
    (1, "Zoom"),
    (2, "Dibujar"),
    (3, "Descanso"),
    (4, "Zoom en vivo"),
    (5, "Captura al portapapeles"),
    (6, "Recorte"),
    (7, "Recorte a archivo"),
    (8, "Foco"),
    (9, "Clics y teclas"),
    (12, "Encontrar el mouse"),
    (13, "Grabar GIF"),
    (20, "Abrir las opciones"),
];

pub const SMOOTHING_NAMES: [&str; 5] = ["Sin suavizado", "Bajo", "Medio", "Alto", "Máximo"];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Hotkeys {
    pub zoom: Hotkey,
    pub draw: Hotkey,
    #[serde(rename = "break")]
    pub brk: Hotkey,
    pub live: Hotkey,
    pub screenshot: Hotkey,
    pub snip: Hotkey,
    pub snip_save: Hotkey,
    pub spotlight: Hotkey,
    pub tutorial: Hotkey,
    pub find_mouse: Hotkey,
    pub gif: Hotkey,
    pub options: Hotkey,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            zoom: Hotkey::ctrl('1' as u32),
            draw: Hotkey::ctrl('2' as u32),
            brk: Hotkey::ctrl('3' as u32),
            live: Hotkey::ctrl('4' as u32),
            screenshot: Hotkey::ctrl('5' as u32),
            snip: Hotkey::ctrl('6' as u32),
            snip_save: Hotkey::ctrl_shift('6' as u32),
            spotlight: Hotkey::ctrl('7' as u32),
            tutorial: Hotkey::ctrl('8' as u32),
            find_mouse: Hotkey::ctrl('0' as u32),
            gif: Hotkey::ctrl('9' as u32),
            // Ctrl+Alt+P: en GNOME no hay bandeja del sistema, así que sin un atajo
            // la ventana de opciones solo se alcanza volviendo a abrir el programa.
            options: Hotkey { ctrl: true, shift: false, alt: true, vk: 'P' as u32 },
        }
    }
}

impl Hotkeys {
    pub fn get(&self, slot: usize) -> Hotkey {
        match slot {
            0 => self.zoom,
            1 => self.draw,
            2 => self.brk,
            3 => self.live,
            4 => self.screenshot,
            5 => self.snip,
            6 => self.snip_save,
            7 => self.spotlight,
            8 => self.tutorial,
            9 => self.find_mouse,
            10 => self.gif,
            _ => self.options,
        }
    }
    pub fn get_mut(&mut self, slot: usize) -> &mut Hotkey {
        match slot {
            0 => &mut self.zoom,
            1 => &mut self.draw,
            2 => &mut self.brk,
            3 => &mut self.live,
            4 => &mut self.screenshot,
            5 => &mut self.snip,
            6 => &mut self.snip_save,
            7 => &mut self.spotlight,
            8 => &mut self.tutorial,
            9 => &mut self.find_mouse,
            10 => &mut self.gif,
            _ => &mut self.options,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub zoom_level: f32,
    pub animate: bool,
    pub smooth_image: bool,
    pub live_level: f32,
    pub pen_color: [u8; 4],
    pub pen_width: f32,
    /// 0 = none … 4 = maximum (see SMOOTHING_NAMES)
    pub stroke_smoothing: u32,
    pub font_size: f32,
    pub break_minutes: u32,
    pub break_show_elapsed: bool,
    pub break_sound: bool,
    pub break_dim: f32,
    pub break_color: [u8; 4],
    pub show_options_on_start: bool,
    pub hotkeys: Hotkeys,
    // live effects
    pub spot_radius: f32,
    pub spot_dim: f32,
    pub show_clicks: bool,
    pub show_keys: bool,
    pub keys_only_combos: bool,
    pub shake_to_find: bool,
    pub gif_fps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom_level: 2.0,
            animate: true,
            smooth_image: true,
            live_level: 2.0,
            pen_color: [230, 30, 30, 255],
            pen_width: 5.0,
            stroke_smoothing: 2,
            font_size: 28.0,
            break_minutes: 10,
            break_show_elapsed: true,
            break_sound: true,
            break_dim: 0.6,
            break_color: [230, 30, 30, 255],
            show_options_on_start: true,
            hotkeys: Hotkeys::default(),
            spot_radius: 150.0,
            spot_dim: 0.6,
            show_clicks: true,
            show_keys: true,
            keys_only_combos: true,
            shake_to_find: false,
            gif_fps: 12,
        }
    }
}

/// Portable mode: if a `portable.ini` file sits next to the .exe, settings live in that
/// same folder (e.g. on a USB stick). Otherwise they go to %APPDATA%\ZoomItRs on Windows
/// and to ~/.config/presenter-toolbox on Linux.
fn path() -> Option<PathBuf> {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        if dir.join("portable.ini").exists() {
            return Some(dir.join("settings.json"));
        }
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(base).join("ZoomItRs").join("settings.json"))
    }
    #[cfg(not(windows))]
    {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        Some(base.join("presenter-toolbox").join("settings.json"))
    }
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, s);
            }
        }
    }
}
