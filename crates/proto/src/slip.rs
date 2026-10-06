//! SLIP framing (RFC 1055) with the LINK's body: `type, seq, payload, crc16le`.

use crate::crc;
use crate::frame::Frame;

pub const END: u8 = 0xC0;
pub const ESC: u8 = 0xDB;
pub const ESC_END: u8 = 0xDC;
pub const ESC_ESC: u8 = 0xDD;

/// Largest radio -> host body (type, seq, payload, CRC).
pub const MAX_BODY: usize = 1024 + 4;

fn push_stuffed(out: &mut Vec<u8>, b: u8) {
    match b {
        END => out.extend_from_slice(&[ESC, ESC_END]),
        ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
        _ => out.push(b),
    }
}

/// One frame as it goes on the wire: END, stuffed body, END.
pub fn encode_frame(ty: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() * 2 + 8);
    out.push(END);
    let mut c = crc::update(crc::INIT, &[ty, seq]);
    c = crc::update(c, payload);
    for &b in [ty, seq].iter().chain(payload.iter()) {
        push_stuffed(&mut out, b);
    }
    push_stuffed(&mut out, (c & 0xFF) as u8);
    push_stuffed(&mut out, (c >> 8) as u8);
    out.push(END);
    out
}

/// Why a frame was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Crc,
    Short,
    Oversize,
    BadEscape,
}

/// What one byte of input produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Frame(Frame),
    Dropped(DropReason),
}

/// Streaming decoder for the radio's output. Bytes that are not a valid
/// frame (noise, text the console sent before the switch) end up as one
/// `Dropped` event at the next END; the following frames decode normally.
#[derive(Debug, Default)]
pub struct Decoder {
    body: Vec<u8>,
    esc: bool,
    bad: Option<DropReason>,
    pub dropped: u64,
    pub frames: u64,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            body: Vec::with_capacity(MAX_BODY),
            ..Default::default()
        }
    }

    pub fn reset(&mut self) {
        self.body.clear();
        self.esc = false;
        self.bad = None;
    }

    /// Feed bytes; every completed frame or drop is pushed to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Event>) {
        for &b in bytes {
            if let Some(ev) = self.byte(b) {
                out.push(ev);
            }
        }
    }

    pub fn byte(&mut self, b: u8) -> Option<Event> {
        if b == END {
            let result = self.finish();
            self.reset();
            return result;
        }
        if self.bad.is_some() {
            return None;
        }
        let v = if self.esc {
            self.esc = false;
            match b {
                ESC_END => END,
                ESC_ESC => ESC,
                _ => {
                    self.bad = Some(DropReason::BadEscape);
                    return None;
                }
            }
        } else if b == ESC {
            self.esc = true;
            return None;
        } else {
            b
        };
        if self.body.len() >= MAX_BODY {
            self.bad = Some(DropReason::Oversize);
            return None;
        }
        self.body.push(v);
        None
    }

    fn finish(&mut self) -> Option<Event> {
        if let Some(r) = self.bad {
            self.dropped += 1;
            return Some(Event::Dropped(r));
        }
        if self.body.is_empty() {
            return None;
        }
        if self.body.len() < 4 || self.esc {
            self.dropped += 1;
            return Some(Event::Dropped(DropReason::Short));
        }
        let n = self.body.len() - 2;
        let got = u16::from_le_bytes([self.body[n], self.body[n + 1]]);
        if crc::crc16(&self.body[..n]) != got {
            self.dropped += 1;
            return Some(Event::Dropped(DropReason::Crc));
        }
        self.frames += 1;
        Some(Event::Frame(Frame {
            ty: self.body[0],
            seq: self.body[1],
            payload: self.body[2..n].to_vec(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_stuffing() {
        let payload = [END, ESC, ESC_END, ESC_ESC, 0, 255];
        let wire = encode_frame(0x08, END, &payload);
        let mut d = Decoder::new();
        let mut ev = Vec::new();
        d.feed(b"garbage before\r\n", &mut ev);
        d.feed(&wire, &mut ev);
        let frames: Vec<_> = ev
            .into_iter()
            .filter_map(|e| if let Event::Frame(f) = e { Some(f) } else { None })
            .collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].ty, 0x08);
        assert_eq!(frames[0].seq, END);
        assert_eq!(frames[0].payload, payload);
    }

    #[test]
    fn corrupt_frame_is_dropped() {
        let mut wire = encode_frame(0x0B, 1, &[1, 2, 3, 4]);
        wire[3] ^= 1;
        let mut d = Decoder::new();
        let mut ev = Vec::new();
        d.feed(&wire, &mut ev);
        assert_eq!(ev, vec![Event::Dropped(DropReason::Crc)]);
        let mut ev = Vec::new();
        d.feed(&encode_frame(0x40, 2, &[]), &mut ev);
        assert!(matches!(ev[0], Event::Frame(_)));
    }
}
