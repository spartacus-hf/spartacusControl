//! `sctrl tx ...` and `sctrl goto ...`: S-CTRL's own transmissions through
//! core::tx (checklist, limits, live watch, audit log), and opening the
//! radio's pages. With an S-CTRL running (window or daemon) these run inside
//! it, where ARM and the session's load answer persist between commands;
//! without one, each command opens the radio itself, so it must arm (--arm)
//! and say the load (--load) every time.

use std::time::{Duration, Instant};

use anyhow::anyhow;
use clap::Subcommand;
use serde_json::json;

use sctrl_core::link::Link;
use sctrl_core::safety::{Action, Context};
use sctrl_core::tx::{self, Load, Region, TxController, TxKind, TxPolicy, TxReport};
use sctrl_proto::frame::Input;
use sctrl_proto::payload::PAGES;

use crate::cli::{Cli, Env, Exit};

#[derive(Subcommand, Clone)]
pub enum TxCmd {
    /// The pre-TX checklist now for tune, key or mem (nothing is keyed).
    Check {
        #[arg(default_value = "key")]
        kind: String,
        #[arg(long)]
        load: Option<String>,
    },
    /// ARM S-CTRL's TX (the radio ends it after the policy's TTL without TX), or --off.
    Arm {
        #[arg(long)]
        off: bool,
        /// What the radio transmits into: dummy (50 ohm load) or antenna.
        #[arg(long)]
        load: Option<String>,
    },
    /// The TUNE carrier on the radio's TX TEST page for --ms (the policy caps it).
    Tune {
        #[arg(long, default_value_t = 800)]
        ms: u32,
        #[arg(long)]
        load: Option<String>,
        /// ARM first (needed without a running S-CTRL).
        #[arg(long)]
        arm: bool,
    },
    /// Hold the paddle for --ms: DIT, --dah, or --both (CW keyer elements; on TX TEST the carrier).
    Key {
        #[arg(long, default_value_t = 300)]
        ms: u32,
        #[arg(long)]
        dah: bool,
        #[arg(long)]
        both: bool,
        #[arg(long)]
        load: Option<String>,
        #[arg(long)]
        arm: bool,
    },
    /// Send a CW memory through the radio's keyer (at most 48 characters).
    Mem {
        text: String,
        #[arg(long)]
        load: Option<String>,
        #[arg(long)]
        arm: bool,
    },
    /// Stop everything S-CTRL keyed and disarm. Always allowed.
    Stop,
    /// Show the TX limits, or change them (saved in ~/.config/sctrl/config.json).
    Policy {
        /// on: every key-down at most 1 s.
        #[arg(long)]
        bench: Option<String>,
        #[arg(long)]
        tune_ms: Option<u32>,
        #[arg(long)]
        key_ms: Option<u32>,
        #[arg(long)]
        memory_s: Option<u32>,
        #[arg(long)]
        duty_pct: Option<u32>,
        #[arg(long)]
        arm_ttl_s: Option<u32>,
        /// IARU region for the band plan: 1, 2 or 3.
        #[arg(long)]
        region: Option<u8>,
        /// Lowest supply voltage to key at, V.
        #[arg(long)]
        min_vin: Option<f32>,
        #[arg(long)]
        max_swr: Option<f32>,
    },
    /// The last lines of the TX audit log.
    Log {
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
}

fn parse_load(s: &str) -> Result<Load, Exit> {
    match s.to_ascii_lowercase().as_str() {
        "dummy" | "load" | "50" | "dummy-load" => Ok(Load::DummyLoad),
        "antenna" | "ant" => Ok(Load::Antenna),
        _ => Err(Exit::Radio(anyhow!("--load dummy or --load antenna"))),
    }
}

fn kind_of(s: &str) -> Result<TxKind, Exit> {
    match s {
        "tune" => Ok(TxKind::Tune),
        "key" | "paddle" => Ok(TxKind::Paddle),
        "mem" | "memory" => Ok(TxKind::Memory),
        _ => Err(Exit::Radio(anyhow!("tune, key or mem"))),
    }
}

/// The controller: the running S-CTRL's, or one for this command.
fn controller(env: &Env) -> TxController {
    match env.service() {
        Some(svc) => svc.tx(),
        None => TxController::new(sctrl_core::config::load().0.tx, "cli"),
    }
}

fn report_lines(r: &TxReport) -> Vec<String> {
    vec![
        format!(
            "{:?}: held {} ms of {} ms, {}",
            r.kind,
            r.held_ms,
            r.requested_ms,
            if r.transmitted {
                "the radio transmitted"
            } else {
                "no TRANSMIT seen"
            }
        ),
        format!(
            "PEP {:.2} W  SWR {}  VIN min {}",
            r.max_pep_w,
            r.max_swr.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            r.min_vin_v.map(|v| format!("{v:.2} V")).unwrap_or("-".into())
        ),
        format!("ended: {}", r.ended),
        if r.txevents.is_empty() {
            "radio TX events: none".into()
        } else {
            format!("radio TX events: {}", r.txevents.join(", "))
        },
    ]
}

async fn wait_status(l: &Link) -> Result<(), Exit> {
    let end = Instant::now() + Duration::from_secs(3);
    while l.snapshot().status.is_none() {
        if Instant::now() > end {
            return Err(Exit::Timeout(anyhow!("no STATUS from the radio")));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    Ok(())
}

pub async fn run(cli: &Cli, env: &mut Env, what: TxCmd) -> Result<(), Exit> {
    let ctl = controller(env);
    // Commands that need no radio.
    match &what {
        TxCmd::Policy {
            bench,
            tune_ms,
            key_ms,
            memory_s,
            duty_pct,
            arm_ttl_s,
            region,
            min_vin,
            max_swr,
        } => {
            let change = |p: &mut TxPolicy| -> Result<(), Exit> {
                if let Some(b) = bench {
                    p.bench = match b.as_str() {
                        "on" | "1" | "true" => true,
                        "off" | "0" | "false" => false,
                        _ => return Err(Exit::Radio(anyhow!("--bench on|off"))),
                    };
                }
                if let Some(v) = tune_ms {
                    p.tune_max_ms = *v;
                }
                if let Some(v) = key_ms {
                    p.key_max_ms = *v;
                }
                if let Some(v) = memory_s {
                    p.memory_max_s = *v;
                }
                if let Some(v) = duty_pct {
                    p.duty_pct = *v;
                }
                if let Some(v) = arm_ttl_s {
                    p.arm_ttl_s = *v;
                }
                if let Some(r) = region {
                    p.region = match r {
                        1 => Region::R1,
                        2 => Region::R2,
                        3 => Region::R3,
                        _ => return Err(Exit::Radio(anyhow!("--region 1, 2 or 3"))),
                    };
                }
                if let Some(v) = min_vin {
                    p.min_vin_mv = (v * 1000.0).round().clamp(0.0, 30_000.0) as u16;
                }
                if let Some(v) = max_swr {
                    p.max_swr = *v;
                }
                p.sanitize();
                Ok(())
            };
            let any = bench.is_some()
                || tune_ms.is_some()
                || key_ms.is_some()
                || memory_s.is_some()
                || duty_pct.is_some()
                || arm_ttl_s.is_some()
                || region.is_some()
                || min_vin.is_some()
                || max_swr.is_some();
            let policy = if any {
                let mut p = ctl.policy();
                change(&mut p)?;
                match env.service() {
                    Some(svc) => {
                        let p2 = p.clone();
                        svc.update_config(move |c| c.tx = p2)?;
                    }
                    None => {
                        let mut c = sctrl_core::config::load().0;
                        c.tx = p.clone();
                        sctrl_core::config::save(&c)?;
                    }
                }
                tx::audit("cli", "policy", json!(p));
                p
            } else {
                ctl.policy()
            };
            if cli.json {
                env.say(json!({"ok": true, "policy": policy}).to_string());
            } else {
                env.say(format!(
                    "TUNE {} ms, paddle key-down {} ms, memory {} s, duty {} % per {} s, ARM TTL {} s",
                    policy.key_ceiling_ms(TxKind::Tune),
                    policy.key_ceiling_ms(TxKind::Paddle),
                    policy.memory_max_s,
                    policy.duty_pct,
                    policy.duty_window_s,
                    policy.arm_ttl_s
                ));
                env.say(format!(
                    "bench {}  band plan {:?}  VIN >= {:.2} V  SWR <= {:.1}  (hard ceiling {} ms)",
                    if policy.bench { "ON (1 s)" } else { "off" },
                    policy.region,
                    policy.min_vin_mv as f32 / 1000.0,
                    policy.max_swr,
                    tx::ABS_KEY_CEIL_MS
                ));
            }
            return Ok(());
        }
        TxCmd::Log { n } => {
            let p = std::env::var_os("XDG_STATE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
                })
                .join("sctrl/tx.log");
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            for l in &lines[lines.len().saturating_sub(*n)..] {
                env.say(l.to_string());
            }
            return Ok(());
        }
        _ => {}
    }

    let dev = env.device(cli.serial.as_deref())?;
    let s = env.open(&dev).await?;
    let l = s.need_link()?;
    wait_status(l).await?;
    let cx: Context = match env.service() {
        Some(svc) => svc.context(cli.take_control),
        None => crate::cli::context(&dev, l.snapshot().status, true, cli.take_control).0,
    };
    let load = match &what {
        TxCmd::Check { load, .. }
        | TxCmd::Arm { load, .. }
        | TxCmd::Tune { load, .. }
        | TxCmd::Key { load, .. }
        | TxCmd::Mem { load, .. } => load.clone(),
        _ => None,
    };
    if let Some(ld) = load {
        ctl.set_load(parse_load(&ld)?);
    }
    let want_arm = matches!(
        what,
        TxCmd::Tune { arm: true, .. } | TxCmd::Key { arm: true, .. } | TxCmd::Mem { arm: true, .. }
    );
    if want_arm {
        ctl.arm(l, &cx, true).await.map_err(|e| Exit::Refused(denied(e)))?;
    }
    let r = match what {
        TxCmd::Check { kind, .. } => {
            let k = kind_of(&kind)?;
            let checks = tx::checklist(k, &ctl.policy(), &ctl.facts(l, &cx, 0));
            let bad: Vec<_> = tx::failed(&checks).into_iter().cloned().collect();
            if cli.json {
                env.say(json!({"ok": bad.is_empty(), "kind": k, "checks": checks}).to_string());
            } else {
                for c in &checks {
                    env.say(format!(
                        "{} {:<16} {}",
                        if c.ok { "ok  " } else { "FAIL" },
                        c.item,
                        c.detail
                    ));
                }
            }
            if bad.is_empty() {
                Ok(())
            } else {
                Err(Exit::Refused(denied(tx::Refused(bad).into())))
            }
        }
        TxCmd::Arm { off, .. } => match ctl.arm(l, &cx, !off).await {
            Ok(()) => {
                let left = l.snapshot().status.map(|s| s.arm_left_s).unwrap_or(0);
                env.say(if cli.json {
                    json!({"ok": true, "armed": !off, "left_s": left}).to_string()
                } else if off {
                    "disarmed".into()
                } else {
                    format!("armed ({left} s without TX)")
                });
                Ok(())
            }
            Err(e) => Err(Exit::Refused(denied(e))),
        },
        TxCmd::Stop => {
            ctl.stop(l).await?;
            env.say(if cli.json {
                json!({"ok": true}).to_string()
            } else {
                "stopped".into()
            });
            Ok(())
        }
        TxCmd::Tune { ms, .. } => done(cli, env, ctl.tune(l, &cx, ms).await),
        TxCmd::Key { ms, dah, both, .. } => done(cli, env, ctl.key(l, &cx, both || !dah, both || dah, ms).await),
        TxCmd::Mem { text, .. } => done(cli, env, ctl.memory(l, &cx, &text).await),
        TxCmd::Policy { .. } | TxCmd::Log { .. } => unreachable!(),
    };
    s.close().await;
    r
}

fn denied(e: anyhow::Error) -> sctrl_core::safety::Denied {
    sctrl_core::safety::Denied {
        rule: "TX",
        reason: format!("{e:#}"),
    }
}

fn done(cli: &Cli, env: &mut Env, r: anyhow::Result<TxReport>) -> Result<(), Exit> {
    match r {
        Ok(rep) => {
            if cli.json {
                env.say(json!({"ok": rep.ended == "done", "report": rep}).to_string());
            } else {
                for line in report_lines(&rep) {
                    env.say(line);
                }
            }
            if rep.ended == "done" {
                Ok(())
            } else {
                Err(Exit::Radio(anyhow!("{}", rep.ended)))
            }
        }
        Err(e) if e.downcast_ref::<tx::Refused>().is_some() => Err(Exit::Refused(denied(e))),
        Err(e) => Err(Exit::Radio(e)),
    }
}

/// Pages `goto` opens (link spec INPUT 14), by name.
pub const GOTO_PAGES: [(&str, u8); 8] = [
    ("home", 0),
    ("freq", 2),
    ("diag", 4),
    ("tx-test", 6),
    ("trainer", 7),
    ("sd", 8),
    ("pd", 9),
    ("waterfall", 15),
];

pub async fn goto(cli: &Cli, env: &mut Env, page: &str) -> Result<(), Exit> {
    let Some(&(_, id)) = GOTO_PAGES.iter().find(|(n, _)| *n == page) else {
        return Err(Exit::Radio(anyhow!(
            "goto {page}: {}",
            GOTO_PAGES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        )));
    };
    let dev = env.device(cli.serial.as_deref())?;
    let s = env.open(&dev).await?;
    let l = s.need_link()?;
    wait_status(l).await?;
    // TX TEST changes the radio's mode: the rules of a mode change apply.
    let action = if id == 6 { Action::Mode } else { Action::PanelInput };
    crate::cli::guard(cli, &dev, l, action).await?;
    let r = l.input(Input::OpenPage, id as i8).await;
    let mut ok = false;
    if r.is_ok() {
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end && !ok {
            ok = l.snapshot().status.is_some_and(|st| st.page == id);
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    s.close().await;
    r?;
    if !ok {
        return Err(Exit::Radio(anyhow!(
            "the radio did not open {} (busy with storage, or a page that keeps the panel)",
            PAGES[id as usize]
        )));
    }
    env.say(if cli.json {
        json!({"ok": true, "page": PAGES[id as usize]}).to_string()
    } else {
        PAGES[id as usize].to_string()
    });
    Ok(())
}
