//! SPECTRUM frames: one waterfall update with its axis (the engine's
//! `spec_view_t`), and the radio's waterfall palettes (alt2 `ui_waterfall.c`).

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpectrumLine {
    pub line_seq: u16,
    pub flags: u8,
    pub palette: u8,
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
        let trace = take(flags & 0x01 != 0)?;
        let peak = take(flags & 0x02 != 0)?;
        Some(Self {
            line_seq: u16_at(0),
            flags,
            palette: p[3],
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

    pub fn centre_px(&self) -> f32 {
        self.width as f32 / 2.0
    }

    /// Frequency of pixel `x` (from the left of the line).
    pub fn hz_at(&self, x: f32) -> f64 {
        self.centre_hz as f64 + (x as f64 - self.centre_px() as f64) * self.hz_per_px as f64
    }
}

/// `ui_waterfall.c` palettes[4][16], RGB565: CLASSIC, GRAY, HEAT, OCEAN.
pub const PALETTES_565: [[u16; 16]; 4] = [
    [
        0x0000, 0x0005, 0x000A, 0x0091, 0x0137, 0x01DE, 0x037E, 0x055C, 0x0673, 0x06C4, 0x5F40,
        0xDFC0, 0xFE00, 0xFB80, 0xFD2E, 0xFFFF,
    ],
    [
        0x0000, 0x1082, 0x2104, 0x3186, 0x4228, 0x52AA, 0x632C, 0x73AE, 0x8C51, 0x9CD3, 0xAD55,
        0xBDD7, 0xCE79, 0xDEFB, 0xEF7D, 0xFFFF,
    ],
    [
        0x0000, 0x1800, 0x3800, 0x5800, 0x7800, 0x9800, 0xB0C0, 0xC1E0, 0xDB00, 0xEC20, 0xFD40,
        0xFE43, 0xFF26, 0xFFEA, 0xFFF5, 0xFFFF,
    ],
    [
        0x0002, 0x0824, 0x1047, 0x1869, 0x208C, 0x210E, 0x220F, 0x22F0, 0x1BD1, 0x2C90, 0x452E,
        0x5DEC, 0x6E8B, 0x9EE9, 0xCF27, 0xFF85,
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_and_trace() {
        let w = 4u16;
        let mut p = vec![0u8; SpectrumLine::HEADER];
        p[0] = 7;
        p[2] = 0x01;
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
        assert!(l.receiving && !l.valid);
        assert_eq!(l.bottom_db, -120);
        assert_eq!(l.hz_at(0.0), 14_073_980.0);
        assert!(SpectrumLine::parse(&p[..p.len() - 1]).is_none());
    }
}
