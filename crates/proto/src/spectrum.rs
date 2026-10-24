//! SPECTRUM frames: one waterfall update with its axis (the engine's
//! `spec_view_t`), and the radio's waterfall palettes.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpectrumLine {
    pub line_seq: u16,
    pub flags: u8,
    pub palette: u8,
    /// The page's layout: [`Self::WATERFALL`], [`Self::SPLIT`] or [`Self::SPECTRUM`].
    pub layout: u8,
    pub width: u16,
    pub centre_hz: u32,
    pub hz_per_px: f32,
    pub dial_hz: u32,
    pub dial_x: i16,
    pub pass_lo_x: i16,
    pub pass_hi_x: i16,
    pub valid: bool,
    pub receiving: bool,
    pub scanning: bool,
    pub sweep_pct: u8,
    pub lines: u8,
    pub load_rx: u8,
    pub load_wf: u8,
    pub guard: bool,
    pub bottom_db: i16,
    pub top_db: i16,
    pub rate10: u16,
    /// 4-bit levels, one per pixel.
    pub levels: Vec<u8>,
    /// dBFS + 160 per pixel (0 = none), when the radio sends a trace.
    pub trace: Option<Vec<u8>>,
    pub peak: Option<Vec<u8>>,
}

impl SpectrumLine {
    pub const HEADER: usize = 38;

    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < Self::HEADER {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
        let i16_at = |o: usize| u16_at(o) as i16;
        let u32_at = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
        let width = u16_at(4) as usize;
        let flags = p[2];
        let packed = width.div_ceil(2);
        let mut at = Self::HEADER;
        let line = p.get(at..at + packed)?;
        at += packed;
        let levels = (0..width)
            .map(|x| {
                let b = line[x / 2];
                if x % 2 == 0 { b & 0x0F } else { b >> 4 }
            })
            .collect();
        let mut take = |on: bool| -> Option<Option<Vec<u8>>> {
            if !on {
                return Some(None);
            }
            let v = p.get(at..at + width)?.to_vec();
            at += width;
            Some(Some(v))
        };
        let trace = take(flags & Self::TRACE != 0)?;
        let peak = take(flags & Self::PEAK != 0)?;
        Some(Self {
            line_seq: u16_at(0),
            flags,
            palette: p[3] & 0x0F,
            layout: p[3] >> 4,
            width: width as u16,
            centre_hz: u32_at(6),
            hz_per_px: f32::from_bits(u32_at(10)),
            dial_hz: u32_at(14),
            dial_x: i16_at(18),
            pass_lo_x: i16_at(20),
            pass_hi_x: i16_at(22),
            valid: p[24] != 0,
            receiving: p[25] != 0,
            scanning: p[26] != 0,
            sweep_pct: p[27],
            lines: p[28],
            load_rx: p[29],
            load_wf: p[30],
            guard: p[31] != 0,
            bottom_db: i16_at(32),
            top_db: i16_at(34),
            rate10: u16_at(36),
            levels,
            trace,
            peak,
        })
    }

    /// Flags: a trace follows the line, a peak follows the trace.
    pub const TRACE: u8 = 0x01;
    pub const PEAK: u8 = 0x02;
    /// Layouts: the history alone, a spectrum over it, the spectrum alone
    /// (its line is not history).
    pub const WATERFALL: u8 = 0;
    pub const SPLIT: u8 = 1;
    pub const SPECTRUM: u8 = 2;

    pub fn has_history(&self) -> bool {
        self.layout != Self::SPECTRUM
    }

    pub fn centre_px(&self) -> f32 {
        self.width as f32 / 2.0
    }

    /// Frequency of pixel `x` (from the left of the line).
    pub fn hz_at(&self, x: f32) -> f64 {
        self.centre_hz as f64 + (x as f64 - self.centre_px() as f64) * self.hz_per_px as f64
    }
}

/// The radio's four waterfall palettes, RGB565: CLASSIC, GRAY, HEAT, OCEAN.
pub const PALETTES_565: [[u16; 16]; 4] = [
    [
        0x0000, 0x0005, 0x000A, 0x0091, 0x0137, 0x01DE, 0x037E, 0x055C, 0x0673, 0x06C4, 0x5F40, 0xDFC0, 0xFE00, 0xFB80,
        0xFD2E, 0xFFFF,
    ],
    [
        0x0000, 0x1082, 0x2104, 0x3186, 0x4228, 0x52AA, 0x632C, 0x73AE, 0x8C51, 0x9CD3, 0xAD55, 0xBDD7, 0xCE79, 0xDEFB,
        0xEF7D, 0xFFFF,
    ],
    [
        0x0000, 0x1800, 0x3800, 0x5800, 0x7800, 0x9800, 0xB0C0, 0xC1E0, 0xDB00, 0xEC20, 0xFD40, 0xFE43, 0xFF26, 0xFFEA,
        0xFFF5, 0xFFFF,
    ],
    [
        0x0002, 0x0824, 0x1047, 0x1869, 0x208C, 0x210E, 0x220F, 0x22F0, 0x1BD1, 0x2C90, 0x452E, 0x5DEC, 0x6E8B, 0x9EE9,
        0xCF27, 0xFF85,
    ],
];

/// The radio's waterfall geometry: the line's 304 pixels
/// start at panel x 8; a history line is 152 bytes; with a spectrum the band
/// also holds its trace (int16), peak and the raster's trace rows.
pub const SPEC_X0: usize = 8;
const LINE_BYTES: usize = 152;
const SPECTRUM_BYTES: usize = 304 * 3 + 304;

/// The radio's raster colours (RGB565).
const C_GRID: u16 = 0x2104;
const C_PASS: u16 = 0x0846;
const C_TRACE: u16 = 0xFFFF;
const C_PEAK: u16 = 0xFD20;
const C_DIAL: u16 = 0xF800;

/// How the radio splits its band between the spectrum and the history
/// (as its own layout does): the spectrum on top (all of the band in
/// the SPECTRUM layout, 3/8 of it in SPLIT), then the history at the
/// shortest line height (2..8 rows) whose ring still fits the band's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandLayout {
    pub spec_rows: usize,
    /// Rows of history (0 for none) and the rows of each line.
    pub wf_rows: usize,
    pub line_h: usize,
}

impl BandLayout {
    pub fn of(pages: usize, spectrum: bool, history: bool) -> Self {
        let rows = pages * 8;
        let bytes = pages * crate::screen::W;
        let (spec_rows, extra) = match (spectrum, history) {
            (false, _) => (0, 0),
            (true, false) => (rows, SPECTRUM_BYTES),
            (true, true) => ((rows * 3 / 8) & !7, SPECTRUM_BYTES),
        };
        let wf_rows = rows - spec_rows;
        match (2..=8).find(|&h| wf_rows > 0 && (wf_rows / h + 1) * LINE_BYTES + extra <= bytes) {
            Some(line_h) => Self {
                spec_rows,
                wf_rows,
                line_h,
            },
            None => Self {
                spec_rows,
                wf_rows: 0,
                line_h: 2,
            },
        }
    }

    /// The history's rows that whole lines fill.
    pub fn history_rows(&self) -> usize {
        self.wf_rows / self.line_h * self.line_h
    }
}

/// The waterfall page's band as the radio draws it (`wf_raster`): the
/// spectrum (trace, fill, peak hold, 10 dB grid, pass band) over the
/// history, newest line on top. The radio's FILL and MARKERS settings do
/// not travel in the frame; their defaults (fill on, dial and pass band)
/// are drawn.
#[derive(Debug, Clone, Default)]
pub struct Waterfall {
    /// 4-bit levels per line, newest first.
    lines: std::collections::VecDeque<Vec<u8>>,
    last_seq: Option<u16>,
    /// The newest frame: palette, scale, trace, peak, dial and pass band.
    view: Option<SpectrumLine>,
}

impl Waterfall {
    /// More than a band ever shows (24 pages at 4 rows a line: 48).
    const KEEP: usize = 120;

    /// A SPECTRUM frame: a new history line when its sequence advanced.
    /// The history is cut to what the radio holds, so what it cleared (a
    /// retune, a new layout) clears here too.
    pub fn push(&mut self, line: &SpectrumLine) {
        if line.has_history() && self.last_seq != Some(line.line_seq) {
            self.lines.push_front(line.levels.clone());
        }
        self.last_seq = Some(line.line_seq);
        self.lines.truncate((line.lines as usize).min(Self::KEEP));
        self.view = Some(line.clone());
    }

    /// The layout of a band of `pages` pages for the newest frame.
    pub fn layout(&self, pages: usize) -> BandLayout {
        let v = self.view.as_ref();
        BandLayout::of(
            pages,
            v.is_some_and(|v| v.layout != SpectrumLine::WATERFALL),
            v.is_none_or(|v| v.has_history()),
        )
    }

    /// Draw band page `page` of `screen` into `out` (RGBA, W x H, row
    /// major); black where there is nothing yet.
    pub fn draw_page(&self, screen: &crate::screen::Screen, page: usize, out: &mut [u8]) {
        use crate::screen::{W, rgb565_to_rgb};
        let band_top = screen.band_first().unwrap_or(page) * 8;
        let lay = self.layout((screen.band_pages.count_ones() as usize).max(1));
        let width = self.view.as_ref().map_or(0, |v| (v.width as usize).min(W - SPEC_X0));
        let rgba = |c: u16| {
            let [r, g, b] = rgb565_to_rgb(c);
            [r, g, b, 255]
        };
        let pal: Vec<[u8; 4]> = self
            .view
            .as_ref()
            .map_or(&PALETTES_565[0], |v| &PALETTES_565[v.palette.min(3) as usize])
            .iter()
            .map(|&c| rgba(c))
            .collect();
        let spec = self
            .view
            .as_ref()
            .filter(|_| lay.spec_rows > 0)
            .map(|v| Spectrum::new(v, lay.spec_rows));
        for r in 0..8 {
            let y = page * 8 + r;
            let row_px = &mut out[y * W * 4..(y + 1) * W * 4];
            for px in row_px.as_chunks_mut::<4>().0.iter_mut() {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
            let Some(v) = &self.view else { continue };
            let px = &mut row_px[SPEC_X0 * 4..(SPEC_X0 + width) * 4];
            let row = y.saturating_sub(band_top);
            if let Some(s) = spec.as_ref().filter(|_| row < lay.spec_rows) {
                s.row(row, &pal, px, &rgba);
            } else if row >= lay.spec_rows && row - lay.spec_rows < lay.history_rows() {
                let r = row - lay.spec_rows;
                if let Some(line) = self.lines.get(r / lay.line_h) {
                    for (x, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        p.copy_from_slice(&pal[line.get(x).copied().unwrap_or(0) as usize & 15]);
                    }
                }
                // The dial as a dotted line, so the signal under it stays readable.
                if (r & 3) < 2 {
                    put(px, v.dial_x, rgba(C_DIAL));
                }
            }
        }
    }
}

/// Pixel `x` of a row, when it is on the row.
fn put(px: &mut [u8], x: i16, c: [u8; 4]) {
    if x >= 0 && (x as usize) < px.len() / 4 {
        px[x as usize * 4..x as usize * 4 + 4].copy_from_slice(&c);
    }
}

/// The spectrum area for one frame (`spectrum_row` with the frame's scale):
/// dBFS x 8 maps to rows through `top8`, `span8` and `k16` as on the radio.
struct Spectrum<'a> {
    v: &'a SpectrumLine,
    rows: i32,
    top8: i32,
    span8: i32,
    k16: i32,
    /// Each column's trace row plus one, clamped to 0..rows+1, or None.
    ycol: Vec<Option<i32>>,
}

impl<'a> Spectrum<'a> {
    fn new(v: &'a SpectrumLine, rows: usize) -> Self {
        let top8 = v.top_db as i32 * 8;
        let span8 = ((v.top_db as i32 - v.bottom_db as i32) * 8).max(80);
        let k16 = ((rows as u32) << 16) as i32 / span8;
        let mut s = Self {
            v,
            rows: rows as i32,
            top8,
            span8,
            k16,
            ycol: Vec::new(),
        };
        s.ycol = (0..v.width as usize)
            .map(|x| {
                let b = *v.trace.as_ref()?.get(x)?;
                (b != 0).then(|| (s.y((b as i32 - 160) * 8) + 1).clamp(0, s.rows + 1))
            })
            .collect();
        s
    }

    /// Row of the area where dBFS x 8 `v8` sits (may be off the area).
    fn y(&self, v8: i32) -> i32 {
        ((self.top8 - v8) * self.k16) >> 16
    }

    fn row(&self, row: usize, pal: &[[u8; 4]], px: &mut [u8], rgba: &dyn Fn(u16) -> [u8; 4]) {
        let (rows, row) = (self.rows, row as i32);
        // dB at this row and the next: a 10 dB grid line where they straddle one.
        let db = self.top8 - ((2 * row + 1) * self.span8) / (2 * rows);
        let db_next = self.top8 - ((2 * row + 3) * self.span8) / (2 * rows);
        let grid = (db + 1600) / 80 != (db_next + 1600) / 80;
        let lv = ((db - (self.top8 - self.span8)) * 16) / self.span8;
        let fill = pal[lv.clamp(0, 15) as usize];
        let bg = rgba(if grid { C_GRID } else { 0 });
        let pass = if grid { bg } else { rgba(C_PASS) };
        let (plo, phi) = (self.v.pass_lo_x as i32, self.v.pass_hi_x as i32);
        let (trace, peak) = (rgba(C_TRACE), rgba(C_PEAK));
        // The trace joins each point to the one before it: rows lo..hi of
        // the column are the line, those under the point the fill.
        let mut prev = self.ycol.first().copied().flatten().unwrap_or(0x7FFF);
        let rr = row + 1;
        for (x, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let xi = x as i32;
            let mut c = if phi >= plo && xi >= plo && xi <= phi { pass } else { bg };
            if let Some(y) = self.ycol.get(x).copied().flatten() {
                let (lo, hi) = (y.min(prev), y.max(prev));
                if rr >= lo && rr <= hi {
                    c = trace;
                } else if rr > y {
                    c = fill;
                }
                prev = y;
            }
            if let Some(&pk) = self.v.peak.as_ref().and_then(|p| p.get(x))
                && pk != 0
                && row == self.y((pk as i32 - 160) * 8)
            {
                c = peak;
            }
            p.copy_from_slice(&c);
        }
        put(px, self.v.dial_x, rgba(C_DIAL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_and_trace() {
        let w = 4u16;
        let mut p = vec![0u8; SpectrumLine::HEADER];
        p[0] = 7;
        p[2] = 0x01;
        p[3] = 0x13;
        p[4..6].copy_from_slice(&w.to_le_bytes());
        p[6..10].copy_from_slice(&14_074_000u32.to_le_bytes());
        p[10..14].copy_from_slice(&10.0f32.to_bits().to_le_bytes());
        p[25] = 1;
        p[32..34].copy_from_slice(&(-120i16).to_le_bytes());
        p.extend_from_slice(&[0x21, 0x43]);
        p.extend_from_slice(&[10, 20, 30, 40]);
        let l = SpectrumLine::parse(&p).unwrap();
        assert_eq!(l.levels, vec![1, 2, 3, 4]);
        assert_eq!(l.trace.clone().unwrap(), vec![10, 20, 30, 40]);
        assert!(l.peak.is_none());
        assert_eq!((l.palette, l.layout), (3, SpectrumLine::SPLIT));
        assert!(l.receiving && !l.valid);
        assert_eq!(l.bottom_db, -120);
        assert_eq!(l.hz_at(0.0), 14_073_980.0);
        assert!(SpectrumLine::parse(&p[..p.len() - 1]).is_none());
    }

    #[test]
    fn band_layout_matches_the_radio() {
        // The whole band (24 pages): 4-row lines; 22 with the CW line: 4;
        // under the settings list (12 pages): 4.
        assert_eq!(
            BandLayout::of(24, false, true),
            BandLayout {
                spec_rows: 0,
                wf_rows: 192,
                line_h: 4
            }
        );
        assert_eq!(BandLayout::of(22, false, true).line_h, 4);
        assert_eq!(BandLayout::of(12, false, true).line_h, 4);
        // SPLIT: 72 spectrum rows over 120 of 3-row history.
        assert_eq!(
            BandLayout::of(24, true, true),
            BandLayout {
                spec_rows: 72,
                wf_rows: 120,
                line_h: 3
            }
        );
        // SPECTRUM: all of it, no history.
        assert_eq!(
            BandLayout::of(24, true, false),
            BandLayout {
                spec_rows: 192,
                wf_rows: 0,
                line_h: 2
            }
        );
        assert_eq!(BandLayout::of(24, true, false).history_rows(), 0);
    }

    fn frame(seq: u16, levels: Vec<u8>, lines: u8) -> SpectrumLine {
        let mut p = vec![0u8; SpectrumLine::HEADER];
        p[4..6].copy_from_slice(&(levels.len() as u16).to_le_bytes());
        p.extend(std::iter::repeat_n(0u8, levels.len().div_ceil(2)));
        let mut l = SpectrumLine::parse(&p).unwrap();
        l.line_seq = seq;
        l.levels = levels;
        l.lines = lines;
        l.dial_x = -1;
        l.pass_lo_x = 1;
        l.pass_hi_x = 0;
        l
    }

    fn band(first: usize, pages: usize) -> crate::screen::Screen {
        let mut s = crate::screen::Screen::new();
        for p in first..first + pages {
            assert!(s.apply(&[p as u8, 0x01]).is_some());
        }
        s
    }

    fn draw(w: &Waterfall, s: &crate::screen::Screen) -> impl Fn(usize, usize) -> [u8; 4] + use<> {
        let mut out = vec![0u8; crate::screen::W * crate::screen::H * 4];
        for p in 0..crate::screen::PAGES {
            if s.is_band(p) {
                w.draw_page(s, p, &mut out);
            }
        }
        move |x: usize, y: usize| {
            let o = (y * crate::screen::W + x) * 4;
            [out[o], out[o + 1], out[o + 2], out[o + 3]]
        }
    }

    fn colour(c: u16) -> [u8; 4] {
        let [r, g, b] = crate::screen::rgb565_to_rgb(c);
        [r, g, b, 255]
    }

    #[test]
    fn waterfall_newest_on_top_at_the_radios_line_height() {
        let mut w = Waterfall::default();
        w.push(&frame(1, vec![15; 304], 1));
        w.push(&frame(1, vec![15; 304], 1)); // the same line again: no new history
        w.push(&frame(2, vec![0; 304], 2));
        let px = draw(&w, &band(2, 24));
        // Rows 16-19: the newest line (level 0); 20-23: the older (15); 24
        // on: no history yet (black); outside the 304-pixel line: black.
        assert_eq!(px(8, 16), colour(PALETTES_565[0][0]));
        assert_eq!(px(311, 19), colour(PALETTES_565[0][0]));
        assert_eq!(px(8, 20), colour(PALETTES_565[0][15]));
        assert_eq!(px(8, 23), colour(PALETTES_565[0][15]));
        assert_eq!(px(8, 24), [0, 0, 0, 255]);
        assert_eq!(px(0, 20), [0, 0, 0, 255]);
        assert_eq!(px(312, 20), [0, 0, 0, 255]);
        // The radio cleared its history (a retune): so does the mirror.
        w.push(&frame(3, vec![15; 304], 1));
        let px = draw(&w, &band(2, 24));
        assert_eq!(px(8, 16), colour(PALETTES_565[0][15]));
        assert_eq!(px(8, 20), [0, 0, 0, 255]);
    }

    #[test]
    fn split_layout_draws_trace_fill_and_dotted_dial() {
        let mut w = Waterfall::default();
        let mut l = frame(1, vec![15; 304], 1);
        l.layout = SpectrumLine::SPLIT;
        l.top_db = -20;
        l.bottom_db = -100;
        l.dial_x = 152;
        // -60 dBFS everywhere: half way down the 72-row spectrum.
        l.trace = Some(vec![100; 304]);
        w.push(&l);
        let px = draw(&w, &band(2, 24));
        // Spectrum rows 16..87: the trace at row 35 (+16; the radio's k16
        // truncates), fill under it.
        assert_eq!(px(8 + 10, 16 + 35), colour(C_TRACE));
        assert_eq!(px(8 + 10, 16 + 34), [0, 0, 0, 255]);
        assert_ne!(px(8 + 10, 16 + 50), [0, 0, 0, 255]);
        assert_eq!(px(8 + 152, 16 + 10), colour(C_DIAL));
        // History from row 88, 3 rows a line; the dial dotted on it.
        assert_eq!(px(8 + 10, 88), colour(PALETTES_565[0][15]));
        assert_eq!(px(8 + 10, 90), colour(PALETTES_565[0][15]));
        assert_eq!(px(8 + 10, 91), [0, 0, 0, 255]);
        assert_eq!(px(8 + 152, 88), colour(C_DIAL));
        assert_eq!(px(8 + 152, 90), colour(PALETTES_565[0][15]));
    }

    #[test]
    fn spectrum_layout_keeps_no_history() {
        let mut w = Waterfall::default();
        let mut l = frame(1, vec![15; 304], 1);
        l.flags = SpectrumLine::TRACE;
        l.layout = SpectrumLine::SPECTRUM;
        l.top_db = -20;
        l.bottom_db = -100;
        l.trace = Some(vec![100; 304]);
        w.push(&l);
        assert_eq!(w.layout(24), BandLayout::of(24, true, false));
        assert!(w.lines.is_empty());
        let px = draw(&w, &band(2, 24));
        // All 192 rows are spectrum: -60 dBFS at row 95.
        assert_eq!(px(18, 16 + 95), colour(C_TRACE));
        assert_ne!(px(18, 200), colour(PALETTES_565[0][15]));
    }
}
