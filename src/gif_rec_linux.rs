//! Linux (X11) counterpart of `gif_rec.rs`: records a region of the screen to an animated GIF.
//! One thread grabs frames (skipping identical ones) and another encodes them to a temporary
//! file, so a slow encoder never makes the recording stutter. A red frame, drawn just
//! *outside* the region (so it never appears in the recording), shows what is being recorded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, RgbaImage};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;

use crate::win::{self, Action, ScreenRect};

static RECORDING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
pub const MAX_SECS: u64 = 60;
const BORDER: i32 = 4;

pub fn recording() -> bool {
    RECORDING.load(Ordering::SeqCst)
}

pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
}

pub fn start(rect: ScreenRect, fps: u32) {
    if rect.w < 8 || rect.h < 8 || RECORDING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP.store(false, Ordering::SeqCst);
    std::thread::spawn(move || {
        run(rect, fps.clamp(5, 30));
        RECORDING.store(false, Ordering::SeqCst);
    });
}

/// A red, click-through frame around the recorded region. Returns 0 if it could not be made.
fn border_window(r: ScreenRect) -> Window {
    let Some(x) = win::x() else { return 0 };
    let (w, h) = (r.w + BORDER * 2, r.h + BORDER * 2);
    let win = x.popup_window(false, r.x - BORDER, r.y - BORDER, w, h, 0x00e6_2828, EventMask::NO_EVENT);
    if win == 0 {
        return 0;
    }
    let b = BORDER as u16;
    let frame = [
        Rectangle { x: 0, y: 0, width: w as u16, height: b },
        Rectangle { x: 0, y: (h - BORDER) as i16, width: w as u16, height: b },
        Rectangle { x: 0, y: BORDER as i16, width: b, height: r.h as u16 },
        Rectangle { x: (w - BORDER) as i16, y: BORDER as i16, width: b, height: r.h as u16 },
    ];
    let _ = x.conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::UNSORTED, win, 0, 0, &frame);
    x.click_through(win);
    let _ = x.conn.map_window(win);
    let _ = x.conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
    let _ = x.conn.flush();
    win
}

fn close_border(win: Window) {
    if let (Some(x), true) = (win::x(), win != 0) {
        let _ = x.conn.destroy_window(win);
        let _ = x.conn.flush();
    }
}

fn rgba_of(rect: ScreenRect) -> Option<Vec<u8>> {
    let img = win::capture(rect)?;
    let mut out = Vec::with_capacity(img.pixels.len() * 4);
    for p in &img.pixels {
        let [r, g, b, _] = p.to_array();
        out.extend_from_slice(&[r, g, b, 255]);
    }
    Some(out)
}

fn run(rect: ScreenRect, fps: u32) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("presenter_toolbox_{stamp}.gif"));
    let (w, h) = (rect.w as u32, rect.h as u32);

    // encoder thread
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u32)>(48);
    let enc_path = path.clone();
    let encoder = std::thread::spawn(move || -> Result<usize, String> {
        let file = std::fs::File::create(&enc_path).map_err(|e| e.to_string())?;
        let mut enc = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
        enc.set_repeat(Repeat::Infinite).map_err(|e| e.to_string())?;
        let mut n = 0;
        for (rgba, ms) in rx {
            let img = RgbaImage::from_raw(w, h, rgba).ok_or("tamaño de imagen inválido")?;
            let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms.max(20), 1));
            enc.encode_frame(frame).map_err(|e| e.to_string())?;
            n += 1;
        }
        Ok(n)
    });

    let key = win::hotkey_label(Action::Gif);
    win::notify(
        "Grabando GIF",
        &format!("Pulsa {key} otra vez (o usa el menú de la bandeja) para terminar. Máximo {MAX_SECS} s."),
    );
    let border = border_window(rect);
    // let the selection overlay and the notification settle before the first frame
    let settle = Instant::now() + Duration::from_millis(250);
    while Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(10));
    }

    let interval = Duration::from_millis(1000 / fps as u64);
    let started = Instant::now();
    let mut pending: Option<(Vec<u8>, Instant)> = None;
    let mut alive = true;
    while alive {
        if STOP.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(MAX_SECS) {
            break;
        }
        let t = Instant::now();
        if let Some(rgba) = rgba_of(rect) {
            let same = pending.as_ref().is_some_and(|(prev, _)| *prev == rgba);
            if !same {
                if let Some((prev, t0)) = pending.take() {
                    let ms = (t - t0).as_millis() as u32;
                    alive = tx.send((prev, ms)).is_ok();
                }
                pending = Some((rgba, t));
            }
        }
        let next = t + interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    if let Some((prev, t0)) = pending {
        let _ = tx.send((prev, (t0.elapsed().as_millis() as u32).max(100)));
    }
    drop(tx);
    close_border(border);

    win::notify("Grabación terminada", "Preparando el GIF…");
    match encoder.join() {
        Ok(Ok(n)) if n > 0 => win::gif_done(path),
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&path);
            win::notify("Grabar GIF", &format!("No se pudo crear el GIF: {e}"));
        }
        _ => {
            let _ = std::fs::remove_file(&path);
            win::notify("Grabar GIF", "No se grabó ningún fotograma.");
        }
    }
}
