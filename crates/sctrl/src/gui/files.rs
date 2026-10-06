//! The Files window: the radio's SD card, read only -- browse folders, view
//! text files, save a copy on this PC (core::sdcard; the card's settings
//! store is the radio's business, so nothing here writes or deletes).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};
use sctrl_core::sdcard::{self, SdEntry};

use super::actions::Actions;
use super::skin;

/// Files this size or smaller with a text name open in the window.
const VIEW_MAX: u64 = 64 * 1024;
const TEXT: [&str; 6] = [".TXT", ".LOG", ".CSV", ".DEF", ".INI", ".MD"];

#[derive(Default)]
struct Shared {
    listing: Option<Result<Vec<SdEntry>, String>>,
    /// A file being read: (name, bytes so far, total).
    reading: Option<(String, u64, u64)>,
    /// A text file shown: (name, text).
    shown: Option<(String, String)>,
}

pub struct Files {
    act: Actions,
    path: String,
    listed: Option<String>,
    sh: Arc<Mutex<Shared>>,
}

impl Files {
    pub fn new(act: Actions) -> Self {
        Self {
            act,
            path: String::new(),
            listed: None,
            sh: Arc::new(Mutex::new(Shared::default())),
        }
    }

    fn list(&mut self) {
        if !self.act.may_act() {
            return;
        }
        let Some(l) = self.act.svc.link() else { return };
        self.listed = Some(self.path.clone());
        let (sh, act, path) = (self.sh.clone(), self.act.clone(), self.path.clone());
        sh.lock().unwrap().listing = None;
        self.act.rt.spawn(async move {
            let r = sdcard::ls(&l, &path).await.map_err(|e| format!("{e:#}")).map(|mut v| {
                // Folders first, then by name (the card lists in FAT order).
                v.sort_by_key(|e| (!e.dir, e.name.to_ascii_uppercase()));
                v
            });
            sh.lock().unwrap().listing = Some(r);
            act.repaint();
        });
    }

    /// Read `name` in the current folder; show it, or save it here.
    fn read(&self, name: &str, show: bool) {
        if !self.act.may_act() || self.sh.lock().unwrap().reading.is_some() {
            return;
        }
        let Some(l) = self.act.svc.link() else { return };
        let full = if self.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.path)
        };
        let (sh, act, name) = (self.sh.clone(), self.act.clone(), name.to_string());
        sh.lock().unwrap().reading = Some((name.clone(), 0, 0));
        self.act.rt.spawn(async move {
            let sh2 = sh.clone();
            let a2 = act.clone();
            let n2 = name.clone();
            let r = sdcard::get(&l, &full, move |done, total| {
                sh2.lock().unwrap().reading = Some((n2.clone(), done, total));
                a2.repaint();
            })
            .await;
            sh.lock().unwrap().reading = None;
            match r {
                Ok(data) if show => {
                    sh.lock().unwrap().shown = Some((full, String::from_utf8_lossy(&data).into_owned()));
                }
                Ok(data) => {
                    let to = free_name(&download_dir(), &name);
                    match std::fs::File::create_new(&to).and_then(|mut f| std::io::Write::write_all(&mut f, &data)) {
                        Ok(()) => act.toast(format!("saved {} ({} bytes)", to.display(), data.len()), false),
                        Err(e) => act.toast(format!("{}: {e}", to.display()), true),
                    }
                }
                Err(e) => act.toast(format!("{full}: {e:#}"), true),
            }
            act.repaint();
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.act.svc.link().is_none() {
            ui.label(RichText::new("waiting for the radio...").color(skin::MUTED));
            return;
        }
        if self.listed.as_deref() != Some(self.path.as_str()) {
            self.list();
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.path.is_empty(), egui::Button::new("up")).clicked() {
                self.path = self
                    .path
                    .rsplit_once('/')
                    .map(|(a, _)| a.to_string())
                    .unwrap_or_default();
            }
            ui.label(
                RichText::new(format!("SD:/{}", self.path))
                    .color(skin::FREQ)
                    .monospace(),
            );
            if ui.small_button("reload").clicked() {
                self.listed = None;
            }
            ui.label(RichText::new("read only").small().color(skin::MUTED));
        });
        let (listing, reading, shown) = {
            let s = self.sh.lock().unwrap();
            (s.listing.clone(), s.reading.clone(), s.shown.clone())
        };
        if let Some((name, done, total)) = reading {
            ui.add(
                egui::ProgressBar::new(if total > 0 { done as f32 / total as f32 } else { 0.0 })
                    .text(format!("{name}: {done}/{total} bytes")),
            );
        }
        ui.separator();
        let mut open_dir: Option<String> = None;
        match listing {
            None => {
                ui.label(RichText::new("reading the card...").color(skin::MUTED));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(skin::ALERT));
            }
            Some(Ok(entries)) if entries.is_empty() => {
                ui.label(RichText::new("(empty)").color(skin::MUTED));
            }
            Some(Ok(entries)) => {
                egui::ScrollArea::vertical()
                    .id_salt("list")
                    .max_height(if shown.is_some() { 180.0 } else { f32::INFINITY })
                    .show(ui, |ui| {
                        egui::Grid::new("files").num_columns(3).striped(true).show(ui, |ui| {
                            for e in &entries {
                                if e.dir {
                                    if ui.link(format!("{}/", e.name)).clicked() {
                                        open_dir = Some(e.name.clone());
                                    }
                                    ui.label("");
                                    ui.label("");
                                } else {
                                    ui.label(&e.name);
                                    ui.label(RichText::new(e.size.to_string()).small().color(skin::MUTED));
                                    ui.horizontal(|ui| {
                                        let up = e.name.to_ascii_uppercase();
                                        let text = TEXT.iter().any(|x| up.ends_with(x)) && e.size <= VIEW_MAX;
                                        if text && ui.small_button("view").clicked() {
                                            self.read(&e.name, true);
                                        }
                                        if ui
                                            .small_button("save")
                                            .on_hover_text(format!("a copy in {}", download_dir().display()))
                                            .clicked()
                                        {
                                            self.read(&e.name, false);
                                        }
                                    });
                                }
                                ui.end_row();
                            }
                        });
                    });
            }
        }
        if let Some(d) = open_dir {
            self.path = if self.path.is_empty() {
                d
            } else {
                format!("{}/{d}", self.path)
            };
        }
        if let Some((name, text)) = shown {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&name).color(skin::ACCENT));
                if ui.small_button("close").clicked() {
                    self.sh.lock().unwrap().shown = None;
                }
            });
            egui::ScrollArea::both().id_salt("view").show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace().small()).selectable(true));
            });
        }
    }
}

/// `name` in `dir`, or `stem-1.ext`, `stem-2.ext` ... when it is taken: a
/// copy never replaces a file.
fn free_name(dir: &std::path::Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    std::iter::once(dir.join(name))
        .chain((1..).map(|n| dir.join(format!("{stem}-{n}{ext}"))))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(name))
}

/// The desktop's download folder (XDG user dirs), else ~/Downloads, else ~.
fn download_dir() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    if let Ok(t) = std::fs::read_to_string(cfg.join("user-dirs.dirs"))
        && let Some(v) = t.lines().find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR="))
    {
        let p = PathBuf::from(v.trim_matches('"').replace("$HOME", &home.to_string_lossy()));
        if p.is_dir() {
            return p;
        }
    }
    let d = home.join("Downloads");
    if d.is_dir() { d } else { home }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_copy_never_replaces_a_file() {
        let d = std::env::temp_dir().join(format!("sctrl-files-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(super::free_name(&d, "README.TXT"), d.join("README.TXT"));
        std::fs::write(d.join("README.TXT"), b"x").unwrap();
        assert_eq!(super::free_name(&d, "README.TXT"), d.join("README-1.TXT"));
        std::fs::write(d.join("README-1.TXT"), b"x").unwrap();
        assert_eq!(super::free_name(&d, "README.TXT"), d.join("README-2.TXT"));
        assert_eq!(super::free_name(&d, "NOEXT"), d.join("NOEXT"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
