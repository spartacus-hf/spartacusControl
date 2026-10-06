//! The radio's screen as a texture: pages the LINK reports dirty are
//! re-rendered into an RGBA buffer and uploaded as a partial update, so the
//! GPU sees only what changed. The waterfall band is drawn from SPECTRUM
//! lines (the band's framebuffer bytes on the radio are not pixels).

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use sctrl_proto::screen::{self, Screen};
use sctrl_proto::spectrum::{SpectrumLine, Waterfall};

pub struct Mirror {
    rgba: Vec<u8>,
    tex: Option<TextureHandle>,
    /// The band's history.
    waterfall: Waterfall,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            rgba: vec![0; screen::W * screen::H * 4],
            tex: None,
            waterfall: Waterfall::default(),
        }
    }
}

impl Mirror {
    pub fn texture(&mut self, ctx: &egui::Context) -> &TextureHandle {
        self.tex.get_or_insert_with(|| {
            ctx.load_texture(
                "radio-screen",
                ColorImage::from_rgba_unmultiplied([screen::W, screen::H], &vec![0; screen::W * screen::H * 4]),
                TextureOptions::NEAREST,
            )
        })
    }

    /// New SPECTRUM data: a new history line when its sequence advanced.
    pub fn spectrum(&mut self, line: &SpectrumLine) {
        self.waterfall.push(line);
    }

    /// Re-render `dirty` pages (and the band) from `s` and upload them.
    pub fn update(&mut self, ctx: &egui::Context, s: &Screen, dirty: u32) {
        if dirty == 0 {
            return;
        }
        for p in 0..screen::PAGES {
            if dirty & (1 << p) != 0 {
                if s.is_band(p) {
                    self.waterfall.draw_page(s, p, &mut self.rgba);
                } else {
                    s.render_page_rgba(p, &mut self.rgba);
                }
            }
        }
        let first = (0..screen::PAGES).find(|p| dirty & (1 << p) != 0).unwrap_or(0);
        let last = (0..screen::PAGES).rev().find(|p| dirty & (1 << p) != 0).unwrap_or(0);
        let y0 = first * 8;
        let rows = (last - first + 1) * 8;
        let part = ColorImage::from_rgba_unmultiplied(
            [screen::W, rows],
            &self.rgba[y0 * screen::W * 4..(y0 + rows) * screen::W * 4],
        );
        let tex = self.texture(ctx);
        let mut t = tex.clone();
        t.set_partial([0, y0], part, TextureOptions::NEAREST);
    }
}
