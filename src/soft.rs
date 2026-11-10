//! Linux only: a tiny anti-aliased software renderer (premultiplied BGRA, the byte order
//! X11 wants for 32-bit images) plus text rasterising with the fonts egui already ships.
//! On Windows the same drawing is done inside `fx.rs` with GDI.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

pub struct Mask {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

pub struct Canvas<'a> {
    pub w: i32,
    pub h: i32,
    pub px: &'a mut [u8],
}

impl Canvas<'_> {
    pub fn blend(&mut self, x: i32, y: i32, rgb: [u8; 3], a: f32) {
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

    pub fn fill(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, rgb: [u8; 3]) {
        for y in y0.max(0)..y1.min(self.h) {
            for x in x0.max(0)..x1.min(self.w) {
                let i = ((y * self.w + x) * 4) as usize;
                self.px[i..i + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        }
    }

    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, th: f32, rgb: [u8; 3], a: f32) {
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

    pub fn disc(&mut self, cx: f32, cy: f32, r: f32, rgb: [u8; 3], a: f32) {
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

    pub fn rrect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, rad: f32, rgb: [u8; 3], a: f32) {
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

    pub fn mask(&mut self, ox: i32, oy: i32, m: &Mask, rgb: [u8; 3], a: f32) {
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

fn fonts() -> &'static Vec<FontRef<'static>> {
    static FONTS: OnceLock<Vec<FontRef<'static>>> = OnceLock::new();
    FONTS.get_or_init(|| {
        [
            epaint_default_fonts::UBUNTU_LIGHT,
            epaint_default_fonts::NOTO_EMOJI_REGULAR,
            epaint_default_fonts::EMOJI_ICON,
            epaint_default_fonts::HACK_REGULAR,
        ]
        .into_iter()
        .filter_map(|b| FontRef::try_from_slice(b).ok())
        .collect()
    })
}

/// Renders one line of text as a coverage mask. `size` is the em height in pixels;
/// `bold` thickens the strokes (the bundled font is a light weight).
pub fn text_mask(text: &str, size: i32, bold: bool) -> (i32, i32, Vec<u8>) {
    let fonts = fonts();
    let Some(main) = fonts.first() else { return (1, 1, vec![0]) };
    let scale_of = |f: &FontRef<'static>| {
        let upm = f.units_per_em().unwrap_or(1000.0);
        PxScale::from(size as f32 * f.height_unscaled() / upm)
    };
    let ms = main.as_scaled(scale_of(main));
    let ascent = ms.ascent().ceil();
    let height = (ms.ascent() - ms.descent()).ceil() as i32 + 1;
    let extra = if bold { 1.0 } else { 0.0 };

    // lay out
    let mut pen = 2.0f32;
    let mut glyphs = Vec::new();
    let mut prev: Option<(usize, ab_glyph::GlyphId)> = None;
    for ch in text.chars() {
        let fi = fonts.iter().position(|f| f.glyph_id(ch).0 != 0).unwrap_or(0);
        let f = &fonts[fi];
        let sf = f.as_scaled(scale_of(f));
        let id = f.glyph_id(ch);
        if let Some((pf, pid)) = prev {
            if pf == fi {
                pen += sf.kern(pid, id);
            }
        }
        glyphs.push((fi, id.with_scale_and_position(scale_of(f), ab_glyph::point(pen, ascent))));
        pen += sf.h_advance(id) + extra * 0.6;
        prev = Some((fi, id));
    }
    let w = (pen.ceil() as i32 + 3).max(1);
    let h = height.max(1);
    let mut px = vec![0u8; (w * h) as usize];
    for (fi, g) in glyphs {
        let Some(o) = fonts[fi].outline_glyph(g) else { continue };
        let b = o.px_bounds();
        let (bx, by) = (b.min.x as i32, b.min.y as i32);
        let offsets: &[i32] = if bold { &[0, 1] } else { &[0] };
        o.draw(|x, y, c| {
            let v = (c.clamp(0.0, 1.0) * 255.0) as u8;
            for &dx in offsets {
                let (xx, yy) = (bx + x as i32 + dx, by + y as i32);
                if xx >= 0 && yy >= 0 && xx < w && yy < h {
                    let p = &mut px[(yy * w + xx) as usize];
                    *p = (*p).max(v);
                }
            }
        });
    }
    (w, h, px)
}

/// Small cache so animated overlays don't rasterise the same label every frame.
#[derive(Default)]
pub struct TextCache {
    masks: HashMap<(String, i32, bool), Rc<Mask>>,
}

impl TextCache {
    pub fn get(&mut self, s: &str, size: i32, bold: bool) -> Rc<Mask> {
        let key = (s.to_string(), size, bold);
        if let Some(m) = self.masks.get(&key) {
            return m.clone();
        }
        if self.masks.len() > 128 {
            self.masks.clear();
        }
        let (w, h, px) = text_mask(s, size, bold);
        let m = Rc::new(Mask { w, h, px });
        self.masks.insert(key, m.clone());
        m
    }
}

/// Turns a premultiplied BGRA buffer into opaque RGBA over a flat background (for PNG files).
pub fn to_rgba_over(px: &[u8], bg: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(px.len());
    for p in px.chunks_exact(4) {
        let inv = 1.0 - p[3] as f32 / 255.0;
        out.extend_from_slice(&[
            (p[2] as f32 + bg[0] as f32 * inv) as u8,
            (p[1] as f32 + bg[1] as f32 * inv) as u8,
            (p[0] as f32 + bg[2] as f32 * inv) as u8,
            255,
        ]);
    }
    out
}
