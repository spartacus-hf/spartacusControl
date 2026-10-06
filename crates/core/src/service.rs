//! The long-lived owner of the radio connection (GUI or `sctrl daemon`).
//!
//! A supervisor watches USB once a second. When a SPARTACUS in its audio
//! personality appears and its S-CTRL port is free, it opens the LINK; when
//! the LINK drops (unplug, suspend, a crash on the radio) it goes back to
//! watching. It never changes the radio's USB personality by itself and
//! never takes a port another program holds: those wait for the user.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, watch};

use crate::device::{self, Personality, UsbDevice};
use crate::hostview::HostView;
use crate::link::{Link, LinkError, LinkEvent, LinkOptions};
use crate::session::port_holders_text;
use sctrl_proto::frame::sub;

/// The channels that cost the radio work per screen change: streamed only
/// while someone looks (the window shown, a `shot` being taken).
const SCREEN_CHANNELS: u16 = sub::SCREEN | sub::SPECTRUM | sub::SPECTRUM_TRACE;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    /// No SPARTACUS on USB.
    Absent {
        others: Vec<String>,
    },
    /// Found, LINK being opened.
    Connecting {
        serial: String,
    },
    Linked {
        serial: String,
        port: String,
    },
    /// Its port is held by another program.
    PortBusy {
        serial: String,
        who: String,
    },
    /// DIAG personality: console only, the user decides to leave it.
    Diag {
        serial: String,
    },
    /// USB DISK MODE.
    Disk {
        serial: String,
    },
    /// Firmware without the LINK, or the LINK refused.
    NoLink {
        serial: String,
        why: String,
    },
}

impl ConnState {
    pub fn summary(&self) -> String {
        match self {
            ConnState::Absent { others } if others.is_empty() => "no SPARTACUS on USB".into(),
            ConnState::Absent { others } => format!("no SPARTACUS on USB ({})", others.join("; ")),
            ConnState::Connecting { .. } => "connecting...".into(),
            ConnState::Linked { .. } => "LINK up".into(),
            ConnState::PortBusy { who, .. } => format!("S-CTRL's port is held by {who}"),
            ConnState::Diag { .. } => "DIAG console personality (no sound card, no LINK)".into(),
            ConnState::Disk { .. } => "USB DISK MODE".into(),
            ConnState::NoLink { why, .. } => why.clone(),
        }
    }
}

struct Inner {
    audio: Mutex<Option<crate::audio::AudioService>>,
    tx: crate::tx::TxController,
    config: Mutex<crate::config::Config>,
    link: Mutex<Option<Arc<Link>>>,
    device: Mutex<Option<UsbDevice>>,
    host: Mutex<HostView>,
    state: watch::Sender<ConnState>,
    events: broadcast::Sender<LinkEvent>,
    serial: Option<String>,
    opts: LinkOptions,
    /// The window shows the radio's screen.
    screen_shown: AtomicBool,
    /// `shot`s (and the like) waiting for a fresh screen.
    screen_leases: AtomicUsize,
}

/// Keeps the screen streaming while held (`Service::screen_lease`).
pub struct ScreenLease(Service);

impl Drop for ScreenLease {
    fn drop(&mut self) {
        self.0.inner.screen_leases.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl Service {
    /// Start the supervisor on the current tokio runtime.
    pub fn start(serial: Option<String>, opts: LinkOptions) -> Self {
        let (state, _) = watch::channel(ConnState::Absent { others: Vec::new() });
        let (events, _) = broadcast::channel(8192);
        let (config, err) = crate::config::load();
        if let Some(e) = err {
            tracing::warn!("{e}");
        }
        let svc = Self {
            inner: Arc::new(Inner {
                audio: Mutex::new(None),
                tx: crate::tx::TxController::new(config.tx.clone(), "s-ctrl"),
                config: Mutex::new(config),
                link: Mutex::new(None),
                device: Mutex::new(None),
                host: Mutex::new(HostView::default()),
                state,
                events,
                serial,
                opts,
                screen_shown: AtomicBool::new(false),
                screen_leases: AtomicUsize::new(0),
            }),
        };
        let s = svc.clone();
        tokio::spawn(async move { s.supervise().await });
        let s = svc.clone();
        tokio::spawn(async move { s.keep_in_step().await });
        svc
    }

    /// What the radio should stream now: the screen and the spectrum only
    /// while the window shows them or a lease is held.
    pub fn wanted(&self) -> LinkOptions {
        let o = self.inner.opts;
        let look =
            self.inner.screen_shown.load(Ordering::Relaxed) || self.inner.screen_leases.load(Ordering::Relaxed) > 0;
        LinkOptions {
            sub_mask: if look {
                o.sub_mask
            } else {
                o.sub_mask & !SCREEN_CHANNELS
            },
            ..o
        }
    }

    /// The window shows (true) or hides (shaded, minimised) the screen.
    pub fn show_screen(&self, on: bool) {
        if self.inner.screen_shown.swap(on, Ordering::Relaxed) != on
            && let Some(l) = self.link()
        {
            let mask = self.wanted().sub_mask;
            tokio::spawn(async move {
                if let Err(e) = l.set_sub_mask(mask).await {
                    tracing::warn!("SUB: {e:#}");
                }
            });
        }
    }

    /// Stream the screen while the lease lives; returns once the radio has
    /// been asked (its pages count as unknown until they arrive).
    pub async fn screen_lease(&self) -> anyhow::Result<ScreenLease> {
        self.inner.screen_leases.fetch_add(1, Ordering::Relaxed);
        let lease = ScreenLease(self.clone());
        if let Some(l) = self.link() {
            l.set_sub_mask(self.wanted().sub_mask).await?;
        }
        Ok(lease)
    }

    /// S-CTRL's transmissions (shared by the window and `sctrl tx`).
    pub fn tx(&self) -> crate::tx::TxController {
        self.inner.tx.clone()
    }

    pub fn config(&self) -> crate::config::Config {
        self.inner.config.lock().unwrap().clone()
    }

    /// Change S-CTRL's settings, apply them, and save them.
    pub fn update_config(&self, f: impl FnOnce(&mut crate::config::Config)) -> anyhow::Result<std::path::PathBuf> {
        let c = {
            let mut c = self.inner.config.lock().unwrap();
            f(&mut c);
            c.tx.sanitize();
            c.clone()
        };
        self.inner.tx.set_policy(c.tx.clone());
        if let Some(a) = self.audio() {
            a.set_auto_protect(c.pc_speaker.tx_guard);
        }
        crate::config::save(&c)
    }

    /// The safety arbiter's view now (took_control: the user's choice).
    pub fn context(&self, took_control: bool) -> crate::safety::Context {
        let link = self.link();
        let status = link.as_ref().and_then(|l| l.snapshot().status);
        let host = self.host();
        let cat0 = status.as_ref().map(|s| s.cat0_age_ds).filter(|&a| a != 0xFFFF);
        crate::safety::Context {
            cat_program: host.cat_program(cat0),
            iq_consumer: host.iq_consumer(),
            took_control,
            armed: status.as_ref().is_some_and(|s| s.armed()),
            link_live: link.as_ref().is_some_and(|l| l.closed().is_none()),
            status,
            ..Default::default()
        }
    }

    /// Four times a second: what S-CTRL holds on the radio follows what it
    /// should. The sleep inhibitor follows the radio's ARM; a CAT program
    /// (WSJT-X) appearing ends S-CTRL's ARM; the radio's monitor mix (its
    /// headphone audio, sidetone and trainer included, into the USB capture)
    /// is on while the PC speaker plays a mode other than DIGI and no other
    /// program records the radio.
    async fn keep_in_step(&self) {
        let mut monitor_sent: Option<(Instant, bool, bool)> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(link) = self.link() else {
                self.inner.tx.inhibit(false);
                monitor_sent = None;
                continue;
            };
            // A lease that ended, or a SUB that failed: the channels follow.
            let mask = self.wanted().sub_mask;
            if link.sub_mask() != mask
                && let Err(e) = link.set_sub_mask(mask).await
            {
                tracing::warn!("SUB: {e:#}");
            }
            let Some(st) = link.snapshot().status else { continue };
            if !st.armed() {
                self.inner.tx.inhibit(false);
            } else {
                let cx = self.context(false);
                if let Some(p) = &cx.cat_program {
                    tracing::warn!("{p} uses the radio: S-CTRL disarms");
                    crate::tx::audit(
                        "s-ctrl",
                        "disarm",
                        serde_json::json!({"why": format!("{p} uses the radio")}),
                    );
                    let _ = self.inner.tx.arm(&link, &cx, false).await;
                }
            }
            if let Some(audio) = self.audio() {
                let phones_off = self.config().pc_speaker.radio_phones_off;
                // Only while the loopback really plays and nothing else
                // records the radio: the mix replaces the receive audio in
                // the capture for every program reading it.
                let on = audio.status().monitor_ok && st.mode != 5 && !st.usb_iq() && !st.diag_personality();
                let phones = on && phones_off;
                let differs = st.monitor_mix() != on || st.phones_off() != phones;
                let recently = monitor_sent
                    .is_some_and(|(t, o, p)| t.elapsed() < Duration::from_secs(2) && o == on && p == phones);
                if differs && !recently && !st.realtime_critical() {
                    monitor_sent = Some((Instant::now(), on, phones));
                    if let Err(e) = link.request_ok(sctrl_proto::payload::monitor(on, phones, 128)).await {
                        tracing::warn!("MONITOR: {e:#}");
                    }
                }
            }
        }
    }

    /// Start PipeWire handling (PC speaker, TX audio guard) for this radio.
    pub fn start_audio(&self) -> crate::audio::AudioService {
        let (a, b) = (self.clone(), self.clone());
        let audio = crate::audio::AudioService::start(
            move || a.link().and_then(|l| l.state.lock().unwrap().status.clone()),
            move || b.device().map(|d| d.serial),
        );
        let c = self.config().pc_speaker;
        let mut w = audio.wish();
        w.volume = c.volume.clamp(0.0, 1.5);
        w.pinned = c.pinned.clone();
        audio.set_wish(w);
        audio.set_auto_protect(c.tx_guard);
        *self.inner.audio.lock().unwrap() = Some(audio.clone());
        audio
    }

    pub fn audio(&self) -> Option<crate::audio::AudioService> {
        self.inner.audio.lock().unwrap().clone()
    }

    pub fn link(&self) -> Option<Arc<Link>> {
        self.inner.link.lock().unwrap().clone()
    }

    pub fn device(&self) -> Option<UsbDevice> {
        self.inner.device.lock().unwrap().clone()
    }

    pub fn host(&self) -> HostView {
        self.inner.host.lock().unwrap().clone()
    }

    pub fn state(&self) -> ConnState {
        self.inner.state.borrow().clone()
    }

    pub fn watch_state(&self) -> watch::Receiver<ConnState> {
        self.inner.state.subscribe()
    }

    /// Everything every LINK pushes, across reconnects.
    pub fn subscribe(&self) -> broadcast::Receiver<LinkEvent> {
        self.inner.events.subscribe()
    }

    fn set_state(&self, s: ConnState) {
        self.inner.state.send_if_modified(|cur| {
            if *cur != s {
                *cur = s;
                true
            } else {
                false
            }
        });
    }

    async fn supervise(&self) {
        let mut host_tick = 0u32;
        loop {
            if host_tick.is_multiple_of(3) {
                let dev = self.device();
                let hv = tokio::task::spawn_blocking(move || HostView::gather(dev.as_ref()))
                    .await
                    .unwrap_or_default();
                *self.inner.host.lock().unwrap() = hv;
            }
            host_tick = host_tick.wrapping_add(1);
            if let Some(l) = self.link() {
                if let Some(why) = l.closed() {
                    tracing::info!("LINK closed: {why}");
                    *self.inner.link.lock().unwrap() = None;
                    let _ = self.inner.events.send(LinkEvent::Closed(why));
                } else {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            }
            self.try_connect().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn try_connect(&self) {
        let found = tokio::task::spawn_blocking({
            let serial = self.inner.serial.clone();
            move || (device::find_spartacus(serial.as_deref()), device::scan())
        })
        .await
        .unwrap_or((None, Vec::new()));
        let (dev, all) = found;
        *self.inner.device.lock().unwrap() = dev.clone();
        let Some(dev) = dev else {
            let others = all
                .iter()
                .filter(|d| !matches!(d.kind, device::Kind::Other))
                .map(UsbDevice::label)
                .collect();
            self.set_state(ConnState::Absent { others });
            return;
        };
        let serial = dev.serial.clone();
        match dev.personality() {
            Some(Personality::Diag) => return self.set_state(ConnState::Diag { serial }),
            Some(Personality::Disk) => return self.set_state(ConnState::Disk { serial }),
            _ => {}
        }
        let Some(port) = dev.link_port() else {
            return self.set_state(ConnState::Connecting { serial });
        };
        if let Some(who) = port_holders_text(&port) {
            return self.set_state(ConnState::PortBusy { serial, who });
        }
        // NoLink is sticky for this device: do not hammer old firmware.
        if let ConnState::NoLink { serial: s, .. } = self.state()
            && s == serial
        {
            return;
        }
        self.set_state(ConnState::Connecting { serial: serial.clone() });
        match Link::connect(&port, self.wanted()).await {
            Ok(link) => {
                let link = Arc::new(link);
                link.spawn_event_reader();
                let mut rx = link.subscribe();
                let fan = self.inner.events.clone();
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let _ = fan.send(ev);
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return,
                        }
                    }
                });
                *self.inner.link.lock().unwrap() = Some(link);
                self.set_state(ConnState::Linked {
                    serial,
                    port: port.display().to_string(),
                });
            }
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!("LINK: {why}");
                match e.downcast_ref::<LinkError>() {
                    Some(LinkError::NoLink(_)) | Some(LinkError::Refused(_)) => {
                        self.set_state(ConnState::NoLink { serial, why })
                    }
                    _ => self.set_state(ConnState::Connecting { serial }),
                }
            }
        }
    }
}
