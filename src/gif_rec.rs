//! Records a region of the screen to an animated GIF.
//! One thread grabs frames (skipping identical ones) and another encodes them to a temporary
//! file, so a slow encoder never makes the recording stutter. A red frame, drawn just
//! *outside* the region and hidden from screen capture, shows what is being recorded.

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, RgbaImage};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win::{self, Action, ScreenRect, wide};

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

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe fn border_window(r: ScreenRect) -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let cls = wide("ZoomItRsRecBorder");
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(border_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        wc.hbrBackground = CreateSolidBrush(0x0028_28E6); // BGR: red
        RegisterClassW(&wc);
        let title = wide("Grabando");
        let (x, y, w, h) = (r.x - BORDER, r.y - BORDER, r.w + BORDER * 2, r.h + BORDER * 2);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        SetLayeredWindowAttributes(hwnd, 0, 235, LWA_ALPHA);
        let outer = CreateRectRgn(0, 0, w, h);
        let inner = CreateRectRgn(BORDER, BORDER, w - BORDER, h - BORDER);
        CombineRgn(outer, outer, inner, RGN_DIFF);
        DeleteObject(inner);
        SetWindowRgn(hwnd, outer, 1);
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        hwnd
    }
}

fn pump() {
    unsafe {
        let mut msg: MSG = zeroed();
        while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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
    let path = std::env::temp_dir().join(format!("zoomitrs_{stamp}.gif"));
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
    let border = unsafe { border_window(rect) };
    // let the selection overlay and the notification settle before the first frame
    let settle = Instant::now() + Duration::from_millis(250);
    while Instant::now() < settle {
        pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    let interval = Duration::from_millis(1000 / fps as u64);
    let started = Instant::now();
    let mut pending: Option<(Vec<u8>, Instant)> = None;
    let mut alive = true;
    while alive {
        pump();
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
    unsafe {
        DestroyWindow(border);
    }
    pump();

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
