//! The floating window (qmmp-style): the radio's live screen, a status
//! strip, the controls, and companion windows. The process also holds the
//! radio for `sctrl` commands (the same socket as `sctrl daemon`).

mod actions;
mod companions;
mod mirror;
mod skin;
mod txwin;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2, ViewportCommand, X11WindowType};
use sctrl_core::ipc;
use sctrl_core::link::{LinkEvent, LinkOptions};
use sctrl_core::safety::Action;
use sctrl_core::service::{ConnState, Service};
use sctrl_proto::frame::{Input, sub};
use sctrl_proto::payload::{Status, tx_owner_name};
use sctrl_proto::screen;
use sctrl_proto::spectrum::SpectrumLine;

use actions::{Actions, Toast};
use mirror::Mirror;

use crate::cli::{fmt_hz, mode_cat, parse_freq, ref_name};

const ALL_PAGES: u32 = (1 << screen::PAGES) - 1;

/// Bands as quick presets (FT8 calling frequencies; tune from there).
const BANDS: [(&str, u32); 10] = [
    ("160m", 1_840_000),
    ("80m", 3_573_000),
    ("60m", 5_357_000),
    ("40m", 7_074_000),
    ("30m", 10_136_000),
    ("20m", 14_074_000),
    ("17m", 18_100_000),
    ("15m", 21_074_000),
    ("12m", 24_915_000),
    ("10m", 28_074_000),
];

pub fn run(serial: Option<String>, rt: tokio::runtime::Runtime) -> i32 {
    if rt.block_on(ipc::running()) {
        eprintln!("sctrl: an S-CTRL is already running (window or daemon); use it, or stop it first");
        return 2;
    }
    let _enter = rt.enter();
    let opts = LinkOptions {
        sub_mask: sub::STATUS | sub::SCREEN | sub::CWDEC | sub::SPECTRUM | sub::SPECTRUM_TRACE,
        ..Default::default()
    };
    let svc = Service::start(serial, opts);
    svc.start_audio();
    let server = rt.spawn(ipc::serve(env!("SCTRL_BUILD_ID"), crate::ipc_handler(svc.clone())));
    let gui = sctrl_core::config::load().0.gui;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("S-CTRL")
        .with_app_id("sctrl")
        .with_decorations(false)
        .with_window_type(X11WindowType::Utility)
        .with_resizable(false)
        .with_inner_size(window_size(gui.scale.clamp(1.0, 3.0), false));
    if gui.always_on_top {
        viewport = viewport.with_always_on_top();
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let r = eframe::run_native(
        "S-CTRL",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, svc, handle)))),
    );
    server.abort();
    ipc::cleanup();
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sctrl: window: {e}");
            6
        }
    }
}

fn window_size(scale: f32, shade: bool) -> Vec2 {
    if shade {
        Vec2::new(screen::W as f32 * scale + 16.0, 30.0)
    } else {
        Vec2::new(screen::W as f32 * scale + 16.0, screen::H as f32 * scale + 198.0)
    }
}

struct Press {
    at: Instant,
    held: bool,
}

pub struct App {
    act: Actions,
    toasts_rx: std::sync::mpsc::Receiver<Toast>,
    toasts: Vec<Toast>,
    mirror: Mirror,
    dirty: Arc<AtomicU32>,
    spectrum: Arc<Mutex<Option<Arc<SpectrumLine>>>>,
    scale: f32,
    shade: bool,
    /// The shade buttons ask; the next frame switches (never mid-frame,
    /// where the rest of the frame would size the window for the old view).
    toggle_shade: bool,
    pinned: bool,
    took_control: bool,
    press: Option<Press>,
    wheel: f32,
    freq_edit: Option<String>,
    comp: companions::Companions,
    /// The height last asked of the window manager (fit to content).
    fit_h: f32,
    /// S-CTRL's own settings (config.rs), saved when they change here.
    cfg: sctrl_core::config::Config,
    /// A window-manager move began: the release it eats is owed to egui.
    wm_drag: bool,
    /// When the radio's screen was last drawn (ms since `epoch`): the radio
    /// streams it only while it is drawn.
    painted: Arc<AtomicU64>,
    epoch: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, svc: Service, rt: tokio::runtime::Handle) -> Self {
        skin::apply(&cc.egui_ctx);
        let (act, toasts_rx) = Actions::new(svc.clone(), rt.clone(), cc.egui_ctx.clone());
        let dirty = Arc::new(AtomicU32::new(ALL_PAGES));
        let spectrum = Arc::new(Mutex::new(None));
        // Pump: what the radio pushes marks pages dirty and wakes the window.
        let (d, sp, ctx) = (dirty.clone(), spectrum.clone(), cc.egui_ctx.clone());
        let mut rx = svc.subscribe();
        let mut state = svc.watch_state();
        rt.spawn(async move {
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(LinkEvent::Screen(m)) => { d.fetch_or(m, Ordering::Relaxed); }
                        Ok(LinkEvent::Hello(_)) | Ok(LinkEvent::Closed(_)) => {
                            d.store(ALL_PAGES, Ordering::Relaxed);
                        }
                        Ok(LinkEvent::Spectrum(l)) => {
                            *sp.lock().unwrap() = Some(l);
                            d.fetch_or(ALL_PAGES, Ordering::Relaxed);
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            d.store(ALL_PAGES, Ordering::Relaxed);
                        }
                        Err(_) => return,
                    },
                    r = state.changed() => if r.is_err() { return },
                }
                ctx.request_repaint();
            }
        });
        // The TX audio guard says what it did.
        if let Some(audio) = svc.audio() {
            let mut rx = audio.subscribe();
            let a2 = act.clone();
            rt.spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    use sctrl_core::audio::AudioEvent;
                    match ev {
                        AudioEvent::Guarded { program, stream } => {
                            a2.toast(format!("moved {program} ({stream}) off the radio's TX audio"), true)
                        }
                        AudioEvent::DefaultMoved { to } => {
                            a2.toast(format!("the radio was the default output; now {to}"), true)
                        }
                        AudioEvent::Note(n) => a2.toast(n, false),
                    }
                }
            });
        }
        // Not drawn for a while (hidden on another workspace, unmapped): the
        // radio stops streaming its screen until it is drawn again.
        let painted = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        {
            let (painted, svc) = (painted.clone(), svc.clone());
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let age = epoch.elapsed().as_millis() as u64 - painted.load(Ordering::Relaxed);
                    if age > 1500 {
                        svc.show_screen(false);
                    }
                }
            });
        }
        let (cfg, cfg_err) = sctrl_core::config::load();
        if let Some(e) = cfg_err {
            act.toast(e, true);
        }
        Self {
            comp: companions::Companions::new(act.clone()),
            act,
            toasts_rx,
            toasts: Vec::new(),
            mirror: Mirror::default(),
            dirty,
            spectrum,
            scale: if [1.0, 1.5, 2.0, 3.0].contains(&cfg.gui.scale) {
                cfg.gui.scale
            } else {
                2.0
            },
            shade: false,
            toggle_shade: false,
            pinned: cfg.gui.always_on_top,
            cfg,
            took_control: false,
            press: None,
            wheel: 0.0,
            freq_edit: None,
            fit_h: 0.0,
            wm_drag: false,
            painted,
            epoch,
        }
    }

    fn status(&self) -> Option<Status> {
        self.act.svc.link().and_then(|l| l.state.lock().unwrap().status.clone())
    }

    fn input(&self, code: Input, value: i8) {
        let Some(l) = self.act.svc.link() else {
            self.act.toast("not connected", true);
            return;
        };
        self.act.run(Action::PanelInput, self.took_control, async move {
            l.input(code, value).await?;
            Ok(None)
        });
    }

    fn cat(&self, action: Action, command: String, done: Option<String>) {
        let Some(l) = self.act.svc.link() else {
            self.act.toast("not connected", true);
            return;
        };
        self.act.run(action, self.took_control, async move {
            l.cat(&command, Duration::from_secs(3)).await?;
            Ok(done)
        });
    }

    /// Hand the move to the window manager. It keeps the button release
    /// (i3 grabs the pointer while moving), so egui would think the button
    /// still down and swallow the next press: raw_input_hook supplies it.
    fn start_drag(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        self.wm_drag = true;
    }

    fn save_gui(&mut self) {
        self.cfg.gui.scale = self.scale;
        self.cfg.gui.always_on_top = self.pinned;
        if let Err(e) = sctrl_core::config::save(&self.cfg) {
            self.act.toast(format!("settings not saved: {e:#}"), true);
        }
    }

    fn resize(&mut self, ctx: &egui::Context) {
        self.fit_h = 0.0; // the next full frame fits the height again
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(window_size(self.scale, self.shade)));
    }

    fn shade_toggle(&mut self, ctx: &egui::Context) {
        if self.act.may_act() {
            self.toggle_shade = true;
            ctx.request_repaint();
        }
    }

    /// The window is as tall as what it shows (asked once per change).
    fn fit(&mut self, ctx: &egui::Context, height: f32) {
        let want = height.ceil();
        if self.act.may_act() && (want - self.fit_h).abs() > 1.0 {
            self.fit_h = want;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
                screen::W as f32 * self.scale + 16.0,
                want,
            )));
        }
    }

    // ---- the parts of the window ----

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // The whole bar drags the window (double click: shade); the buttons,
        // added after, sit on top of it and take their own clicks.
        let bar = ui.interact(ui.max_rect(), ui.id().with("drag"), Sense::click_and_drag());
        if bar.drag_started() {
            self.start_drag(&ctx);
        }
        if bar.double_clicked() {
            self.shade_toggle(&ctx);
        }
        let mut bar_hover = String::new();
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new("S-CTRL").color(skin::FREQ).strong()).selectable(false));
            let (text, colour, hover) = match self.act.svc.state() {
                ConnState::Linked { serial, port } => ("LINK".to_string(), skin::OK, format!("{serial} on {port}")),
                ConnState::Connecting { serial } => ("connecting".into(), skin::WARN, serial),
                other => {
                    let s = other.summary();
                    (s.clone(), skin::WARN, s)
                }
            };
            // Truncated so the buttons always keep their room, whatever the
            // scale. No sense of its own: it would take the bar's drag.
            ui.add(
                egui::Label::new(RichText::new(text).color(colour).small())
                    .selectable(false)
                    .truncate(),
            );
            bar_hover = hover;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("x").on_hover_text("close S-CTRL").clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                if ui.small_button("\u{23f6}").on_hover_text("shade: one line").clicked() {
                    self.shade_toggle(&ctx);
                }
                let pin = if self.pinned { "pin*" } else { "pin" };
                if ui.small_button(pin).on_hover_text("always on top").clicked() && self.act.may_act() {
                    self.pinned = !self.pinned;
                    self.save_gui();
                    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.pinned {
                        egui::WindowLevel::AlwaysOnTop
                    } else {
                        egui::WindowLevel::Normal
                    }));
                }
                // One button for the scale: four would not fit at 1x.
                ui.menu_button(format!("{}x", self.scale), |ui| {
                    for s in [1.0f32, 1.5, 2.0, 3.0] {
                        if ui
                            .selectable_label((self.scale - s).abs() < 0.01, format!("{s}x"))
                            .clicked()
                            && self.act.may_act()
                        {
                            self.scale = s;
                            self.resize(&ctx);
                            self.save_gui();
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("screen scale");
            });
        });
        bar.on_hover_text(bar_hover);
    }

    /// Shaded: one line, dragged like the title bar; the arrow brings the
    /// whole window back (so does a double click).
    fn shade_line(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        let ctx = ui.ctx().clone();
        let bar = ui.interact(ui.max_rect(), ui.id().with("drag"), Sense::click_and_drag());
        if bar.drag_started() {
            self.start_drag(&ctx);
        }
        if bar.double_clicked() {
            self.shade_toggle(&ctx);
        }
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new("S-CTRL").color(skin::FREQ).strong()).selectable(false));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("x").on_hover_text("close S-CTRL").clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                if ui
                    .small_button("\u{23f7}")
                    .on_hover_text("back to the full window")
                    .clicked()
                {
                    self.shade_toggle(&ctx);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let text = match st {
                        Some(s) => {
                            tx_lamp(ui, s);
                            format!(
                                "{} {}  S {:.0}  {:.1} V",
                                fmt_hz(s.dial_hz),
                                s.mode_name(),
                                s.rx_dbfs_x10 as f32 / 10.0,
                                s.vin_mv as f32 / 1000.0
                            )
                        }
                        None => self.act.svc.state().summary(),
                    };
                    // Truncated, so the buttons keep their room at 1x.
                    ui.add(
                        egui::Label::new(RichText::new(text).color(skin::FREQ).monospace())
                            .selectable(false)
                            .truncate(),
                    );
                });
            });
        });
    }

    fn screen(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mask = self.dirty.swap(0, Ordering::Relaxed);
        if mask != 0 {
            if let Some(l) = self.spectrum.lock().unwrap().take() {
                self.mirror.spectrum(&l);
            }
            match self.act.svc.link() {
                Some(link) => {
                    let st = link.state.lock().unwrap();
                    self.mirror.update(&ctx, &st.screen, mask);
                }
                None => self.mirror.update(&ctx, &sctrl_proto::screen::Screen::new(), ALL_PAGES),
            }
        }
        let size = Vec2::new(screen::W as f32 * self.scale, screen::H as f32 * self.scale);
        let tex = self.mirror.texture(&ctx).clone();
        let resp = ui.add(egui::Image::new((tex.id(), size)).sense(Sense::click_and_drag()));
        let linked = self.act.svc.link().is_some();
        if !linked {
            ui.painter().text(
                resp.rect.center(),
                egui::Align2::CENTER_CENTER,
                self.act.svc.state().summary(),
                egui::FontId::monospace(14.0),
                skin::WARN,
            );
            return;
        }
        // The knob: wheel notches over the screen (Shift: shaft pressed, Ctrl: FUNC held).
        if resp.hovered() {
            let (steps, mods) = ctx.input(|i| {
                let mut s = 0.0f32;
                for e in &i.events {
                    // One event per notch from a real wheel (winit drops the
                    // XInput2-emulated button press); xdotool's synthetic
                    // clicks arrive twice, so they count double.
                    if let egui::Event::MouseWheel { unit, delta, .. } = e {
                        s += match unit {
                            egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => delta.y,
                            egui::MouseWheelUnit::Point => delta.y / 40.0,
                        };
                    }
                }
                (s, i.modifiers)
            });
            if self.act.may_act() {
                self.wheel += steps;
            }
            let whole = self.wheel.trunc();
            if whole != 0.0 {
                self.wheel -= whole;
                let code = if mods.ctrl {
                    Input::FuncTurn
                } else if mods.shift {
                    Input::TurnHeld
                } else {
                    Input::Turn
                };
                let sign = wheel_sign(self.status().as_ref());
                self.input(code, (whole * sign).clamp(-24.0, 24.0) as i8);
            }
        }
        // Click = knob click; held 0.6 s = knob hold (sent while held, as the panel does).
        if !self.act.may_act() {
            // A repeated pass of the same frame: no new presses or releases.
        } else if resp.is_pointer_button_down_on() && ctx.input(|i| i.pointer.primary_down()) {
            match &mut self.press {
                None => {
                    self.press = Some(Press {
                        at: Instant::now(),
                        held: false,
                    })
                }
                Some(p) if !p.held && p.at.elapsed() >= Duration::from_millis(600) => {
                    p.held = true;
                    self.input(Input::Hold, 0);
                }
                _ => {}
            }
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if let Some(p) = self.press.take()
            && !p.held
        {
            self.input(Input::Click, 0);
        }
        if resp.secondary_clicked() {
            self.input(Input::Hold, 0);
        }
        resp.on_hover_text(
            "wheel: knob (up = tune up / value up / list up; Shift: pressed, Ctrl: FUNC)\nclick: knob click   hold or right click: knob hold",
        );
    }

    fn strip(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        let Some(s) = st else {
            ui.label(RichText::new("waiting for the radio...").color(skin::MUTED));
            return;
        };
        let cat_program = self.act.context(self.took_control, false).cat_program;
        ui.horizontal_wrapped(|ui| {
            tx_lamp(ui, s);
            // The dial: click to type a frequency.
            match &mut self.freq_edit {
                Some(text) => {
                    let r = ui.add(
                        egui::TextEdit::singleline(text)
                            .desired_width(120.0)
                            .font(egui::TextStyle::Heading),
                    );
                    r.request_focus();
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        match parse_freq(text) {
                            Ok(hz) => self.cat(Action::Tune, format!("FA{hz:011};"), None),
                            Err(e) => self.act.toast(format!("{e:#}"), true),
                        }
                        self.freq_edit = None;
                    } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) || r.lost_focus() {
                        self.freq_edit = None;
                    }
                }
                None => {
                    let r = ui
                        .add(
                            egui::Label::new(
                                RichText::new(fmt_hz(s.dial_hz))
                                    .color(skin::FREQ)
                                    .size(22.0)
                                    .monospace(),
                            )
                            .sense(Sense::click()),
                        )
                        .on_hover_text("click to type a frequency (14074000, 14.074M, 7074k)");
                    if r.clicked() {
                        self.freq_edit = Some(format!("{}", s.dial_hz));
                    }
                }
            }
            ui.label(RichText::new(s.state_name()).color(if s.transmitting() { skin::ALERT } else { skin::DIM }));
            egui::ComboBox::from_id_salt("band")
                .selected_text("band")
                .width(70.0)
                .show_ui(ui, |ui| {
                    for (name, hz) in BANDS {
                        if ui.selectable_label(false, format!("{name} {}", fmt_hz(hz))).clicked() {
                            self.cat(Action::Band, format!("FA{hz:011};"), None);
                        }
                    }
                });
        });
        ui.horizontal_wrapped(|ui| {
            for m in ["USB", "LSB", "CW", "AM", "FM", "DIGI"] {
                let on = s.mode_name() == m;
                if ui.selectable_label(on, m).clicked() && !on
                    && let Ok(md) = mode_cat(m) {
                        self.cat(Action::Mode, md.to_string(), None);
                    }
            }
            ui.separator();
            if let Some(p) = &cat_program {
                let t = if self.took_control { format!("{p}: control taken") } else { format!("{p} has the radio") };
                if ui
                    .selectable_label(self.took_control, RichText::new(t).color(skin::WARN))
                    .on_hover_text("frequency/mode/band locked while a CAT program uses the radio; click to take control (never while it transmits)")
                    .clicked()
                {
                    self.took_control = !self.took_control;
                }
            } else {
                self.took_control = false;
            }
        });
        ui.horizontal_wrapped(|ui| {
            meter(
                ui,
                "S",
                ((s.rx_dbfs_x10 as f32 / 10.0) + 130.0) / 100.0,
                &format!("{:.0} dBFS", s.rx_dbfs_x10 as f32 / 10.0),
            );
            ui.label(format!("PWR {:.2} W", s.pep_w()));
            ui.label(match s.swr() {
                Some(v) => format!("SWR {v:.2}"),
                None => "SWR -".into(),
            });
            ui.label(format!("VIN {:.2} V", s.vin_mv as f32 / 1000.0));
            ui.label(RichText::new(format!("PD {:.0} V", s.pd_mv as f32 / 1000.0)).color(skin::MUTED));
        });
        ui.horizontal_wrapped(|ui| {
            for (name, code) in [
                ("UP", Input::Up),
                ("DN", Input::Dn),
                ("SEL", Input::Sel),
                ("FUNC", Input::Func),
            ] {
                let r = ui.button(name).on_hover_text("click: press   right click: long press");
                if r.clicked() {
                    self.input(code, 0);
                }
                if r.secondary_clicked() {
                    self.input(code.long(), 0);
                }
            }
            if ui.button("<").on_hover_text("knob one detent left").clicked() {
                self.input(Input::Turn, -1);
            }
            if ui.button(">").on_hover_text("knob one detent right").clicked() {
                self.input(Input::Turn, 1);
            }
            ui.separator();
            let flags: Vec<(&str, bool, Color32)> = vec![
                ("TX MUTE", s.tx_mute(), skin::WARN),
                ("IQ", s.usb_iq(), skin::WARN),
                ("VOX", s.usb_vox(), skin::WARN),
                ("CAT PTT", s.cat_ptt(), skin::ALERT),
                ("LOCKOUT", s.lockout(), skin::ALERT),
            ];
            for (name, on, c) in flags {
                if on {
                    ui.label(RichText::new(name).color(c).small());
                }
            }
            let f = s.fault_names();
            if !f.is_empty() {
                ui.label(RichText::new(format!("FAULT {}", f.join(","))).color(skin::ALERT));
            }
        });
        self.audio_row(ui);
        ui.horizontal_wrapped(|ui| {
            self.comp.toggles(ui);
            ui.label(
                RichText::new(format!(
                    "ref {} {:+.1} ppm  page {}",
                    ref_name(s.ref_state),
                    s.ref_ppm_x100 as f32 / 100.0,
                    s.page_name()
                ))
                .color(skin::MUTED)
                .small(),
            );
        });
    }

    /// PC speaker: on/off, volume, DIGI override, where it plays, and why not.
    fn audio_row(&mut self, ui: &mut egui::Ui) {
        let Some(audio) = self.act.svc.audio() else { return };
        let mut w = audio.wish();
        let st = audio.status();
        let before = w.clone();
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut w.enabled, "PC SPEAKER")
                .on_hover_text("the radio's audio on this PC's output (PipeWire loopback; DIGI only on request)");
            let vol = ui
                .add(egui::Slider::new(&mut w.volume, 0.0..=1.5).show_value(false))
                .on_hover_text("PC speaker volume");
            // Kept for the next start once the slider is let go.
            if (vol.drag_stopped() || (vol.changed() && !vol.dragged())) && self.act.may_act() {
                let v = w.volume;
                if let Err(e) = self.act.svc.update_config(move |c| c.pc_speaker.volume = v) {
                    self.act.toast(format!("volume not saved: {e:#}"), true);
                }
            }
            ui.checkbox(&mut w.digi, "DIGI")
                .on_hover_text("play DIGI too (off by default)");
            if w.enabled {
                use sctrl_core::audio::PcPlan;
                let (text, c) = match &st.plan {
                    Some(PcPlan::Play { muted: None, latency }) => (
                        format!(
                            "-> {} ({} ms)",
                            st.output.clone().unwrap_or("?".into()),
                            latency * 1000 / 48000
                        ),
                        skin::OK,
                    ),
                    Some(PcPlan::Play { muted: Some(why), .. }) => (format!("muted: {why}"), skin::WARN),
                    Some(PcPlan::Unavailable { reason }) => (reason.clone(), skin::WARN),
                    _ => ("".into(), skin::MUTED),
                };
                ui.label(RichText::new(text).small().color(c));
            }
        });
        for n in st
            .notes
            .iter()
            .filter(|n| !n.starts_with("DIGI") && !n.starts_with("voice TX"))
        {
            ui.label(RichText::new(n).small().color(skin::WARN));
        }
        if self.act.may_act()
            && (w.enabled != before.enabled || w.digi != before.digi || (w.volume - before.volume).abs() > 0.001)
        {
            audio.set_wish(w);
        }
    }

    fn toasts(&mut self, ui: &mut egui::Ui) {
        while let Ok(t) = self.toasts_rx.try_recv() {
            self.toasts.push(t);
        }
        self.toasts
            .retain(|t| t.at.elapsed() < Duration::from_secs(if t.bad { 6 } else { 3 }));
        if self.toasts.is_empty() {
            return;
        }
        let ctx = ui.ctx().clone();
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 34.0))
            .show(&ctx, |ui| {
                for t in &self.toasts {
                    egui::Frame::NONE
                        .fill(if t.bad { skin::ALERT_BG } else { skin::PANEL_HI })
                        .stroke(Stroke::new(1.0, if t.bad { skin::ALERT } else { skin::LINE }))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.label(&t.text);
                        });
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn keys(&mut self, ui: &mut egui::Ui) {
        if !self.act.may_act() || self.freq_edit.is_some() || ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (up, down, enter, esc) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if up {
            self.input(Input::Turn, 1);
        }
        if down {
            self.input(Input::Turn, -1);
        }
        if enter {
            self.input(Input::Click, 0);
        }
        if esc {
            // STOP: whatever S-CTRL keyed ends (always allowed).
            if let Some(l) = self.act.svc.link() {
                self.act.run(Action::StopTransmit, true, async move {
                    l.request_ok(sctrl_proto::payload::tx_stop()).await?;
                    Ok(Some("TX stopped".into()))
                });
            }
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if std::mem::take(&mut self.wm_drag) {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            raw.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.act.set_pass(ui.ctx());
        if std::mem::take(&mut self.toggle_shade) {
            self.shade = !self.shade;
            self.fit_h = 0.0;
        }
        let st = self.status();
        let hidden = ui.ctx().input(|i| {
            let v = i.viewport();
            v.minimized.unwrap_or(false) || v.occluded.unwrap_or(false)
        });
        let shown = !self.shade && !hidden;
        if shown {
            self.painted
                .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        self.act.svc.show_screen(shown);
        if self.shade {
            let h = egui::Panel::top("shade")
                .frame(egui::Frame::NONE.fill(skin::PANEL).inner_margin(4.0))
                .show(ui, |ui| {
                    self.shade_line(ui, st.as_ref());
                })
                .response
                .rect
                .height();
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(skin::BG))
                .show(ui, |_| {});
            self.fit(ui.ctx(), h);
            self.toasts(ui);
            self.comp.show(ui.ctx(), st.as_ref());
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            return;
        }
        let top_h = egui::Panel::top("title")
            .frame(egui::Frame::NONE.fill(skin::PANEL).inner_margin(4.0))
            .show(ui, |ui| {
                self.title_bar(ui);
            })
            .response
            .rect
            .height();
        let strip_h = egui::Panel::bottom("strip")
            .frame(egui::Frame::NONE.fill(skin::BG).inner_margin(6.0))
            .show(ui, |ui| {
                self.strip(ui, st.as_ref());
            })
            .response
            .rect
            .height();
        // The window fits what it shows: at 1x the strip's rows wrap and need
        // more height than at 3x; never let them cover the screen.
        self.fit(ui.ctx(), top_h + screen::H as f32 * self.scale + 16.0 + strip_h);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(skin::BG).inner_margin(8.0))
            .show(ui, |ui| {
                self.screen(ui);
            });
        self.keys(ui);
        self.toasts(ui);
        self.comp.show(ui.ctx(), st.as_ref());
        // STATUS arrives at 10 Hz; the window follows it.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

/// Which way the wheel turns the knob. Clockwise (+) tunes up and raises a
/// value, but moves *down* a list: in lists the wheel is reversed so that
/// wheel-up moves the selection up, as anywhere else on the desktop.
pub fn wheel_sign(s: Option<&Status>) -> f32 {
    let Some(s) = s else { return 1.0 };
    let editing = s.ui & 0x40 != 0;
    let home_level = s.ui & 0x03;
    let list = match s.page {
        1 => !editing,        // MENU, browsing (editing a value: up = more)
        8 | 13 => true,       // SD files, SELFTEST
        0 => home_level == 1, // HOME zone picker (zones stacked top to bottom)
        _ => false,
    };
    if list { -1.0 } else { 1.0 }
}

fn tx_lamp(ui: &mut egui::Ui, s: &Status) {
    let (text, fg, bg) = if s.transmitting() {
        (
            format!(" TX {} ", tx_owner_name(s.tx_owner)),
            Color32::WHITE,
            skin::ALERT_BG,
        )
    } else {
        (" RX ".to_string(), skin::OK, skin::PANEL)
    };
    egui::Frame::NONE
        .fill(bg)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(fg).strong());
        });
}

fn meter(ui: &mut egui::Ui, name: &str, frac: f32, text: &str) {
    ui.label(name);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(90.0, 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 1.0, skin::PANEL);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, 1.0, skin::OK);
    ui.label(RichText::new(text).small());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(page: u8, ui: u8) -> Status {
        Status {
            page,
            ui,
            ..Default::default()
        }
    }

    #[test]
    fn wheel_up_moves_up_in_lists_and_tunes_up_elsewhere() {
        assert_eq!(wheel_sign(Some(&st(0, 0))), 1.0); // HOME tuning
        assert_eq!(wheel_sign(Some(&st(0, 1))), -1.0); // HOME zone picker
        assert_eq!(wheel_sign(Some(&st(1, 0))), -1.0); // MENU browsing
        assert_eq!(wheel_sign(Some(&st(1, 0x40))), 1.0); // MENU editing a value
        assert_eq!(wheel_sign(Some(&st(8, 0))), -1.0); // SD list
        assert_eq!(wheel_sign(Some(&st(15, 0))), 1.0); // WATERFALL
        assert_eq!(wheel_sign(None), 1.0);
    }
}
