//! S-CTRL's own transmissions -- TUNE, the virtual paddle, CW memories --
//! and the safeguards around them.
//!
//! The radio's guard holds whatever happens here (LINK spec section 6: ARM,
//! heartbeat, lever refresh, key-down and memory caps, one TX owner, TUNE
//! only on TX TEST). On top of it S-CTRL adds:
//! - its own, stricter limits (`TxPolicy`, edited in S-CTRL's settings,
//!   conservative by default; bench mode 1 s; never more than 5 s of
//!   continuous key-down);
//! - a pre-TX checklist whose every item must pass (`checklist`), the
//!   safety arbiter's rules among them (no TX next to WSJT-X, none during a
//!   mode change, none in DIAG ...);
//! - the session's answer to "antenna or 50 ohm load?";
//! - a duty-cycle budget;
//! - a live watch during every key-down that stops at once on SWR, VIN, a
//!   fault, a stale LINK or another TX owner;
//! - an audit log of every TX action (`$XDG_STATE_HOME/sctrl/tx.log`);
//! - a sleep inhibitor while armed.
//!
//! ARM is the radio's (STATUS says whether it holds); S-CTRL never arms at
//! start and never queues a refused action for later.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use sctrl_proto::frame::{AckCode, caps};
use sctrl_proto::payload::{self, Status};

use crate::link::{Link, LinkEvent};
use crate::safety::{self, Action, Context};

/// Continuous key-down, whatever the settings say.
pub const ABS_KEY_CEIL_MS: u32 = 5000;
/// Bench mode: every key-down at most this long.
pub const BENCH_KEY_CEIL_MS: u32 = 1000;
/// The radio's page and mode numbers (link spec, STATUS).
pub const PAGE_TX_TEST: u8 = 6;
pub const MODE_CW: u8 = 2;

// ---- the band plan ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Region {
    /// IARU Region 1: Europe, Africa, the Middle East.
    #[default]
    R1,
    /// IARU Region 2: the Americas.
    R2,
    /// IARU Region 3: Asia-Pacific.
    R3,
}

/// HF amateur allocations the radio covers, by region (Hz).
pub fn band_plan(region: Region) -> &'static [(&'static str, u32, u32)] {
    const R1: &[(&str, u32, u32)] = &[
        ("160m", 1_810_000, 2_000_000),
        ("80m", 3_500_000, 3_800_000),
        ("60m", 5_351_500, 5_366_500),
        ("40m", 7_000_000, 7_200_000),
        ("30m", 10_100_000, 10_150_000),
        ("20m", 14_000_000, 14_350_000),
        ("17m", 18_068_000, 18_168_000),
        ("15m", 21_000_000, 21_450_000),
        ("12m", 24_890_000, 24_990_000),
        ("10m", 28_000_000, 29_700_000),
    ];
    const R2: &[(&str, u32, u32)] = &[
        ("160m", 1_800_000, 2_000_000),
        ("80m", 3_500_000, 4_000_000),
        ("60m", 5_330_500, 5_406_400),
        ("40m", 7_000_000, 7_300_000),
        ("30m", 10_100_000, 10_150_000),
        ("20m", 14_000_000, 14_350_000),
        ("17m", 18_068_000, 18_168_000),
        ("15m", 21_000_000, 21_450_000),
        ("12m", 24_890_000, 24_990_000),
        ("10m", 28_000_000, 29_700_000),
    ];
    const R3: &[(&str, u32, u32)] = &[
        ("160m", 1_800_000, 2_000_000),
        ("80m", 3_500_000, 3_900_000),
        ("60m", 5_351_500, 5_366_500),
        ("40m", 7_000_000, 7_300_000),
        ("30m", 10_100_000, 10_150_000),
        ("20m", 14_000_000, 14_350_000),
        ("17m", 18_068_000, 18_168_000),
        ("15m", 21_000_000, 21_450_000),
        ("12m", 24_890_000, 24_990_000),
        ("10m", 28_000_000, 29_700_000),
    ];
    match region {
        Region::R1 => R1,
        Region::R2 => R2,
        Region::R3 => R3,
    }
}

/// The band `hz` lies in, for that region.
pub fn band_of(region: Region, hz: u32) -> Option<&'static str> {
    band_plan(region)
        .iter()
        .find(|(_, lo, hi)| hz >= *lo && hz <= *hi)
        .map(|(n, _, _)| *n)
}

// ---- the policy ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TxPolicy {
    /// TUNE carrier, longest.
    pub tune_max_ms: u32,
    /// One continuous paddle key-down, longest (longer is a stuck key).
    pub key_max_ms: u32,
    /// A CW memory, longest (also checked against its length at the WPM).
    pub memory_max_s: u32,
    /// Key-down time allowed per window, percent.
    pub duty_pct: u32,
    pub duty_window_s: u32,
    /// ARM ends this long after the last TX activity.
    pub arm_ttl_s: u32,
    /// Bench: every key-down at most 1 s.
    pub bench: bool,
    pub region: Region,
    /// Lowest supply voltage to key at, and to stay keyed at.
    pub min_vin_mv: u16,
    /// Highest SWR to key at (last reading) and to stay keyed at.
    pub max_swr: f32,
}

impl Default for TxPolicy {
    fn default() -> Self {
        Self {
            tune_max_ms: 5000,
            key_max_ms: 3000,
            memory_max_s: 60,
            duty_pct: 50,
            duty_window_s: 300,
            arm_ttl_s: 60,
            bench: false,
            region: Region::R1,
            min_vin_mv: 7000,
            max_swr: 3.0,
        }
    }
}

impl TxPolicy {
    /// Whatever was loaded, back inside the hard limits.
    pub fn sanitize(&mut self) {
        self.tune_max_ms = self.tune_max_ms.clamp(100, ABS_KEY_CEIL_MS);
        self.key_max_ms = self.key_max_ms.clamp(100, ABS_KEY_CEIL_MS);
        self.memory_max_s = self.memory_max_s.clamp(1, 255);
        self.duty_pct = self.duty_pct.clamp(1, 100);
        self.duty_window_s = self.duty_window_s.clamp(10, 3600);
        self.arm_ttl_s = self.arm_ttl_s.clamp(5, 255);
        if !self.max_swr.is_finite() || self.max_swr < 1.0 {
            self.max_swr = 3.0;
        }
    }

    /// The longest continuous key-down for `kind` (a memory's elements are
    /// short; its whole length is `memory_max_s`).
    pub fn key_ceiling_ms(&self, kind: TxKind) -> u32 {
        let want = match kind {
            TxKind::Tune => self.tune_max_ms,
            TxKind::Paddle | TxKind::Memory => self.key_max_ms,
        };
        let ceil = if self.bench { BENCH_KEY_CEIL_MS } else { ABS_KEY_CEIL_MS };
        want.min(ceil)
    }

    pub fn duty_budget_ms(&self) -> u64 {
        self.duty_window_s as u64 * 1000 * self.duty_pct as u64 / 100
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxKind {
    Tune,
    Paddle,
    Memory,
}

/// What the radio's RF output goes into, as the user said this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Load {
    Antenna,
    DummyLoad,
}

// ---- the checklist (pure) ----

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    pub item: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// What the checklist looks at.
#[derive(Debug, Clone)]
pub struct Facts {
    pub status: Option<Status>,
    pub status_age: Option<Duration>,
    pub link_caps: u32,
    pub cx: Context,
    pub load: Option<Load>,
    pub duty_used_ms: u64,
    pub request_ms: u64,
}

fn chk(item: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        item,
        ok,
        detail: detail.into(),
    }
}

/// Every item must pass before S-CTRL keys the radio.
pub fn checklist(kind: TxKind, policy: &TxPolicy, f: &Facts) -> Vec<Check> {
    let mut v = Vec::new();
    let fresh = f.status_age.is_some_and(|a| a < Duration::from_millis(800));
    let Some(s) = f.status.as_ref().filter(|_| fresh) else {
        v.push(chk("LINK", false, "no fresh STATUS from the radio"));
        return v;
    };
    v.push(chk(
        "LINK",
        f.cx.link_live && s.link & 1 != 0,
        if s.link & 1 != 0 {
            "up, heartbeat OK"
        } else {
            "the radio does not see S-CTRL's heartbeat"
        },
    ));
    v.push(chk(
        "firmware",
        f.link_caps & caps::TX != 0,
        if f.link_caps & caps::TX != 0 {
            "TX over the LINK supported"
        } else {
            "this firmware has no LINK TX"
        },
    ));
    // The arbiter's rules: CAT program (WSJT-X), DIAG, mode change, CAT PTT, faults.
    let mut cx = f.cx.clone();
    cx.armed = true; // ARM is checked separately below
    match safety::decide(Action::Transmit, &cx) {
        Ok(()) => v.push(chk(
            "safety rules",
            true,
            "no CAT program, no other TX owner, no mode change",
        )),
        Err(d) => v.push(chk("safety rules", false, d.to_string())),
    }
    v.push(chk(
        "radio state",
        s.state == 1,
        format!("{} (must be RECEIVE)", s.state_name()),
    ));
    v.push(chk(
        "faults",
        s.faults == 0 && !s.lockout(),
        if s.faults == 0 && !s.lockout() {
            "none".to_string()
        } else {
            format!(
                "{:?}{}",
                s.fault_names(),
                if s.lockout() { " + pipeline lockout" } else { "" }
            )
        },
    ));
    v.push(chk(
        "supply",
        s.vin_mv >= policy.min_vin_mv,
        format!(
            "VIN {:.2} V (at least {:.2} V)",
            s.vin_mv as f32 / 1000.0,
            policy.min_vin_mv as f32 / 1000.0
        ),
    ));
    match band_of(policy.region, s.dial_hz) {
        Some(b) => v.push(chk("band plan", true, format!("{b} ({:?})", policy.region))),
        None => v.push(chk(
            "band plan",
            false,
            format!(
                "{:.4} MHz is outside the {:?} amateur bands",
                s.dial_hz as f64 / 1e6,
                policy.region
            ),
        )),
    }
    let (mode_ok, mode_why) = match kind {
        TxKind::Tune => (
            s.page == PAGE_TX_TEST,
            if s.page == PAGE_TX_TEST {
                "TX TEST page: the carrier at TUNE %".to_string()
            } else {
                format!(
                    "TUNE needs the radio's TX TEST page (now {}): `sctrl goto tx-test`",
                    s.page_name()
                )
            },
        ),
        TxKind::Paddle => (
            s.mode == MODE_CW || s.page == PAGE_TX_TEST,
            format!("{} / page {}", s.mode_name(), s.page_name()),
        ),
        TxKind::Memory => (
            s.mode == MODE_CW && s.page != PAGE_TX_TEST,
            format!("{} (memories send in CW)", s.mode_name()),
        ),
    };
    v.push(chk("mode", mode_ok, mode_why));
    v.push(chk(
        "TX owner",
        s.tx_owner == 0 && !s.cat_ptt(),
        if s.tx_owner == 0 && !s.cat_ptt() {
            "nobody transmits".to_string()
        } else {
            format!("{} transmits", payload::tx_owner_name(s.tx_owner))
        },
    ));
    match s.swr() {
        Some(swr) => v.push(chk(
            "SWR",
            swr <= policy.max_swr,
            format!("last reading {swr:.2} (at most {:.1})", policy.max_swr),
        )),
        None => v.push(chk("SWR", true, "no recent reading")),
    }
    if kind == TxKind::Tune {
        v.push(chk(
            "TX MUTE",
            !s.tx_mute(),
            if s.tx_mute() {
                "TX MUTE is on: TUNE would make no RF"
            } else {
                "off"
            },
        ));
    } else {
        v.push(chk(
            "TX MUTE",
            true,
            if s.tx_mute() {
                "on: practice, sidetone only"
            } else {
                "off: this transmits"
            },
        ));
    }
    if kind != TxKind::Tune && s.mode == MODE_CW && s.cw_arm_pending() {
        v.push(chk(
            "CW power-up arm",
            kind == TxKind::Paddle,
            "pending: the radio keys CW only after a lever has been held 1 s and released",
        ));
    }
    v.push(match f.load {
        Some(Load::DummyLoad) => chk("antenna/load", true, "50 ohm dummy load"),
        Some(Load::Antenna) => chk("antenna/load", true, "antenna"),
        None => chk(
            "antenna/load",
            false,
            "say what the radio transmits into: --load dummy or --load antenna",
        ),
    });
    let budget = policy.duty_budget_ms();
    v.push(chk(
        "duty cycle",
        f.duty_used_ms + f.request_ms <= budget,
        format!(
            "{:.1} s used of {:.0} s per {} s",
            f.duty_used_ms as f64 / 1000.0,
            budget as f64 / 1000.0,
            policy.duty_window_s
        ),
    ));
    v.push(chk(
        "ARM",
        s.armed(),
        if s.armed() {
            format!("armed, {} s left", s.arm_left_s)
        } else {
            "not armed: `sctrl tx arm` (or ARM in the window)".to_string()
        },
    ));
    v
}

pub fn failed(checks: &[Check]) -> Vec<&Check> {
    checks.iter().filter(|c| !c.ok).collect()
}

// ---- Morse length of a memory (to bound it before sending) ----

fn morse_units(c: char) -> Option<u32> {
    // Elements: dit 1, dah 3, a unit between elements.
    let code = match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        '.' => ".-.-.-",
        ',' => "--..--",
        '?' => "..--..",
        '/' => "-..-.",
        '=' => "-...-",
        '+' => ".-.-.",
        '-' => "-....-",
        _ => return None,
    };
    let el: u32 = code.chars().map(|e| if e == '.' { 1 } else { 3 }).sum();
    Some(el + code.len() as u32 - 1)
}

/// How long `text` takes at `wpm` (PARIS timing), in ms; None for text the
/// host cannot time (the radio may still accept prosigns like `<AR>`, which
/// count as their letters run together).
pub fn memory_ms(text: &str, wpm: u8) -> Option<u64> {
    if wpm == 0 {
        return None;
    }
    let dit_ms = 1200.0 / wpm as f64;
    let mut units = 0u32;
    let mut prev_char = false;
    let mut in_prosign = false;
    for c in text.chars() {
        match c {
            '<' => in_prosign = true,
            '>' => in_prosign = false,
            ' ' => {
                units += 7;
                prev_char = false;
            }
            _ => {
                let u = morse_units(c)?;
                if prev_char {
                    units += if in_prosign { 1 } else { 3 };
                }
                units += u;
                prev_char = true;
            }
        }
    }
    Some((units as f64 * dit_ms).ceil() as u64)
}

// ---- the controller ----

#[derive(Debug, Clone, Serialize)]
pub struct TxReport {
    pub kind: TxKind,
    pub requested_ms: u64,
    /// From the request until S-CTRL let go.
    pub held_ms: u64,
    /// The radio reported TRANSMIT.
    pub transmitted: bool,
    pub max_pep_w: f32,
    pub max_swr: Option<f32>,
    pub min_vin_v: Option<f32>,
    /// Why it ended: "done", or the stop reason.
    pub ended: String,
    pub txevents: Vec<String>,
}

struct Inner {
    load: Option<Load>,
    duty: VecDeque<(Instant, u64)>,
    inhibitor: Option<Child>,
    busy: bool,
}

/// S-CTRL's TX: one per running S-CTRL, shared by the window and `sctrl tx`.
#[derive(Clone)]
pub struct TxController {
    inner: Arc<Mutex<Inner>>,
    policy: Arc<Mutex<TxPolicy>>,
    origin: &'static str,
}

/// Refused before keying: the checklist items that failed.
#[derive(Debug, thiserror::Error)]
#[error("{}", .0.iter().map(|c| format!("{}: {}", c.item, c.detail)).collect::<Vec<_>>().join("; "))]
pub struct Refused(pub Vec<Check>);

fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("sctrl/tx.log")
}

/// One line of the audit log (JSON).
pub fn audit(origin: &str, what: &str, detail: serde_json::Value) {
    use std::io::Write;
    let p = log_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let line = serde_json::json!({"t": secs, "by": origin, "what": what, "detail": detail});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{line}");
    }
}

impl TxController {
    pub fn new(policy: TxPolicy, origin: &'static str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                load: None,
                duty: VecDeque::new(),
                inhibitor: None,
                busy: false,
            })),
            policy: Arc::new(Mutex::new(policy)),
            origin,
        }
    }

    pub fn policy(&self) -> TxPolicy {
        self.policy.lock().unwrap().clone()
    }

    pub fn set_policy(&self, mut p: TxPolicy) {
        p.sanitize();
        *self.policy.lock().unwrap() = p;
    }

    pub fn load(&self) -> Option<Load> {
        self.inner.lock().unwrap().load
    }

    pub fn set_load(&self, load: Load) {
        self.inner.lock().unwrap().load = Some(load);
        audit(self.origin, "load", serde_json::json!(load));
    }

    /// Key-down time in the policy's window.
    pub fn duty_used_ms(&self) -> u64 {
        let window = Duration::from_secs(self.policy().duty_window_s as u64);
        let mut i = self.inner.lock().unwrap();
        while i.duty.front().is_some_and(|(t, _)| t.elapsed() > window) {
            i.duty.pop_front();
        }
        i.duty.iter().map(|(_, ms)| ms).sum()
    }

    pub fn facts(&self, link: &Link, cx: &Context, request_ms: u64) -> Facts {
        let snap = link.snapshot();
        Facts {
            status_age: snap.status_at.map(|t| t.elapsed()),
            status: snap.status,
            link_caps: snap.hello.map(|h| h.caps).unwrap_or(0),
            cx: cx.clone(),
            load: self.load(),
            duty_used_ms: self.duty_used_ms(),
            request_ms,
        }
    }

    /// ARM (or disarm) the radio for S-CTRL's TX with the policy's limits.
    pub async fn arm(&self, link: &Link, cx: &Context, on: bool) -> Result<()> {
        if !on {
            let r = link.request_ok(payload::tx_arm(false, 100, 1, 5)).await;
            audit(self.origin, "disarm", serde_json::json!({"ok": r.is_ok()}));
            self.inhibit(false);
            return r;
        }
        let mut c = cx.clone();
        c.armed = true;
        safety::decide(Action::Transmit, &c).map_err(|d| anyhow!("{d}"))?;
        let p = self.policy();
        let key_max = p.key_ceiling_ms(TxKind::Tune).max(p.key_ceiling_ms(TxKind::Paddle));
        let r = link
            .request_ok(payload::tx_arm(true, key_max, p.memory_max_s, p.arm_ttl_s))
            .await;
        audit(
            self.origin,
            "arm",
            serde_json::json!({"ok": r.is_ok(), "key_max_ms": key_max, "memory_max_s": p.memory_max_s,
                               "ttl_s": p.arm_ttl_s, "bench": p.bench, "err": r.as_ref().err().map(|e| format!("{e:#}"))}),
        );
        r?;
        self.inhibit(true);
        Ok(())
    }

    /// Keep the PC awake while armed (systemd-inhibit; gone with S-CTRL).
    pub fn inhibit(&self, on: bool) {
        let mut i = self.inner.lock().unwrap();
        if on && i.inhibitor.is_none() {
            let mut c = Command::new("systemd-inhibit");
            c.args([
                "--what=sleep:idle",
                "--who=S-CTRL",
                "--why=the radio's transmitter is armed",
                "--mode=block",
                "sleep",
                "infinity",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
            // SAFETY: prctl is async-signal-safe.
            unsafe {
                c.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            i.inhibitor = c.spawn().ok();
        } else if !on {
            i.inhibitor = None; // kill_on_drop
        }
    }

    /// STOP: everything S-CTRL keyed ends, and it disarms. Always allowed.
    pub async fn stop(&self, link: &Link) -> Result<()> {
        let r = link.request_ok(payload::tx_stop()).await;
        audit(self.origin, "stop", serde_json::json!({"ok": r.is_ok()}));
        self.inhibit(false);
        r
    }

    fn take_busy(&self) -> Result<BusyGuard> {
        let mut i = self.inner.lock().unwrap();
        if i.busy {
            bail!("S-CTRL is already keying the radio");
        }
        i.busy = true;
        Ok(BusyGuard(self.inner.clone()))
    }

    fn checked(&self, kind: TxKind, link: &Link, cx: &Context, ms: u64) -> Result<()> {
        let checks = checklist(kind, &self.policy(), &self.facts(link, cx, ms));
        let bad: Vec<Check> = failed(&checks).into_iter().cloned().collect();
        if !bad.is_empty() {
            audit(
                self.origin,
                "refused",
                serde_json::json!({"kind": kind, "ms": ms, "failed": bad}),
            );
            return Err(Refused(bad).into());
        }
        Ok(())
    }

    /// A TUNE carrier on the TX TEST page for `ms` (at most the policy's).
    pub async fn tune(&self, link: &Link, cx: &Context, ms: u32) -> Result<TxReport> {
        let _busy = self.take_busy()?;
        let p = self.policy();
        let ms = ms.min(p.key_ceiling_ms(TxKind::Tune));
        self.checked(TxKind::Tune, link, cx, ms as u64)?;
        // The radio's own cap just past the host's: it ends a key-down a hung
        // S-CTRL would not.
        link.request_ok(payload::tx_arm(true, ms + 100, p.memory_max_s, p.arm_ttl_s))
            .await?;
        audit(self.origin, "tune", serde_json::json!({"ms": ms}));
        let ack = link.request(payload::tune(true), Duration::from_secs(2)).await;
        if let Err(e) = &ack {
            let _ = self.stop(link).await;
            bail!("TUNE: {e:#}");
        }
        if let Ok(a) = &ack
            && a.code() != Some(AckCode::Ok)
        {
            bail!(
                "TUNE refused by the radio: {}",
                a.code().map(|c| c.explain()).unwrap_or("?")
            );
        }
        let rep = self
            .hold(link, TxKind::Tune, ms, || async { Ok(()) }, || payload::tune(false))
            .await;
        self.finish(rep)
    }

    /// Hold the paddle (`dit`, `dah`) for `ms`: on TX TEST the TUNE carrier,
    /// in CW the keyer's elements.
    pub async fn key(&self, link: &Link, cx: &Context, dit: bool, dah: bool, ms: u32) -> Result<TxReport> {
        let _busy = self.take_busy()?;
        let p = self.policy();
        let ms = ms.min(p.key_ceiling_ms(TxKind::Paddle));
        self.checked(TxKind::Paddle, link, cx, ms as u64)?;
        link.request_ok(payload::tx_arm(true, ms + 100, p.memory_max_s, p.arm_ttl_s))
            .await?;
        audit(
            self.origin,
            "key",
            serde_json::json!({"ms": ms, "dit": dit, "dah": dah}),
        );
        let rep = self
            .hold(
                link,
                TxKind::Paddle,
                ms,
                || async move {
                    // PADDLE is ACKed only when refused: refresh every 100 ms.
                    link.send(payload::paddle(dit, dah)).await.map(|_| ())
                },
                || payload::paddle(false, false),
            )
            .await;
        self.finish(rep)
    }

    /// Send a CW memory through the radio's keyer.
    pub async fn memory(&self, link: &Link, cx: &Context, text: &str) -> Result<TxReport> {
        let _busy = self.take_busy()?;
        let p = self.policy();
        let wpm = link.snapshot().status.map(|s| s.wpm).unwrap_or(0);
        let est = memory_ms(text, wpm).ok_or_else(|| anyhow!("cannot time {text:?} at {wpm} WPM"))?;
        if est > p.memory_max_s as u64 * 1000 {
            bail!(
                "{text:?} takes {:.1} s at {wpm} WPM, over the {} s memory limit",
                est as f64 / 1000.0,
                p.memory_max_s
            );
        }
        let frame = payload::memsend(text).ok_or_else(|| anyhow!("a memory is at most 48 ASCII characters"))?;
        // Each element is short; the whole is bounded by its length.
        self.checked(TxKind::Memory, link, cx, est / 2)?;
        let mem_s = (est / 1000 + 2).min(p.memory_max_s as u64) as u32;
        link.request_ok(payload::tx_arm(
            true,
            p.key_ceiling_ms(TxKind::Memory),
            mem_s,
            p.arm_ttl_s,
        ))
        .await?;
        audit(
            self.origin,
            "memory",
            serde_json::json!({"text": text, "wpm": wpm, "est_ms": est}),
        );
        link.request_ok(frame).await?;
        let rep = self
            .hold(
                link,
                TxKind::Memory,
                (est + 1500) as u32,
                || async { Ok(()) },
                payload::tx_stop,
            )
            .await;
        self.finish(rep)
    }

    fn finish(&self, rep: Result<TxReport>) -> Result<TxReport> {
        match &rep {
            Ok(r) => {
                let keyed = if r.kind == TxKind::Memory {
                    r.held_ms / 2
                } else {
                    r.held_ms
                };
                if r.transmitted {
                    self.inner.lock().unwrap().duty.push_back((Instant::now(), keyed));
                }
                audit(self.origin, "done", serde_json::json!(r));
            }
            Err(e) => audit(self.origin, "failed", serde_json::json!({"error": format!("{e:#}")})),
        }
        rep
    }

    /// Hold a key-down for up to `ms`, refreshing with `tick` every 100 ms,
    /// watching STATUS every 20 ms, and end it with `release` -- or TX_STOP
    /// at the first thing out of bounds or any error.
    async fn hold<T, TF, R>(&self, link: &Link, kind: TxKind, ms: u32, tick: T, release: R) -> Result<TxReport>
    where
        T: Fn() -> TF,
        TF: std::future::Future<Output = Result<()>>,
        R: Fn() -> payload::HostFrame,
    {
        let p = self.policy();
        let start = Instant::now();
        let mut rx = link.subscribe();
        let mut rep = TxReport {
            kind,
            requested_ms: ms as u64,
            held_ms: 0,
            transmitted: false,
            max_pep_w: 0.0,
            max_swr: None,
            min_vin_v: None,
            ended: "done".into(),
            txevents: Vec::new(),
        };
        let mut last_tick = Instant::now() - Duration::from_secs(1);
        let mut memory_started = false;
        let stop_reason: Option<String> = loop {
            if start.elapsed() >= Duration::from_millis(ms as u64) {
                break if kind == TxKind::Memory {
                    Some("the memory ran past its expected length".into())
                } else {
                    None
                };
            }
            if last_tick.elapsed() >= Duration::from_millis(100) {
                last_tick = Instant::now();
                if let Err(e) = tick().await {
                    break Some(format!("link: {e:#}"));
                }
            }
            while let Ok(ev) = rx.try_recv() {
                if let LinkEvent::TxEvent(e) = ev {
                    rep.txevents.push(format!("{:?}", e.kind()));
                }
            }
            if let Some(why) = link.closed() {
                break Some(format!("LINK closed: {why}"));
            }
            let snap = link.snapshot();
            let Some(s) = snap.status else {
                break Some("no STATUS".into());
            };
            if snap.status_at.is_none_or(|t| t.elapsed() > Duration::from_millis(600)) {
                break Some("STATUS stopped arriving".into());
            }
            if s.transmitting() {
                rep.transmitted = true;
                rep.max_pep_w = rep.max_pep_w.max(s.pep_w());
            }
            if let Some(swr) = s.swr() {
                rep.max_swr = Some(rep.max_swr.map_or(swr, |m: f32| m.max(swr)));
                if s.transmitting() && swr > p.max_swr {
                    break Some(format!("SWR {swr:.2} over {:.1}", p.max_swr));
                }
            }
            let vin = s.vin_mv as f32 / 1000.0;
            rep.min_vin_v = Some(rep.min_vin_v.map_or(vin, |m: f32| m.min(vin)));
            if s.vin_mv < p.min_vin_mv {
                break Some(format!("VIN {vin:.2} V under {:.2} V", p.min_vin_mv as f32 / 1000.0));
            }
            if s.faults != 0 || s.lockout() || s.state == 4 {
                break Some(format!("radio fault {:?}", s.fault_names()));
            }
            if s.transmitting() && !matches!(s.tx_owner, 3 | 4) {
                break Some(format!("{} took the transmitter", payload::tx_owner_name(s.tx_owner)));
            }
            if kind == TxKind::Memory {
                if s.memory_busy() {
                    memory_started = true;
                } else if memory_started && start.elapsed() > Duration::from_millis(300) {
                    break None; // sent
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // Let go, the normal way first; TX_STOP if anything was wrong.
        let released = link.send(release()).await;
        if stop_reason.is_some() || released.is_err() {
            let _ = link.request_ok(payload::tx_stop()).await;
            self.inhibit(false);
        }
        rep.held_ms = start.elapsed().as_millis() as u64;
        if let Some(r) = stop_reason {
            rep.ended = format!("stopped: {r}");
        }
        Ok(rep)
    }
}

struct BusyGuard(Arc<Mutex<Inner>>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().busy = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_status() -> Status {
        Status {
            state: 1,
            mode: MODE_CW,
            page: 0,
            vin_mv: 8800,
            dial_hz: 7_030_000,
            link: 1,
            flags1: 0x01, // armed
            arm_left_s: 40,
            ..Default::default()
        }
    }

    fn facts(s: Status) -> Facts {
        Facts {
            status: Some(s.clone()),
            status_age: Some(Duration::from_millis(50)),
            link_caps: caps::TX,
            cx: Context {
                status: Some(s),
                link_live: true,
                ..Default::default()
            },
            load: Some(Load::DummyLoad),
            duty_used_ms: 0,
            request_ms: 500,
        }
    }

    /// One way to spoil otherwise clean facts.
    type Spoil = Box<dyn Fn(&mut Facts)>;

    fn bad(kind: TxKind, f: &Facts) -> Vec<&'static str> {
        checklist(kind, &TxPolicy::default(), f)
            .into_iter()
            .filter(|c| !c.ok)
            .map(|c| c.item)
            .collect()
    }

    #[test]
    fn a_clean_paddle_passes_and_tune_needs_tx_test() {
        let f = facts(ok_status());
        assert!(bad(TxKind::Paddle, &f).is_empty(), "{:?}", bad(TxKind::Paddle, &f));
        assert_eq!(bad(TxKind::Tune, &f), vec!["mode"]);
        let mut s = ok_status();
        s.page = PAGE_TX_TEST;
        assert!(bad(TxKind::Tune, &facts(s)).is_empty());
    }

    #[test]
    fn every_item_can_refuse() {
        let base = ok_status();
        let cases: Vec<(&str, Spoil)> = vec![
            ("LINK", Box::new(|f| f.status_age = Some(Duration::from_secs(2)))),
            ("firmware", Box::new(|f| f.link_caps = 0)),
            ("safety rules", Box::new(|f| f.cx.cat_program = Some("WSJT-X".into()))),
            ("radio state", Box::new(|f| f.status.as_mut().unwrap().state = 3)),
            ("faults", Box::new(|f| f.status.as_mut().unwrap().faults = 0x10)),
            ("supply", Box::new(|f| f.status.as_mut().unwrap().vin_mv = 6000)),
            (
                "band plan",
                Box::new(|f| f.status.as_mut().unwrap().dial_hz = 6_050_000),
            ),
            ("mode", Box::new(|f| f.status.as_mut().unwrap().mode = 0)),
            ("TX owner", Box::new(|f| f.status.as_mut().unwrap().tx_owner = 1)),
            ("SWR", Box::new(|f| f.status.as_mut().unwrap().swr_x100 = 450)),
            ("antenna/load", Box::new(|f| f.load = None)),
            ("duty cycle", Box::new(|f| f.duty_used_ms = 149_800)),
            ("ARM", Box::new(|f| f.status.as_mut().unwrap().flags1 = 0)),
        ];
        for (item, f) in cases {
            let mut x = facts(base.clone());
            f(&mut x);
            x.cx.status = x.status.clone();
            let b = bad(TxKind::Paddle, &x);
            assert!(b.contains(&item), "{item}: failed {b:?}");
        }
    }

    #[test]
    fn tune_with_tx_mute_is_refused_and_practice_is_fine() {
        let mut s = ok_status();
        s.page = PAGE_TX_TEST;
        s.flags0 = 0x01;
        assert_eq!(bad(TxKind::Tune, &facts(s.clone())), vec!["TX MUTE"]);
        s.page = 0;
        assert!(bad(TxKind::Paddle, &facts(s)).is_empty());
    }

    #[test]
    fn limits_never_pass_the_ceilings() {
        let mut p = TxPolicy {
            tune_max_ms: 60_000,
            ..Default::default()
        };
        p.sanitize();
        assert_eq!(p.key_ceiling_ms(TxKind::Tune), ABS_KEY_CEIL_MS);
        p.bench = true;
        assert_eq!(p.key_ceiling_ms(TxKind::Tune), BENCH_KEY_CEIL_MS);
        assert_eq!(p.key_ceiling_ms(TxKind::Paddle), BENCH_KEY_CEIL_MS);
        assert_eq!(TxPolicy::default().duty_budget_ms(), 150_000);
    }

    #[test]
    fn band_plan_by_region() {
        assert_eq!(band_of(Region::R1, 7_150_000), Some("40m"));
        assert_eq!(band_of(Region::R1, 7_250_000), None);
        assert_eq!(band_of(Region::R2, 7_250_000), Some("40m"));
        assert_eq!(band_of(Region::R1, 3_900_000), None);
        assert_eq!(band_of(Region::R2, 3_900_000), Some("80m"));
        assert_eq!(band_of(Region::R1, 6_050_000), None);
    }

    #[test]
    fn memory_length() {
        // PARIS is 50 units: 1 s at 60 WPM's 20 ms dits... at 20 WPM 60 ms each.
        assert_eq!(memory_ms("PARIS", 20), Some(43 * 60));
        assert_eq!(memory_ms("E E", 20), Some(9 * 60));
        assert!(memory_ms("CQ DE TEST", 0).is_none());
        assert!(memory_ms("@", 20).is_none());
        // A prosign runs its letters together.
        assert_eq!(memory_ms("<AR>", 20), Some((5 + 1 + 7) * 60));
    }
}
