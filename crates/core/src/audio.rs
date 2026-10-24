//! PipeWire: PC speaker mode and the guard on the radio's TX audio input.
//!
//! - The graph comes from `pw-dump --monitor` (restarted with backoff if
//!   PipeWire restarts). The radio's nodes are found by their device's
//!   properties (vendor 0x0483, product 0x5740, serial), never by node name:
//!   names gain `.2`, `.3` after a re-plug.
//! - PC speaker: a supervised `pw-loopback` from the radio's capture to the
//!   default output (or a pinned one). Not run in DIGI (unless asked), muted
//!   during voice TX, unavailable while the capture carries raw I/Q. It
//!   leaves the rest of the PC's audio alone: no media role (role policies
//!   would duck or pause other programs), a moderate latency (the graph
//!   follows the smallest one asked for), and both ends pinned so that
//!   nothing moves them -- the capture never onto a microphone (EasyEffects
//!   moves input streams), the playback never into the radio's own sink;
//!   when the output changes S-CTRL restarts it there. The child gets
//!   PR_SET_PDEATHSIG, so it cannot outlive S-CTRL.
//! - The radio's monitor mix (sidetone in the capture, STATUS flags0 bit 6)
//!   is asked for only while S-CTRL's loopback is the radio's only
//!   recorder: fldigi or a recording keep the receive audio as it is.
//! - TX guard: the radio's playback sink is its transmit audio (with USB
//!   VOX on, anything played there transmits). A stream from a program that
//!   is not a digital-mode program is moved off it at once, and the radio's
//!   sink is never left the default output.
//! - Nothing is configured on the system: no WirePlumber rule, no files.
//!   One trade-off of that: the radio's capture outranks the PC's output as
//!   the graph's clock, so when the PC speaker starts, sound already playing
//!   on that output may click once as it follows the radio.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, watch};

use sctrl_proto::payload::Status;

// ---- the graph ----

#[derive(Debug, Clone, Default, Serialize)]
pub struct PwNode {
    pub id: u32,
    pub serial: u64,
    pub name: String,
    pub class: String,
    pub description: String,
    pub device_id: Option<u32>,
    pub client_id: Option<u32>,
    pub props: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PwLink {
    pub id: u32,
    pub out_node: u32,
    pub in_node: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Graph {
    pub nodes: HashMap<u32, PwNode>,
    pub devices: HashMap<u32, HashMap<String, Value>>,
    pub clients: HashMap<u32, HashMap<String, Value>>,
    pub links: HashMap<u32, PwLink>,
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}

fn s(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn u(v: Option<&Value>) -> Option<u64> {
    match v {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

impl Graph {
    /// One object of `pw-dump` output (an update or a removal).
    pub fn apply(&mut self, o: &Value) {
        let Some(id) = o.get("id").and_then(Value::as_u64).map(|i| i as u32) else {
            return;
        };
        let info = o.get("info");
        if info.is_none_or(Value::is_null) && o.get("metadata").is_none() {
            self.nodes.remove(&id);
            self.devices.remove(&id);
            self.clients.remove(&id);
            self.links.remove(&id);
            return;
        }
        let ty = o.get("type").and_then(Value::as_str).unwrap_or("");
        let props: HashMap<String, Value> = info
            .and_then(|i| i.get("props"))
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        match ty {
            "PipeWire:Interface:Node" => {
                let n = PwNode {
                    id,
                    serial: u(props.get("object.serial")).unwrap_or(0),
                    name: s(props.get("node.name")),
                    class: s(props.get("media.class")),
                    description: s(props.get("node.description")),
                    device_id: u(props.get("device.id")).map(|v| v as u32),
                    client_id: u(props.get("client.id")).map(|v| v as u32),
                    props,
                };
                self.nodes.insert(id, n);
            }
            "PipeWire:Interface:Device" => {
                self.devices.insert(id, props);
            }
            "PipeWire:Interface:Client" => {
                self.clients.insert(id, props);
            }
            "PipeWire:Interface:Link" => {
                let i = info.unwrap();
                let out_node = i.get("output-node-id").and_then(Value::as_u64).unwrap_or(0) as u32;
                let in_node = i.get("input-node-id").and_then(Value::as_u64).unwrap_or(0) as u32;
                self.links.insert(id, PwLink { id, out_node, in_node });
            }
            "PipeWire:Interface:Metadata" => {
                // Metadata objects carry their props at the top level, no `info`.
                let name = o
                    .get("props")
                    .and_then(|p| p.get("metadata.name"))
                    .and_then(Value::as_str);
                if name.or(props.get("metadata.name").and_then(Value::as_str)) != Some("default") {
                    return;
                }
                for m in o.get("metadata").and_then(Value::as_array).into_iter().flatten() {
                    let key = m.get("key").and_then(Value::as_str).unwrap_or("");
                    let name = m
                        .get("value")
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    match key {
                        "default.audio.sink" => self.default_sink = name,
                        "default.audio.source" => self.default_source = name,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// The radio's PipeWire device: the whole descriptor, as device.rs.
    pub fn radio_device(&self, serial: &str) -> Option<u32> {
        let want = format!("Chireix_SPARTACUS_REV1_{serial}");
        self.devices.iter().find_map(|(id, p)| {
            (s(p.get("device.vendor.id")) == "0x0483"
                && s(p.get("device.product.id")) == "0x5740"
                && s(p.get("device.serial")) == want)
                .then_some(*id)
        })
    }

    fn radio_node(&self, serial: &str, class: &str) -> Option<&PwNode> {
        let dev = self.radio_device(serial)?;
        self.nodes
            .values()
            .find(|n| n.device_id == Some(dev) && n.class == class)
    }

    pub fn radio_source(&self, serial: &str) -> Option<&PwNode> {
        self.radio_node(serial, "Audio/Source")
    }

    pub fn radio_sink(&self, serial: &str) -> Option<&PwNode> {
        self.radio_node(serial, "Audio/Sink")
    }

    pub fn node_named(&self, name: &str) -> Option<&PwNode> {
        self.nodes.values().find(|n| n.name == name)
    }

    /// Hardware or virtual sinks that are not any SPARTACUS.
    pub fn other_sinks(&self) -> Vec<&PwNode> {
        let mut v: Vec<&PwNode> = self
            .nodes
            .values()
            .filter(|n| n.class == "Audio/Sink" && !n.name.contains("Chireix_SPARTACUS"))
            .collect();
        v.sort_by_key(|n| n.id);
        v
    }

    /// Where a stream with no target plays: the default sink, unless that is
    /// a radio; then the first other sink.
    pub fn safe_output(&self) -> Option<&PwNode> {
        let def = self.default_sink.as_deref().and_then(|n| self.node_named(n));
        match def {
            Some(n) if !n.name.contains("Chireix_SPARTACUS") => Some(n),
            _ => self.other_sinks().into_iter().next(),
        }
    }

    pub fn client_binary(&self, node: &PwNode) -> String {
        let from_node = s(node.props.get("application.process.binary"));
        if !from_node.is_empty() {
            return from_node;
        }
        node.client_id
            .and_then(|c| self.clients.get(&c))
            .map(|p| s(p.get("application.process.binary")))
            .unwrap_or_default()
    }

    /// Programs other than S-CTRL's loopback recording from node `id`;
    /// level meters (pavucontrol's peak streams) aside.
    pub fn recorders(&self, id: u32) -> Vec<String> {
        let ours = format!("input.{LOOPBACK}");
        let mut v: Vec<String> = self
            .links
            .values()
            .filter(|l| l.out_node == id)
            .filter_map(|l| self.nodes.get(&l.in_node))
            .filter(|n| n.class.starts_with("Stream/Input") && n.name != ours)
            .filter(|n| {
                let monitor = n.props.get("stream.monitor");
                !(matches!(monitor, Some(Value::Bool(true))) || s(monitor) == "true")
                    && s(n.props.get("media.name")) != "Peak detect"
            })
            .map(|n| {
                let b = self.client_binary(n);
                if b.is_empty() { n.name.clone() } else { b }
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Streams linked into node `id`.
    pub fn feeding(&self, id: u32) -> Vec<(&PwLink, &PwNode)> {
        self.links
            .values()
            .filter(|l| l.in_node == id)
            .filter_map(|l| self.nodes.get(&l.out_node).map(|n| (l, n)))
            .collect()
    }
}

// ---- PC speaker policy (pure) ----

#[derive(Debug, Clone, Serialize)]
pub struct PcWish {
    pub enabled: bool,
    /// Play DIGI too (off by default: the user's choice).
    pub digi: bool,
    /// 0.0 ..= 1.5
    pub volume: f32,
    /// Output sink node.name; None follows the default.
    pub pinned: Option<String>,
}

impl Default for PcWish {
    fn default() -> Self {
        Self {
            enabled: false,
            digi: false,
            volume: 0.8,
            pinned: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PcPlan {
    Off,
    Unavailable { reason: String },
    Play { latency: u32, muted: Option<String> },
}

/// The radio's CW trainer page (ui_page_t).
const PAGE_CW_TRAINER: u8 = 7;

/// Modes: 0 USB, 1 LSB, 2 CW, 3 AM, 4 FM, 5 DIGI.
pub fn pc_plan(wish: &PcWish, status: Option<&Status>, have_source: bool, pipewire: bool) -> PcPlan {
    if !wish.enabled {
        return PcPlan::Off;
    }
    if !pipewire {
        return PcPlan::Unavailable {
            reason: "PipeWire is not running".into(),
        };
    }
    if !have_source {
        return PcPlan::Unavailable {
            reason: "the radio's sound card is not in PipeWire".into(),
        };
    }
    let Some(s) = status else {
        return PcPlan::Unavailable {
            reason: "waiting for the radio".into(),
        };
    };
    if s.diag_personality() {
        return PcPlan::Unavailable {
            reason: "DIAG personality: no sound card".into(),
        };
    }
    if s.usb_iq() {
        return PcPlan::Unavailable {
            reason: "USB RAW IQ is on: the capture carries I/Q, not audio".into(),
        };
    }
    // CW and the trainer: low latency for the sidetone (2.7 ms quanta);
    // voice: 10.7 ms. The PC's whole graph runs at the smallest latency any
    // stream asks for, so less would cost every other program wake-ups.
    let latency = if s.mode == 2 || s.page == PAGE_CW_TRAINER {
        128
    } else {
        512
    };
    let muted = if s.mode == 5 && !wish.digi {
        Some("DIGI: off unless 'play DIGI' is on".to_string())
    } else if s.transmitting() && matches!(s.mode, 0 | 1 | 3 | 4) {
        Some("voice TX: kept quiet (no feedback into the radio's microphone)".to_string())
    } else {
        None
    };
    PcPlan::Play { latency, muted }
}

// ---- TX guard policy (pure) ----

/// Programs allowed to play into the radio's TX audio.
pub const TX_AUDIO_PROGRAMS: [&str; 6] = ["wsjtx", "jtdx", "js8call", "fldigi", "wsjtx-improved", "ft8call"];

pub fn tx_audio_allowed(binary: &str) -> bool {
    let b = binary.to_ascii_lowercase();
    TX_AUDIO_PROGRAMS
        .iter()
        .any(|p| b == *p || b.starts_with(&format!("{p}-")) || b.starts_with(&format!("{p}.")))
}

// ---- the service ----

#[derive(Debug, Clone, Serialize, Default)]
pub struct AudioStatus {
    pub pipewire: bool,
    pub radio_source: Option<String>,
    pub radio_sink: Option<String>,
    pub output: Option<String>,
    pub plan: Option<PcPlan>,
    pub volume: f32,
    pub notes: Vec<String>,
    /// The loopback plays and is the radio's only recorder: the radio may mix
    /// its headphone audio (sidetone) into the capture (MONITOR).
    pub monitor_ok: bool,
    /// Other programs recording from the radio.
    pub recorders: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub enum AudioEvent {
    /// A stream was moved off the radio's TX audio input.
    Guarded {
        program: String,
        stream: String,
    },
    /// The radio's sink was the default output; it no longer is.
    DefaultMoved {
        to: String,
    },
    Note(String),
}

struct Inner {
    graph: Mutex<Graph>,
    pipewire: Mutex<bool>,
    changed: watch::Sender<u64>,
    wish: Mutex<PcWish>,
    status: Mutex<AudioStatus>,
    events: broadcast::Sender<AudioEvent>,
    auto_protect: Mutex<bool>,
}

#[derive(Clone)]
pub struct AudioService {
    inner: Arc<Inner>,
}

const LOOPBACK: &str = "sctrl-pc-speaker";

/// Child processes die with S-CTRL (SIGTERM on the parent thread's exit).
fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.kill_on_drop(true).stdin(Stdio::null());
    // SAFETY: prctl is async-signal-safe; no allocation between fork and exec.
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    c
}

async fn run_quiet(program: &str, args: &[&str]) -> bool {
    command(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

impl AudioService {
    /// Start watching PipeWire. `status` gives the radio's latest STATUS and
    /// `serial` its USB serial, both read each time the plan is made.
    pub fn start<S, R>(status: S, serial: R) -> Self
    where
        S: Fn() -> Option<Status> + Send + Sync + 'static,
        R: Fn() -> Option<String> + Send + Sync + 'static,
    {
        let (changed, _) = watch::channel(0);
        let (events, _) = broadcast::channel(64);
        let me = Self {
            inner: Arc::new(Inner {
                graph: Mutex::new(Graph::default()),
                pipewire: Mutex::new(false),
                changed,
                wish: Mutex::new(PcWish::default()),
                status: Mutex::new(AudioStatus::default()),
                events,
                auto_protect: Mutex::new(true),
            }),
        };
        let w = me.clone();
        tokio::spawn(async move { w.watch_graph().await });
        let c = me.clone();
        tokio::spawn(async move { c.control(status, serial).await });
        me
    }

    pub fn wish(&self) -> PcWish {
        self.inner.wish.lock().unwrap().clone()
    }

    pub fn set_wish(&self, w: PcWish) {
        *self.inner.wish.lock().unwrap() = w;
        self.inner.changed.send_modify(|n| *n += 1);
    }

    pub fn status(&self) -> AudioStatus {
        self.inner.status.lock().unwrap().clone()
    }

    pub fn graph(&self) -> Graph {
        self.inner.graph.lock().unwrap().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.inner.events.subscribe()
    }

    pub fn set_auto_protect(&self, on: bool) {
        *self.inner.auto_protect.lock().unwrap() = on;
    }

    async fn watch_graph(&self) {
        let mut backoff = Duration::from_millis(500);
        loop {
            match self.dump_monitor().await {
                Ok(()) => tracing::info!("pw-dump ended"),
                Err(e) => tracing::warn!("pw-dump: {e:#}"),
            }
            *self.inner.pipewire.lock().unwrap() = false;
            *self.inner.graph.lock().unwrap() = Graph::default();
            self.inner.changed.send_modify(|n| *n += 1);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    /// `pw-dump --monitor`: a stream of JSON arrays, each a batch of changes.
    async fn dump_monitor(&self) -> Result<()> {
        let mut child = command("pw-dump")
            .args(["--monitor", "--no-colors"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("pw-dump")?;
        let mut out = child.stdout.take().context("pw-dump stdout")?;
        let mut buf = Vec::<u8>::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            let n = out.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            // Take every complete top-level array.
            let mut consumed = 0;
            {
                let mut it = serde_json::Deserializer::from_slice(&buf).into_iter::<Value>();
                while let Some(Ok(v)) = it.next() {
                    consumed = it.byte_offset();
                    if let Value::Array(objs) = v {
                        let mut g = self.inner.graph.lock().unwrap();
                        for o in &objs {
                            g.apply(o);
                        }
                    }
                }
            }
            if consumed > 0 {
                buf.drain(..consumed);
                *self.inner.pipewire.lock().unwrap() = true;
                self.inner.changed.send_modify(|n| *n += 1);
            }
        }
    }

    async fn control<S, R>(&self, status: S, serial: R)
    where
        S: Fn() -> Option<Status> + Send + Sync + 'static,
        R: Fn() -> Option<String> + Send + Sync + 'static,
    {
        let mut changed = self.inner.changed.subscribe();
        let mut loopback: Option<(Child, String)> = None; // child and the key it was started for
        let mut applied: Option<(bool, u32)> = None; // (muted, volume %) on the current playback node
        loop {
            let _ = tokio::time::timeout(Duration::from_millis(200), changed.changed()).await;
            let st = status();
            let serial = serial();
            let graph = self.graph();
            let pipewire = *self.inner.pipewire.lock().unwrap();
            let wish = self.wish();
            let source = serial.as_deref().and_then(|s| graph.radio_source(s)).cloned();
            let sink = serial.as_deref().and_then(|s| graph.radio_sink(s)).cloned();
            let plan = pc_plan(&wish, st.as_ref(), source.is_some(), pipewire);
            let mut notes = Vec::new();

            // The TX guard, first.
            if *self.inner.auto_protect.lock().unwrap()
                && let Some(sink) = &sink
            {
                self.guard(&graph, sink).await;
            }

            // Where the PC speaker plays.
            let output = match &wish.pinned {
                Some(name) if !name.contains("Chireix_SPARTACUS") => graph.node_named(name).cloned(),
                _ => graph.safe_output().cloned(),
            };
            if let Some(name) = &wish.pinned
                && output.is_none()
                && wish.enabled
            {
                notes.push(format!(
                    "the chosen output {name} is not there: pick another, or follow the default"
                ));
            }
            if let Some(o) = &output {
                let n = o.name.to_ascii_lowercase();
                if n.starts_with("bluez") {
                    notes.push("Bluetooth output: 150-250 ms of delay, not for CW".into());
                }
                if n.contains("easyeffects") {
                    notes.push(
                        "EasyEffects is the default output: its effects add delay (pin a hardware output for CW)"
                            .into(),
                    );
                }
            }
            // DIGI, not asked for: no stream at all (a muted one would still
            // hold the PC's graph at its latency).
            let digi_off = st.as_ref().is_some_and(|s| s.mode == 5) && !wish.digi;
            // Always pinned to the output chosen here, never following the
            // default by itself: a default that became the radio's sink would
            // otherwise take the radio's audio into its own TX input.
            let want = match (&plan, &source, &output) {
                (PcPlan::Play { latency, .. }, Some(src), Some(out)) if !digi_off => {
                    Some(format!("{}|{}|{}", src.serial, latency, out.name))
                }
                _ => None,
            };
            // (Re)start or stop the loopback.
            let alive = match &mut loopback {
                Some((c, _)) => c.try_wait().map(|r| r.is_none()).unwrap_or(false),
                None => false,
            };
            let current = loopback.as_ref().map(|(_, k)| k.clone());
            if want != current || (want.is_some() && !alive) {
                if let Some((mut c, _)) = loopback.take() {
                    let _ = c.kill().await;
                }
                applied = None;
                if let Some(key) = want.clone() {
                    let mut parts = key.splitn(3, '|');
                    let src = parts.next().unwrap_or_default().to_string();
                    let lat = parts.next().unwrap_or("256").to_string();
                    let pin = parts.next().unwrap_or_default().to_string();
                    // Both ends stay where they are put: no fallback when the
                    // target goes (S-CTRL restarts it), no moves by policy
                    // or by EasyEffects. No media.role: role policies would
                    // duck or pause the PC's other audio for it.
                    let pinned = "node.dont-reconnect = true node.dont-fallback = true node.dont-move = true";
                    let cap = format!(
                        "{{ node.description = \"S-CTRL PC speaker (from the radio)\" media.name = \"radio audio\" node.latency = \"{lat}/48000\" stream.dont-remix = true {pinned} }}"
                    );
                    let play = format!(
                        "{{ node.description = \"S-CTRL PC speaker\" media.name = \"radio audio\" node.latency = \"{lat}/48000\" {pinned} }}"
                    );
                    let mut c = command("pw-loopback");
                    c.args(["-n", LOOPBACK, "-C", &src, "-i", &cap, "-o", &play, "-P", &pin]);
                    match c.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                        Ok(child) => loopback = Some((child, key)),
                        Err(e) => notes.push(format!("pw-loopback: {e}")),
                    }
                }
            }
            // Mute and volume on our playback node, once it exists.
            if let PcPlan::Play { muted, .. } = &plan {
                let node = graph.node_named(&format!("output.{LOOPBACK}")).map(|n| n.id);
                let target = (muted.is_some(), (wish.volume.clamp(0.0, 1.5) * 100.0).round() as u32);
                if let Some(id) = node
                    && applied != Some(target)
                {
                    let ids = id.to_string();
                    let vol = format!("{:.2}", target.1 as f32 / 100.0);
                    let ok = run_quiet("wpctl", &["set-mute", &ids, if target.0 { "1" } else { "0" }]).await
                        && run_quiet("wpctl", &["set-volume", &ids, &vol]).await;
                    if ok {
                        applied = Some(target);
                    }
                }
                if let Some(m) = muted {
                    notes.push(m.clone());
                }
            }
            let playing = loopback.is_some() && graph.node_named(&format!("output.{LOOPBACK}")).is_some();
            let recorders = source.as_ref().map(|n| graph.recorders(n.id)).unwrap_or_default();
            if playing && !recorders.is_empty() {
                notes.push(format!(
                    "no sidetone on the PC speaker while {} records the radio",
                    recorders.join(", ")
                ));
            }
            let mut stat = self.inner.status.lock().unwrap();
            *stat = AudioStatus {
                pipewire,
                radio_source: source.map(|n| n.name),
                radio_sink: sink.map(|n| n.name),
                output: output.map(|n| {
                    if n.description.is_empty() {
                        n.name
                    } else {
                        n.description
                    }
                }),
                plan: Some(plan),
                volume: wish.volume,
                notes,
                monitor_ok: playing && recorders.is_empty(),
                recorders,
            };
        }
    }

    /// Move anything that is not a digital-mode program off the radio's TX
    /// audio, and keep the radio's sink from being the default output.
    async fn guard(&self, graph: &Graph, sink: &PwNode) {
        let safe = graph.other_sinks().into_iter().next().map(|n| n.name.clone());
        for (_link, stream) in graph.feeding(sink.id) {
            if !stream.class.starts_with("Stream/Output") {
                continue;
            }
            let bin = graph.client_binary(stream);
            if tx_audio_allowed(&bin) {
                continue;
            }
            let program = if bin.is_empty() { stream.name.clone() } else { bin };
            if let Some(to) = &safe {
                let id = stream.id.to_string();
                let moved = run_quiet("pw-metadata", &["-n", "default", &id, "target.object", to]).await;
                if !moved {
                    let _ = run_quiet("wpctl", &["set-mute", &id, "1"]).await;
                }
            }
            let _ = self.inner.events.send(AudioEvent::Guarded {
                program,
                stream: stream.description.clone(),
            });
        }
        if graph.default_sink.as_deref() == Some(sink.name.as_str())
            && let Some(to) = graph.other_sinks().into_iter().next()
            && run_quiet("wpctl", &["set-default", &to.id.to_string()]).await
        {
            let _ = self.inner.events.send(AudioEvent::DefaultMoved {
                to: to.description.clone(),
            });
        }
    }
}

/// The graph once (`pw-dump`), for one-shot commands without a running S-CTRL.
pub async fn graph_once() -> Result<Graph> {
    let out = command("pw-dump")
        .arg("--no-colors")
        .stderr(Stdio::null())
        .output()
        .await
        .context("pw-dump")?;
    let v: Value = serde_json::from_slice(&out.stdout).context("pw-dump output")?;
    let mut g = Graph::default();
    for o in v.as_array().into_iter().flatten() {
        g.apply(o);
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(mode: u8, state: u8, flags0: u8) -> Status {
        Status {
            mode,
            state,
            flags0,
            ..Default::default()
        }
    }

    #[test]
    fn plan_follows_the_rules() {
        let on = PcWish {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(pc_plan(&PcWish::default(), None, true, true), PcPlan::Off);
        assert!(matches!(
            pc_plan(&on, Some(&status(0, 1, 0)), false, true),
            PcPlan::Unavailable { .. }
        ));
        assert!(matches!(
            pc_plan(&on, Some(&status(0, 1, 0)), true, false),
            PcPlan::Unavailable { .. }
        ));
        assert_eq!(
            pc_plan(&on, Some(&status(0, 1, 0)), true, true),
            PcPlan::Play {
                latency: 512,
                muted: None
            }
        );
        assert_eq!(
            pc_plan(&on, Some(&status(2, 1, 0)), true, true),
            PcPlan::Play {
                latency: 128,
                muted: None
            }
        );
        // DIGI off by default, on with the override.
        assert!(matches!(
            pc_plan(&on, Some(&status(5, 1, 0)), true, true),
            PcPlan::Play { muted: Some(_), .. }
        ));
        let digi = PcWish {
            digi: true,
            ..on.clone()
        };
        assert_eq!(
            pc_plan(&digi, Some(&status(5, 1, 0)), true, true),
            PcPlan::Play {
                latency: 512,
                muted: None
            }
        );
        // I/Q capture: nothing to play.
        assert!(matches!(
            pc_plan(&on, Some(&status(0, 1, 0x02)), true, true),
            PcPlan::Unavailable { .. }
        ));
        // Voice TX muted; CW TX (sidetone) not.
        assert!(matches!(
            pc_plan(&on, Some(&status(0, 3, 0)), true, true),
            PcPlan::Play { muted: Some(_), .. }
        ));
        assert_eq!(
            pc_plan(&on, Some(&status(2, 3, 0)), true, true),
            PcPlan::Play {
                latency: 128,
                muted: None
            }
        );
        // The CW trainer is low-latency whatever the mode.
        let trainer = Status {
            page: PAGE_CW_TRAINER,
            ..status(0, 1, 0)
        };
        assert_eq!(
            pc_plan(&on, Some(&trainer), true, true),
            PcPlan::Play {
                latency: 128,
                muted: None
            }
        );
    }

    #[test]
    fn tx_audio_whitelist() {
        assert!(tx_audio_allowed("wsjtx"));
        assert!(tx_audio_allowed("JS8Call"));
        assert!(!tx_audio_allowed("firefox"));
        assert!(!tx_audio_allowed(""));
        assert!(!tx_audio_allowed("wsjtxfake"));
    }

    fn graph() -> Graph {
        let mut g = Graph::default();
        for o in [
            json!({"id": 126, "type": "PipeWire:Interface:Device", "info": {"props": {
                "device.vendor.id": "0x0483", "device.product.id": "0x5740",
                "device.serial": "Chireix_SPARTACUS_REV1_1A2B3C4D5E6F"}}}),
            json!({"id": 131, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "alsa_output.usb-Chireix_SPARTACUS_REV1_1A2B3C4D5E6F-00.analog-stereo.2",
                "object.serial": 362, "device.id": 126}}}),
            json!({"id": 130, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Source", "node.name": "alsa_input.usb-Chireix_SPARTACUS_REV1_1A2B3C4D5E6F-00.analog-stereo.2",
                "object.serial": 363, "device.id": 126}}}),
            json!({"id": 91, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "alsa_output.pci-0000_63_00.6.analog-stereo", "object.serial": 91}}}),
            json!({"id": 50, "type": "PipeWire:Interface:Client", "info": {"props": {"application.process.binary": "firefox"}}}),
            json!({"id": 200, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Stream/Output/Audio", "node.name": "Firefox", "client.id": 50}}}),
            json!({"id": 300, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 200, "input-node-id": 131}}),
            json!({"id": 0, "type": "PipeWire:Interface:Metadata", "props": {"metadata.name": "default"},
                   "metadata": [{"subject": 0, "key": "default.audio.sink", "value": {"name": "alsa_output.usb-Chireix_SPARTACUS_REV1_1A2B3C4D5E6F-00.analog-stereo.2"}}]}),
        ] {
            g.apply(&o);
        }
        g
    }

    #[test]
    fn finds_the_radio_by_device_not_name() {
        let g = graph();
        assert_eq!(g.radio_source("1A2B3C4D5E6F").unwrap().serial, 363);
        assert_eq!(g.radio_sink("1A2B3C4D5E6F").unwrap().id, 131);
        assert!(g.radio_source("OTHER").is_none());
        // The default is the radio's own sink: the safe output is another one.
        assert_eq!(g.safe_output().unwrap().id, 91);
        let feeding = g.feeding(131);
        assert_eq!(feeding.len(), 1);
        assert_eq!(g.client_binary(feeding[0].1), "firefox");
    }

    #[test]
    fn recorders_leave_out_the_loopback_and_meters() {
        let mut g = graph();
        for o in [
            // S-CTRL's own loopback capture, pavucontrol's level meter, fldigi.
            json!({"id": 400, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Stream/Input/Audio", "node.name": "input.sctrl-pc-speaker"}}}),
            json!({"id": 401, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Stream/Input/Audio", "node.name": "pavucontrol", "media.name": "Peak detect",
                "stream.monitor": true}}}),
            json!({"id": 51, "type": "PipeWire:Interface:Client", "info": {"props": {"application.process.binary": "fldigi"}}}),
            json!({"id": 402, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Stream/Input/Audio", "node.name": "fldigi", "client.id": 51}}}),
            json!({"id": 500, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 130, "input-node-id": 400}}),
            json!({"id": 501, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 130, "input-node-id": 401}}),
        ] {
            g.apply(&o);
        }
        assert!(g.recorders(130).is_empty());
        g.apply(&json!({"id": 502, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 130, "input-node-id": 402}}));
        assert_eq!(g.recorders(130), vec!["fldigi".to_string()]);
    }

    #[test]
    fn removals_drop_objects() {
        let mut g = graph();
        g.apply(&json!({"id": 130, "info": null}));
        assert!(g.radio_source("1A2B3C4D5E6F").is_none());
        g.apply(&json!({"id": 300, "info": null}));
        assert!(g.feeding(131).is_empty());
    }
}
