//! The shell-styled views rendered into the central panel. All data comes from
//! the lock-free snapshot; all actions send atomics/Cmds — never block.

use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};

use super::app::RatioUpApp;
use super::theme;
use crate::control::{self, Cmd, TorrentView};
use crate::utils::format_bytes_u64 as fb;

fn dot(t: &TorrentView) -> (RichText, Color32) {
    let c = if t.error_count > 0 {
        theme::RD
    } else if t.paused {
        theme::DIM
    } else if t.downloading {
        theme::YL
    } else if t.up_speed > 0 {
        theme::GN
    } else {
        theme::DIM
    };
    (RichText::new("●").color(c), c)
}

fn mmss(s: u64) -> String {
    format!("{:02}:{:02}", s / 60, s % 60)
}

pub fn dashboard(app: &mut RatioUpApp, ui: &mut egui::Ui) {
    let snap = app.snap.clone();
    if let Some(cl) = &snap.client {
        ui.horizontal(|ui| {
            ui.label(RichText::new("client").color(theme::DIM));
            ui.label(RichText::new(&cl.name).color(theme::FG).strong());
            ui.label(RichText::new("peer").color(theme::DIM));
            ui.label(RichText::new(&cl.peer_id).color(theme::DIM));
            ui.label(RichText::new("key").color(theme::DIM));
            ui.label(RichText::new(&cl.key).color(theme::DIM));
        });
    }
    ui.separator();
    torrent_table(app, ui, true);
}

pub fn torrents(app: &mut RatioUpApp, ui: &mut egui::Ui) {
    ui.label(RichText::new("Torrents — click a row, act below").color(theme::DIM));
    torrent_table(app, ui, false);
    ui.separator();
    // per-selected actions
    let snap = app.snap.clone();
    if let Some(t) = snap.rows.get(app.sel) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&t.name).color(theme::CY));
            if ui.button(if t.paused { "resume" } else { "pause" }).clicked() {
                control::send(if t.paused { Cmd::ResumeTorrent(t.info_hash) } else { Cmd::PauseTorrent(t.info_hash) });
            }
            if ui.button("force announce").clicked() {
                control::send(Cmd::ForceAnnounce(t.info_hash));
            }
            if ui.button(RichText::new("remove").color(theme::RD)).clicked() {
                control::send(Cmd::Remove(t.info_hash));
            }
        });
    }
}

fn torrent_table(app: &mut RatioUpApp, ui: &mut egui::Ui, compact: bool) {
    let snap = app.snap.clone();
    let mut clicked: Option<usize> = None;
    TableBuilder::new(ui)
        .striped(true)
        .column(Column::auto().at_least(18.0))
        .column(Column::remainder().at_least(160.0))
        .column(Column::auto().at_least(40.0))
        .column(Column::auto().at_least(40.0))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(64.0))
        .column(Column::auto().at_least(60.0))
        .header(18.0, |mut h| {
            for t in ["", "TORRENT", "S", "L", "↑ SPEED", "UPLOADED", "NEXT", "STATE"] {
                h.col(|ui| { ui.label(RichText::new(t).color(theme::DIM).small()); });
            }
        })
        .body(|mut body| {
            for (i, t) in snap.rows.iter().enumerate() {
                body.row(20.0, |mut row| {
                    let (d, _) = dot(t);
                    row.col(|ui| { ui.label(d); });
                    row.col(|ui| {
                        let name = RichText::new(&t.name).color(if i == app.sel { theme::FG } else { theme::FG });
                        if ui.selectable_label(i == app.sel, name).clicked() { clicked = Some(i); }
                    });
                    let scol = if t.seeders > 5 { theme::GN } else if t.seeders > 0 { theme::YL } else { theme::DIM };
                    row.col(|ui| { ui.label(RichText::new(if t.error_count>0 {"-".into()} else {t.seeders.to_string()}).color(scol)); });
                    let lcol = if t.leechers > 0 { theme::GN } else { theme::DIM };
                    row.col(|ui| { ui.label(RichText::new(if t.error_count>0 {"-".into()} else {t.leechers.to_string()}).color(lcol)); });
                    row.col(|ui| {
                        let (s, c) = if t.downloading {
                            (format!("DL {}%", t.dl_percent), theme::YL)
                        } else if t.up_speed > 0 {
                            (format!("{}/s", fb(t.up_speed as u64)), theme::YL)
                        } else {
                            ("idle".into(), theme::DIM)
                        };
                        ui.label(RichText::new(s).color(c));
                    });
                    row.col(|ui| { ui.label(RichText::new(fb(t.uploaded)).color(theme::FG)); });
                    row.col(|ui| {
                        let nx = if t.paused || t.error_count > 0 { "--".into() } else { mmss(t.secs_to_announce) };
                        ui.label(RichText::new(nx).color(theme::DIM));
                    });
                    row.col(|ui| {
                        let st = if t.downloading { "leech" } else if t.paused { "paused" } else if t.error_count>0 { "error" } else { "seed" };
                        ui.label(RichText::new(st).color(theme::DIM));
                    });
                });
            }
        });
    if let Some(i) = clicked { app.sel = i; }
    let _ = compact;
}

pub fn speeds(app: &mut RatioUpApp, ui: &mut egui::Ui) {
    ui.heading(RichText::new("Speeds").color(theme::CY));
    ui.add_space(6.0);
    let d = &mut app.draft;
    let mb = 1024 * 1024;
    rate_slider(ui, "min upload", &mut d.min_up, 8 * 1024, 64 * mb);
    rate_slider(ui, "max upload", &mut d.max_up, 8 * 1024, 64 * mb);
    rate_slider(ui, "min download", &mut d.min_dl, 8 * 1024, 128 * mb);
    rate_slider(ui, "max download", &mut d.max_dl, 8 * 1024, 128 * mb);
    ui.horizontal(|ui| {
        ui.label(RichText::new("numwant").color(theme::DIM));
        ui.add(egui::DragValue::new(&mut d.numwant).range(1..=200));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("multiplier").color(theme::DIM));
        ui.label(RichText::new(format!("x{:.2}", crate::torrent::speed_multiplier())).color(theme::YL));
        if ui.button("−").clicked() { crate::torrent::bump_multiplier(-1); }
        if ui.button("+").clicked() { crate::torrent::bump_multiplier(1); }
    });
    ui.add_space(8.0);
    if ui.button(RichText::new("apply + save config").color(theme::GN)).clicked() {
        // clamp and store live
        let mut c = (**crate::CONFIG.load()).clone();
        c.min_upload_rate = d.min_up.min(d.max_up);
        c.max_upload_rate = d.max_up.max(d.min_up);
        c.min_download_rate = d.min_dl.min(d.max_dl);
        c.max_download_rate = d.max_dl.max(d.min_dl);
        c.numwant = Some(d.numwant);
        crate::CONFIG.store(std::sync::Arc::new(c));
        control::send(Cmd::SaveConfig);
    }
}

fn rate_slider(ui: &mut egui::Ui, label: &str, val: &mut u32, lo: u32, hi: u32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{label:<14}")).color(theme::DIM));
        let mut v = *val as f64;
        ui.add(egui::Slider::new(&mut v, lo as f64..=hi as f64).logarithmic(true).show_value(false));
        *val = v as u32;
        ui.label(RichText::new(format!("{}/s", fb(*val as u64))).color(theme::YL));
    });
}

pub fn client(app: &mut RatioUpApp, ui: &mut egui::Ui) {
    ui.heading(RichText::new("Client & Stealth").color(theme::CY));
    ui.add_space(6.0);
    let snap = app.snap.clone();
    if let Some(cl) = &snap.client {
        kv(ui, "client", &cl.name, theme::FG);
        kv(ui, "peer_id", &cl.peer_id, theme::DIM);
        kv(ui, "user-agent", &cl.user_agent, theme::DIM);
        kv(ui, "key", &cl.key, theme::DIM);
    }
    ui.add_space(8.0);
    if ui.button("re-init client (new key)").clicked() {
        control::send(Cmd::ReinitClient);
    }
    ui.add_space(8.0);
    ui.label(RichText::new("what the tracker sees:").color(theme::DIM));
    if let Some(cl) = &snap.client {
        ui.label(
            RichText::new(format!(
                "GET /announce?info_hash=…&peer_id={}&key={}&numwant={}&compact=1&event=started",
                cl.peer_id, cl.key, crate::CONFIG.load().numwant.unwrap_or(80)
            ))
            .color(theme::DIM)
            .monospace(),
        );
    }
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str, c: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{k:<12}")).color(theme::DIM));
        ui.label(RichText::new(v).color(c));
    });
}

pub fn logs(_app: &mut RatioUpApp, ui: &mut egui::Ui) {
    ui.heading(RichText::new("Logs").color(theme::CY));
    ui.add_space(6.0);
    ui.label(
        RichText::new("Live tracing logs print to stderr (run from a terminal to see them).\nThe dashboard event feed is in the TTY mode.")
            .color(theme::DIM),
    );
}
