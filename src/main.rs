#![windows_subsystem = "windows"]

mod app;
mod fx;
mod gif_rec;
mod settings;
mod win;

use eframe::egui;

fn main() -> eframe::Result {
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

    if let Some(i) = args.iter().position(|a| a == "--fx-sample") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "fx-sample.png".into());
        fx::render_sample(std::path::Path::new(&out));
        return Ok(());
    }

    if screenshot.is_none() && !win::single_instance() {
        return Ok(());
    }

    let settings = settings::Settings::load();
    let start_hidden = screenshot.is_none() && !settings.show_options_on_start;

    let icon = egui::IconData { rgba: win::icon_rgba(64), width: 64, height: 64 };
    let viewport = egui::ViewportBuilder::default()
        .with_title("Presenter Toolbox")
        .with_app_id("presenter_toolbox")
        .with_inner_size([680.0, 580.0])
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
