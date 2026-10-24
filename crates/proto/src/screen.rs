//! The radio's display as the LINK mirrors it: 320 x 240, a one-bit
//! framebuffer in 30 pages of 8 rows, and 40 x 30 tiles of 8 x 8 pixels each
//! with a foreground and a background colour from the radio's 16-colour
//! palette. The waterfall's "band" pages are not pixels: the host
//! draws that rectangle from SPECTRUM frames.

use crate::rle;

pub const W: usize = 320;
pub const H: usize = 240;
pub const PAGES: usize = H / 8;
pub const TCOLS: usize = W / 8;
pub const PAGE_BYTES: usize = W + TCOLS;

/// The radio's palette as RGB565 (index = GFX_* colour).
pub const PALETTE_565: [u16; 16] = [
    0x0000, 0x00E2, 0x01A4, 0x0347, 0xE7FD, 0x6712, 0x2D0C, 0x075C, 0x02CB, 0x4FEF, 0x378C, 0xFE20, 0xFA06, 0x8000,
    0x0284, 0xFFFF,
];

/// RGB565 expanded to 8 bits per channel the usual way (bit replication).
pub fn rgb565_to_rgb(c: u16) -> [u8; 3] {
    let r = ((c >> 11) & 0x1F) as u8;
    let g = ((c >> 5) & 0x3F) as u8;
    let b = (c & 0x1F) as u8;
    [(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]
}

#[derive(Clone)]
pub struct Screen {
    /// Per page: 320 bitmap bytes then 40 tile colour bytes.
    pages: Vec<[u8; PAGE_BYTES]>,
    /// Pages that have been received at least once.
    pub known: [bool; PAGES],
    /// Pages the waterfall's band covers (SCREEN flag bit 0), bit per page.
    pub band_pages: u32,
    /// Pages changed since the consumer last called `take_dirty`.
    dirty: u32,
    /// Full refreshes completed (pages flagged "last of a full refresh").
    pub refreshes: u32,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            pages: vec![[0u8; PAGE_BYTES]; PAGES],
            known: [false; PAGES],
            band_pages: 0,
            dirty: 0,
            refreshes: 0,
        }
    }

    /// Apply a SCREEN payload. Returns the page index when it decoded.
    pub fn apply(&mut self, payload: &[u8]) -> Option<usize> {
        if payload.len() < 2 {
            return None;
        }
        let page = payload[0] as usize;
        if page >= PAGES {
            return None;
        }
        let flags = payload[1];
        let done = |s: &mut Self| {
            if flags & 0x02 != 0 {
                s.refreshes = s.refreshes.wrapping_add(1);
            }
        };
        if flags & 0x01 != 0 {
            // A band page: the waterfall covers it, the host draws it.
            self.band_pages |= 1 << page;
            self.known[page] = true;
            self.dirty |= 1 << page;
            done(self);
            return Some(page);
        }
        let data = rle::decode(&payload[2..], PAGE_BYTES)?;
        self.band_pages &= !(1 << page);
        self.pages[page].copy_from_slice(&data);
        self.known[page] = true;
        self.dirty |= 1 << page;
        done(self);
        Some(page)
    }

    /// Set a page from raw bytes (console `shot`, SWD grabs, tests).
    pub fn set_page(&mut self, page: usize, bits: &[u8], attrs: &[u8]) {
        if page < PAGES && bits.len() == W && attrs.len() == TCOLS {
            self.pages[page][..W].copy_from_slice(bits);
            self.pages[page][W..].copy_from_slice(attrs);
            self.known[page] = true;
            self.dirty |= 1 << page;
        }
    }

    pub fn is_band(&self, page: usize) -> bool {
        page < PAGES && self.band_pages & (1 << page) != 0
    }

    /// The band's first page, when there is one.
    pub fn band_first(&self) -> Option<usize> {
        (self.band_pages != 0).then(|| self.band_pages.trailing_zeros() as usize)
    }

    pub fn take_dirty(&mut self) -> u32 {
        std::mem::take(&mut self.dirty)
    }

    pub fn page_bytes(&self, page: usize) -> &[u8; PAGE_BYTES] {
        &self.pages[page]
    }

    /// Colour index of one pixel.
    pub fn pixel(&self, x: usize, y: usize) -> u8 {
        let p = &self.pages[y / 8];
        let attr = p[W + x / 8];
        if p[x] & (1 << (y % 8)) != 0 {
            attr >> 4
        } else {
            attr & 0x0F
        }
    }

    /// Rows `page*8 .. page*8+8` as RGBA into `out` (W * H * 4 bytes, row
    /// major). Band pages are left as they are: the caller draws them.
    pub fn render_page_rgba(&self, page: usize, out: &mut [u8]) {
        if self.is_band(page) {
            return;
        }
        let lut: Vec<[u8; 3]> = PALETTE_565.iter().map(|&c| rgb565_to_rgb(c)).collect();
        for row in 0..8 {
            let y = page * 8 + row;
            for x in 0..W {
                let c = lut[self.pixel(x, y) as usize];
                let o = (y * W + x) * 4;
                out[o..o + 3].copy_from_slice(&c);
                out[o + 3] = 255;
            }
        }
    }

    /// The whole screen as RGBA; band pages from `waterfall` (black
    /// without one).
    pub fn render_rgba(&self, waterfall: Option<&crate::spectrum::Waterfall>) -> Vec<u8> {
        let mut out = vec![0u8; W * H * 4];
        for p in 0..PAGES {
            if self.is_band(p) {
                match waterfall {
                    Some(w) => w.draw_page(self, p, &mut out),
                    None => crate::spectrum::Waterfall::default().draw_page(self, p, &mut out),
                }
            } else {
                self.render_page_rgba(p, &mut out);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_lookup() {
        let mut s = Screen::new();
        let mut bits = [0u8; W];
        bits[3] = 0b0000_0100; // column 3, row 2 of the page
        let mut attrs = [0x40u8; TCOLS]; // TEXT on BG
        attrs[0] = 0x9C; // FREQ on ALERT
        s.set_page(1, &bits, &attrs);
        assert_eq!(s.pixel(3, 10), 9);
        assert_eq!(s.pixel(3, 11), 0xC);
        assert_eq!(s.pixel(100, 10), 0);
        assert_eq!(s.take_dirty(), 1 << 1);
    }

    #[test]
    fn band_pages_follow_the_flag() {
        let mut s = Screen::new();
        assert!(s.apply(&[20, 0x01]).is_some());
        assert!(s.apply(&[21, 0x01]).is_some());
        assert!(s.is_band(20) && s.is_band(21) && !s.is_band(19));
        assert_eq!(s.band_first(), Some(20));
        // The band goes: the page comes back as pixels.
        let mut page = vec![20u8, 0x00];
        page.extend(crate::rle::encode(&[0u8; PAGE_BYTES]));
        assert!(s.apply(&page).is_some());
        assert!(!s.is_band(20) && s.is_band(21));
        assert_eq!(s.band_first(), Some(21));
    }

    #[test]
    fn a_full_refresh_is_counted_at_its_last_page() {
        let mut s = Screen::new();
        let mut page = vec![3u8, 0x00];
        page.extend(crate::rle::encode(&[0u8; PAGE_BYTES]));
        s.apply(&page).unwrap();
        assert_eq!(s.refreshes, 0);
        page[1] = 0x02;
        s.apply(&page).unwrap();
        assert_eq!(s.refreshes, 1);
        assert!(s.apply(&[4, 0x03]).is_some()); // a band page can be the last
        assert_eq!(s.refreshes, 2);
    }

    #[test]
    fn palette_expands() {
        assert_eq!(rgb565_to_rgb(0xFFFF), [255, 255, 255]);
        assert_eq!(rgb565_to_rgb(0x0000), [0, 0, 0]);
        assert_eq!(rgb565_to_rgb(0xF800), [255, 0, 0]);
    }
}
