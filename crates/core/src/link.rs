//! The LINK client: `link 1` on the radio's S-CTRL port, then frames both
//! ways (docs/link-protocol.md). One reader task decodes everything the radio
//! pushes into `RadioState` and broadcasts it; requests (console lines, CAT
//! commands, input, TX) go out from the caller with their replies matched up.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use sctrl_proto::frame::{AckCode, HostType, Input, RadioType, sub};
use sctrl_proto::payload::{self, Ack, Hello, HostFrame, PipelineEvent, Status, TxEvent};
use sctrl_proto::screen::Screen;
use sctrl_proto::slip::{Decoder, Event, encode_frame};
use sctrl_proto::spectrum::SpectrumLine;
use serde::Serialize;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::tty::Tty;

/// What the radio pushed, as the GUI and the CLI read it.
#[derive(Clone, Default)]
pub struct RadioState {
    pub hello: Option<Hello>,
    pub status: Option<Status>,
    pub status_at: Option<Instant>,
    pub screen: Screen,
    pub cw_text: String,
    pub cw_count: u32,
    /// Pipeline events as the console's `events` printed them, oldest first.
    pub events: VecDeque<(Instant, PipelineEvent)>,
    pub spectrum: Option<Arc<SpectrumLine>>,
    pub txevents: VecDeque<(Instant, TxEvent)>,
    pub stats: LinkStats,
    pub closed: Option<String>,
}

#[derive(Clone, Default, Debug, Serialize)]
pub struct LinkStats {
    pub frames: u64,
    pub dropped: u64,
    pub bytes: u64,
    pub seq_gaps: u64,
    pub screen_pages: u64,
    pub spectrum_lines: u64,
}

#[derive(Clone, Debug)]
pub enum LinkEvent {
    Hello(Hello),
    Status(Box<Status>),
    /// Screen pages that changed (bit per page).
    Screen(u32),
    Spectrum(Arc<SpectrumLine>),
    /// A new pipeline event (read with the console's `events` when STATUS's
    /// event sequence moved).
    Event(PipelineEvent),
    CwDec(String),
    TxEvent(TxEvent),
    Ack(Ack),
    Bye(u8),
    Closed(String),
}

#[derive(Debug, Clone, Copy)]
pub struct LinkOptions {
    pub sub_mask: u16,
    pub packets_per_frame: u8,
    pub status_hz: u8,
}

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            sub_mask: sub::STATUS | sub::SCREEN | sub::CWDEC,
            packets_per_frame: 2,
            status_hz: 10,
        }
    }
}

/// Why `link 1` did not work.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("this firmware has no LINK (needs alt2 v8.11 or later): {0}")]
    NoLink(String),
    #[error("the radio refused the LINK: {0}")]
    Refused(String),
    #[error("the other CAT port holds the radio's console; close the terminal on it")]
    ConsoleElsewhere,
    #[error("no answer to `link 1` on {0}")]
    NoAnswer(String),
}

struct ConsoleWaiter {
    lines: Vec<String>,
    partial: String,
    done: Option<oneshot::Sender<std::result::Result<Vec<String>, String>>>,
}

#[derive(Default)]
struct Shared {
    acks: HashMap<(u8, u8), oneshot::Sender<Ack>>,
    console: Option<ConsoleWaiter>,
    cat: Option<oneshot::Sender<std::result::Result<String, String>>>,
    last_seq: Option<u8>,
}

pub struct Link {
    tty: Arc<Tty>,
    seq: Mutex<u8>,
    write_lock: tokio::sync::Mutex<()>,
    console_lock: tokio::sync::Mutex<()>,
    cat_lock: tokio::sync::Mutex<()>,
    shared: Arc<Mutex<Shared>>,
    pub state: Arc<Mutex<RadioState>>,
    events: broadcast::Sender<LinkEvent>,
    tasks: Vec<JoinHandle<()>>,
    pub port: PathBuf,
}

impl Drop for Link {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Link {
    /// Open `port`, switch it with `link 1`, subscribe, and wait for HELLO.
    pub async fn connect(port: &Path, opts: LinkOptions) -> Result<Self> {
        let tty = Arc::new(Tty::open(port)?);
        tty.fresh_session().await?;
        tty.write_all(b"link 1\n").await?;
        let mut text = Vec::<u8>::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        let rest = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(LinkError::NoAnswer(port.display().to_string()).into());
            }
            let mut buf = [0u8; 512];
            let n = match tokio::time::timeout(left, tty.read(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(LinkError::NoAnswer(port.display().to_string()).into()),
            };
            text.extend_from_slice(&buf[..n]);
            if let Some(i) = find(&text, b"OK\r\n") {
                break text.split_off(i + 4);
            }
            let s = String::from_utf8_lossy(&text);
            if s.contains("?;") {
                return Err(LinkError::ConsoleElsewhere.into());
            }
            if let Some(line) = s.lines().find(|l| l.starts_with("ERR")) {
                return Err(if line.contains("unknown command") {
                    LinkError::NoLink(line.to_string())
                } else {
                    LinkError::Refused(line.to_string())
                }
                .into());
            }
        };
        let (events, _) = broadcast::channel(4096);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = Arc::new(Mutex::new(RadioState::default()));
        let mut link = Self {
            tty: tty.clone(),
            seq: Mutex::new(0),
            write_lock: tokio::sync::Mutex::new(()),
            console_lock: tokio::sync::Mutex::new(()),
            cat_lock: tokio::sync::Mutex::new(()),
            shared: shared.clone(),
            state: state.clone(),
            events: events.clone(),
            tasks: Vec::new(),
            port: port.to_path_buf(),
        };
        let mut hello_rx = events.subscribe();
        link.tasks.push(tokio::spawn(reader(tty, rest, shared, state, events)));
        link.send(payload::heartbeat(0)).await?;
        link.send(payload::sub(opts.sub_mask, opts.packets_per_frame, opts.status_hz))
            .await?;
        let wait_hello = async {
            loop {
                match hello_rx.recv().await {
                    Ok(LinkEvent::Hello(_)) => return Ok(()),
                    Ok(LinkEvent::Closed(e)) => return Err(anyhow!(e)),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => return Err(anyhow!(e)),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), wait_hello)
            .await
            .context("no HELLO from the radio")??;
        // The heartbeat the transmit guard needs; the radio releases
        // everything the LINK keyed 1.5 s after the last one.
        let hb = link.heartbeat_task();
        link.tasks.push(hb);
        Ok(link)
    }

    /// The console's `events`: the radio's pipeline event ring, newest
    /// first (index 0).
    pub async fn read_events(&self, timeout: Duration) -> Result<Vec<PipelineEvent>> {
        let lines = self.cmd("events", timeout).await?;
        Ok(lines.iter().filter_map(|l| PipelineEvent::parse_line(l)).collect())
    }

    /// Read new pipeline events when STATUS says there are some: the
    /// console's `events` (names included), at most once a second. Runs
    /// for as long as the LINK holds an `Arc` to itself elsewhere.
    pub fn spawn_event_reader(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let mut rx = self.subscribe();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut seen: Option<u8> = None;
            let mut last = Instant::now() - Duration::from_secs(10);
            loop {
                let seq = match rx.recv().await {
                    Ok(LinkEvent::Status(s)) => s.event_seq,
                    Ok(LinkEvent::Closed(_)) | Err(broadcast::error::RecvError::Closed) => return,
                    _ => continue,
                };
                if seen == Some(seq) || last.elapsed() < Duration::from_secs(1) {
                    continue;
                }
                let Some(link) = me.upgrade() else { return };
                last = Instant::now();
                let Ok(mut got) = link.read_events(Duration::from_secs(10)).await else {
                    continue;
                };
                got.sort_by_key(|e| std::cmp::Reverse(e.index)); // oldest first
                // The first read is history; later ones add what is new.
                let fresh = match seen {
                    None => got.len(),
                    Some(old) => (seq.wrapping_sub(old) as usize).min(got.len()),
                };
                seen = Some(seq);
                let now = Instant::now();
                let mut st = link.state.lock().unwrap();
                if st.events.is_empty() {
                    for e in &got {
                        st.events.push_back((now, e.clone()));
                    }
                } else {
                    for e in &got[got.len() - fresh..] {
                        st.events.push_back((now, e.clone()));
                        let _ = events.send(LinkEvent::Event(e.clone()));
                    }
                }
                while st.events.len() > 200 {
                    st.events.pop_front();
                }
            }
        });
    }

    fn heartbeat_task(&self) -> JoinHandle<()> {
        let tty = self.tty.clone();
        let state = self.state.clone();
        let start = Instant::now();
        tokio::spawn(async move {
            let mut seq: u8 = 0x80; // its own seq space: heartbeats are never ACKed
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if state.lock().unwrap().closed.is_some() {
                    return;
                }
                let ms = (start.elapsed().as_millis() & 0xFFFF) as u16;
                let (ty, p) = payload::heartbeat(ms);
                if tty.write_all(&encode_frame(ty as u8, seq, &p)).await.is_err() {
                    return;
                }
                seq = seq.wrapping_add(1) | 0x80;
            }
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LinkEvent> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> RadioState {
        self.state.lock().unwrap().clone()
    }

    pub fn closed(&self) -> Option<String> {
        self.state.lock().unwrap().closed.clone()
    }

    fn next_seq(&self) -> u8 {
        let mut s = self.seq.lock().unwrap();
        let v = *s;
        *s = s.wrapping_add(1) & 0x7F; // 0x80.. are the heartbeat's
        v
    }

    /// Send a frame; returns its seq.
    pub async fn send(&self, (ty, p): HostFrame) -> Result<u8> {
        let _w = self.write_lock.lock().await;
        let seq = self.next_seq();
        self.tty.write_all(&encode_frame(ty as u8, seq, &p)).await?;
        Ok(seq)
    }

    /// Send a frame the radio ACKs, and wait for the ACK.
    pub async fn request(&self, frame: HostFrame, timeout: Duration) -> Result<Ack> {
        let ty = frame.0 as u8;
        let _w = self.write_lock.lock().await;
        let seq = self.next_seq();
        let (tx, rx) = oneshot::channel();
        self.shared.lock().unwrap().acks.insert((ty, seq), tx);
        let wire = encode_frame(ty, seq, &frame.1);
        self.tty.write_all(&wire).await?;
        drop(_w);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(a)) => Ok(a),
            Ok(Err(_)) => bail!("link closed"),
            Err(_) => {
                self.shared.lock().unwrap().acks.remove(&(ty, seq));
                bail!("no ACK from the radio within {timeout:?}")
            }
        }
    }

    /// A request whose ACK must be OK; the radio's reason otherwise.
    pub async fn request_ok(&self, frame: HostFrame) -> Result<()> {
        let a = self.request(frame, Duration::from_secs(2)).await?;
        match a.code() {
            Some(AckCode::Ok) => Ok(()),
            Some(c) => bail!("{}", c.explain()),
            None => bail!("unknown ACK code {}", a.code),
        }
    }

    /// One console command; its reply lines (without the OK). The console
    /// waits while the radio is realtime-critical (keyed CW, TX streams):
    /// give long timeouts while transmitting.
    pub async fn cmd(&self, line: &str, timeout: Duration) -> Result<Vec<String>> {
        let frame = payload::cmd(line).ok_or_else(|| anyhow!("console line too long or not one line"))?;
        let _c = self.console_lock.lock().await;
        let (tx, rx) = oneshot::channel();
        self.shared.lock().unwrap().console = Some(ConsoleWaiter {
            lines: Vec::new(),
            partial: String::new(),
            done: Some(tx),
        });
        self.send(frame).await?;
        let r = tokio::time::timeout(timeout, rx).await;
        self.shared.lock().unwrap().console = None;
        match r {
            Ok(Ok(Ok(lines))) => Ok(lines),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("link closed"),
            Err(_) => bail!("no reply to `{line}` within {timeout:?}"),
        }
    }

    /// One CAT command through the LINK's tunnel (TS-480 + QMX MM/ML).
    pub async fn cat(&self, command: &str, timeout: Duration) -> Result<String> {
        let frame = payload::cat(command).ok_or_else(|| anyhow!("CAT command too long"))?;
        let _c = self.cat_lock.lock().await;
        let (tx, rx) = oneshot::channel();
        self.shared.lock().unwrap().cat = Some(tx);
        self.send(frame).await?;
        let r = tokio::time::timeout(timeout, rx).await;
        self.shared.lock().unwrap().cat = None;
        match r {
            Ok(Ok(Ok(s))) => Ok(s),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("link closed"),
            Err(_) => bail!("no reply to {command} within {timeout:?}"),
        }
    }

    pub async fn input(&self, code: Input, value: i8) -> Result<()> {
        self.request_ok(payload::input(code, value)).await
    }

    pub async fn refresh(&self, what: u8) -> Result<()> {
        self.request_ok(payload::refresh(what)).await
    }

    pub async fn subscribe_channels(&self, opts: LinkOptions) -> Result<()> {
        self.request_ok(payload::sub(opts.sub_mask, opts.packets_per_frame, opts.status_hz))
            .await
    }

    /// LINK_OFF: the port is a CAT port again.
    pub async fn close(&self) {
        let _ = self.send(payload::link_off()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn reader(
    tty: Arc<Tty>,
    first: Vec<u8>,
    shared: Arc<Mutex<Shared>>,
    state: Arc<Mutex<RadioState>>,
    events: broadcast::Sender<LinkEvent>,
) {
    let mut dec = Decoder::new();
    let mut evs = Vec::new();
    let mut buf = vec![0u8; 4096];
    let mut pending = first;
    loop {
        if pending.is_empty() {
            match tty.read(&mut buf).await {
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e) => {
                    let why = format!("{e:#}");
                    state.lock().unwrap().closed = Some(why.clone());
                    fail_waiters(&shared, &why);
                    let _ = events.send(LinkEvent::Closed(why));
                    return;
                }
            }
        }
        state.lock().unwrap().stats.bytes += pending.len() as u64;
        dec.feed(&pending, &mut evs);
        pending.clear();
        for ev in evs.drain(..) {
            match ev {
                Event::Dropped(_) => state.lock().unwrap().stats.dropped += 1,
                Event::Frame(f) => {
                    {
                        let mut sh = shared.lock().unwrap();
                        let mut st = state.lock().unwrap();
                        st.stats.frames += 1;
                        if let Some(prev) = sh.last_seq
                            && f.seq != prev.wrapping_add(1)
                        {
                            st.stats.seq_gaps += 1;
                        }
                        sh.last_seq = Some(f.seq);
                    }
                    if let Some(out) = handle(&f.ty, &f.payload, &shared, &state) {
                        let _ = events.send(out);
                    }
                }
            }
        }
    }
}

fn fail_waiters(shared: &Arc<Mutex<Shared>>, why: &str) {
    let mut sh = shared.lock().unwrap();
    sh.acks.clear();
    if let Some(mut c) = sh.console.take()
        && let Some(d) = c.done.take()
    {
        let _ = d.send(Err(why.to_string()));
    }
    if let Some(c) = sh.cat.take() {
        let _ = c.send(Err(why.to_string()));
    }
}

fn handle(ty: &u8, p: &[u8], shared: &Arc<Mutex<Shared>>, state: &Arc<Mutex<RadioState>>) -> Option<LinkEvent> {
    let now = Instant::now();
    match RadioType::from_u8(*ty)? {
        RadioType::Hello => {
            let h = Hello::parse(p)?;
            state.lock().unwrap().hello = Some(h.clone());
            Some(LinkEvent::Hello(h))
        }
        RadioType::Status => {
            let s = Status::parse(p)?;
            let mut st = state.lock().unwrap();
            st.status = Some(s.clone());
            st.status_at = Some(now);
            Some(LinkEvent::Status(Box::new(s)))
        }
        RadioType::Screen => {
            let mut st = state.lock().unwrap();
            let page = st.screen.apply(p)?;
            st.stats.screen_pages += 1;
            Some(LinkEvent::Screen(1 << page))
        }
        RadioType::Spectrum => {
            let l = Arc::new(SpectrumLine::parse(p)?);
            let mut st = state.lock().unwrap();
            st.spectrum = Some(l.clone());
            st.stats.spectrum_lines += 1;
            Some(LinkEvent::Spectrum(l))
        }
        RadioType::CwDec => {
            let (count, text) = payload::parse_cwdec(p)?;
            let mut st = state.lock().unwrap();
            st.cw_count = count;
            st.cw_text.push_str(&text);
            if st.cw_text.len() > 4000 {
                let cut = st.cw_text.len() - 3000;
                st.cw_text.drain(..cut);
            }
            Some(LinkEvent::CwDec(text))
        }
        RadioType::Console => {
            let mut sh = shared.lock().unwrap();
            if let Some(w) = sh.console.as_mut() {
                w.partial.push_str(&String::from_utf8_lossy(p));
                while let Some(i) = w.partial.find('\n') {
                    let line: String = w.partial.drain(..=i).collect();
                    let line = line.trim_end_matches(['\r', '\n']).to_string();
                    if line.is_empty() || line.starts_with("# ") {
                        continue;
                    }
                    if line == "OK" {
                        if let Some(d) = w.done.take() {
                            let _ = d.send(Ok(std::mem::take(&mut w.lines)));
                        }
                    } else if let Some(e) = line.strip_prefix("ERR") {
                        if let Some(d) = w.done.take() {
                            let _ = d.send(Err(format!("radio: {}", e.trim())));
                        }
                    } else {
                        w.lines.push(line);
                    }
                }
            }
            None
        }
        RadioType::CatReply => {
            if let Some(c) = shared.lock().unwrap().cat.take() {
                let _ = c.send(Ok(String::from_utf8_lossy(p).into_owned()));
            }
            None
        }
        RadioType::Ack => {
            let a = Ack::parse(p)?;
            let mut sh = shared.lock().unwrap();
            if let Some(w) = sh.acks.remove(&(a.req_type, a.req_seq)) {
                let _ = w.send(a);
            } else if a.req_type == HostType::Cmd as u8 {
                if let Some(mut c) = sh.console.take()
                    && let Some(d) = c.done.take()
                {
                    let _ = d.send(Err(a.code().map(|c| c.explain()).unwrap_or("refused").to_string()));
                }
            } else if a.req_type == HostType::Cat as u8
                && let Some(c) = sh.cat.take()
            {
                let _ = c.send(Err(a.code().map(|c| c.explain()).unwrap_or("refused").to_string()));
            }
            Some(LinkEvent::Ack(a))
        }
        RadioType::TxEvent => {
            let e = TxEvent::parse(p)?;
            let mut st = state.lock().unwrap();
            st.txevents.push_back((now, e));
            while st.txevents.len() > 100 {
                st.txevents.pop_front();
            }
            Some(LinkEvent::TxEvent(e))
        }
        RadioType::Bye => Some(LinkEvent::Bye(payload::parse_bye(p)?)),
    }
}
