//! Everything the window asks of the radio runs as a job on the tokio
//! runtime, behind the safety arbiter; results come back as toasts.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use sctrl_core::safety::{self, Action, Context};
use sctrl_core::service::Service;
use tokio::runtime::Handle;

pub struct Toast {
    pub at: Instant,
    pub text: String,
    pub bad: bool,
}

#[derive(Clone)]
pub struct Actions {
    pub svc: Service,
    pub rt: Handle,
    tx: mpsc::Sender<Toast>,
    repaint: eframe::egui::Context,
    /// egui may run a frame's UI code more than once (multi-pass layout) with
    /// the same input: only the first pass may act on the radio.
    first_pass: Arc<AtomicBool>,
}

impl Actions {
    pub fn new(svc: Service, rt: Handle, repaint: eframe::egui::Context) -> (Self, mpsc::Receiver<Toast>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                svc,
                rt,
                tx,
                repaint,
                first_pass: Arc::new(AtomicBool::new(true)),
            },
            rx,
        )
    }

    /// Called at the start of every UI pass.
    pub fn set_pass(&self, ctx: &eframe::egui::Context) {
        self.first_pass.store(ctx.current_pass_index() == 0, Ordering::Relaxed);
    }

    /// May this pass act (send to the radio, start jobs)?
    pub fn may_act(&self) -> bool {
        self.first_pass.load(Ordering::Relaxed)
    }

    pub fn toast(&self, text: impl Into<String>, bad: bool) {
        let _ = self.tx.send(Toast {
            at: Instant::now(),
            text: text.into(),
            bad,
        });
        self.repaint.request_repaint();
    }

    /// Draw the window again (a job finished).
    pub fn repaint(&self) {
        self.repaint.request_repaint();
    }

    /// The arbiter's view right now.
    pub fn context(&self, took_control: bool, armed: bool) -> Context {
        let link = self.svc.link();
        let status = link.as_ref().and_then(|l| l.snapshot().status);
        let host = self.svc.host();
        let cat0 = status.as_ref().map(|s| s.cat0_age_ds).filter(|&a| a != 0xFFFF);
        Context {
            cat_program: host.cat_program(cat0),
            iq_consumer: host.iq_consumer(),
            took_control,
            armed,
            link_live: link.as_ref().is_some_and(|l| l.closed().is_none()),
            status,
            ..Default::default()
        }
    }

    /// Run `job` if the arbiter allows `action`; toast its error, and its
    /// success text when it has one.
    pub fn run<F>(&self, action: Action, took_control: bool, job: F)
    where
        F: Future<Output = anyhow::Result<Option<String>>> + Send + 'static,
    {
        if !self.may_act() {
            return;
        }
        if let Err(d) = safety::decide(action, &self.context(took_control, false)) {
            self.toast(d.to_string(), true);
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), job).await {
                Ok(Ok(Some(msg))) => me.toast(msg, false),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => me.toast(format!("{e:#}"), true),
                Err(_) => me.toast("the radio did not answer in time", true),
            }
        });
    }
}
