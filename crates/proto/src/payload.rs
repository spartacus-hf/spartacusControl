//! Payload layouts (docs/link-protocol.md): parsers for what the radio sends,
//! builders for what the host sends. Readers accept longer payloads than they
//! know (fields are only ever appended).

use serde::Serialize;

use crate::frame::{AckCode, HostType, Input, TxEventKind};

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn need(&self, n: usize) -> Option<()> {
        (self.at + n <= self.b.len()).then_some(())
    }
    fn u8(&mut self) -> Option<u8> {
        self.need(1)?;
        self.at += 1;
        Some(self.b[self.at - 1])
    }
    fn u16(&mut self) -> Option<u16> {
        self.need(2)?;
        self.at += 2;
        Some(u16::from_le_bytes([self.b[self.at - 2], self.b[self.at - 1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }
    fn u32(&mut self) -> Option<u32> {
        self.need(4)?;
        self.at += 4;
        let s = &self.b[self.at - 4..self.at];
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn rest(&self) -> &'a [u8] {
        &self.b[self.at..]
    }
}

// ---- HELLO ----

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hello {
    pub proto: u8,
    pub fw_major: u8,
    pub fw_minor: u8,
    /// 0 AUDIO, 1 DIAG
    pub personality: u8,
    pub caps: u32,
    pub image_crc: u32,
    pub fb_w: u16,
    pub fb_h: u16,
    pub pages: u8,
    pub tcols: u8,
    pub max_body: u16,
    pub port: u8,
    pub packets_per_frame: u8,
}

impl Hello {
    pub fn parse(p: &[u8]) -> Option<Self> {
        let mut r = Rd::new(p);
        Some(Self {
            proto: r.u8()?,
            fw_major: r.u8()?,
            fw_minor: r.u8()?,
            personality: r.u8()?,
            caps: r.u32()?,
            image_crc: r.u32()?,
            fb_w: r.u16()?,
            fb_h: r.u16()?,
            pages: r.u8()?,
            tcols: r.u8()?,
            max_body: r.u16()?,
            port: r.u8()?,
            packets_per_frame: r.u8()?,
        })
    }

    pub fn has(&self, cap: u32) -> bool {
        self.caps & cap != 0
    }
}

// ---- STATUS ----

pub const RADIO_STATES: [&str; 6] = ["SETUP", "RECEIVE", "TX_PREPARE", "TRANSMIT", "FAULT", "OFF"];
pub const MODES: [&str; 6] = ["USB", "LSB", "CW", "AM", "FM", "DIGI"];
pub const PAGES: [&str; 16] = [
    "HOME",
    "MENU",
    "FREQ",
    "CAL",
    "DIAG",
    "TESTS",
    "TX_TEST",
    "CW_TRAINER",
    "SD",
    "PD",
    "BPF_SWEEP",
    "AF_SWEEP",
    "IQ_SCOPE",
    "SELFTEST",
    "RUNTIME",
    "WATERFALL",
];
pub const FAULT_BITS: [&str; 8] = ["ADC", "VIN", "5V", "BIAS", "SWR", "ENV", "DMA", "CLOCK"];

pub fn name_of(table: &[&'static str], i: u8) -> &'static str {
    table.get(i as usize).copied().unwrap_or("?")
}

/// The TX owner byte.
pub fn tx_owner_name(v: u8) -> &'static str {
    match v {
        0 => "none",
        1 => "CAT port 0",
        2 => "CAT port 1",
        3 => "LINK paddle/TUNE",
        4 => "LINK memory",
        5 => "radio key/paddle",
        6 => "microphone/VOX",
        _ => "other",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Status {
    pub uptime_ms: u32,
    pub dial_hz: u32,
    pub state: u8,
    pub mode: u8,
    pub page: u8,
    pub ui: u8,
    pub flags0: u8,
    pub flags1: u8,
    pub tx_owner: u8,
    pub load_pct: u8,
    pub faults: u16,
    pub vin_mv: u16,
    pub pep_cw: u16,
    pub swr_x100: u16,
    pub rx_dbfs_x10: i16,
    pub wpm: u8,
    pub cat: u8,
    pub cat0_age_ds: u16,
    /// Pipeline events logged since boot, wrapping: when it moves, read
    /// them with the console's `events`.
    pub event_seq: u8,
    pub reserved31: u8,
    pub settings_hash: u32,
    pub pd_mv: u16,
    pub rx_overruns: u16,
    /// USB capture gaps the host heard in the last 10 minutes (the DIGI page's GAPS).
    pub cap_gaps: u16,
    pub ref_state: u8,
    pub link: u8,
    pub ref_ppm_x100: i16,
    pub band: u8,
    pub arm_left_s: u8,
}

impl Status {
    pub const LEN: usize = 48;

    pub fn parse(p: &[u8]) -> Option<Self> {
        let mut r = Rd::new(p);
        Some(Self {
            uptime_ms: r.u32()?,
            dial_hz: r.u32()?,
            state: r.u8()?,
            mode: r.u8()?,
            page: r.u8()?,
            ui: r.u8()?,
            flags0: r.u8()?,
            flags1: r.u8()?,
            tx_owner: r.u8()?,
            load_pct: r.u8()?,
            faults: r.u16()?,
            vin_mv: r.u16()?,
            pep_cw: r.u16()?,
            swr_x100: r.u16()?,
            rx_dbfs_x10: r.i16()?,
            wpm: r.u8()?,
            cat: r.u8()?,
            cat0_age_ds: r.u16()?,
            event_seq: r.u8()?,
            reserved31: r.u8()?,
            settings_hash: r.u32()?,
            pd_mv: r.u16()?,
            rx_overruns: r.u16()?,
            cap_gaps: r.u16()?,
            ref_state: r.u8()?,
            link: r.u8()?,
            ref_ppm_x100: r.i16()?,
            band: r.u8()?,
            arm_left_s: r.u8()?,
        })
    }

    pub fn state_name(&self) -> &'static str {
        name_of(&RADIO_STATES, self.state)
    }
    pub fn mode_name(&self) -> &'static str {
        name_of(&MODES, self.mode)
    }
    pub fn page_name(&self) -> &'static str {
        name_of(&PAGES, self.page)
    }
    pub fn transmitting(&self) -> bool {
        self.state == 2 || self.state == 3
    }
    pub fn tx_mute(&self) -> bool {
        self.flags0 & 0x01 != 0
    }
    pub fn usb_iq(&self) -> bool {
        self.flags0 & 0x02 != 0
    }
    pub fn usb_vox(&self) -> bool {
        self.flags0 & 0x04 != 0
    }
    pub fn lsb(&self) -> bool {
        self.flags0 & 0x08 != 0
    }
    pub fn realtime_critical(&self) -> bool {
        self.flags0 & 0x10 != 0
    }
    pub fn background_blocked(&self) -> bool {
        self.flags0 & 0x20 != 0
    }
    pub fn monitor_mix(&self) -> bool {
        self.flags0 & 0x40 != 0
    }
    pub fn phones_off(&self) -> bool {
        self.flags0 & 0x80 != 0
    }
    pub fn armed(&self) -> bool {
        self.flags1 & 0x01 != 0
    }
    pub fn cat_ptt(&self) -> bool {
        self.flags1 & 0x02 != 0
    }
    pub fn memory_busy(&self) -> bool {
        self.flags1 & 0x04 != 0
    }
    pub fn change_pending(&self) -> bool {
        self.flags1 & 0x08 != 0
    }
    pub fn lockout(&self) -> bool {
        self.flags1 & 0x10 != 0
    }
    pub fn diag_personality(&self) -> bool {
        self.flags1 & 0x20 != 0
    }
    pub fn keyer_cannot_radiate(&self) -> bool {
        self.flags1 & 0x40 != 0
    }
    /// CW keys nothing until a lever has been held 1 s and released (the
    /// radio's power-up arm); practice, the trainer and TX TEST key without it.
    pub fn cw_arm_pending(&self) -> bool {
        self.flags1 & 0x80 != 0
    }
    pub fn cat_ports_open(&self) -> u8 {
        self.cat & 0x03
    }
    pub fn console_port(&self) -> Option<u8> {
        let p = self.cat >> 4;
        (p != 0x0F).then_some(p)
    }
    pub fn fault_names(&self) -> Vec<&'static str> {
        (0..8)
            .filter(|b| self.faults & (1 << b) != 0)
            .map(|b| FAULT_BITS[b])
            .collect()
    }
    pub fn swr(&self) -> Option<f32> {
        (self.swr_x100 != 0).then(|| self.swr_x100 as f32 / 100.0)
    }
    pub fn pep_w(&self) -> f32 {
        self.pep_cw as f32 / 100.0
    }
}

// ---- pipeline events (console `events`), ACK, TXEVENT, BYE, CWDEC ----

/// One line of the console's `events`: `<index> age_ms=<ms> <SEVERITY>
/// <STAGE> <REASON> <value hex>`, index 0 the newest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineEvent {
    pub index: u8,
    pub age_ms: u32,
    pub severity: String,
    pub stage: String,
    pub reason: String,
    pub value: u32,
}

impl PipelineEvent {
    pub fn parse_line(line: &str) -> Option<Self> {
        let mut t = line.split_whitespace();
        let index = t.next()?.parse().ok()?;
        let age_ms = t.next()?.strip_prefix("age_ms=")?.parse().ok()?;
        let severity = t.next()?.to_string();
        let stage = t.next()?.to_string();
        let reason = t.next()?.to_string();
        let value = u32::from_str_radix(t.next()?, 16).ok()?;
        Some(Self {
            index,
            age_ms,
            severity,
            stage,
            reason,
            value,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub req_type: u8,
    pub req_seq: u8,
    pub code: u8,
    pub detail: u8,
}

impl Ack {
    pub fn parse(p: &[u8]) -> Option<Self> {
        let mut r = Rd::new(p);
        Some(Self {
            req_type: r.u8()?,
            req_seq: r.u8()?,
            code: r.u8()?,
            detail: r.u8()?,
        })
    }
    pub fn code(&self) -> Option<AckCode> {
        AckCode::from_u8(self.code)
    }
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxEvent {
    pub kind: u8,
    pub detail: u16,
}

impl TxEvent {
    pub fn parse(p: &[u8]) -> Option<Self> {
        let mut r = Rd::new(p);
        let kind = r.u8()?;
        let _ = r.u8()?;
        Some(Self { kind, detail: r.u16()? })
    }
    pub fn kind(&self) -> Option<TxEventKind> {
        TxEventKind::from_u8(self.kind)
    }
}

pub fn parse_bye(p: &[u8]) -> Option<u8> {
    p.first().copied()
}

/// CWDEC: (character count after this frame, new characters).
pub fn parse_cwdec(p: &[u8]) -> Option<(u32, String)> {
    let mut r = Rd::new(p);
    let count = r.u32()?;
    Some((count, String::from_utf8_lossy(r.rest()).into_owned()))
}

// ---- Host -> radio builders: (type, payload) ----

pub type HostFrame = (HostType, Vec<u8>);

pub fn heartbeat(host_ms: u16) -> HostFrame {
    (HostType::Heartbeat, host_ms.to_le_bytes().to_vec())
}

pub fn sub(mask: u16, packets_per_frame: u8, status_hz: u8) -> HostFrame {
    let mut p = mask.to_le_bytes().to_vec();
    p.push(packets_per_frame.clamp(1, 4));
    p.push(status_hz.clamp(1, 20));
    (HostType::Sub, p)
}

pub fn refresh(what: u8) -> HostFrame {
    (HostType::Refresh, vec![what])
}

pub fn input(code: Input, value: i8) -> HostFrame {
    (HostType::Input, vec![code as u8, value as u8])
}

pub fn paddle(dit: bool, dah: bool) -> HostFrame {
    (HostType::Paddle, vec![(dit as u8) | ((dah as u8) << 1)])
}

/// ARM with the host's limits: key-down max in ms (rounded up to 0.1 s,
/// at most 25.5 s), memory max in s, ARM time-to-live in s.
pub fn tx_arm(on: bool, key_max_ms: u32, mem_max_s: u32, ttl_s: u32) -> HostFrame {
    let kd = key_max_ms.div_ceil(100).clamp(1, 255) as u8;
    (
        HostType::TxArm,
        vec![on as u8, kd, mem_max_s.clamp(1, 255) as u8, ttl_s.clamp(5, 255) as u8],
    )
}

pub fn tx_stop() -> HostFrame {
    (HostType::TxStop, Vec::new())
}

pub fn tune(on: bool) -> HostFrame {
    (HostType::Tune, vec![on as u8])
}

/// None when the text cannot be sent (too long, not ASCII).
pub fn memsend(text: &str) -> Option<HostFrame> {
    (text.len() <= 48 && text.is_ascii()).then(|| (HostType::MemSend, text.as_bytes().to_vec()))
}

/// None when the line cannot be sent (too long for the console, or has a line end).
pub fn cmd(line: &str) -> Option<HostFrame> {
    (line.len() <= 120 && !line.contains(['\r', '\n'])).then(|| (HostType::Cmd, line.as_bytes().to_vec()))
}

pub fn cat(command: &str) -> Option<HostFrame> {
    (command.len() <= 48 && command.is_ascii()).then(|| (HostType::Cat, command.as_bytes().to_vec()))
}

pub fn monitor(mix: bool, phones_off: bool, gain: u8) -> HostFrame {
    (HostType::Monitor, vec![(mix as u8) | ((phones_off as u8) << 1), gain])
}

pub fn link_off() -> HostFrame {
    (HostType::LinkOff, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_its_fields() {
        let mut b = vec![0u8; 48];
        b[0..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        b[4..8].copy_from_slice(&14_074_000u32.to_le_bytes());
        b[8] = 3;
        b[9] = 5;
        b[16] = 0x40;
        b[27] = 0xF1;
        b[46] = 6;
        let s = Status::parse(&b).unwrap();
        assert_eq!(s.dial_hz, 14_074_000);
        assert_eq!(s.state_name(), "TRANSMIT");
        assert!(s.transmitting());
        assert_eq!(s.mode_name(), "DIGI");
        assert_eq!(s.fault_names(), vec!["DMA"]);
        assert_eq!(s.cat_ports_open(), 1);
        assert_eq!(s.console_port(), None);
        assert!(Status::parse(&b[..47]).is_none());
    }

    #[test]
    fn console_event_lines() {
        let e = PipelineEvent::parse_line("1 age_ms=57568 NOTE USB FB FEED0001").unwrap();
        assert_eq!((e.index, e.age_ms, e.value), (1, 57568, 0xFEED_0001));
        assert_eq!(
            (e.severity.as_str(), e.stage.as_str(), e.reason.as_str()),
            ("NOTE", "USB", "FB")
        );
        assert!(PipelineEvent::parse_line("latched=0 hard_lockout=0").is_none());
    }

    #[test]
    fn builders_respect_limits() {
        assert_eq!(tx_arm(true, 3000, 60, 2).1, vec![1, 30, 60, 5]);
        assert_eq!(tx_arm(true, 100_000, 999, 999).1, vec![1, 255, 255, 255]);
        assert!(cmd("set put 5 1").is_some());
        assert!(cmd("a\nb").is_none());
        assert!(cmd(&"x".repeat(121)).is_none());
        assert!(memsend("CQ CQ DE TEST").is_some());
        assert!(memsend(&"E".repeat(49)).is_none());
        assert_eq!(paddle(true, true).1, vec![3]);
    }
}
