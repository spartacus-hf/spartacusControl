//! The safety arbiter: whether an action may go to the radio now, and why
//! not. A pure function of what S-CTRL knows (radio STATUS, other programs,
//! the user's confirmations), so every rule has a test. The GUI and the CLI
//! both ask it before anything that changes the radio; the radio's own
//! guards (firmware ACK codes, its TX guard) still hold behind it.
//!
//! Rules (docs/plan, C3):
//! - R2 nothing that changes frequency, mode, band, audio-path settings,
//!   storage or the USB personality while the radio transmits;
//! - R3 a CAT program (WSJT-X ...) active on the other port locks
//!   frequency, mode and band unless the user took control, and never while
//!   it is transmitting; S-CTRL never keys TX next to it;
//! - R4 nothing keys TX while a mode or band change is in flight;
//! - R5 S-CTRL's TX needs ARM, a live LINK, no fault, no other TX owner;
//! - R6 gqrx (I/Q capture) running: never change I/Q, no PC speaker.

use serde::Serialize;

use sctrl_proto::payload::Status;

/// What an action is, as far as the rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Read-only: status, screenshots, diagnostics that change nothing.
    Observe,
    /// Panel input (knob, buttons): may lead anywhere, so treated like a
    /// setting change while transmitting.
    PanelInput,
    Tune,
    Mode,
    Band,
    /// A setting that does not restart the audio path.
    Setting,
    /// TX AUDIO INPUT, USB RAW IQ, microphone source: restart audio paths.
    AudioPathSetting,
    /// USB RAW IQ specifically (gqrx needs it on, WSJT-X off).
    IqSetting,
    SaveSettings,
    SdWrite,
    /// DIAG / DISK personality switches, firmware flashing.
    Personality,
    /// LINK-originated transmission: TUNE, paddle, memory.
    Transmit,
    /// Stopping TX is always allowed.
    StopTransmit,
    /// PC speaker routing.
    PcSpeaker,
}

/// What S-CTRL knows when it decides.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Context {
    pub status: Option<Status>,
    /// The LINK is up and its heartbeat flows.
    pub link_live: bool,
    /// A CAT program holds the other port or sent CAT recently.
    pub cat_program: Option<String>,
    /// That program says it is transmitting or has TX enabled (WSJT-X UDP).
    pub cat_program_tx: bool,
    /// The user confirmed "take control" from the CAT program.
    pub took_control: bool,
    /// gqrx (or another I/Q consumer) is running.
    pub iq_consumer: Option<String>,
    /// S-CTRL's own TX ARM.
    pub armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Denied {
    pub rule: &'static str,
    pub reason: String,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.reason, self.rule)
    }
}

fn deny(rule: &'static str, reason: impl Into<String>) -> Result<(), Denied> {
    Err(Denied {
        rule,
        reason: reason.into(),
    })
}

pub fn decide(action: Action, cx: &Context) -> Result<(), Denied> {
    use Action::*;
    if matches!(action, Observe | StopTransmit) {
        return Ok(());
    }
    let tx = cx.status.as_ref().is_some_and(Status::transmitting);
    let changes_radio = !matches!(action, PcSpeaker);

    // R2: the radio is transmitting.
    if tx && changes_radio {
        return deny("R2", "the radio is transmitting; wait for receive");
    }
    // R3: a CAT program shares the radio.
    if let Some(prog) = &cx.cat_program {
        if matches!(action, Tune | Mode | Band | IqSetting | AudioPathSetting | Personality) {
            if cx.cat_program_tx {
                return deny("R3", format!("{prog} is transmitting or has TX enabled"));
            }
            if !cx.took_control {
                return deny("R3", format!("{prog} controls the radio; take control first"));
            }
        }
        if action == Transmit {
            return deny(
                "R3",
                format!("{prog} shares the radio: S-CTRL does not transmit next to it"),
            );
        }
    }
    // R6: an I/Q consumer.
    if let Some(prog) = &cx.iq_consumer
        && action == IqSetting
    {
        return deny("R6", format!("{prog} needs the I/Q capture"));
    }
    if action == Transmit {
        let Some(s) = &cx.status else {
            return deny("R5", "no STATUS from the radio");
        };
        if !cx.link_live {
            return deny("R5", "the LINK is not up");
        }
        if s.change_pending() {
            return deny("R4", "a mode or band change is in flight");
        }
        if s.faults != 0 || s.lockout() || s.state == 4 {
            return deny("R5", format!("a fault is latched: {:?}", s.fault_names()));
        }
        if s.cat_ptt() {
            return deny("R5", "CAT PTT is on: another program owns TX");
        }
        if s.diag_personality() {
            return deny("R5", "DIAG personality: TX is muted there");
        }
        if !cx.armed {
            return deny("R5", "TX is not armed");
        }
    }
    if action == PcSpeaker
        && let Some(s) = &cx.status
    {
        if s.usb_iq() {
            return deny("R6", "the radio sends raw I/Q on USB: there is no audio to play");
        }
        if s.diag_personality() {
            return deny("R6", "DIAG personality: no sound card");
        }
    }
    // Storage writes and saves need the radio idle (the firmware also says so).
    if matches!(action, SaveSettings | SdWrite)
        && let Some(s) = &cx.status
        && s.background_blocked()
    {
        return deny("R2", "the radio's storage is busy");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: u8) -> Status {
        Status {
            state,
            ..Default::default()
        }
    }

    fn cx(state: u8) -> Context {
        Context {
            status: Some(status(state)),
            link_live: true,
            ..Default::default()
        }
    }

    #[test]
    fn receive_allows_changes() {
        for a in [
            Action::Tune,
            Action::Mode,
            Action::Setting,
            Action::SaveSettings,
            Action::PanelInput,
        ] {
            assert_eq!(decide(a, &cx(1)), Ok(()), "{a:?}");
        }
    }

    #[test]
    fn transmitting_blocks_every_change_but_stop() {
        for st in [2, 3] {
            for a in [
                Action::Tune,
                Action::Mode,
                Action::Band,
                Action::AudioPathSetting,
                Action::SdWrite,
                Action::Personality,
            ] {
                assert_eq!(decide(a, &cx(st)).unwrap_err().rule, "R2", "{a:?}");
            }
            assert_eq!(decide(Action::StopTransmit, &cx(st)), Ok(()));
            assert_eq!(decide(Action::Observe, &cx(st)), Ok(()));
        }
    }

    #[test]
    fn wsjtx_locks_until_control_taken_and_never_during_its_tx() {
        let mut c = cx(1);
        c.cat_program = Some("WSJT-X".into());
        assert_eq!(decide(Action::Mode, &c).unwrap_err().rule, "R3");
        assert_eq!(decide(Action::Tune, &c).unwrap_err().rule, "R3");
        assert_eq!(decide(Action::Setting, &c), Ok(()));
        c.took_control = true;
        assert_eq!(decide(Action::Mode, &c), Ok(()));
        c.cat_program_tx = true;
        assert_eq!(decide(Action::Mode, &c).unwrap_err().rule, "R3");
        c.armed = true;
        assert_eq!(decide(Action::Transmit, &c).unwrap_err().rule, "R3");
    }

    #[test]
    fn transmit_needs_everything_in_order() {
        let mut c = cx(1);
        assert_eq!(decide(Action::Transmit, &c).unwrap_err().reason, "TX is not armed");
        c.armed = true;
        assert_eq!(decide(Action::Transmit, &c), Ok(()));
        c.link_live = false;
        assert_eq!(decide(Action::Transmit, &c).unwrap_err().rule, "R5");
        c.link_live = true;
        c.status.as_mut().unwrap().flags1 = 0x08; // change pending
        assert_eq!(decide(Action::Transmit, &c).unwrap_err().rule, "R4");
        c.status.as_mut().unwrap().flags1 = 0x02; // CAT PTT
        assert_eq!(decide(Action::Transmit, &c).unwrap_err().rule, "R5");
        c.status.as_mut().unwrap().flags1 = 0;
        c.status.as_mut().unwrap().faults = 0x10;
        assert!(decide(Action::Transmit, &c).unwrap_err().reason.contains("SWR"));
    }

    #[test]
    fn iq_consumer_and_pc_speaker() {
        let mut c = cx(1);
        c.iq_consumer = Some("gqrx".into());
        assert_eq!(decide(Action::IqSetting, &c).unwrap_err().rule, "R6");
        c.status.as_mut().unwrap().flags0 = 0x02; // usb_iq
        assert_eq!(decide(Action::PcSpeaker, &c).unwrap_err().rule, "R6");
        // The PC speaker is not a radio change: allowed while transmitting.
        let t = cx(3);
        assert_eq!(decide(Action::PcSpeaker, &t), Ok(()));
    }
}
