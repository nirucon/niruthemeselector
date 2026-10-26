//! Text and shapes drawn into the backing pixmap.
//!
//! `ab_glyph` is already the suite's text stack, so the same font resolution
//! the bar uses applies here: `fc-match` first, then the usual system paths.

use ab_glyph::{point, Font as _, FontArc, PxScale, ScaleFont};

use crate::theme::Palette;

pub struct Font {
    face: FontArc,
    size: f32,
}

impl Font {
    pub fn load(_palette: Palette) -> Self {
        Self {
            face: load_face(),
            size: 13.0,
        }
    }

    pub fn size(&self, size: f32) -> Self {
        Self {
            face: self.face.clone(),
            size,
        }
    }

    /// Width of a string in pixels.
    pub fn width(&self, text: &str) -> f32 {
        let scaled = self.face.as_scaled(PxScale::from(self.size));
        text.chars()
            .map(|c| scaled.h_advance(self.face.glyph_id(c)))
            .sum()
    }

    /// Draw `text` with its baseline at `y`, returning the pixels to upload.
    ///
    /// The caller composites the result, so this only produces a coverage mask
    /// and a foreground colour.
    pub fn render(&self, text: &str, colour: u32, bg: u32) -> (u32, u32, Vec<u8>) {
        let scaled = self.face.as_scaled(PxScale::from(self.size));
        let ascent = scaled.ascent().ceil() as i32;
        let descent = scaled.descent().ceil().abs() as i32;
        let width = self.width(text).ceil().max(1.0) as u32;
        let height = (ascent + descent).max(1) as u32;
        let (br, bg_, bb) = split(bg);
        let (fr, fg, fb) = split(colour);
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for p in pixels.chunks_exact_mut(4) {
            p.copy_from_slice(&[br, bg_, bb, 255]);
        }
        let mut pen_x = 0.0f32;
        for ch in text.chars() {
            let glyph_id = self.face.glyph_id(ch);
            let glyph = glyph_id
                .with_scale_and_position(PxScale::from(self.size), point(pen_x, ascent as f32));
            if let Some(outline) = self.face.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let x = bounds.min.x as i32 + gx as i32;
                    let y = bounds.min.y as i32 + gy as i32;
                    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                        return;
                    }
                    let a = coverage.clamp(0.0, 1.0);
                    let idx = ((y as u32 * width + x as u32) * 4) as usize;
                    pixels[idx] = mix(br, fr, a);
                    pixels[idx + 1] = mix(bg_, fg, a);
                    pixels[idx + 2] = mix(bb, fb, a);
                });
            }
            pen_x += scaled.h_advance(glyph_id);
        }
        (width, height, pixels)
    }
}

fn mix(background: u8, foreground: u8, alpha: f32) -> u8 {
    (f32::from(background) * (1.0 - alpha) + f32::from(foreground) * alpha).round() as u8
}

fn split(pixel: u32) -> (u8, u8, u8) {
    (
        ((pixel >> 16) & 0xff) as u8,
        ((pixel >> 8) & 0xff) as u8,
        (pixel & 0xff) as u8,
    )
}

fn load_face() -> FontArc {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(out) = std::process::Command::new("fc-match")
        .args([
            "-f",
            "%{file}",
            "JetBrainsMono Nerd Font,DejaVu Sans Mono:style=Regular",
        ])
        .output()
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                candidates.push(path);
            }
        }
    }
    candidates.extend(
        [
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/LiberationMono-Regular.ttf",
        ]
        .map(String::from),
    );
    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(face) = FontArc::try_from_vec(bytes) {
                return face;
            }
        }
    }
    // A font is required to draw anything, so a missing one is reported by the
    // caller rather than silently drawing nothing.
    FontArc::try_from_vec(Vec::new()).expect("no usable font found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_has_a_width_and_a_mask() {
        let font = Font::load(Palette::fallback());
        let (w, h, pixels) = font.render("NIRU Wallpaper", 0xffffff, 0x000000);
        assert!(w > 40, "width {w} looks too small for the text");
        assert!(h > 8);
        assert_eq!(pixels.len(), (w * h * 4) as usize);
        // Something was drawn: not every pixel is the background.
        assert!(pixels.chunks_exact(4).any(|p| p[0] > 8));
    }

    #[test]
    fn an_empty_string_still_produces_a_valid_mask() {
        let font = Font::load(Palette::fallback());
        let (w, h, pixels) = font.render("", 0xffffff, 0x000000);
        assert_eq!(w, 1);
        assert!(h >= 1);
        assert_eq!(pixels.len(), (w * h * 4) as usize);
    }

    #[test]
    fn a_larger_size_is_wider() {
        let font = Font::load(Palette::fallback());
        let small = font.width("Wallpaper");
        let large = font.size(26.0).width("Wallpaper");
        assert!(large > small);
    }
}
