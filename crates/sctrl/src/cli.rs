//! The commands. They run in two places: directly from a shell (opening the
//! radio's port themselves), or inside the S-CTRL that already holds the
//! port (GUI or `sctrl daemon`), forwarded over its socket. `Env` hides the
//! difference: where output goes, and how the radio is reached.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use sctrl_core::device::{self, UsbDevice};
use sctrl_core::hostview::HostView;
use sctrl_core::link::{Link, LinkEvent, LinkOptions};
use sctrl_core::safety::{self, Action, Context, Denied};
use sctrl_core::service::Service;
use sctrl_core::session::{OpenError, Session};
use sctrl_proto::frame::Input;
use sctrl_proto::payload::{Status, tx_owner_name};
use sctrl_proto::screen;

#[derive(Parser)]
#[command(
    name = "sctrl",
    version,
    about = "S-CTRL: control the Chireix SPARTACUS REV1 from this PC"
)]
pub struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
    /// The radio's USB serial, when several are attached.
    #[arg(long, global = true)]
    pub serial: Option<String>,
    /// Seconds to wait for the radio.
    #[arg(long, global = true, default_value_t = 10.0)]
    pub timeout: f64,
    /// Change frequency/mode/band although a CAT program (WSJT-X ...) shares the radio.
    #[arg(long, global = true)]
    pub take_control: bool,
    /// Open the radio directly even if an S-CTRL is running (it will refuse: the port is held).
    #[arg(long, global = true)]
    pub direct: bool,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Clone)]
pub enum Cmd {
    /// The floating window (the default with no command).
    Gui,
    /// Hold the radio and serve `sctrl` commands, without a window.
    Daemon,
    /// Radios and related devices on USB, their ports, and radio programs running.
    Ls,
    /// What the radio is doing, decoded, plus who else uses it.
    Status,
    /// The radio's screen as a PNG (live, over the LINK).
    Shot {
        #[arg(short, long, default_value = "sctrl-shot.png")]
        out: PathBuf,
        #[arg(long, default_value_t = 2)]
        scale: u32,
    },
    /// Run one console command on the radio and print its reply.
    Console { line: Vec<String> },
    /// One CAT command (TS-480/QMX set) through the LINK, e.g. 'FA;'.
    Cat { command: String },
    /// A panel button: click, hold, up, dn, sel, func.
    Press {
        name: String,
        /// The long press of that button.
        #[arg(long)]
        long: bool,
    },
    /// Turn the knob by N detents (negative: counter-clockwise).
    Knob {
        #[arg(allow_hyphen_values = true)]
        steps: i8,
        /// With the shaft pressed.
        #[arg(long)]
        held: bool,
        /// With FUNC held.
        #[arg(long)]
        func: bool,
    },
    /// Tune the dial: 14074000, 14.074M, 7074k.
    Tune { freq: String },
    /// Operating mode: usb lsb cw am fm digi digi-lsb.
    Mode { mode: String },
    /// Read a setting by id or name.
    Get { setting: String },
    /// Write a setting by id or name.
    Set { setting: String, value: String },
    /// Follow STATUS, pipeline events or the CW decoder until Ctrl-C (or --count).
    Watch {
        #[arg(default_value = "status")]
        what: String,
        #[arg(long)]
        count: Option<u64>,
    },
    /// Status, faults, pipeline, events and host checks in one go, problems first.
    Diagnose,
    /// Switch the radio's USB personality: `audio` leaves the DIAG console for the
    /// sound card + CAT/LINK ports (what S-CTRL needs).
    Usb { target: String },
    /// S-CTRL's own transmissions behind its safeguards: check, arm, tune, key, mem, stop, policy, log.
    Tx {
        #[command(subcommand)]
        what: crate::tx_cli::TxCmd,
    },
    /// Open a radio page: home, freq, diag, tx-test, trainer, sd, pd, waterfall.
    Goto { page: String },
    /// The radio's SD card, read only: `ls [folder]`, `get <file> [-o out]`.
    Sd {
        what: String,
        path: Option<String>,
        /// Where `get` writes (default: the file's name, here).
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },
    /// PipeWire: `status`; `pc on|off` (PC speaker; --digi/--no-digi, --volume, --output; kept for the next start);
    /// `phones on|off` (the radio's own headphones while the PC speaker plays);
    /// `guard on|off` (moving other programs off the radio's TX audio).
    Audio {
        what: String,
        arg: Option<String>,
        /// Play DIGI too (off by default).
        #[arg(long, conflicts_with = "no_digi")]
        digi: bool,
        /// Stop playing DIGI.
        #[arg(long)]
        no_digi: bool,
        /// 0.0 to 1.5.
        #[arg(long)]
        volume: Option<f32>,
        /// Output sink node.name (default: follow the default output).
        #[arg(long)]
        output: Option<String>,
    },
}

#[derive(Debug)]
pub enum Exit {
    NotConnected(anyhow::Error),
    Refused(Denied),
    Timeout(anyhow::Error),
    Radio(anyhow::Error),
}

impl From<anyhow::Error> for Exit {
    fn from(e: anyhow::Error) -> Self {
        let s = format!("{e:#}");
        if e.downcast_ref::<OpenError>().is_some() {
            Exit::NotConnected(e)
        } else if s.contains("within") || s.contains("no HELLO") || s.contains("no answer") {
            Exit::Timeout(e)
        } else {
            Exit::Radio(e)
        }
    }
}

impl Exit {
    pub fn code(&self) -> i32 {
        match self {
            Exit::NotConnected(_) => 3,
            Exit::Refused(_) => 4,
            Exit::Timeout(_) => 5,
            Exit::Radio(_) => 6,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Exit::NotConnected(_) => "not_connected",
            Exit::Refused(_) => "refused",
            Exit::Timeout(_) => "timeout",
            Exit::Radio(_) => "error",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Exit::NotConnected(e) | Exit::Timeout(e) | Exit::Radio(e) => format!("{e:#}"),
            Exit::Refused(d) => d.to_string(),
        }
    }
}

/// Where a command's output goes and how it reaches the radio.
pub struct Env {
    out: Box<dyn FnMut(String) + Send + Sync>,
    shared: Option<Service>,
}

impl Env {
    pub fn stdout() -> Self {
        Self {
            out: Box::new(|s| {
                use std::io::Write;
                // A closed pipe (`sctrl ... | head`) is not an error worth a panic.
                let _ = writeln!(std::io::stdout(), "{s}");
            }),
            shared: None,
        }
    }

    pub fn shared(service: Service, out: Box<dyn FnMut(String) + Send + Sync>) -> Self {
        Self {
            out,
            shared: Some(service),
        }
    }

    /// The running S-CTRL this command runs inside, if any.
    pub fn service(&self) -> Option<&Service> {
        self.shared.as_ref()
    }

    pub(crate) fn say(&mut self, s: impl Into<String>) {
        (self.out)(s.into());
    }

    pub(crate) fn device(&self, serial: Option<&str>) -> Result<UsbDevice, Exit> {
        if let Some(svc) = &self.shared
            && let Some(d) = svc.device()
        {
            return Ok(d);
        }
        device::find_spartacus(serial).ok_or_else(|| {
            let others: Vec<String> = device::scan()
                .into_iter()
                .filter(|d| !matches!(d.kind, device::Kind::Other))
                .map(|d| d.label())
                .collect();
            let extra = if others.is_empty() {
                String::new()
            } else {
                format!(" (seen: {})", others.join("; "))
            };
            Exit::NotConnected(anyhow!("no SPARTACUS on USB{extra}"))
        })
    }

    pub(crate) async fn open(&self, dev: &UsbDevice) -> Result<Sess, Exit> {
        if let Some(svc) = &self.shared {
            return match svc.link() {
                Some(l) => Ok(Sess::Shared(l)),
                None => Err(Exit::NotConnected(anyhow!("{}", svc.state().summary()))),
            };
        }
        Ok(Sess::Owned(Box::new(Session::open(dev, LinkOptions::default()).await?)))
    }
}

/// A radio session: our own, or the serving S-CTRL's LINK.
pub(crate) enum Sess {
    Owned(Box<Session>),
    Shared(Arc<Link>),
}

impl Sess {
    fn link(&self) -> Option<&Link> {
        match self {
            Sess::Owned(s) => s.link(),
            Sess::Shared(l) => Some(l),
        }
    }

    pub(crate) fn need_link(&self) -> Result<&Link, Exit> {
        match self {
            Sess::Owned(s) if let Session::Text { why, .. } = &**s => {
                Err(Exit::Radio(anyhow!("this needs the LINK ({why})")))
            }
            _ => Ok(self.link().expect("a LINK")),
        }
    }

    fn basic_why(&self) -> Option<String> {
        match self {
            Sess::Owned(s) if let Session::Text { why, .. } = &**s => Some(why.clone()),
            _ => None,
        }
    }

    async fn cmd(&mut self, line: &str, timeout: Duration) -> Result<Vec<String>> {
        match self {
            Sess::Owned(s) => s.cmd(line, timeout).await,
            Sess::Shared(l) => l.cmd(line, timeout).await,
        }
    }

    /// Our own session closes; a shared LINK stays up.
    pub(crate) async fn close(self) {
        if let Sess::Owned(s) = self {
            s.close().await;
        }
    }
}

async fn wait_status(l: &Link, timeout: Duration) -> Result<Status> {
    let end = Instant::now() + timeout;
    loop {
        if let Some(s) = l.snapshot().status {
            return Ok(s);
        }
        if Instant::now() > end {
            bail!("no STATUS within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(crate) fn context(
    dev: &UsbDevice,
    status: Option<Status>,
    link_live: bool,
    take_control: bool,
) -> (Context, HostView) {
    let host = HostView::gather(Some(dev));
    let cat0 = status.as_ref().map(|s| s.cat0_age_ds).filter(|&a| a != 0xFFFF);
    let cx = Context {
        cat_program: host.cat_program(cat0),
        iq_consumer: host.iq_consumer(),
        took_control: take_control,
        status,
        link_live,
        ..Default::default()
    };
    (cx, host)
}

pub(crate) async fn guard(cli: &Cli, dev: &UsbDevice, l: &Link, action: Action) -> Result<(), Exit> {
    let st = wait_status(l, Duration::from_secs(2)).await.ok();
    let (cx, _) = context(dev, st, l.closed().is_none(), cli.take_control);
    safety::decide(action, &cx).map_err(Exit::Refused)
}

pub fn parse_freq(s: &str) -> Result<u32> {
    let t = s.trim().to_ascii_lowercase().replace(['_', ','], "");
    let (num, mul) = if let Some(n) = t.strip_suffix("mhz").or(t.strip_suffix('m')) {
        (n, 1e6)
    } else if let Some(n) = t.strip_suffix("khz").or(t.strip_suffix('k')) {
        (n, 1e3)
    } else {
        (t.strip_suffix("hz").unwrap_or(&t), 1.0)
    };
    let v: f64 = num.trim().parse().map_err(|_| anyhow!("not a frequency: {s}"))?;
    let hz = (v * mul).round();
    if !(100_000.0..=60_000_000.0).contains(&hz) {
        bail!("{s} is outside 0.1-60 MHz");
    }
    Ok(hz as u32)
}

pub fn mode_cat(m: &str) -> Result<&'static str> {
    Ok(match m.to_ascii_lowercase().as_str() {
        "usb" => "MD2;",
        "lsb" => "MD1;",
        "cw" => "MD3;",
        "fm" => "MD4;",
        "am" => "MD5;",
        "digi" | "digi-usb" | "data" | "pktusb" => "MD9;",
        "digi-lsb" | "pktlsb" => "MD6;",
        _ => bail!("unknown mode {m} (usb lsb cw am fm digi digi-lsb)"),
    })
}

pub fn fmt_hz(hz: u32) -> String {
    format!("{}.{:03}.{:03}", hz / 1_000_000, (hz / 1000) % 1000, hz % 1000)
}

pub fn ref_name(state: u8) -> &'static str {
    ["idle", "acquiring", "locked"]
        .get(state as usize)
        .copied()
        .unwrap_or("?")
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

pub fn status_json(s: &Status) -> serde_json::Value {
    json!({
        "state": s.state_name(), "dial_hz": s.dial_hz, "mode": s.mode_name(), "page": s.page_name(),
        "transmitting": s.transmitting(), "tx_owner": tx_owner_name(s.tx_owner),
        "vin_v": s.vin_mv as f32 / 1000.0, "pd_v": s.pd_mv as f32 / 1000.0,
        "rx_dbfs": s.rx_dbfs_x10 as f32 / 10.0, "pep_w": s.pep_w(), "swr": s.swr(),
        "faults": s.fault_names(), "lockout": s.lockout(), "load_pct": s.load_pct,
        "tx_mute": s.tx_mute(), "usb_iq": s.usb_iq(), "usb_vox": s.usb_vox(), "cat_ptt": s.cat_ptt(),
        "armed": s.armed(), "arm_left_s": s.arm_left_s, "change_pending": s.change_pending(),
        "realtime_critical": s.realtime_critical(), "storage_busy": s.background_blocked(),
        "keyer_cannot_radiate": s.keyer_cannot_radiate(), "wpm": s.wpm,
        "monitor_mix": s.monitor_mix(), "phones_off": s.phones_off(),
        "cat_ports_open": s.cat_ports_open(), "console_port": s.console_port(),
        "cat0_age_s": (s.cat0_age_ds != 0xFFFF).then(|| s.cat0_age_ds as f32 / 10.0),
        "rx_overruns": s.rx_overruns, "capture_gaps_10min": s.cap_gaps,
        "ref": ref_name(s.ref_state), "ref_ppm": s.ref_ppm_x100 as f32 / 100.0,
        "band": s.band, "uptime_ms": s.uptime_ms, "settings_hash": format!("{:08x}", s.settings_hash),
    })
}

fn status_lines(dev: &UsbDevice, l: &Link, s: &Status, host: &HostView, cx: &Context) -> Vec<String> {
    let h = l.snapshot().hello;
    let mut v = vec![
        format!(
            "SPARTACUS REV1 {}  firmware v{}  LINK v{}",
            dev.serial,
            h.as_ref()
                .map(|h| format!("{}.{:02}", h.fw_major, h.fw_minor))
                .unwrap_or("?".into()),
            h.as_ref().map(|h| h.proto).unwrap_or(0)
        ),
        format!(
            "{:<10} {} Hz  {}{}  page {}",
            s.state_name(),
            fmt_hz(s.dial_hz),
            s.mode_name(),
            if s.mode == 5 {
                if s.lsb() { " (LSB)" } else { " (USB)" }
            } else {
                ""
            },
            s.page_name()
        ),
        format!(
            "VIN {:.2} V   PD {:.1} V   S {:.1} dBFS   ref {} {:+.2} ppm",
            s.vin_mv as f32 / 1000.0,
            s.pd_mv as f32 / 1000.0,
            s.rx_dbfs_x10 as f32 / 10.0,
            ref_name(s.ref_state),
            s.ref_ppm_x100 as f32 / 100.0
        ),
        format!(
            "TX: owner {}  CAT PTT {}  TX MUTE {}  armed {}  USB VOX {}  USB RAW IQ {}",
            tx_owner_name(s.tx_owner),
            onoff(s.cat_ptt()),
            onoff(s.tx_mute()),
            onoff(s.armed()),
            onoff(s.usb_vox()),
            onoff(s.usb_iq())
        ),
    ];
    let faults = s.fault_names();
    v.push(format!(
        "faults {}  lockout {}  overruns {}  capture gaps {}/10 min{}",
        if faults.is_empty() {
            "none".into()
        } else {
            faults.join(",")
        },
        onoff(s.lockout()),
        s.rx_overruns,
        s.cap_gaps,
        if s.realtime_critical() {
            "  (realtime-critical: console paused)"
        } else {
            ""
        }
    ));
    v.push(format!(
        "CAT port 0 (WSJT-X's): {}; last command {}",
        if s.cat_ports_open() & 1 != 0 { "open" } else { "closed" },
        if s.cat0_age_ds == 0xFFFF {
            "never".into()
        } else {
            format!("{:.1} s ago", s.cat0_age_ds as f32 / 10.0)
        }
    ));
    let apps: Vec<String> = host
        .apps
        .iter()
        .map(|r| format!("{} (pid {})", r.app.name(), r.proc.pid))
        .collect();
    v.push(format!(
        "host: {}",
        if apps.is_empty() {
            "no radio programs running".into()
        } else {
            apps.join(", ")
        }
    ));
    if let Some(p) = &cx.cat_program {
        v.push(format!(
            "CAT program: {p}: frequency/mode/band locked unless --take-control"
        ));
    }
    v
}

/// Run one command. `Gui` and `Daemon` are handled by main.
pub async fn run(cli: &Cli, env: &mut Env) -> Result<(), Exit> {
    let timeout = Duration::from_secs_f64(cli.timeout);
    let Some(cmd) = cli.cmd.clone() else { return Ok(()) };
    match cmd {
        Cmd::Gui | Cmd::Daemon => Ok(()),
        Cmd::Ls => {
            let devs = device::scan();
            let host = HostView::gather(device::find_spartacus(cli.serial.as_deref()).as_ref());
            let shown: Vec<&UsbDevice> = devs.iter().filter(|d| !matches!(d.kind, device::Kind::Other)).collect();
            if cli.json {
                env.say(json!({"ok": true, "devices": shown, "host": host}).to_string());
                return Ok(());
            }
            for d in &shown {
                env.say(d.label());
                if d.is_spartacus() {
                    for (i, t) in &d.ttys {
                        let role = match i {
                            3 => "CAT (WSJT-X's)",
                            5 => "LINK (S-CTRL's)",
                            0 => "console",
                            _ => "",
                        };
                        let p = d.port(*i).unwrap_or(t.clone());
                        let who = sctrl_core::procs::holders(t);
                        let held = if who.is_empty() {
                            "free".to_string()
                        } else {
                            format!(
                                "held by {}",
                                who.iter()
                                    .map(|w| format!("{} ({})", w.name, w.pid))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        };
                        env.say(format!("  if{i:02} {role:<16} {}  {held}", p.display()));
                    }
                    if let Some(c) = d.sound_card {
                        env.say(format!("  sound card {c}"));
                    }
                }
            }
            if shown.is_empty() {
                env.say("no SPARTACUS, QRP Labs radio or ST-LINK on USB");
            }
            for r in &host.apps {
                env.say(format!(
                    "running: {} (pid {}){}",
                    r.app.name(),
                    r.proc.pid,
                    r.rig_name
                        .as_ref()
                        .map(|n| format!(" --rig-name={n}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(svc) = &env.shared {
                let st = svc.state().summary();
                env.say(format!("S-CTRL: {st}"));
            }
            Ok(())
        }
        Cmd::Status => {
            let dev = env.device(cli.serial.as_deref())?;
            let mut s = env.open(&dev).await?;
            if let Some(why) = s.basic_why() {
                let lines = s.cmd("status", timeout).await?;
                if cli.json {
                    env.say(json!({"ok": true, "basic_mode": why, "status": lines}).to_string());
                } else {
                    env.say(format!("({why})"));
                    for l in lines {
                        env.say(l);
                    }
                }
                s.close().await;
                return Ok(());
            }
            let l = s.need_link()?;
            let st = wait_status(l, timeout).await?;
            let (cx, host) = context(&dev, Some(st.clone()), true, cli.take_control);
            if cli.json {
                let snap = l.snapshot();
                env.say(
                    json!({"ok": true, "serial": dev.serial, "hello": snap.hello, "status": status_json(&st),
                           "host": host, "cat_program": cx.cat_program, "iq_consumer": cx.iq_consumer,
                           "link": snap.stats})
                    .to_string(),
                );
            } else {
                for line in status_lines(&dev, l, &st, &host, &cx) {
                    env.say(line);
                }
            }
            s.close().await;
            Ok(())
        }
        Cmd::Shot { out, scale } => {
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            // A running S-CTRL streams the screen only while it is looked at.
            let _lease = match env.service() {
                Some(svc) => Some(svc.screen_lease().await.map_err(Exit::Radio)?),
                None => None,
            };
            let (screen, waterfall) = l.fresh_screen(timeout).await.map_err(Exit::Timeout)?;
            let rgba = screen.render_rgba(Some(&waterfall));
            let img =
                image::RgbaImage::from_raw(screen::W as u32, screen::H as u32, rgba).ok_or_else(|| anyhow!("image"))?;
            let img = image::imageops::resize(
                &img,
                screen::W as u32 * scale,
                screen::H as u32 * scale,
                image::imageops::FilterType::Nearest,
            );
            img.save(&out).map_err(|e| anyhow!("{}: {e}", out.display()))?;
            if cli.json {
                env.say(json!({"ok": true, "file": out, "band_pages": screen.band_pages}).to_string());
            } else {
                env.say(out.display().to_string());
            }
            s.close().await;
            Ok(())
        }
        Cmd::Console { line } => {
            let line = line.join(" ");
            let dev = env.device(cli.serial.as_deref())?;
            let mut s = env.open(&dev).await?;
            let r = s.cmd(&line, timeout).await;
            s.close().await;
            let lines = r?;
            if cli.json {
                env.say(json!({"ok": true, "lines": lines}).to_string());
            } else {
                for l in lines {
                    env.say(l);
                }
            }
            Ok(())
        }
        Cmd::Cat { command } => {
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            // Reads are free; anything that sets goes through the arbiter.
            let c = command.trim().to_ascii_uppercase();
            let is_read = (c.ends_with(';') && c.len() <= 4) || c.ends_with("?;");
            if !is_read {
                let action = if c.starts_with("FA") || c.starts_with("FB") {
                    Action::Tune
                } else if c.starts_with("MD") {
                    Action::Mode
                } else if c.starts_with("Q9") {
                    Action::IqSetting
                } else {
                    Action::Setting
                };
                guard(cli, &dev, l, action).await?;
            }
            let r = l.cat(&command, timeout).await;
            s.close().await;
            let reply = r?;
            if cli.json {
                env.say(json!({"ok": true, "reply": reply}).to_string());
            } else {
                env.say(reply);
            }
            Ok(())
        }
        Cmd::Press { name, long } => {
            let mut code = Input::from_name(&name.to_ascii_lowercase())
                .ok_or_else(|| anyhow!("unknown button {name}: click hold up dn sel func"))?;
            if long {
                code = code.long();
            }
            input(cli, env, code, 0).await
        }
        Cmd::Knob { steps, held, func } => {
            let code = if func {
                Input::FuncTurn
            } else if held {
                Input::TurnHeld
            } else {
                Input::Turn
            };
            input(cli, env, code, steps).await
        }
        Cmd::Tune { freq } => {
            let hz = parse_freq(&freq)?;
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            guard(cli, &dev, l, Action::Tune).await?;
            let r = l.cat(&format!("FA{hz:011};"), timeout).await;
            // Confirmed by STATUS: the dial the panel shows.
            let mut confirmed = false;
            if r.is_ok() {
                let end = Instant::now() + Duration::from_secs(2);
                while Instant::now() < end && !confirmed {
                    confirmed = l.snapshot().status.is_some_and(|st| st.dial_hz == hz);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            s.close().await;
            r?;
            if !confirmed {
                return Err(Exit::Radio(anyhow!(
                    "the radio did not take {} Hz (a page that owns the dial? out of band?)",
                    fmt_hz(hz)
                )));
            }
            if cli.json {
                env.say(json!({"ok": true, "dial_hz": hz}).to_string());
            } else {
                env.say(format!("{} Hz", fmt_hz(hz)));
            }
            Ok(())
        }
        Cmd::Mode { mode } => {
            let md = mode_cat(&mode)?;
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            guard(cli, &dev, l, Action::Mode).await?;
            let r = l.cat(md, timeout).await;
            // The firmware applies a mode once the audio path is idle (up to 5 s).
            let want = md.trim_end_matches(';').to_string();
            let mut got = String::new();
            if r.is_ok() {
                let end = Instant::now() + Duration::from_secs(6);
                while Instant::now() < end {
                    got = l.cat("MD;", Duration::from_secs(2)).await.unwrap_or_default();
                    if got.trim_end_matches(';') == want {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            s.close().await;
            r?;
            if got.trim_end_matches(';') != want {
                return Err(Exit::Radio(anyhow!("the radio did not change mode (it reads {got})")));
            }
            if cli.json {
                env.say(json!({"ok": true, "mode": mode}).to_string());
            } else {
                env.say(mode.to_ascii_uppercase());
            }
            Ok(())
        }
        Cmd::Usb { target } => match target.as_str() {
            "audio" => usb_audio(cli, env).await,
            other => Err(Exit::Radio(anyhow!(
                "usb {other}: only `audio` (DIAG and DISK MODE are entered on the radio's panel)"
            ))),
        },
        Cmd::Audio {
            what,
            arg,
            digi,
            no_digi,
            volume,
            output,
        } => {
            let digi = (digi || no_digi).then_some(digi);
            audio_cmd(cli, env, &what, arg.as_deref(), digi, volume, output).await
        }
        Cmd::Tx { what } => crate::tx_cli::run(cli, env, what).await,
        Cmd::Goto { page } => crate::tx_cli::goto(cli, env, &page).await,
        Cmd::Sd { what, path, out } => {
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            let path = path.unwrap_or_default();
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            let r = match what.as_str() {
                "ls" => sctrl_core::sdcard::ls(l, &path).await.map(|entries| {
                    if cli.json {
                        env.say(json!({"ok": true, "path": path, "entries": entries}).to_string());
                    } else {
                        for e in entries {
                            env.say(if e.dir {
                                format!("{:>10}  {}/", "", e.name)
                            } else {
                                format!("{:>10}  {}", e.size, e.name)
                            });
                        }
                    }
                }),
                "get" if !path.is_empty() && out.is_none() && std::path::Path::new(&name).exists() => {
                    // Only an explicit -o replaces a file.
                    Err(anyhow!(
                        "{name} exists here: -o <file> to replace it or to save elsewhere"
                    ))
                }
                "get" if !path.is_empty() => {
                    let quiet = cli.json;
                    let data = sctrl_core::sdcard::get(l, &path, |done, total| {
                        if !quiet {
                            eprint!("\r{done}/{total} bytes");
                        }
                    })
                    .await;
                    if !quiet {
                        eprintln!();
                    }
                    data.and_then(|d| {
                        let to = out.unwrap_or_else(|| name.into());
                        std::fs::write(&to, &d).map_err(|e| anyhow!("{}: {e}", to.display()))?;
                        env.say(if cli.json {
                            json!({"ok": true, "file": to, "bytes": d.len()}).to_string()
                        } else {
                            format!("{} ({} bytes)", to.display(), d.len())
                        });
                        Ok(())
                    })
                }
                _ => Err(anyhow!("sd ls [folder] | sd get <file> [-o out]")),
            };
            s.close().await;
            r.map_err(Exit::Radio)
        }
        Cmd::Get { setting } => setting_cmd(cli, env, &setting, None).await,
        Cmd::Set { setting, value } => setting_cmd(cli, env, &setting, Some(&value)).await,
        Cmd::Watch { what, count } => {
            let dev = env.device(cli.serial.as_deref())?;
            let s = env.open(&dev).await?;
            let l = s.need_link()?;
            let mut rx = l.subscribe();
            let mut n = 0u64;
            // Events: STATUS says when the radio logged new ones; the console's
            // `events` gives them with their names.
            let mut event_seq: Option<u8> = None;
            loop {
                let ev = tokio::select! {
                    ev = rx.recv() => ev,
                    _ = tokio::signal::ctrl_c() => break,
                };
                if what == "events"
                    && let Ok(LinkEvent::Status(st)) = &ev
                    && event_seq != Some(st.event_seq)
                {
                    let fresh = event_seq.map(|o| st.event_seq.wrapping_sub(o) as usize);
                    event_seq = Some(st.event_seq);
                    if let Some(fresh) = fresh
                        && let Ok(mut evs) = l.read_events(timeout).await
                    {
                        evs.retain(|e| (e.index as usize) < fresh);
                        evs.sort_by_key(|e| std::cmp::Reverse(e.index));
                        for e in evs {
                            env.say(if cli.json {
                                json!(e).to_string()
                            } else {
                                format!("{} {} {} {:08X}", e.severity, e.stage, e.reason, e.value)
                            });
                            n += 1;
                        }
                        if count.is_some_and(|c| n >= c) {
                            break;
                        }
                    }
                    continue;
                }
                let line = match ev {
                    Ok(LinkEvent::Status(st)) if what == "status" => Some(if cli.json {
                        status_json(&st).to_string()
                    } else {
                        format!(
                            "{:>9} ms {:<10} {} {:<4} S {:6.1}  VIN {:.2}  {}  {}",
                            st.uptime_ms,
                            st.state_name(),
                            fmt_hz(st.dial_hz),
                            st.mode_name(),
                            st.rx_dbfs_x10 as f32 / 10.0,
                            st.vin_mv as f32 / 1000.0,
                            st.page_name(),
                            tx_owner_name(st.tx_owner)
                        )
                    }),
                    Ok(LinkEvent::CwDec(t)) if what == "cw" => Some(t),
                    Ok(LinkEvent::Closed(e)) => return Err(Exit::NotConnected(anyhow!("link closed: {e}"))),
                    _ => None,
                };
                if let Some(line) = line {
                    env.say(line);
                    n += 1;
                    if count.is_some_and(|c| n >= c) {
                        break;
                    }
                }
            }
            s.close().await;
            Ok(())
        }
        Cmd::Diagnose => {
            let dev = env.device(cli.serial.as_deref())?;
            let mut s = env.open(&dev).await?;
            let mut report = Vec::new();
            let mut problems = Vec::new();
            for c in [
                "status",
                "faults",
                "pipeline",
                "events",
                "rx",
                "digi",
                "cat",
                "pd",
                "clock sof",
                "cpu",
                "mem",
            ] {
                match s.cmd(c, timeout).await {
                    Ok(lines) => {
                        for l in &lines {
                            if problem_line(l) {
                                problems.push(format!("{c}: {l}"));
                            }
                        }
                        report.push((c.to_string(), lines));
                    }
                    Err(e) => problems.push(format!("{c}: {e:#}")),
                }
            }
            let st = s.link().and_then(|l| l.snapshot().status);
            let (cx, host) = context(&dev, st.clone(), s.link().is_some(), false);
            if let Some(st) = &st {
                if st.rx_overruns != 0 {
                    problems.push(format!("receive overruns: {}", st.rx_overruns));
                }
                if st.usb_iq() {
                    problems.push("USB RAW IQ is on: the sound card carries I/Q, not audio".into());
                }
            }
            for h in &host.link_port_holders {
                problems.push(format!("S-CTRL's port is held by {} (pid {})", h.name, h.pid));
            }
            s.close().await;
            if cli.json {
                env.say(
                    json!({"ok": true, "problems": problems, "cat_program": cx.cat_program, "host": host,
                           "report": report.iter().map(|(c, l)| json!({"command": c, "lines": l})).collect::<Vec<_>>()})
                    .to_string(),
                );
            } else {
                if problems.is_empty() {
                    env.say("problems: none found");
                } else {
                    env.say("problems:");
                    for p in &problems {
                        env.say(format!("  ! {p}"));
                    }
                }
                for (c, lines) in report {
                    env.say(format!("--- {c}"));
                    for l in lines {
                        env.say(l);
                    }
                }
            }
            Ok(())
        }
    }
}

/// DIAG -> AUDIO: `exit` on the DIAG console, then wait for the sound card
/// personality. Only on the user's command, only when nobody holds the port.
async fn usb_audio(cli: &Cli, env: &mut Env) -> Result<(), Exit> {
    let dev = env.device(cli.serial.as_deref())?;
    match dev.personality() {
        Some(device::Personality::Audio) => {
            env.say("already the sound card + CAT/LINK personality");
            return Ok(());
        }
        Some(device::Personality::Disk) => {
            return Err(Exit::Radio(anyhow!("USB DISK MODE: leave it on the radio")));
        }
        _ => {}
    }
    let port = dev
        .link_port()
        .ok_or_else(|| Exit::NotConnected(anyhow!("no DIAG port")))?;
    if let Some(who) = sctrl_core::session::port_holders_text(&port) {
        return Err(Exit::NotConnected(anyhow!("{} is held by {who}", port.display())));
    }
    let mut c = sctrl_core::console::TextConsole::open(&port).await?;
    let _ = c.cmd("exit", Duration::from_secs(3)).await; // the port goes away with the reply
    drop(c);
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(d) = device::find_spartacus(Some(&dev.serial))
            && d.personality() == Some(device::Personality::Audio)
            && d.link_port().is_some()
        {
            env.say(if cli.json {
                json!({"ok": true, "personality": "audio"}).to_string()
            } else {
                "sound card + CAT/LINK ports up".into()
            });
            return Ok(());
        }
    }
    Err(Exit::Timeout(anyhow!(
        "the radio did not come back as a sound card within 20 s"
    )))
}

/// The PC speaker's plan, for people.
fn plan_words(p: Option<&sctrl_core::audio::PcPlan>) -> String {
    use sctrl_core::audio::PcPlan;
    match p {
        None | Some(PcPlan::Off) => "off".into(),
        Some(PcPlan::Unavailable { reason }) => format!("not playing ({reason})"),
        Some(PcPlan::Play { muted: Some(why), .. }) => format!("quiet ({why})"),
        Some(PcPlan::Play { latency, .. }) => format!("playing ({:.1} ms buffer)", *latency as f32 * 1000.0 / 48000.0),
    }
}

async fn audio_cmd(
    cli: &Cli,
    env: &mut Env,
    what: &str,
    arg: Option<&str>,
    digi: Option<bool>,
    volume: Option<f32>,
    output: Option<String>,
) -> Result<(), Exit> {
    use sctrl_core::audio;
    let svc_audio = env.shared.as_ref().and_then(|s| s.audio());
    match what {
        "status" => {
            let (st, graph) = match &svc_audio {
                Some(a) => (Some(a.status()), a.graph()),
                None => (None, audio::graph_once().await?),
            };
            let serial = env.device(cli.serial.as_deref()).ok().map(|d| d.serial);
            let src = serial
                .as_deref()
                .and_then(|s| graph.radio_source(s))
                .map(|n| n.name.clone());
            let sink = serial
                .as_deref()
                .and_then(|s| graph.radio_sink(s))
                .map(|n| n.name.clone());
            let def = graph.default_sink.clone();
            let feeding: Vec<String> = serial
                .as_deref()
                .and_then(|s| graph.radio_sink(s))
                .map(|k| {
                    graph
                        .feeding(k.id)
                        .iter()
                        .map(|(_, n)| format!("{} ({})", graph.client_binary(n), n.name))
                        .collect()
                })
                .unwrap_or_default();
            if cli.json {
                env.say(
                    json!({"ok": true, "radio_source": src, "radio_sink": sink, "default_sink": def,
                    "into_radio_tx": feeding, "pc_speaker": st})
                    .to_string(),
                );
            } else {
                env.say(format!("radio capture (RX):  {}", src.unwrap_or("-".into())));
                env.say(format!("radio playback (TX): {}", sink.unwrap_or("-".into())));
                env.say(format!("default output:      {}", def.unwrap_or("-".into())));
                env.say(format!(
                    "playing into the radio's TX audio: {}",
                    if feeding.is_empty() {
                        "nothing".into()
                    } else {
                        feeding.join(", ")
                    }
                ));
                match st {
                    Some(s) => {
                        env.say(format!(
                            "PC speaker: {}  output {}  volume {:.0}%",
                            plan_words(s.plan.as_ref()),
                            s.output.unwrap_or("-".into()),
                            s.volume * 100.0
                        ));
                        for n in s.notes {
                            env.say(format!("  note: {n}"));
                        }
                    }
                    None => env.say("PC speaker: needs the running S-CTRL (window or `sctrl daemon`)"),
                }
            }
            Ok(())
        }
        "pc" => {
            let a = svc_audio.ok_or_else(|| {
                Exit::NotConnected(anyhow!(
                    "the PC speaker needs the running S-CTRL (window or `sctrl daemon`)"
                ))
            })?;
            let mut w = a.wish();
            match arg {
                Some("on") => w.enabled = true,
                Some("off") => w.enabled = false,
                None => {}
                Some(x) => return Err(Exit::Radio(anyhow!("audio pc {x}: on or off"))),
            }
            if let Some(d) = digi {
                w.digi = d;
            }
            if let Some(v) = volume {
                w.volume = v.clamp(0.0, 1.5);
            }
            if let Some(o) = output {
                w.pinned = if o == "default" { None } else { Some(o) };
            }
            if let Some(d) = &w.pinned
                && d.contains("Chireix_SPARTACUS")
            {
                return Err(Exit::Refused(Denied {
                    rule: "R6",
                    reason: "never play the radio into its own TX audio".into(),
                }));
            }
            a.set_wish(w.clone());
            if let Some(svc) = env.service() {
                let w = w.clone();
                svc.update_config(move |c| {
                    c.pc_speaker.enabled = w.enabled;
                    c.pc_speaker.digi = w.digi;
                    c.pc_speaker.volume = w.volume;
                    c.pc_speaker.pinned = w.pinned;
                })?;
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            let st = a.status();
            if cli.json {
                env.say(json!({"ok": true, "wish": w, "status": st}).to_string());
            } else {
                env.say(format!(
                    "PC speaker {}{}: {}  output {}",
                    if w.enabled { "on" } else { "off" },
                    if w.digi { " (DIGI too)" } else { "" },
                    plan_words(st.plan.as_ref()),
                    st.output.unwrap_or("-".into())
                ));
            }
            Ok(())
        }
        "phones" => {
            let svc = env
                .service()
                .ok_or_else(|| Exit::NotConnected(anyhow!("needs the running S-CTRL (window or `sctrl daemon`)")))?
                .clone();
            let off = match arg {
                Some("off") => true,
                Some("on") => false,
                _ => return Err(Exit::Radio(anyhow!("audio phones on|off"))),
            };
            svc.update_config(|c| c.pc_speaker.radio_phones_off = off)?;
            env.say(format!(
                "the radio's headphones {} while the PC speaker plays (not DIGI)",
                if off { "silent" } else { "on" }
            ));
            Ok(())
        }
        "guard" => {
            let a = svc_audio.ok_or_else(|| Exit::NotConnected(anyhow!("the guard runs in the running S-CTRL")))?;
            let on = match arg {
                Some("on") => true,
                Some("off") => false,
                _ => return Err(Exit::Radio(anyhow!("audio guard on|off"))),
            };
            // For this run only: the guard is on again at the next start.
            a.set_auto_protect(on);
            env.say(format!(
                "TX audio guard {}",
                if on { "on" } else { "off (bench: re-enable after)" }
            ));
            Ok(())
        }
        other => Err(Exit::Radio(anyhow!("audio {other}: status, pc, phones or guard"))),
    }
}

async fn input(cli: &Cli, env: &mut Env, code: Input, value: i8) -> Result<(), Exit> {
    let dev = env.device(cli.serial.as_deref())?;
    let s = env.open(&dev).await?;
    let l = s.need_link()?;
    guard(cli, &dev, l, Action::PanelInput).await?;
    let r = l.input(code, value).await;
    s.close().await;
    r?;
    if cli.json {
        env.say(json!({"ok": true}).to_string());
    }
    Ok(())
}

async fn setting_cmd(cli: &Cli, env: &mut Env, setting: &str, value: Option<&str>) -> Result<(), Exit> {
    let timeout = Duration::from_secs_f64(cli.timeout);
    let dev = env.device(cli.serial.as_deref())?;
    let mut s = env.open(&dev).await?;
    let id = setting_id(&mut s, setting, timeout).await?;
    let line = match value {
        Some(v) => {
            if let Some(l) = s.link() {
                guard(cli, &dev, l, Action::Setting).await?;
            }
            format!("set put {id} {v}")
        }
        None => format!("set get {id}"),
    };
    let r = s.cmd(&line, timeout).await;
    s.close().await;
    let lines = r?;
    let kv = lines.first().cloned().unwrap_or_default();
    if cli.json {
        let (name, v) = kv.split_once('=').unwrap_or((&kv, ""));
        env.say(json!({"ok": true, "id": id, "name": name, "value": v}).to_string());
    } else {
        env.say(kv);
    }
    Ok(())
}

/// A console line that reports trouble: a latched fault or lockout, or
/// fault bits (`faults=00` and `latched=0` are fine).
fn problem_line(l: &str) -> bool {
    l.split_whitespace().any(|tok| match tok.split_once('=') {
        Some(("latched" | "hard_lockout" | "lockout" | "rearm_pending", v)) => v != "0",
        Some(("faults", v)) => !v.trim_start_matches('0').is_empty(),
        _ => false,
    })
}

/// `set list` lines (`12 "CW SPEED WPM" 20 5..60 0`): the id named
/// `want`, exactly or as the one name it begins; Err lists the names a
/// prefix fits when it fits several.
fn find_setting(lines: &[String], want: &str) -> Result<u32, Vec<String>> {
    let rows: Vec<(u32, &str)> = lines
        .iter()
        .filter_map(|l| {
            let (id, rest) = l.split_once(' ')?;
            Some((id.parse().ok()?, rest.split('"').nth(1)?))
        })
        .collect();
    if let Some((id, _)) = rows.iter().find(|(_, n)| n.eq_ignore_ascii_case(want)) {
        return Ok(*id);
    }
    let want = want.to_ascii_uppercase();
    let fits: Vec<&(u32, &str)> = rows
        .iter()
        .filter(|(_, n)| n.to_ascii_uppercase().starts_with(&want))
        .collect();
    match fits.as_slice() {
        [(id, _)] => Ok(*id),
        _ => Err(fits.iter().map(|(_, n)| n.to_string()).collect()),
    }
}

/// A setting by numeric id, or by name through `set list`
/// (case-insensitive): the exact name, else the only name it begins.
async fn setting_id(s: &mut Sess, setting: &str, timeout: Duration) -> Result<u32, Exit> {
    if let Ok(id) = setting.parse::<u32>() {
        return Ok(id);
    }
    let lines = s.cmd("set list", timeout).await?;
    match find_setting(&lines, setting) {
        Ok(id) => return Ok(id),
        Err(several) if !several.is_empty() => {
            return Err(Exit::Radio(anyhow!(
                "{setting} could be {}: say which",
                several.join(", ")
            )));
        }
        Err(_) => {}
    }
    Err(Exit::Radio(anyhow!(
        "no setting named {setting} (sctrl console set list)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequencies() {
        assert_eq!(parse_freq("14074000").unwrap(), 14_074_000);
        assert_eq!(parse_freq("14.074M").unwrap(), 14_074_000);
        assert_eq!(parse_freq("7074k").unwrap(), 7_074_000);
        assert_eq!(parse_freq("7.0745 MHz").unwrap(), 7_074_500);
        assert!(parse_freq("70M").is_err());
        assert!(parse_freq("abc").is_err());
    }

    #[test]
    fn diagnose_flags_only_real_trouble() {
        assert!(!problem_line(
            "latched=0 hard_lockout=0 rearm_pending=0 total=266 soft=0"
        ));
        assert!(problem_line(
            "latched=1 hard_lockout=0 rearm_pending=0 total=266 soft=0"
        ));
        assert!(problem_line("latched=0 hard_lockout=1"));
        assert!(!problem_line("faults=00"));
        assert!(problem_line("faults=10"));
        assert!(!problem_line("pipeline=clean"));
    }

    #[test]
    fn settings_by_name_or_unique_prefix() {
        let lines: Vec<String> = [
            r#"5 "TRANSMIT MUTE" 1 - 1"#,
            r#"8 "CW SPEED WPM" 20 5..60 0"#,
            r#"225 "LISTEN WPM" 20 5..60 0"#,
            r#"226 "LISTEN TONE" 600 300..1000 0"#,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(find_setting(&lines, "transmit mute"), Ok(5));
        assert_eq!(find_setting(&lines, "CW SPEED"), Ok(8));
        assert_eq!(
            find_setting(&lines, "listen"),
            Err(vec!["LISTEN WPM".to_string(), "LISTEN TONE".to_string()])
        );
        assert_eq!(find_setting(&lines, "nothing"), Err(vec![]));
    }

    #[test]
    fn modes() {
        assert_eq!(mode_cat("DIGI").unwrap(), "MD9;");
        assert_eq!(mode_cat("cw").unwrap(), "MD3;");
        assert!(mode_cat("rtty").is_err());
    }
}
