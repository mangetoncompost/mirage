//! The eframe App: reads the engine's published `control::SNAPSHOT` each frame
//! (lock-free) and turns user actions into atomics / `control::Cmd`s. Never
//! locks engine state or blocks — UI stays at the speed of the GPU.

use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, RichText};

use super::theme;
use super::views;
use crate::control::{self, Cmd, Snapshot};

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Dashboard,
    Torrents,
    Speeds,
    Client,
    Logs,
}

pub struct RatioUpApp {
    pub snap: Arc<Snapshot>,
    pub view: View,
    pub sel: usize,
    pub cmd: String,
    /// Draft config edited in the Speeds view; applied on "Save".
    pub draft: DraftCfg,
}

/// Mutable rate draft mirrored from CONFIG; sliders edit this, Save stores it.
pub struct DraftCfg {
    pub min_up: u32,
    pub max_up: u32,
    pub min_dl: u32,
    pub max_dl: u32,
    pub numwant: u16,
    pub loaded: bool,
}
impl Default for DraftCfg {
    fn default() -> Self {
        Self { min_up: 8192, max_up: 2_097_152, min_dl: 8192, max_dl: 16_777_216, numwant: 80, loaded: false }
    }
}

impl RatioUpApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        // Register the context so the engine can wake repaint after each publish.
        let _ = control::EGUI.set(cc.egui_ctx.clone());
        Self {
            snap: control::SNAPSHOT.load_full(),
            view: View::Dashboard,
            sel: 0,
            cmd: String::new(),
            draft: DraftCfg::default(),
        }
    }

    fn load_draft(&mut self) {
        if self.draft.loaded {
            return;
        }
        let c = crate::CONFIG.load();
        self.draft.min_up = c.min_upload_rate;
        self.draft.max_up = c.max_upload_rate;
        self.draft.min_dl = c.min_download_rate;
        self.draft.max_dl = c.max_download_rate;
        self.draft.numwant = c.numwant.unwrap_or(80);
        self.draft.loaded = true;
    }
}

impl eframe::App for RatioUpApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Lock-free read of the latest engine snapshot.
        self.snap = control::SNAPSHOT.load_full();
        self.load_draft();
        // Idle heartbeat so countdowns/speeds stay live even without a publish.
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        self.top_bar(ui);
        self.bottom_bar(ui);
        egui::CentralPanel::default().show_inside(ui, |ui| match self.view {
            View::Dashboard => views::dashboard(self, ui),
            View::Torrents => views::torrents(self, ui),
            View::Speeds => views::speeds(self, ui),
            View::Client => views::client(self, ui),
            View::Logs => views::logs(self, ui),
        });
    }
}

impl RatioUpApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("›Ratio").color(theme::CY).strong());
                ui.separator();
                for (v, label) in [
                    (View::Dashboard, "1 dash"),
                    (View::Torrents, "2 tor"),
                    (View::Speeds, "3 spd"),
                    (View::Client, "4 cli"),
                    (View::Logs, "5 log"),
                ] {
                    let on = self.view == v;
                    let txt = RichText::new(label).color(if on { Color32::BLACK } else { theme::DIM });
                    let mut b = egui::Button::new(txt);
                    if on {
                        b = b.fill(theme::CY);
                    }
                    if ui.add(b).clicked() {
                        self.view = v;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // multiplier control
                    if ui.button("+").clicked() {
                        crate::torrent::bump_multiplier(1);
                    }
                    ui.label(
                        RichText::new(format!("x{:.2}", crate::torrent::speed_multiplier()))
                            .color(theme::YL),
                    );
                    if ui.button("−").clicked() {
                        crate::torrent::bump_multiplier(-1);
                    }
                    ui.separator();
                    let paused = control::is_paused();
                    let plabel = if paused { "▶ resume" } else { "⏸ pause" };
                    let col = if paused { theme::RD } else { theme::GN };
                    if ui.button(RichText::new(plabel).color(col)).clicked() {
                        control::toggle_paused();
                    }
                });
            });
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("bottom").show_inside(ui, |ui| {
            // status line
            ui.horizontal(|ui| {
                let s = &self.snap;
                ui.label(RichText::new(format!("{} torrents", s.rows.len())).color(theme::DIM));
                ui.label(RichText::new(format!("↑ {}", crate::utils::format_bytes_u64(s.total_uploaded))).color(theme::GN));
                ui.label(RichText::new(format!("up {}/s", crate::utils::format_bytes_u64(s.total_up_speed))).color(theme::YL));
                let ec = if s.error_count > 0 { theme::RD } else { theme::DIM };
                ui.label(RichText::new(format!("err {}", s.error_count)).color(ec));
                if s.paused {
                    ui.label(RichText::new("[PAUSED]").color(theme::RD));
                }
            });
            // command line
            ui.horizontal(|ui| {
                ui.label(RichText::new("ratioup›").color(theme::GN));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.cmd)
                        .desired_width(f32::INFINITY)
                        .hint_text("help · pause · mult 4 · add <path> · save"),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let c = std::mem::take(&mut self.cmd);
                    self.run_cmd(&c);
                    resp.request_focus();
                }
            });
        });
    }

    /// Tiny REPL mirroring the shell mockup verbs, driving the REAL engine.
    pub fn run_cmd(&mut self, raw: &str) {
        let a: Vec<&str> = raw.split_whitespace().collect();
        match a.first().map(|s| s.to_lowercase()).as_deref() {
            None => {}
            Some("pause") => control::set_paused(true),
            Some("resume") => control::set_paused(false),
            Some("mult") => {
                if let Some(n) = a.get(1).and_then(|s| s.parse::<f64>().ok()) {
                    let steps = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
                    if let Some(idx) = steps.iter().position(|&x| (x - n).abs() < 1e-9) {
                        crate::torrent::SPEED_STEP_IDX
                            .store(idx, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            Some("up") => { crate::torrent::bump_multiplier(1); }
            Some("down") => { crate::torrent::bump_multiplier(-1); }
            Some("add") => {
                if let Some(p) = a.get(1) {
                    control::send(Cmd::Add(std::path::PathBuf::from(p)));
                } else if let Some(path) = rfd::FileDialog::new().add_filter("torrent", &["torrent"]).pick_file() {
                    control::send(Cmd::Add(path));
                }
            }
            Some("save") => control::send(Cmd::SaveConfig),
            _ => {}
        }
    }
}
