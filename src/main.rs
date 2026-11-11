#![windows_subsystem = "windows"]

mod app;
mod settings;

// Everything that talks to the operating system lives in three modules with the same
// names and the same functions on each system, so `app.rs` is shared as it is.
#[cfg(windows)]
mod fx;
#[cfg(windows)]
mod gif_rec;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
#[path = "fx_linux.rs"]
mod fx;
#[cfg(target_os = "linux")]
#[path = "gif_rec_linux.rs"]
mod gif_rec;
#[cfg(target_os = "linux")]
mod soft;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod win;

use eframe::egui;

fn main() -> eframe::Result {
    // Global shortcuts, screen capture and overlays need X11: when an X server is reachable
    // (also true inside a Wayland session, through XWayland) make the window toolkit use it.
    #[cfg(target_os = "linux")]
    preload_bundled_libs();
    #[cfg(target_os = "linux")]
    win::detect_session();
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() {
        // SAFETY: nothing else is running yet, so no other thread can be reading the environment.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    let args: Vec<String> = std::env::args().collect();
    let screenshot = args
        .iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from);

    if let Some(i) = args.iter().position(|a| a == "--check-live") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "live-check.txt".into());
        let _ = std::fs::write(out, win::check_live());
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--write-icon") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "icon.ico".into());
        let _ = std::fs::write(out, ico_file());
        return Ok(());
    }

    // Icons for a Linux desktop: one PNG per size the hicolor theme expects.
    if let Some(i) = args.iter().position(|a| a == "--write-png") {
        let dir = std::path::PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| ".".into()));
        let _ = std::fs::create_dir_all(&dir);
        for s in [16u32, 24, 32, 48, 64, 128, 256] {
            if let Some(img) = image::RgbaImage::from_raw(s, s, win::icon_rgba(s as usize)) {
                let _ = img.save(dir.join(format!("{s}.png")));
            }
        }
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--write-msix-assets") {
        let dir = std::path::PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| "Assets".into()));
        write_msix_assets(&dir);
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--showcase") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "showcase.png".into());
        let m = win::monitor_at_cursor();
        let desk = app::demo_desktop(m.w.max(1366) as usize, m.h.max(768) as usize);
        fx::render_showcase(std::path::Path::new(&out), &desk);
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--fx-sample") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "fx-sample.png".into());
        fx::render_sample(std::path::Path::new(&out));
        return Ok(());
    }

    // `--do zoom`: what a desktop shortcut runs (see linux_gnome.rs)
    #[cfg(target_os = "linux")]
    win::set_initial_action(
        args.iter().position(|a| a == "--do").and_then(|i| args.get(i + 1)).and_then(|n| win::action_from_name(n)),
    );

    if screenshot.is_none() && !win::single_instance() {
        return Ok(());
    }

    let settings = settings::Settings::load();
    let start_hidden = screenshot.is_none() && !settings.show_options_on_start;
    #[cfg(target_os = "linux")]
    let start_hidden = start_hidden || (screenshot.is_none() && win::initial_action().is_some_and(|a| a != win::Action::Options));

    let icon = egui::IconData { rgba: win::icon_rgba(64), width: 64, height: 64 };
    let viewport = egui::ViewportBuilder::default()
        .with_title("Presenter Toolbox")
        .with_app_id("presenter_toolbox")
        .with_inner_size([680.0, 700.0])
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(!start_hidden)
        .with_icon(icon);

    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };

    eframe::run_native(
        "Presenter Toolbox",
        options,
        Box::new(move |cc| Ok(Box::new(app::ZoomApp::new(cc, settings, screenshot, start_hidden)))),
    )
}

/// Builds a multi-size .ico (PNG-compressed entries) from the same magnifier drawn for the tray.
fn ico_file() -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let pngs: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let mut buf = std::io::Cursor::new(Vec::new());
            if let Some(img) = image::RgbaImage::from_raw(s, s, win::icon_rgba(s as usize)) {
                let _ = img.write_to(&mut buf, image::ImageFormat::Png);
            }
            buf.into_inner()
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, png) in sizes.iter().zip(&pngs) {
        let d = if *s >= 256 { 0u8 } else { *s as u8 };
        out.extend_from_slice(&[d, d, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &pngs {
        out.extend_from_slice(png);
    }
    out
}

/// Tile and taskbar images for the MSIX package (MRT names: scale-* and targetsize-*).
fn write_msix_assets(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let padded = |w: u32, h: u32, scale: f32| {
        let s = ((w.min(h) as f32) * scale).round().max(1.0) as u32;
        let mut canvas = image::RgbaImage::new(w, h);
        if let Some(icon) = image::RgbaImage::from_raw(s, s, win::icon_rgba(s as usize)) {
            image::imageops::overlay(&mut canvas, &icon, ((w - s) / 2) as i64, ((h - s) / 2) as i64);
        }
        canvas
    };
    let mut list: Vec<(String, u32, u32, f32)> = vec![
        ("StoreLogo.scale-100.png".into(), 50, 50, 1.0),
        ("StoreLogo.scale-200.png".into(), 100, 100, 1.0),
        ("Square44x44Logo.scale-100.png".into(), 44, 44, 1.0),
        ("Square44x44Logo.scale-200.png".into(), 88, 88, 1.0),
        ("Square150x150Logo.scale-100.png".into(), 150, 150, 0.62),
        ("Square150x150Logo.scale-200.png".into(), 300, 300, 0.62),
        ("Wide310x150Logo.scale-100.png".into(), 310, 150, 0.62),
        ("Wide310x150Logo.scale-200.png".into(), 620, 300, 0.62),
    ];
    for t in [16u32, 24, 32, 48, 256] {
        list.push((format!("Square44x44Logo.targetsize-{t}.png"), t, t, 1.0));
        list.push((format!("Square44x44Logo.targetsize-{t}_altform-unplated.png"), t, t, 1.0));
    }
    for (name, w, h, scale) in list {
        let _ = padded(w, h, scale).save(dir.join(name));
    }
}

/// A stock Ubuntu/Debian desktop (Wayland) no longer installs the small X11 keyboard
/// library the window toolkit needs (libxkbcommon-x11). The download carries it in a `lib`
/// folder next to the program, together with the exact libxkbcommon it was built against
/// (the two must match). They are used only when the system does not have its own.
#[cfg(target_os = "linux")]
fn preload_bundled_libs() {
    use std::ffi::CString;
    let open = |name: &std::ffi::OsStr| -> bool {
        // SAFETY: dlopen with a valid, NUL-terminated name; the handle is kept for the whole
        // run on purpose, so later lookups by library name find this copy.
        CString::new(name.as_encoded_bytes())
            .is_ok_and(|n| unsafe { !libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL).is_null() })
    };
    if open("libxkbcommon-x11.so.0".as_ref()) {
        return; // the system has it
    }
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("lib"))) else { return };
    if !dir.join("libxkbcommon-x11.so.0").exists() {
        eprintln!("Falta la biblioteca libxkbcommon-x11. Instálala con:  sudo apt install libxkbcommon-x11-0");
        return;
    }
    // order matters: each one is found by the next when it looks for its dependencies
    open(dir.join("libxkbcommon.so.0").as_os_str());
    if !open("libxcb-xkb.so.1".as_ref()) {
        open(dir.join("libxcb-xkb.so.1").as_os_str());
    }
    open(dir.join("libxkbcommon-x11.so.0").as_os_str());
}
