//! The radio's screen as a texture: pages the LINK reports dirty are
//! re-rendered into an RGBA buffer and uploaded as a partial update, so the
//! GPU sees only what changed. The waterfall band is drawn from SPECTRUM
//! lines (the band's framebuffer bytes on the radio are not pixels).

use std::collections::VecDeque;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use sctrl_proto::screen::{self, Screen, rgb565_to_rgb};
use sctrl_proto::spectrum::{PALETTES_565, SpectrumLine};

pub struct Mirror {
    rgba: Vec<u8>,
    tex: Option<TextureHandle>,
    /// Waterfall history for the band rectangle (newest first).
    history: VecDeque<Vec<u8>>,
    last_seq: Option<u16>,
    palette: u8,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            rgba: vec![0; screen::W * screen::H * 4],
            tex: None,
            history: VecDeque::new(),
            last_seq: None,
            palette: 0,
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
        self.palette = line.palette.min(3);
        if self.last_seq != Some(line.line_seq) {
            self.last_seq = Some(line.line_seq);
            self.history.push_front(line.levels.clone());
            while self.history.len() > 400 {
                self.history.pop_back();
            }
        }
    }

    /// Re-render `dirty` pages (and the band) from `s` and upload them.
    pub fn update(&mut self, ctx: &egui::Context, s: &Screen, dirty: u32) {
        if dirty == 0 {
            return;
        }
        for p in 0..screen::PAGES {
            if dirty & (1 << p) != 0 {
                if s.is_band(p) {
                    self.draw_band_page(s, p);
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

    /// The band's pages: the waterfall history, newest at the top, centred
    /// in the 320-pixel row the way the radio draws its 304-pixel band.
    fn draw_band_page(&mut self, s: &Screen, page: usize) {
        let pal: Vec<[u8; 3]> = PALETTES_565[self.palette as usize]
            .iter()
            .map(|&c| rgb565_to_rgb(c))
            .collect();
        let band_top = s.band_first().unwrap_or(page) * 8;
        for r in 0..8 {
            let y = page * 8 + r;
            let line = self.history.get((y - band_top) / 2);
            for x in 0..screen::W {
                let o = (y * screen::W + x) * 4;
                let c = match line {
                    Some(l) if x >= 8 && x - 8 < l.len() => pal[l[x - 8] as usize & 15],
                    _ => [0, 0, 0],
                };
                self.rgba[o..o + 3].copy_from_slice(&c);
                self.rgba[o + 3] = 255;
            }
        }
    }
}
