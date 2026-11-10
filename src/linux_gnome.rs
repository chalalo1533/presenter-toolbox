//! Wayland sessions — the default desktop of Ubuntu, Debian, Fedora…
//!
//! Wayland does not let a program grab keys globally or read the screen on its own, so in
//! those sessions two things are done differently (the app's windows still run through
//! XWayland, like in an X11 session):
//!  * global shortcuts are registered as GNOME "custom shortcuts" that run
//!    `presenter_toolbox --do <action>`; that short-lived process hands the action to the
//!    copy that is already running (or becomes it);
//!  * the screen is captured through the desktop portal (org.freedesktop.portal.Screenshot).
//! Spotlight, clicks/keys, find-mouse and GIF recording need global input or overlays that
//! Wayland does not offer to applications; they are reported as unavailable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use zbus::zvariant::{OwnedValue, Value};

use super::{Action, HotkeyDef};
use crate::settings::Hotkey;

static WAYLAND: AtomicBool = AtomicBool::new(false);

/// Must run before `main` hides WAYLAND_DISPLAY from the window toolkit.
pub fn detect_session() {
    let wl = std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    WAYLAND.store(wl, Ordering::SeqCst);
}

pub fn wayland() -> bool {
    WAYLAND.load(Ordering::SeqCst)
}

fn is_gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"))
}

// ---------------------------------------------------------------------------
// `--do <name>`

const NAMES: [(Action, &str); 8] = [
    (Action::Zoom, "zoom"),
    (Action::Draw, "draw"),
    (Action::Break, "break"),
    (Action::Screenshot, "screenshot"),
    (Action::Snip, "snip"),
    (Action::SnipSave, "snip-save"),
    (Action::Options, "options"),
    (Action::Exit, "exit"),
];

pub fn action_from_name(name: &str) -> Option<Action> {
    NAMES.iter().find(|(_, n)| *n == name).map(|(a, _)| *a)
}

fn name_of(id: i32) -> Option<&'static str> {
    NAMES.iter().find(|(a, _)| *a as i32 == id).map(|(_, n)| *n)
}

// ---------------------------------------------------------------------------
// Shortcuts through GNOME's own "custom shortcuts"

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const LIST_KEY: &str = "custom-keybindings";
const PREFIX: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/presenter-toolbox-";
static GSETTINGS_LOCK: Mutex<()> = Mutex::new(());

fn gsettings(args: &[&str]) -> Option<String> {
    let out = Command::new("gsettings").args(args).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// "<Control><Shift>6" — the accelerator format GNOME uses.
fn accelerator(k: &Hotkey) -> Option<String> {
    let key = match k.vk {
        0x30..=0x39 => char::from_u32(k.vk)?.to_string(),
        0x41..=0x5A => char::from_u32(k.vk + 0x20)?.to_string(),
        0x70..=0x87 => format!("F{}", k.vk - 0x6F),
        0x20 => "space".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "Page_Up".into(),
        0x22 => "Page_Down".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        _ => return None,
    };
    let mut s = String::new();
    if k.ctrl {
        s.push_str("<Control>");
    }
    if k.alt {
        s.push_str("<Alt>");
    }
    if k.shift {
        s.push_str("<Shift>");
    }
    Some(s + &key)
}

/// A GVariant string literal.
fn gv(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// One word for a shell-style command line (paths with spaces, quotes…).
fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The paths currently in GNOME's list of custom shortcuts (ours and the user's own).
fn current_paths() -> Option<Vec<String>> {
    let raw = gsettings(&["get", SCHEMA, LIST_KEY])?;
    Some(raw.split('\'').filter(|p| p.starts_with('/')).map(str::to_string).collect())
}

fn write_list(paths: &[String]) -> bool {
    let value = if paths.is_empty() {
        "@as []".to_string()
    } else {
        format!("[{}]", paths.iter().map(|p| gv(p)).collect::<Vec<_>>().join(", "))
    };
    gsettings(&["set", SCHEMA, LIST_KEY, &value]).is_some()
}

/// Registers the shortcuts and returns one line per shortcut for the "Acerca de" list.
pub fn apply_shortcuts(defs: &[HotkeyDef]) -> Vec<(String, bool)> {
    let _guard = GSETTINGS_LOCK.lock().unwrap();
    let existing = if is_gnome() { current_paths() } else { None };
    let exe = std::env::current_exe().ok().map(|p| p.display().to_string());
    let mut wanted: Vec<String> = Vec::new();
    let mut status = Vec::new();
    for d in defs {
        if d.key.vk == 0 {
            status.push((format!("Sin asignar  {}", d.name), true));
            continue;
        }
        let action = Action::from_id(d.id as usize);
        if action.is_some_and(|a| super::unavailable(a).is_some()) {
            status.push((format!("{}  (no disponible en este escritorio)", d.name), true));
            continue;
        }
        let label = format!("{}  {}", d.key.label(), d.name);
        let (Some(_), Some(exe), Some(name), Some(accel)) = (&existing, &exe, name_of(d.id), accelerator(&d.key))
        else {
            status.push((label, false));
            continue;
        };
        let path = format!("{PREFIX}{name}/");
        let target = format!("{SCHEMA}.custom-keybinding:{path}");
        let command = format!("{} --do {name}", shell_word(exe));
        let ok = gsettings(&["set", &target, "name", &gv(&format!("Presenter Toolbox: {}", d.name))]).is_some()
            && gsettings(&["set", &target, "command", &gv(&command)]).is_some()
            && gsettings(&["set", &target, "binding", &gv(&accel)]).is_some();
        if ok {
            wanted.push(path);
        }
        status.push((label, ok));
    }
    if let Some(existing) = existing {
        for stale in existing.iter().filter(|p| p.starts_with(PREFIX) && !wanted.contains(p)) {
            gsettings(&["reset-recursively", &format!("{SCHEMA}.custom-keybinding:{stale}")]);
        }
        let mut list: Vec<String> = existing.into_iter().filter(|p| !p.starts_with(PREFIX)).collect();
        list.extend(wanted);
        write_list(&list);
    }
    status
}

/// Takes our shortcuts out of GNOME (while a new combination is being typed, and on exit).
pub fn clear_shortcuts() {
    let _guard = GSETTINGS_LOCK.lock().unwrap();
    if !is_gnome() {
        return;
    }
    let Some(existing) = current_paths() else { return };
    if !existing.iter().any(|p| p.starts_with(PREFIX)) {
        return;
    }
    let (ours, others): (Vec<String>, Vec<String>) = existing.into_iter().partition(|p| p.starts_with(PREFIX));
    write_list(&others);
    for p in ours {
        gsettings(&["reset-recursively", &format!("{SCHEMA}.custom-keybinding:{p}")]);
    }
}

// ---------------------------------------------------------------------------
// Screen capture through the desktop portal

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn dbus() -> Option<&'static zbus::blocking::Connection> {
    static CONN: OnceLock<Option<zbus::blocking::Connection>> = OnceLock::new();
    CONN.get_or_init(|| zbus::blocking::Connection::session().ok()).as_ref()
}

/// Asks the portal for a screenshot and waits for the answer: the file it was saved to.
fn portal_screenshot() -> Option<PathBuf> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let conn = dbus()?;
    let unique = conn.unique_name()?.as_str().trim_start_matches(':').replace('.', "_");
    let token = format!("presenter_toolbox_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst));
    let request = format!("/org/freedesktop/portal/desktop/request/{unique}/{token}");
    // listen for the answer before asking, so it cannot be missed
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .ok()?
        .member("Response")
        .ok()?
        .path(request.as_str())
        .ok()?
        .build();
    let mut answers = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(4)).ok()?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("interactive", Value::from(false));
    conn.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.Screenshot"),
        "Screenshot",
        &("", options),
    )
    .ok()?;
    let msg = answers.next()?.ok()?;
    let body = msg.body();
    let (code, results): (u32, HashMap<String, OwnedValue>) = body.deserialize().ok()?;
    if code != 0 {
        return None; // 1 = the user said no, 2 = it failed
    }
    let uri = match &**results.get("uri")? {
        Value::Str(s) => s.as_str().to_string(),
        _ => return None,
    };
    Some(PathBuf::from(percent_decode(uri.strip_prefix("file://")?)))
}

/// The whole desktop as the compositor shows it, or None if the portal refused or timed out.
pub fn capture_desktop() -> Option<image::RgbaImage> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(portal_screenshot());
    });
    let path = rx.recv_timeout(Duration::from_secs(30)).ok()??;
    let img = image::open(&path).ok().map(|i| i.to_rgba8());
    // the portal leaves the picture in the user's Pictures folder; it was only meant for us
    let _ = std::fs::remove_file(&path);
    img
}
