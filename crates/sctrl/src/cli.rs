use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use sctrl_core::device::{self, UsbDevice};
use sctrl_core::hostview::HostView;
use sctrl_core::link::{Link, LinkEvent, LinkOptions};
use sctrl_core::safety::{self, Action, Context, Denied};
use sctrl_core::session::{OpenError, Session};
use sctrl_proto::frame::Input;
use sctrl_proto::payload::{Status, tx_owner_name};
use sctrl_proto::screen::{self, Screen};

#[derive(Parser)]
#[command(
    name = "sctrl",
    version,
    about = "S-CTRL: control the Chireix SPARTACUS REV1 from this PC"
)]
struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// The radio's USB serial, when several are attached.
    #[arg(long, global = true)]
    serial: Option<String>,
    /// Seconds to wait for the radio.
    #[arg(long, global = true, default_value_t = 10.0)]
    timeout: f64,
    /// Change frequency/mode/band although a CAT program (WSJT-X ...) shares the radio.
    #[arg(long, global = true)]
    take_control: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
}

#[derive(Debug)]
enum Exit {
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
        } else if s.contains("within") || s.contains("no HELLO") || s.contains("No answer") {
            Exit::Timeout(e)
        } else {
            Exit::Radio(e)
        }
    }
}

pub fn main() -> i32 {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let json = cli.json;
    match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(e) => {
            let (code, kind, msg) = match &e {
                Exit::NotConnected(e) => (3, "not_connected", format!("{e:#}")),
                Exit::Refused(d) => (4, "refused", d.to_string()),
                Exit::Timeout(e) => (5, "timeout", format!("{e:#}")),
                Exit::Radio(e) => (6, "error", format!("{e:#}")),
            };
            if json {
                println!("{}", json!({"ok": false, "error": kind, "message": msg}));
            } else {
                eprintln!("sctrl: {msg}");
            }
            code
        }
    }
}

fn find_radio(serial: Option<&str>) -> Result<UsbDevice, Exit> {
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

async fn open(dev: &UsbDevice) -> Result<Session, Exit> {
    Ok(Session::open(dev, LinkOptions::default()).await?)
}

fn need_link(s: &Session) -> Result<&Link, Exit> {
    match s {
        Session::Link(l) => Ok(l),
        Session::Text { why, .. } => Err(Exit::Radio(anyhow!("this needs the LINK ({why})"))),
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

fn context(
    dev: &UsbDevice,
    status: Option<Status>,
    link_live: bool,
    take_control: bool,
) -> (Context, HostView) {
    let host = HostView::gather(Some(dev));
    let cat0 = status
        .as_ref()
        .map(|s| s.cat0_age_ds)
        .filter(|&a| a != 0xFFFF);
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

async fn guard(cli: &Cli, dev: &UsbDevice, l: &Link, action: Action) -> Result<(), Exit> {
    let st = wait_status(l, Duration::from_secs(2)).await.ok();
    let (cx, _) = context(dev, st, l.closed().is_none(), cli.take_control);
    safety::decide(action, &cx).map_err(Exit::Refused)
}

fn parse_freq(s: &str) -> Result<u32> {
    let t = s.trim().to_ascii_lowercase().replace(['_', ','], "");
    let (num, mul) = if let Some(n) = t.strip_suffix("mhz").or(t.strip_suffix('m')) {
        (n, 1e6)
    } else if let Some(n) = t.strip_suffix("khz").or(t.strip_suffix('k')) {
        (n, 1e3)
    } else {
        (t.strip_suffix("hz").unwrap_or(&t), 1.0)
    };
    let v: f64 = num.parse().map_err(|_| anyhow!("not a frequency: {s}"))?;
    let hz = (v * mul).round();
    if !(100_000.0..=60_000_000.0).contains(&hz) {
        bail!("{s} is outside 0.1-60 MHz");
    }
    Ok(hz as u32)
}

fn mode_cat(m: &str) -> Result<&'static str> {
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

fn fmt_hz(hz: u32) -> String {
    format!(
        "{}.{:03}.{:03}",
        hz / 1_000_000,
        (hz / 1000) % 1000,
        hz % 1000
    )
}

fn status_json(s: &Status) -> serde_json::Value {
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
        "cat_ports_open": s.cat_ports_open(), "console_port": s.console_port(),
        "cat0_age_s": (s.cat0_age_ds != 0xFFFF).then(|| s.cat0_age_ds as f32 / 10.0),
        "rx_overruns": s.rx_overruns, "capture_gaps": s.cap_gaps,
        "ref": ref_name(s.ref_state),
        "ref_ppm": s.ref_ppm_x100 as f32 / 100.0, "band": s.band, "uptime_ms": s.uptime_ms,
        "settings_hash": format!("{:08x}", s.settings_hash),
    })
}

fn print_status(dev: &UsbDevice, l: &Link, s: &Status, host: &HostView, cx: &Context) {
    let h = l.snapshot().hello;
    println!(
        "SPARTACUS REV1 {}  firmware v{}  LINK v{}",
        dev.serial,
        h.as_ref()
            .map(|h| format!("{}.{:02}", h.fw_major, h.fw_minor))
            .unwrap_or("?".into()),
        h.as_ref().map(|h| h.proto).unwrap_or(0)
    );
    println!(
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
    );
    println!(
        "VIN {:.2} V   PD {:.1} V   S {:.1} dBFS   load {}%   ref {} {:+.2} ppm",
        s.vin_mv as f32 / 1000.0,
        s.pd_mv as f32 / 1000.0,
        s.rx_dbfs_x10 as f32 / 10.0,
        s.load_pct,
        ref_name(s.ref_state),
        s.ref_ppm_x100 as f32 / 100.0
    );
    println!(
        "TX: owner {}  CAT PTT {}  TX MUTE {}  armed {}  USB VOX {}  USB RAW IQ {}",
        tx_owner_name(s.tx_owner),
        onoff(s.cat_ptt()),
        onoff(s.tx_mute()),
        onoff(s.armed()),
        onoff(s.usb_vox()),
        onoff(s.usb_iq())
    );
    let faults = s.fault_names();
    println!(
        "faults {}  lockout {}  overruns {}  capture gaps {}{}",
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
    );
    println!(
        "CAT ports open: {}  port 0 last command: {}",
        match s.cat_ports_open() {
            0 => "none".to_string(),
            m => (0..2)
                .filter(|p| m & (1 << p) != 0)
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(","),
        },
        if s.cat0_age_ds == 0xFFFF {
            "never".into()
        } else {
            format!("{:.1} s ago", s.cat0_age_ds as f32 / 10.0)
        }
    );
    let apps: Vec<String> = host
        .apps
        .iter()
        .map(|r| format!("{} (pid {})", r.app.name(), r.proc.pid))
        .collect();
    println!(
        "host: {}",
        if apps.is_empty() {
            "no radio programs running".into()
        } else {
            apps.join(", ")
        }
    );
    if let Some(p) = &cx.cat_program {
        println!("CAT program: {p}: frequency/mode/band locked unless --take-control");
    }
}

fn ref_name(state: u8) -> &'static str {
    ["idle", "acquiring", "locked"]
        .get(state as usize)
        .copied()
        .unwrap_or("?")
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

async fn run(cli: Cli) -> Result<(), Exit> {
    let timeout = Duration::from_secs_f64(cli.timeout);
    match &cli.cmd {
        Cmd::Ls => {
            let devs = device::scan();
            let host = HostView::gather(device::find_spartacus(cli.serial.as_deref()).as_ref());
            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "devices": devs.iter().filter(|d| !matches!(d.kind, device::Kind::Other)).collect::<Vec<_>>(), "host": host})
                );
                return Ok(());
            }
            let mut any = false;
            for d in devs
                .iter()
                .filter(|d| !matches!(d.kind, device::Kind::Other))
            {
                any = true;
                println!("{}", d.label());
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
                        println!("  if{i:02} {role:<16} {}  {held}", p.display());
                    }
                    if let Some(c) = d.sound_card {
                        println!("  sound card {c}");
                    }
                }
            }
            if !any {
                println!("no SPARTACUS, QRP Labs radio or ST-LINK on USB");
            }
            for r in &host.apps {
                println!(
                    "running: {} (pid {}){}",
                    r.app.name(),
                    r.proc.pid,
                    r.rig_name
                        .as_ref()
                        .map(|n| format!(" --rig-name={n}"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        Cmd::Status => {
            let dev = find_radio(cli.serial.as_deref())?;
            let mut s = open(&dev).await?;
            if let Session::Text { why, .. } = &s {
                let why = why.clone();
                let lines = s.cmd("status", timeout).await?;
                if cli.json {
                    println!(
                        "{}",
                        json!({"ok": true, "basic_mode": why, "status": lines})
                    );
                } else {
                    println!("({why})");
                    for l in lines {
                        println!("{l}");
                    }
                }
                return Ok(());
            }
            let l = need_link(&s)?;
            let st = wait_status(l, timeout).await?;
            let (cx, host) = context(&dev, Some(st.clone()), true, cli.take_control);
            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "serial": dev.serial, "hello": l.snapshot().hello,
                           "status": status_json(&st), "host": host, "cat_program": cx.cat_program,
                           "iq_consumer": cx.iq_consumer, "link": l.snapshot().stats})
                );
            } else {
                print_status(&dev, l, &st, &host, &cx);
            }
            s.close().await;
            Ok(())
        }
        Cmd::Shot { out, scale } => {
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            let end = Instant::now() + timeout;
            let screen: Screen = loop {
                let snap = l.snapshot();
                if snap.screen.known.iter().all(|k| *k) {
                    break snap.screen;
                }
                if Instant::now() > end {
                    return Err(Exit::Timeout(anyhow!(
                        "the screen did not arrive within {timeout:?}"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            };
            let rgba = screen.render_rgba();
            let img = image::RgbaImage::from_raw(screen::W as u32, screen::H as u32, rgba)
                .ok_or_else(|| anyhow!("image"))?;
            let img = image::imageops::resize(
                &img,
                screen::W as u32 * scale,
                screen::H as u32 * scale,
                image::imageops::FilterType::Nearest,
            );
            img.save(out)
                .map_err(|e| anyhow!("{}: {e}", out.display()))?;
            if cli.json {
                println!("{}", json!({"ok": true, "file": out, "band": screen.band}));
            } else {
                println!("{}", out.display());
            }
            s.close().await;
            Ok(())
        }
        Cmd::Console { line } => {
            let line = line.join(" ");
            let dev = find_radio(cli.serial.as_deref())?;
            let mut s = open(&dev).await?;
            let r = s.cmd(&line, timeout).await;
            s.close().await;
            let lines = r?;
            if cli.json {
                println!("{}", json!({"ok": true, "lines": lines}));
            } else {
                for l in lines {
                    println!("{l}");
                }
            }
            Ok(())
        }
        Cmd::Cat { command } => {
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            // Reads are free; anything that sets goes through the arbiter.
            let c = command.trim().to_ascii_uppercase();
            let is_read = c.ends_with(';') && c.len() <= 4 || c.ends_with("?;");
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
                guard(&cli, &dev, l, action).await?;
            }
            let r = l.cat(command, timeout).await;
            s.close().await;
            let reply = r?;
            if cli.json {
                println!("{}", json!({"ok": true, "reply": reply}));
            } else {
                println!("{reply}");
            }
            Ok(())
        }
        Cmd::Press { name, long } => {
            let mut code = Input::from_name(&name.to_ascii_lowercase())
                .ok_or_else(|| anyhow!("unknown button {name}: click hold up dn sel func"))?;
            if *long {
                code = code.long();
            }
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            guard(&cli, &dev, l, Action::PanelInput).await?;
            let r = l.input(code, 0).await;
            s.close().await;
            r?;
            ok(&cli);
            Ok(())
        }
        Cmd::Knob { steps, held, func } => {
            let code = if *func {
                Input::FuncTurn
            } else if *held {
                Input::TurnHeld
            } else {
                Input::Turn
            };
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            guard(&cli, &dev, l, Action::PanelInput).await?;
            let r = l.input(code, *steps).await;
            s.close().await;
            r?;
            ok(&cli);
            Ok(())
        }
        Cmd::Tune { freq } => {
            let hz = parse_freq(freq)?;
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            guard(&cli, &dev, l, Action::Tune).await?;
            let r = l.cat(&format!("FA{hz:011};"), timeout).await;
            // Confirm from STATUS: the dial the panel shows.
            let mut confirmed = None;
            if r.is_ok() {
                let end = Instant::now() + Duration::from_secs(2);
                while Instant::now() < end {
                    if let Some(st) = l.snapshot().status
                        && st.dial_hz == hz
                    {
                        confirmed = Some(st.dial_hz);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            s.close().await;
            r?;
            match confirmed {
                Some(_) => {
                    if cli.json {
                        println!("{}", json!({"ok": true, "dial_hz": hz}));
                    } else {
                        println!("{} Hz", fmt_hz(hz));
                    }
                    Ok(())
                }
                None => Err(Exit::Radio(anyhow!(
                    "the radio did not take {} Hz (a page that owns the dial? out of band?)",
                    fmt_hz(hz)
                ))),
            }
        }
        Cmd::Mode { mode } => {
            let md = mode_cat(mode)?;
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            guard(&cli, &dev, l, Action::Mode).await?;
            let r = l.cat(md, timeout).await;
            // The firmware applies a mode once the audio path is idle (up to 5 s).
            let want = md.trim_end_matches(';').to_string();
            let mut got = String::new();
            if r.is_ok() {
                let end = Instant::now() + Duration::from_secs(6);
                while Instant::now() < end {
                    got = l
                        .cat("MD;", Duration::from_secs(2))
                        .await
                        .unwrap_or_default();
                    if got.trim_end_matches(';') == want {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            s.close().await;
            r?;
            if got.trim_end_matches(';') != want {
                return Err(Exit::Radio(anyhow!(
                    "the radio did not change mode (it reads {got})"
                )));
            }
            if cli.json {
                println!("{}", json!({"ok": true, "mode": mode}));
            } else {
                println!("{}", mode.to_ascii_uppercase());
            }
            Ok(())
        }
        Cmd::Get { setting } | Cmd::Set { setting, .. } => {
            let dev = find_radio(cli.serial.as_deref())?;
            let mut s = open(&dev).await?;
            let id = setting_id(&mut s, setting, timeout).await?;
            let line = match &cli.cmd {
                Cmd::Set { value, .. } => {
                    if let Some(l) = s.link() {
                        guard(&cli, &dev, l, Action::Setting).await?;
                    }
                    format!("set put {id} {value}")
                }
                _ => format!("set get {id}"),
            };
            let r = s.cmd(&line, timeout).await;
            s.close().await;
            let lines = r?;
            let kv = lines.first().cloned().unwrap_or_default();
            if cli.json {
                let (name, value) = kv.split_once('=').unwrap_or((&kv, ""));
                println!(
                    "{}",
                    json!({"ok": true, "id": id, "name": name, "value": value})
                );
            } else {
                println!("{kv}");
            }
            Ok(())
        }
        Cmd::Watch { what, count } => {
            let dev = find_radio(cli.serial.as_deref())?;
            let s = open(&dev).await?;
            let l = need_link(&s)?;
            let mut rx = l.subscribe();
            let mut n = 0u64;
            loop {
                let ev = tokio::select! {
                    ev = rx.recv() => ev,
                    _ = tokio::signal::ctrl_c() => break,
                };
                let printed = match ev {
                    Ok(LinkEvent::Status(st)) if what == "status" => {
                        if cli.json {
                            println!("{}", status_json(&st));
                        } else {
                            println!(
                                "{:>9} ms {:<10} {} {:<4} S {:6.1}  VIN {:.2}  {}  {}",
                                st.uptime_ms,
                                st.state_name(),
                                fmt_hz(st.dial_hz),
                                st.mode_name(),
                                st.rx_dbfs_x10 as f32 / 10.0,
                                st.vin_mv as f32 / 1000.0,
                                st.page_name(),
                                tx_owner_name(st.tx_owner)
                            );
                        }
                        true
                    }
                    Ok(LinkEvent::Event(e)) if what == "events" => {
                        println!("{}", json!(e));
                        true
                    }
                    Ok(LinkEvent::CwDec(t)) if what == "cw" => {
                        use std::io::Write;
                        print!("{t}");
                        let _ = std::io::stdout().flush();
                        true
                    }
                    Ok(LinkEvent::Closed(e)) => {
                        return Err(Exit::NotConnected(anyhow!("link closed: {e}")));
                    }
                    _ => false,
                };
                if printed {
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
            let dev = find_radio(cli.serial.as_deref())?;
            let mut s = open(&dev).await?;
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
                            let low = l.to_ascii_lowercase();
                            if low.contains("latched")
                                || (low.starts_with("faults=") && !low.starts_with("faults=00"))
                                || low.contains("lockout=1")
                            {
                                problems.push(format!("{c}: {l}"));
                            }
                        }
                        report.push((c.to_string(), lines));
                    }
                    Err(e) => problems.push(format!("{c}: {e:#}")),
                }
            }
            let host = HostView::gather(Some(&dev));
            let st = s.link().and_then(|l| l.snapshot().status);
            let cx = context(&dev, st.clone(), s.link().is_some(), false).0;
            if let Some(st) = &st {
                if st.rx_overruns != 0 {
                    problems.push(format!("receive overruns: {}", st.rx_overruns));
                }
                if st.usb_iq() {
                    problems.push("USB RAW IQ is on: the sound card carries I/Q, not audio".into());
                }
            }
            s.close().await;
            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "problems": problems, "cat_program": cx.cat_program, "host": host,
                           "report": report.iter().map(|(c, l)| json!({"command": c, "lines": l})).collect::<Vec<_>>()})
                );
            } else {
                if problems.is_empty() {
                    println!("problems: none found");
                } else {
                    println!("problems:");
                    for p in &problems {
                        println!("  ! {p}");
                    }
                }
                for (c, lines) in report {
                    println!("--- {c}");
                    for l in lines {
                        println!("{l}");
                    }
                }
            }
            Ok(())
        }
    }
}

fn ok(cli: &Cli) {
    if cli.json {
        println!("{}", json!({"ok": true}));
    }
}

/// A setting by numeric id, or by name through `set list` (case-insensitive).
async fn setting_id(s: &mut Session, setting: &str, timeout: Duration) -> Result<u32, Exit> {
    if let Ok(id) = setting.parse::<u32>() {
        return Ok(id);
    }
    let lines = s.cmd("set list", timeout).await?;
    let want = setting.to_ascii_uppercase();
    for l in &lines {
        // 12 "CW SPEED" 20 5..40 3
        let Some((id, rest)) = l.split_once(' ') else {
            continue;
        };
        let Some(name) = rest.split('"').nth(1) else {
            continue;
        };
        if name.eq_ignore_ascii_case(&want) {
            return id
                .parse()
                .map_err(|_| Exit::Radio(anyhow!("bad set list line {l}")));
        }
    }
    Err(Exit::Radio(anyhow!(
        "no setting named {setting} (sctrl console set list)"
    )))
}
