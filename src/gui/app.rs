//! The eframe App: a pure monospace terminal screen (see `views`/`term`) reading
//! the lock-free `control::SNAPSHOT` and driving the real engine via atomics and
//! `control::Cmd`s. Keyboard-first (1-9 tabs, ↑↓ select, +/- speed, REPL).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::theme;
use super::views;
use crate::control::{self, Cmd, Snapshot};

#[derive(Clone, Copy, PartialEq)]
#[repr(usize)]
pub enum View {
    Dashboard = 0,
    Torrents = 1,
    Trackers = 2,
    Speeds = 3,
    Client = 4,
    Schedule = 5,
    Network = 6,
    Logs = 7,
    Config = 8,
}
impl View {
    fn from_index(i: usize) -> View {
        use View::*;
        [Dashboard, Torrents, Trackers, Speeds, Client, Schedule, Network, Logs, Config][i.min(8)]
    }
}

pub struct RatioUpApp {
    pub snap: Arc<Snapshot>,
    pub view: View,
    pub sel: usize,
    pub cmd: String,
    pub spin: usize,
    started: Instant,
    last_spin: Instant,
}

impl RatioUpApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let _ = control::EGUI.set(cc.egui_ctx.clone());
        Self {
            snap: control::SNAPSHOT.load_full(),
            view: View::Dashboard,
            sel: 0,
            cmd: String::new(),
            spin: 0,
            started: Instant::now(),
            last_spin: Instant::now(),
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Number of selectable settings rows per view (for ↑↓ clamp).
    fn sel_count(&self) -> usize {
        match self.view {
            View::Dashboard | View::Torrents | View::Trackers => self.snap.rows.len().max(1),
            View::Speeds => 6,
            _ => 1,
        }
    }

    fn selected_hash(&self) -> Option<[u8; 20]> {
        self.snap.rows.get(self.sel).map(|t| t.info_hash)
    }
}

impl eframe::App for RatioUpApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.snap = control::SNAPSHOT.load_full();
        // spinner advances ~8/s
        if self.last_spin.elapsed() >= Duration::from_millis(120) {
            self.spin = (self.spin + 1) % 10;
            self.last_spin = Instant::now();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(120));

        self.handle_keys(ui.ctx());

        egui::Panel::bottom("repl").show_inside(ui, |ui| self.repl(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(egui::Margin::same(6)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    views::render(self, ui);
                });
            });
    }
}

impl RatioUpApp {
    fn repl(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(egui::RichText::new("ratioup›").monospace().color(theme::GN));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.cmd)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("help · pause · mult 4 · add <path> · save · 1-9 tabs"),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let c = std::mem::take(&mut self.cmd);
                self.run_cmd(&c);
                resp.request_focus();
            }
        });
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Ignore global keys while typing a command (the TextEdit has focus then).
        let typing = ctx.memory(|m| m.focused().is_some());
        if typing {
            return;
        }
        ctx.input(|i| {
            for ev in &i.events {
                if let egui::Event::Key { key, pressed: true, .. } = ev {
                    use egui::Key::*;
                    match key {
                        Num1 => self.view = View::from_index(0),
                        Num2 => self.view = View::from_index(1),
                        Num3 => self.view = View::from_index(2),
                        Num4 => self.view = View::from_index(3),
                        Num5 => self.view = View::from_index(4),
                        Num6 => self.view = View::from_index(5),
                        Num7 => self.view = View::from_index(6),
                        Num8 => self.view = View::from_index(7),
                        Num9 => self.view = View::from_index(8),
                        ArrowDown => self.sel = (self.sel + 1).min(self.sel_count().saturating_sub(1)),
                        ArrowUp => self.sel = self.sel.saturating_sub(1),
                        Plus | Equals | ArrowRight => self.edit(1),
                        Minus | ArrowLeft => self.edit(-1),
                        P => {
                            if self.view == View::Torrents {
                                if let Some(h) = self.selected_hash() {
                                    control::send(Cmd::PauseTorrent(h));
                                }
                            } else {
                                control::toggle_paused();
                            }
                        }
                        R => {
                            if self.view == View::Torrents {
                                if let Some(h) = self.selected_hash() {
                                    control::send(Cmd::ResumeTorrent(h));
                                }
                            }
                        }
                        F => {
                            if let Some(h) = self.selected_hash() {
                                control::send(Cmd::ForceAnnounce(h));
                            }
                        }
                        X => {
                            if self.view == View::Torrents {
                                if let Some(h) = self.selected_hash() {
                                    control::send(Cmd::Remove(h));
                                }
                            }
                        }
                        K => {
                            if self.view == View::Client {
                                control::send(Cmd::ReinitClient);
                            }
                        }
                        S => {
                            if self.view == View::Config {
                                control::send(Cmd::SaveConfig);
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        // clamp selection if the list shrank
        self.sel = self.sel.min(self.sel_count().saturating_sub(1));
    }

    /// ◂▸ / +- editing: speed multiplier on most views; on Speeds, edit the
    /// selected setting row (rates / numwant).
    fn edit(&mut self, dir: i32) {
        if self.view == View::Speeds {
            let mut c = (**crate::CONFIG.load()).clone();
            let step = |v: u32, d: i32| -> u32 {
                if d > 0 { v.saturating_mul(2).min(268_435_456) } else { (v / 2).max(4096) }
            };
            match self.sel {
                0 => c.min_upload_rate = step(c.min_upload_rate, dir).min(c.max_upload_rate),
                1 => c.max_upload_rate = step(c.max_upload_rate, dir).max(c.min_upload_rate),
                2 => { crate::torrent::bump_multiplier(if dir > 0 { 1 } else { -1 }); return; }
                3 => c.min_download_rate = step(c.min_download_rate, dir).min(c.max_download_rate),
                4 => c.max_download_rate = step(c.max_download_rate, dir).max(c.min_download_rate),
                5 => {
                    let n = c.numwant.unwrap_or(80) as i32 + dir * 10;
                    c.numwant = Some(n.clamp(1, 200) as u16);
                }
                _ => {}
            }
            crate::CONFIG.store(Arc::new(c));
        } else {
            crate::torrent::bump_multiplier(if dir > 0 { 1 } else { -1 });
        }
    }

    /// Bottom-line REPL mirroring the shell mockup verbs, driving the real engine.
    pub fn run_cmd(&mut self, raw: &str) {
        let a: Vec<&str> = raw.split_whitespace().collect();
        match a.first().map(|s| s.to_lowercase()).as_deref() {
            None => {}
            Some("pause") => control::set_paused(true),
            Some("resume") => control::set_paused(false),
            Some("up") => { crate::torrent::bump_multiplier(1); }
            Some("down") => { crate::torrent::bump_multiplier(-1); }
            Some("mult") => {
                if let Some(n) = a.get(1).and_then(|s| s.parse::<f64>().ok()) {
                    let steps = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
                    if let Some(idx) = steps.iter().position(|&x| (x - n).abs() < 1e-9) {
                        crate::torrent::SPEED_STEP_IDX.store(idx, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            Some("add") => {
                if let Some(p) = a.get(1) {
                    control::send(Cmd::Add(std::path::PathBuf::from(p)));
                } else if let Some(path) = rfd::FileDialog::new().add_filter("torrent", &["torrent"]).pick_file() {
                    control::send(Cmd::Add(path));
                }
            }
            Some("rm") => {
                if let Some(h) = self.selected_hash() {
                    control::send(Cmd::Remove(h));
                }
            }
            Some("save") => control::send(Cmd::SaveConfig),
            Some(v) if v.parse::<usize>().is_ok() => {
                let n: usize = v.parse().unwrap();
                if (1..=9).contains(&n) {
                    self.view = View::from_index(n - 1);
                }
            }
            _ => {}
        }
    }
}
