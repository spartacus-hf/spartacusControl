//! Frame types and codes (docs/link-protocol.md sections 3 and 4).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub seq: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn radio_type(&self) -> Option<RadioType> {
        RadioType::from_u8(self.ty)
    }
}

/// Radio -> host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RadioType {
    Hello = 0x01,
    Status = 0x02,
    Screen = 0x03,
    // 0x04 BAND, 0x06 EVENT and 0x0A SETVAL: reserved (not sent by v1 firmware).
    Spectrum = 0x05,
    CwDec = 0x07,
    Console = 0x08,
    CatReply = 0x09,
    Ack = 0x0B,
    Bye = 0x0C,
    TxEvent = 0x0D,
}

impl RadioType {
    pub fn from_u8(v: u8) -> Option<Self> {
        use RadioType::*;
        Some(match v {
            0x01 => Hello,
            0x02 => Status,
            0x03 => Screen,
            0x05 => Spectrum,
            0x07 => CwDec,
            0x08 => Console,
            0x09 => CatReply,
            0x0B => Ack,
            0x0C => Bye,
            0x0D => TxEvent,
            _ => return None,
        })
    }
}

/// Host -> radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HostType {
    Heartbeat = 0x40,
    Sub = 0x41,
    Refresh = 0x42,
    Input = 0x43,
    Paddle = 0x44,
    TxArm = 0x45,
    TxStop = 0x46,
    MemSend = 0x47,
    Cmd = 0x48,
    Cat = 0x49,
    // 0x4A GETVAL: reserved.
    Monitor = 0x4B,
    LinkOff = 0x4C,
    Tune = 0x4D,
}

impl HostType {
    pub fn from_u8(v: u8) -> Option<Self> {
        use HostType::*;
        Some(match v {
            0x40 => Heartbeat,
            0x41 => Sub,
            0x42 => Refresh,
            0x43 => Input,
            0x44 => Paddle,
            0x45 => TxArm,
            0x46 => TxStop,
            0x47 => MemSend,
            0x48 => Cmd,
            0x49 => Cat,
            0x4B => Monitor,
            0x4C => LinkOff,
            0x4D => Tune,
            _ => return None,
        })
    }
}

/// ACK codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AckCode {
    Ok = 0,
    BusyTx = 1,
    BusyOwner = 2,
    BusyRt = 3,
    NotArmed = 4,
    Fault = 5,
    Bad = 6,
    Unsupported = 7,
    Busy = 8,
    Capped = 9,
    Muted = 10,
    Range = 11,
    Diag = 12,
    Pending = 13,
    Page = 14,
}

impl AckCode {
    pub fn from_u8(v: u8) -> Option<Self> {
        use AckCode::*;
        Some(match v {
            0 => Ok,
            1 => BusyTx,
            2 => BusyOwner,
            3 => BusyRt,
            4 => NotArmed,
            5 => Fault,
            6 => Bad,
            7 => Unsupported,
            8 => Busy,
            9 => Capped,
            10 => Muted,
            11 => Range,
            12 => Diag,
            13 => Pending,
            14 => Page,
            _ => return None,
        })
    }

    /// What a person should read.
    pub fn explain(self) -> &'static str {
        use AckCode::*;
        match self {
            Ok => "done",
            BusyTx => "refused: the radio is transmitting",
            BusyOwner => "refused: another program or the radio's own key is transmitting",
            BusyRt => "not now: the radio's audio is realtime-critical (keyed CW or a TX stream); retry in receive",
            NotArmed => "refused: TX is not armed",
            Fault => "refused: a fault or lockout is latched on the radio",
            Bad => "refused: malformed request",
            Unsupported => "this firmware does not support it",
            Busy => "busy: the previous console command has not finished",
            Capped => "released by a cap",
            Muted => "TX MUTE is on: sidetone only, no RF",
            Range => "value out of range",
            Diag => "not in the DIAG personality (TX is muted there)",
            Pending => "refused: a mode or band change is in flight",
            Page => "refused: TUNE needs the radio's TX TEST page (a held key is a carrier only there)",
        }
    }
}

/// TXEVENT kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TxEventKind {
    Armed = 1,
    Disarmed = 2,
    KeyCap = 3,
    HeartbeatLost = 4,
    Stopped = 5,
    MemoryDone = 6,
    LeverStale = 7,
    MemoryCap = 8,
    OtherOwner = 9,
    TuneLeft = 10,
}

impl TxEventKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        use TxEventKind::*;
        Some(match v {
            1 => Armed,
            2 => Disarmed,
            3 => KeyCap,
            4 => HeartbeatLost,
            5 => Stopped,
            6 => MemoryDone,
            7 => LeverStale,
            8 => MemoryCap,
            9 => OtherOwner,
            10 => TuneLeft,
            _ => return None,
        })
    }
}

/// SUB channel bits.
pub mod sub {
    pub const STATUS: u16 = 1 << 0;
    pub const SCREEN: u16 = 1 << 1;
    pub const SPECTRUM: u16 = 1 << 2;
    pub const SPECTRUM_TRACE: u16 = 1 << 3;
    // 4 and 6 reserved (EVENTS and SETVAL push before v1 shipped).
    pub const CWDEC: u16 = 1 << 5;
}

/// HELLO caps bits.
pub mod caps {
    pub const SCREEN: u32 = 1 << 0;
    pub const SPECTRUM: u32 = 1 << 1;
    pub const CWDEC: u32 = 1 << 2;
    // 3, 4 and 9 reserved (EVENTS, SETVAL, BAND before v1 shipped).
    pub const CAT: u32 = 1 << 5;
    pub const TX: u32 = 1 << 6;
    pub const MONITOR: u32 = 1 << 7;
    pub const INPUT: u32 = 1 << 8;
}

/// REFRESH bits.
pub mod refresh {
    pub const SCREEN: u8 = 1 << 0;
    // 1 and 2 reserved.
    pub const CWDEC: u8 = 1 << 3;
    pub const STATUS: u8 = 1 << 4;
}

/// INPUT codes: panel parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Input {
    Turn = 1,
    TurnHeld = 2,
    Click = 3,
    Hold = 4,
    Up = 5,
    Dn = 6,
    Sel = 7,
    UpLong = 8,
    DnLong = 9,
    SelLong = 10,
    Func = 11,
    FuncLong = 12,
    FuncTurn = 13,
    /// Open a page (value = page number) as its menu entry would.
    OpenPage = 14,
}

impl Input {
    pub fn from_name(s: &str) -> Option<Self> {
        use Input::*;
        Some(match s {
            "click" => Click,
            "hold" => Hold,
            "up" => Up,
            "dn" | "down" => Dn,
            "sel" | "select" => Sel,
            "up-long" => UpLong,
            "dn-long" | "down-long" => DnLong,
            "sel-long" | "select-long" => SelLong,
            "func" => Func,
            "func-long" => FuncLong,
            _ => return None,
        })
    }

    /// The long version of a button press.
    pub fn long(self) -> Self {
        use Input::*;
        match self {
            Up => UpLong,
            Dn => DnLong,
            Sel => SelLong,
            Func => FuncLong,
            Click => Hold,
            other => other,
        }
    }
}
