//! The TX window: S-CTRL's own transmissions -- TUNE, the paddle, CW
//! memories -- through core::tx, the same controller, checklist, limits and
//! audit log as `sctrl tx`. STOP (here, or `sctrl tx stop`) always works and
//! is never refused.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use sctrl_core::link::Link;
use sctrl_core::safety::{Action, Context};
use sctrl_core::tx::{self, Load, Region, TxController, TxKind, TxPolicy};
use sctrl_proto::frame::Input;
use sctrl_proto::payload::Status;

use super::actions::Actions;
use super::skin;

pub struct TxWin {
    act: Actions,
    /// A TX job (ARM, TUNE, a memory, the paddle) is running.
    busy: Arc<AtomicBool>,
    /// The paddle's levers now, bit 0 DIT, bit 1 DAH (mouse or keys).
    levers: Arc<AtomicU8>,
    /// The last job's result, as lines.
    last: Arc<Mutex<Vec<String>>>,
    tune_ms: u32,
    memories: Vec<String>,
    /// The limits being edited (None: not open).
    policy: Option<TxPolicy>,
    log: Vec<String>,
    show_kind: TxKind,
}

impl TxWin {
    pub fn new(act: Actions) -> Self {
        let cfg = act.svc.config();
        Self {
            busy: Arc::new(AtomicBool::new(false)),
            levers: Arc::new(AtomicU8::new(0)),
            last: Arc::new(Mutex::new(Vec::new())),
            tune_ms: 800.min(cfg.tx.key_ceiling_ms(TxKind::Tune)),
            memories: cfg.cw.memories,
            policy: None,
            log: Vec::new(),
            show_kind: TxKind::Paddle,
            act,
        }
    }

    /// The window closed or lost the keyboard: the paddle lets go.
    pub fn release(&self) {
        self.levers.store(0, Ordering::Relaxed);
    }

    /// Run a TX job (never two at once); its lines become the result shown.
    fn job<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Link>, Context, TxController) -> Fut,
        Fut: Future<Output = anyhow::Result<Vec<String>>> + Send + 'static,
    {
        if !self.act.may_act() {
            return;
        }
        let Some(link) = self.act.svc.link() else {
            self.act.toast("not connected", true);
            return;
        };
        if self.busy.swap(true, Ordering::Relaxed) {
            return;
        }
        let fut = f(link, self.act.svc.context(false), self.act.svc.tx());
        let (busy, last, act) = (self.busy.clone(), self.last.clone(), self.act.clone());
        self.act.rt.spawn(async move {
            let lines = match fut.await {
                Ok(l) => l,
                Err(e) => {
                    act.toast(format!("{e:#}"), true);
                    vec![format!("refused or failed: {e:#}")]
                }
            };
            *last.lock().unwrap() = lines;
            busy.store(false, Ordering::Relaxed);
            act.repaint();
        });
    }

    fn stop(&self) {
        self.release();
        let Some(link) = self.act.svc.link() else { return };
        let (act, last, ctl) = (self.act.clone(), self.last.clone(), self.act.svc.tx());
        // Not a job: STOP never waits for one, and is never refused.
        self.act.rt.spawn(async move {
            match ctl.stop(&link).await {
                Ok(()) => *last.lock().unwrap() = vec!["STOP: everything S-CTRL keyed ended, disarmed".into()],
                Err(e) => act.toast(format!("STOP: {e:#}"), true),
            }
            act.repaint();
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        let ctl = self.act.svc.tx();
        let policy = ctl.policy();
        let busy = self.busy.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            ui.heading("Transmit");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stop = egui::Button::new(RichText::new(" STOP ").strong().color(Color32::WHITE))
                    .fill(skin::ALERT_BG)
                    .min_size(Vec2::new(80.0, 28.0));
                if ui
                    .add(stop)
                    .on_hover_text("ends whatever S-CTRL keyed and disarms")
                    .clicked()
                {
                    self.stop();
                }
            });
        });
        let Some(s) = st else {
            ui.label(RichText::new("waiting for the radio...").color(skin::MUTED));
            return;
        };
        // Where the radio stands.
        ui.horizontal_wrapped(|ui| {
            let (text, c) = if s.transmitting() {
                (
                    format!("TX ({})", sctrl_proto::payload::tx_owner_name(s.tx_owner)),
                    skin::ALERT,
                )
            } else {
                ("RX".to_string(), skin::OK)
            };
            ui.label(RichText::new(text).strong().color(c));
            ui.label(if s.armed() {
                RichText::new(format!("ARMED {} s", s.arm_left_s))
                    .color(skin::WARN)
                    .strong()
            } else {
                RichText::new("not armed").color(skin::MUTED)
            });
            ui.label(if s.tx_mute() {
                RichText::new("TX MUTE on: practice, sidetone only").color(skin::OK)
            } else {
                RichText::new("TX MUTE off: the paddle and memories transmit").color(skin::WARN)
            });
        });
        // The session's answer and ARM.
        ui.horizontal_wrapped(|ui| {
            ui.label("RF goes into");
            let load = ctl.load();
            if ui.radio(load == Some(Load::DummyLoad), "50 ohm load").clicked() && self.act.may_act() {
                ctl.set_load(Load::DummyLoad);
            }
            if ui.radio(load == Some(Load::Antenna), "antenna").clicked() && self.act.may_act() {
                ctl.set_load(Load::Antenna);
            }
            ui.separator();
            let can_arm = load.is_some() && !busy;
            if s.armed() {
                if ui.button("DISARM").clicked() {
                    self.job(|l, cx, ctl| async move {
                        ctl.arm(&l, &cx, false).await?;
                        Ok(vec!["disarmed".into()])
                    });
                }
            } else if ui
                .add_enabled(can_arm, egui::Button::new(RichText::new("ARM").strong()))
                .on_hover_text(format!(
                    "allow S-CTRL to key the radio for {} s after the last TX (say first what RF goes into)",
                    policy.arm_ttl_s
                ))
                .on_disabled_hover_text("say first what the radio's RF goes into")
                .clicked()
            {
                self.job(|l, cx, ctl| async move {
                    ctl.arm(&l, &cx, true).await?;
                    Ok(vec!["armed".into()])
                });
            }
        });
        ui.separator();
        // TUNE: a carrier on the TX TEST page.
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("TUNE").strong());
            if s.page != tx::PAGE_TX_TEST {
                if ui
                    .button("open TX TEST")
                    .on_hover_text("the radio's TX TEST page: TUNE keys its carrier at the TUNE power")
                    .clicked()
                    && let Some(l) = self.act.svc.link()
                {
                    self.act.run(Action::Mode, false, async move {
                        l.input(Input::OpenPage, tx::PAGE_TX_TEST as i8).await?;
                        Ok(None)
                    });
                }
            } else {
                ui.label(RichText::new("TX TEST open").color(skin::OK).small());
            }
            let cap = policy.key_ceiling_ms(TxKind::Tune);
            self.tune_ms = self.tune_ms.clamp(100, cap);
            ui.add(egui::Slider::new(&mut self.tune_ms, 100..=cap).suffix(" ms"));
            if ui
                .add_enabled(!busy && s.page == tx::PAGE_TX_TEST, egui::Button::new("TUNE"))
                .on_disabled_hover_text("open TX TEST first")
                .clicked()
            {
                let ms = self.tune_ms;
                self.show_kind = TxKind::Tune;
                self.job(
                    move |l, cx, ctl| async move { Ok(crate::tx_cli::report_lines(&ctl.tune(&l, &cx, ms).await?)) },
                );
            }
        });
        ui.separator();
        self.paddle(ui, s, busy);
        ui.separator();
        // Memories.
        ui.label(RichText::new("CW memories").strong());
        let mut save = false;
        for i in 0..self.memories.len() {
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.memories[i])
                        .char_limit(48)
                        .desired_width(ui.available_width() - 70.0),
                );
                if r.lost_focus() {
                    save = true;
                }
                let text = self.memories[i].trim().to_string();
                if ui
                    .add_enabled(!busy && !text.is_empty(), egui::Button::new("SEND"))
                    .on_hover_text("sent by the radio's keyer at its WPM")
                    .clicked()
                {
                    self.show_kind = TxKind::Memory;
                    self.job(move |l, cx, ctl| async move {
                        Ok(crate::tx_cli::report_lines(&ctl.memory(&l, &cx, &text).await?))
                    });
                }
            });
        }
        if save && self.act.may_act() {
            let m = self.memories.clone();
            if let Err(e) = self.act.svc.update_config(move |c| c.cw.memories = m) {
                self.act.toast(format!("memories not saved: {e:#}"), true);
            }
        }
        ui.separator();
        // The checklist as it stands now.
        ui.horizontal(|ui| {
            ui.label(RichText::new("Checklist for").strong());
            for (k, name) in [
                (TxKind::Paddle, "paddle"),
                (TxKind::Memory, "memory"),
                (TxKind::Tune, "TUNE"),
            ] {
                ui.selectable_value(&mut self.show_kind, k, name);
            }
        });
        if let Some(l) = self.act.svc.link() {
            let cx = self.act.svc.context(false);
            let checks = tx::checklist(self.show_kind, &policy, &ctl.facts(&l, &cx, 0));
            egui::Grid::new("checks").num_columns(3).show(ui, |ui| {
                for c in &checks {
                    ui.label(if c.ok {
                        RichText::new("ok").color(skin::OK)
                    } else {
                        RichText::new("NO").color(skin::ALERT).strong()
                    });
                    ui.label(c.item);
                    ui.label(RichText::new(&c.detail).small());
                    ui.end_row();
                }
            });
        }
        let last = self.last.lock().unwrap().clone();
        if !last.is_empty() {
            ui.separator();
            for l in last {
                ui.label(RichText::new(l).small());
            }
        }
        ui.separator();
        self.limits(ui, &policy);
        egui::CollapsingHeader::new("Audit log").show(ui, |ui| {
            if ui.small_button("reload").clicked() || self.log.is_empty() {
                let text = std::fs::read_to_string(tx::log_path()).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                self.log = lines[lines.len().saturating_sub(12)..]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                if self.log.is_empty() {
                    self.log.push("(empty)".into());
                }
            }
            for l in &self.log {
                ui.add(egui::Label::new(RichText::new(l).monospace().small()).selectable(true));
            }
        });
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    /// The paddle: DIT and DAH held with the mouse, or with , and . (or [
    /// and ]) while this window has the keyboard; keyed while held.
    fn paddle(&mut self, ui: &mut egui::Ui, s: &Status, busy: bool) {
        let mut v = 0u8;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Paddle").strong());
            for (bit, name) in [(1u8, "DIT"), (2u8, "DAH")] {
                let r = ui.add(
                    egui::Button::new(RichText::new(name).strong())
                        .min_size(Vec2::new(64.0, 28.0))
                        .sense(Sense::click_and_drag()),
                );
                if r.is_pointer_button_down_on() {
                    v |= bit;
                }
            }
            ui.label(
                RichText::new("hold; keys , and . (or [ and ]) while this window has focus")
                    .small()
                    .color(skin::MUTED),
            );
        });
        let focused = ui.input(|i| i.viewport().focused.unwrap_or(false));
        if focused && !ui.ctx().egui_wants_keyboard_input() {
            ui.input(|i| {
                if i.key_down(egui::Key::Comma) || i.key_down(egui::Key::OpenBracket) {
                    v |= 1;
                }
                if i.key_down(egui::Key::Period) || i.key_down(egui::Key::CloseBracket) {
                    v |= 2;
                }
            });
        }
        if !self.act.may_act() {
            return;
        }
        let before = self.levers.swap(v, Ordering::Relaxed);
        if v != 0 {
            // Watch the levers while they are held: a release must be seen.
            ui.ctx().request_repaint_after(Duration::from_millis(10));
            if before == 0 && !busy {
                if s.mode != tx::MODE_CW && s.page != tx::PAGE_TX_TEST {
                    self.act.toast("the paddle keys in CW (or on TX TEST)", true);
                    return;
                }
                self.show_kind = TxKind::Paddle;
                let levers = self.levers.clone();
                self.job(move |l, cx, ctl| async move {
                    Ok(crate::tx_cli::report_lines(&ctl.paddle(&l, &cx, levers).await?))
                });
            }
        }
    }

    /// The limits, edited here and saved in S-CTRL's settings.
    fn limits(&mut self, ui: &mut egui::Ui, current: &TxPolicy) {
        let r = egui::CollapsingHeader::new("Limits").show(ui, |ui| {
            let p = self.policy.get_or_insert_with(|| current.clone());
            egui::Grid::new("limits").num_columns(2).show(ui, |ui| {
                ui.label("bench mode (every key-down 1 s)");
                ui.checkbox(&mut p.bench, "");
                ui.end_row();
                ui.label("TUNE, longest");
                ui.add(
                    egui::DragValue::new(&mut p.tune_max_ms)
                        .range(100..=tx::ABS_KEY_CEIL_MS)
                        .suffix(" ms"),
                );
                ui.end_row();
                ui.label("paddle key-down, longest");
                ui.add(
                    egui::DragValue::new(&mut p.key_max_ms)
                        .range(100..=tx::ABS_KEY_CEIL_MS)
                        .suffix(" ms"),
                );
                ui.end_row();
                ui.label("memory, longest");
                ui.add(egui::DragValue::new(&mut p.memory_max_s).range(1..=255).suffix(" s"));
                ui.end_row();
                ui.label("duty cycle");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.duty_pct).range(1..=100).suffix(" %"));
                    ui.label("per");
                    ui.add(egui::DragValue::new(&mut p.duty_window_s).range(10..=3600).suffix(" s"));
                });
                ui.end_row();
                ui.label("ARM ends after");
                ui.add(egui::DragValue::new(&mut p.arm_ttl_s).range(5..=255).suffix(" s idle"));
                ui.end_row();
                ui.label("band plan");
                egui::ComboBox::from_id_salt("region")
                    .selected_text(format!("{:?}", p.region))
                    .show_ui(ui, |ui| {
                        for r in [Region::R1, Region::R2, Region::R3] {
                            ui.selectable_value(&mut p.region, r, format!("{r:?}"));
                        }
                    });
                ui.end_row();
                let mut vin = p.min_vin_mv as f32 / 1000.0;
                ui.label("lowest supply");
                if ui
                    .add(
                        egui::DragValue::new(&mut vin)
                            .range(0.0..=30.0)
                            .speed(0.05)
                            .suffix(" V"),
                    )
                    .changed()
                {
                    p.min_vin_mv = (vin * 1000.0).round() as u16;
                }
                ui.end_row();
                ui.label("highest SWR");
                ui.add(egui::DragValue::new(&mut p.max_swr).range(1.0..=10.0).speed(0.05));
                ui.end_row();
            });
            ui.label(
                RichText::new(format!(
                    "never more than {} ms of continuous key-down, whatever is set",
                    tx::ABS_KEY_CEIL_MS
                ))
                .small()
                .color(skin::MUTED),
            );
            ui.horizontal(|ui| {
                let changed = *p != *current;
                if ui.add_enabled(changed, egui::Button::new("save")).clicked() && self.act.may_act() {
                    let mut np = p.clone();
                    np.sanitize();
                    match self.act.svc.update_config(move |c| c.tx = np) {
                        Ok(path) => self.act.toast(format!("limits saved ({})", path.display()), false),
                        Err(e) => self.act.toast(format!("limits not saved: {e:#}"), true),
                    }
                }
                if ui.button("defaults").clicked() {
                    *p = TxPolicy::default();
                }
            });
        });
        if r.body_returned.is_none() {
            self.policy = None; // closed: start from the saved limits next time
        }
    }
}
