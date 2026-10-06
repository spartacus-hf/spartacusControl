//! Companion windows docked beside the main one: Diagnostics, Settings,
//! Console, and the CW decoder. Each is its own native window (i3 floats
//! them like the main one).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, RichText, ViewportBuilder, ViewportId, X11WindowType};
use sctrl_core::safety::Action;
use sctrl_proto::payload::{Status, tx_owner_name};

use super::actions::Actions;
use super::files::Files;
use super::skin;
use super::txwin::TxWin;
use crate::cli::{fmt_hz, ref_name};

/// One setting as `set list` gives it.
#[derive(Clone)]
struct SettingRow {
    id: u32,
    name: String,
    value: String,
    range: String,
    edit: String,
}

pub struct Companions {
    act: Actions,
    diag: bool,
    settings: bool,
    console: bool,
    cw: bool,
    tx: bool,
    txwin: TxWin,
    files: bool,
    filesw: Files,
    /// Console window: the log and the line being typed.
    log: Arc<Mutex<Vec<String>>>,
    line: String,
    /// Diagnostics: the last command run and its output.
    diag_out: Arc<Mutex<(String, Vec<String>)>>,
    rows: Arc<Mutex<Vec<SettingRow>>>,
    filter: String,
    rows_loading: bool,
}

const DIAG_COMMANDS: [&str; 16] = [
    "status",
    "faults",
    "pipeline",
    "events",
    "rx",
    "digi",
    "cat",
    "pd",
    "clock sof",
    "telem",
    "meter",
    "cpu",
    "mem",
    "cwdec",
    "sd status",
    "storage",
];

impl Companions {
    pub fn new(act: Actions) -> Self {
        Self {
            txwin: TxWin::new(act.clone()),
            filesw: Files::new(act.clone()),
            act,
            diag: false,
            settings: false,
            console: false,
            cw: false,
            tx: false,
            files: false,
            log: Arc::new(Mutex::new(Vec::new())),
            line: String::new(),
            diag_out: Arc::new(Mutex::new((String::new(), Vec::new()))),
            rows: Arc::new(Mutex::new(Vec::new())),
            filter: String::new(),
            rows_loading: false,
        }
    }

    pub fn toggles(&mut self, ui: &mut egui::Ui) {
        ui.toggle_value(&mut self.diag, "DIAG");
        if ui.toggle_value(&mut self.settings, "SET").changed() && self.settings {
            self.load_settings();
        }
        ui.toggle_value(&mut self.cw, "CW");
        ui.toggle_value(&mut self.console, "CON");
        ui.toggle_value(&mut self.files, "FILES")
            .on_hover_text("the radio's SD card: browse, view, save a copy (read only)");
        ui.toggle_value(&mut self.tx, RichText::new("TX").color(skin::WARN))
            .on_hover_text("S-CTRL's own transmissions: ARM, TUNE, the paddle, CW memories, STOP");
    }

    fn run_cmd(&self, line: String, out: Arc<Mutex<(String, Vec<String>)>>) {
        if !self.act.may_act() {
            return;
        }
        let Some(l) = self.act.svc.link() else {
            self.act.toast("not connected", true);
            return;
        };
        let act = self.act.clone();
        self.act.rt.spawn(async move {
            let r = l.cmd(&line, Duration::from_secs(10)).await;
            let lines = match r {
                Ok(v) => v,
                Err(e) => vec![format!("ERR {e:#}")],
            };
            *out.lock().unwrap() = (line, lines);
            act.repaint();
        });
    }

    fn load_settings(&mut self) {
        if !self.act.may_act() {
            return;
        }
        let Some(l) = self.act.svc.link() else { return };
        self.rows_loading = true;
        let rows = self.rows.clone();
        let act = self.act.clone();
        self.act.rt.spawn(async move {
            match l.cmd("set list", Duration::from_secs(20)).await {
                Ok(lines) => {
                    let parsed: Vec<SettingRow> = lines
                        .iter()
                        .filter_map(|l| {
                            // 12 "CW SPEED" 20 5..40 3
                            let (id, rest) = l.split_once(' ')?;
                            let mut q = rest.splitn(3, '"');
                            q.next()?;
                            let name = q.next()?.to_string();
                            let mut tail = q.next()?.split_whitespace();
                            let value = tail.next().unwrap_or("-").to_string();
                            let range = tail.next().unwrap_or("-").to_string();
                            Some(SettingRow {
                                id: id.parse().ok()?,
                                name,
                                edit: value.clone(),
                                value,
                                range,
                            })
                        })
                        .collect();
                    *rows.lock().unwrap() = parsed;
                }
                Err(e) => act.toast(format!("set list: {e:#}"), true),
            }
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, st: Option<&Status>) {
        if self.diag {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("diag"),
                builder("S-CTRL diagnostics", [560.0, 520.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.diag_ui(ui, st));
                },
            );
            self.diag = open;
        }
        if self.settings {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("settings"),
                builder("S-CTRL settings", [520.0, 600.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.settings_ui(ui));
                },
            );
            self.settings = open;
        }
        if self.console {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("console"),
                builder("S-CTRL console", [600.0, 420.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.console_ui(ui));
                },
            );
            self.console = open;
        }
        if self.tx {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("tx"),
                builder("S-CTRL TX", [560.0, 640.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| self.txwin.ui(ui, st));
                    });
                },
            );
            self.tx = open;
        }
        if !self.tx {
            self.txwin.release();
        }
        if self.files {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("files"),
                builder("S-CTRL files", [560.0, 520.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.filesw.ui(ui));
                },
            );
            self.files = open;
        }
        if self.cw {
            let mut open = true;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("cw"),
                builder("S-CTRL CW", [520.0, 220.0]),
                |ui, _| {
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                    egui::CentralPanel::default().show(ui, |ui| self.cw_ui(ui, st));
                },
            );
            self.cw = open;
        }
    }

    fn diag_ui(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.heading("Diagnostics");
        if let Some(s) = st {
            egui::Grid::new("diag-status")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    let rows: Vec<(&str, String)> = vec![
                        ("state", s.state_name().into()),
                        ("dial", format!("{} Hz", fmt_hz(s.dial_hz))),
                        ("mode", s.mode_name().into()),
                        ("page", s.page_name().into()),
                        ("TX owner", tx_owner_name(s.tx_owner).into()),
                        ("VIN", format!("{:.2} V", s.vin_mv as f32 / 1000.0)),
                        ("PD contract", format!("{:.1} V", s.pd_mv as f32 / 1000.0)),
                        ("S", format!("{:.1} dBFS", s.rx_dbfs_x10 as f32 / 10.0)),
                        (
                            "ref",
                            format!("{} {:+.2} ppm", ref_name(s.ref_state), s.ref_ppm_x100 as f32 / 100.0),
                        ),
                        (
                            "faults",
                            if s.faults == 0 {
                                "none".into()
                            } else {
                                s.fault_names().join(",")
                            },
                        ),
                        ("overruns", s.rx_overruns.to_string()),
                        ("capture gaps", format!("{} /10 min", s.cap_gaps)),
                        ("USB RAW IQ", if s.usb_iq() { "on" } else { "off" }.into()),
                        ("USB VOX", if s.usb_vox() { "on" } else { "off" }.into()),
                        ("TX MUTE", if s.tx_mute() { "on" } else { "off" }.into()),
                        ("uptime", format!("{:.0} s", s.uptime_ms as f32 / 1000.0)),
                    ];
                    for pair in rows.chunks(2) {
                        for (k, v) in pair {
                            ui.label(RichText::new(*k).color(skin::MUTED));
                            ui.label(v);
                        }
                        ui.end_row();
                    }
                });
        }
        if let Some(l) = self.act.svc.link() {
            let snap = l.snapshot();
            ui.label(
                RichText::new(format!(
                    "LINK: {} frames, {} dropped, {} seq gaps, {} screen pages, {} KB",
                    snap.stats.frames,
                    snap.stats.dropped,
                    snap.stats.seq_gaps,
                    snap.stats.screen_pages,
                    snap.stats.bytes / 1024
                ))
                .small()
                .color(skin::MUTED),
            );
        }
        let host = self.act.svc.host();
        let apps: Vec<String> = host
            .apps
            .iter()
            .map(|r| format!("{} ({})", r.app.name(), r.proc.pid))
            .collect();
        ui.label(
            RichText::new(format!(
                "radio programs: {}",
                if apps.is_empty() {
                    "none".into()
                } else {
                    apps.join(", ")
                }
            ))
            .small(),
        );
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            for c in DIAG_COMMANDS {
                if ui.small_button(c).clicked() {
                    self.run_cmd(c.to_string(), self.diag_out.clone());
                }
            }
        });
        let (cmd, lines) = self.diag_out.lock().unwrap().clone();
        if !cmd.is_empty() {
            ui.label(RichText::new(format!("> {cmd}")).color(skin::ACCENT));
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for l in lines {
                    ui.add(egui::Label::new(RichText::new(l).monospace()).selectable(true));
                }
            });
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Settings");
            if ui.button("reload").clicked() {
                self.load_settings();
            }
            ui.label("filter");
            ui.text_edit_singleline(&mut self.filter);
        });
        let mut rows = self.rows.lock().unwrap().clone();
        if rows.is_empty() {
            ui.label(if self.rows_loading {
                "reading the radio's settings..."
            } else {
                "no settings read yet"
            });
            return;
        }
        let f = self.filter.to_ascii_uppercase();
        let mut put: Option<(u32, String)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("settings").num_columns(4).striped(true).show(ui, |ui| {
                for r in rows.iter_mut().filter(|r| f.is_empty() || r.name.contains(&f)) {
                    ui.label(RichText::new(r.id.to_string()).color(skin::MUTED).small());
                    ui.label(&r.name);
                    let resp = ui.add(egui::TextEdit::singleline(&mut r.edit).desired_width(90.0));
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && r.edit != r.value {
                        put = Some((r.id, r.edit.clone()));
                    }
                    ui.label(RichText::new(&r.range).small().color(skin::MUTED));
                    ui.end_row();
                }
            });
        });
        // Keep what is being typed.
        {
            let mut stored = self.rows.lock().unwrap();
            for (a, b) in stored.iter_mut().zip(rows.iter()) {
                a.edit = b.edit.clone();
            }
        }
        if let Some((id, value)) = put
            && let Some(l) = self.act.svc.link()
        {
            let rows = self.rows.clone();
            self.act.run(Action::Setting, false, async move {
                let out = l.cmd(&format!("set put {id} {value}"), Duration::from_secs(5)).await?;
                let kv = out.first().cloned().unwrap_or_default();
                if let Some((_, v)) = kv.split_once('=') {
                    let mut r = rows.lock().unwrap();
                    if let Some(row) = r.iter_mut().find(|r| r.id == id) {
                        row.value = v.to_string();
                        row.edit = v.to_string();
                    }
                }
                Ok(Some(kv))
            });
        }
    }

    fn console_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(">");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.line)
                    .desired_width(ui.available_width() - 60.0)
                    .hint_text("help"),
            );
            let send = ui.button("run").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if send && self.act.may_act() && !self.line.trim().is_empty() {
                let line = std::mem::take(&mut self.line);
                let log = self.log.clone();
                log.lock().unwrap().push(format!("> {line}"));
                if let Some(l) = self.act.svc.link() {
                    let act = self.act.clone();
                    self.act.rt.spawn(async move {
                        let out = match l.cmd(&line, Duration::from_secs(20)).await {
                            Ok(v) => v,
                            Err(e) => vec![format!("ERR {e:#}")],
                        };
                        log.lock().unwrap().extend(out);
                        act.repaint();
                    });
                }
                r.request_focus();
            }
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            for l in self.log.lock().unwrap().iter() {
                ui.add(egui::Label::new(RichText::new(l).monospace()).selectable(true));
            }
        });
    }

    fn cw_ui(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.horizontal(|ui| {
            ui.heading("CW decoder");
            if let Some(s) = st {
                ui.label(format!("keyer {} WPM", s.wpm));
            }
        });
        let text = self
            .act
            .svc
            .link()
            .map(|l| l.state.lock().unwrap().cw_text.clone())
            .unwrap_or_default();
        if text.is_empty() {
            ui.label(
                RichText::new("nothing decoded yet: the radio's CW decoder text appears here (CW mode)")
                    .color(skin::MUTED),
            );
        }
        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).monospace().size(18.0).color(skin::FREQ)).selectable(true));
        });
    }
}

fn builder(title: &str, size: [f32; 2]) -> ViewportBuilder {
    let b = ViewportBuilder::default()
        .with_title(title)
        .with_app_id("sctrl")
        .with_window_type(X11WindowType::Utility)
        .with_inner_size(size);
    match super::ICON.clone() {
        Some(icon) => b.with_icon(icon),
        None => b,
    }
}
