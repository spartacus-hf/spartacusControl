//! The long-lived owner of the radio connection (GUI or `sctrl daemon`).
//!
//! A supervisor watches USB once a second. When a SPARTACUS in its audio
//! personality appears and its S-CTRL port is free, it opens the LINK; when
//! the LINK drops (unplug, suspend, a crash on the radio) it goes back to
//! watching. It never changes the radio's USB personality by itself and
//! never takes a port another program holds: those wait for the user.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{broadcast, watch};

use crate::device::{self, Personality, UsbDevice};
use crate::hostview::HostView;
use crate::link::{Link, LinkError, LinkEvent, LinkOptions};
use crate::session::port_holders_text;

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
    link: Mutex<Option<Arc<Link>>>,
    device: Mutex<Option<UsbDevice>>,
    host: Mutex<HostView>,
    state: watch::Sender<ConnState>,
    events: broadcast::Sender<LinkEvent>,
    serial: Option<String>,
    opts: LinkOptions,
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
        let svc = Self {
            inner: Arc::new(Inner {
                audio: Mutex::new(None),
                link: Mutex::new(None),
                device: Mutex::new(None),
                host: Mutex::new(HostView::default()),
                state,
                events,
                serial,
                opts,
            }),
        };
        let s = svc.clone();
        tokio::spawn(async move { s.supervise().await });
        svc
    }

    /// Start PipeWire handling (PC speaker, TX audio guard) for this radio.
    pub fn start_audio(&self) -> crate::audio::AudioService {
        let (a, b) = (self.clone(), self.clone());
        let audio = crate::audio::AudioService::start(
            move || a.link().and_then(|l| l.state.lock().unwrap().status.clone()),
            move || b.device().map(|d| d.serial),
        );
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
        match Link::connect(&port, self.inner.opts).await {
            Ok(link) => {
                let link = Arc::new(link);
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
