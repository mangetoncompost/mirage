//! All views rendered as pure monospace "terminal" screens (box-drawing,
//! aligned columns, ASCII bars) reading the live snapshot — the native twin of
//! mockup/RatioUp_shell.html. Interaction is keyboard + the bottom REPL.

use super::app::{RatioUpApp, View};
use super::term::{self, Line, Screen, bar, lpad, rpad};
use super::theme;
use crate::control::TorrentView;
use crate::utils::format_bytes_u64 as fb;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dotcol(t: &TorrentView) -> egui::Color32 {
    if t.error_count > 0 {
        theme::RD
    } else if t.downloading {
        theme::YL
    } else if t.up_speed > 0 {
        theme::GN
    } else {
        theme::DIM
    }
}
fn mmss(s: u64) -> String {
    format!("{:02}:{:02}", s / 60, s % 60)
}
fn hms(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Header + tab strip shared by every view.
fn chrome(app: &RatioUpApp, sc: &mut Screen) {
    let mult = crate::torrent::speed_multiplier();
    let up = app.uptime_secs();
    // top border with title + spinner + mult + uptime
    let l = sc.line();
    l.push("╭", theme::LINE);
    l.push(" ›Ratio ", theme::CY);
    let right = format!(" {}  x{:.2}  up {} ", SPIN[app.spin % 10], mult, hms(up));
    let used = 1 + " ›Ratio ".chars().count() + right.chars().count();
    if used < term::W + 2 {
        l.push("─".repeat(term::W + 2 - used), theme::LINE);
    }
    l.push(right, theme::DIM);
    l.push("╮", theme::LINE);
    // tab strip
    sc.bordered(|l| {
        for (i, name) in ["dash", "tor", "trk", "spd", "cli", "sch", "net", "log", "cfg"]
            .iter()
            .enumerate()
        {
            let on = app.view as usize == i;
            let lbl = format!(" [{}]{} ", i + 1, name);
            l.push(lbl, if on { theme::CY } else { theme::DIM });
        }
    });
    sc.rule("├", "┤", None);
}

pub fn render(app: &mut RatioUpApp, ui: &mut egui::Ui) {
    let mut sc = Screen::new();
    chrome(app, &mut sc);
    match app.view {
        View::Dashboard => v_dash(app, &mut sc),
        View::Torrents => v_tor(app, &mut sc),
        View::Trackers => v_trk(app, &mut sc),
        View::Speeds => v_spd(app, &mut sc),
        View::Client => v_cli(app, &mut sc),
        View::Schedule => v_sch(app, &mut sc),
        View::Network => v_net(app, &mut sc),
        View::Logs => v_log(app, &mut sc),
        View::Config => v_cfg(app, &mut sc),
    }
    sc.rule("╰", "╯", None);
    sc.show(ui);
}

fn v_dash(app: &RatioUpApp, sc: &mut Screen) {
    let snap = &app.snap;
    if let Some(cl) = &snap.client {
        sc.bordered(|l| {
            l.push(" client ", theme::DIM);
            l.push(cl.name.clone(), theme::FG);
            l.push("  peer ", theme::DIM);
            l.push(format!("{}…", cl.peer_id.chars().take(14).collect::<String>()), theme::DIM);
            l.push("  key ", theme::DIM);
            l.push(cl.key.clone(), theme::DIM);
        });
    }
    // header row
    sc.bordered(|l| {
        l.push(
            format!(
                " {}{}{}{}{}{}",
                lpad("TORRENT", 24),
                lpad("S", 5),
                lpad("L", 5),
                lpad("UP/s", 9),
                lpad("TOTAL", 7),
                "NEXT"
            ),
            theme::DIM,
        );
    });
    let sel = app.sel;
    for (i, t) in snap.rows.iter().take(12).enumerate() {
        sc.bordered(|l| torrent_row(l, t, i == sel));
    }
    // selected progress
    if let Some(t) = snap.rows.get(sel) {
        sc.bordered(|l| {
            l.push(format!(" {} ", lpad(if t.downloading { "downloading" } else { "next announce" }, 13)), theme::DIM);
            let (done, total) = if t.downloading {
                (t.dl_percent as u64, 100)
            } else {
                (t.interval.saturating_sub(t.secs_to_announce), t.interval.max(1))
            };
            bar(l, done, total, 30, t.downloading);
        });
    }
    sc.rule("├", "┤", Some("recent"));
    // footer totals (the engine doesn't expose an event feed to the snapshot yet)
    sc.bordered(|l| {
        l.push(" tip: ", theme::DIM);
        l.push("logs print to the terminal you launched from", theme::DIM);
    });
    sc.rule("├", "┤", None);
    footer(app, sc);
}

fn torrent_row(l: &mut Line, t: &TorrentView, sel: bool) {
    l.push(" ", theme::FG);
    l.push("●", dotcol(t));
    l.push(" ", theme::FG);
    l.push(lpad(&t.name, 22), if sel { theme::CY } else { theme::FG });
    l.push(rpad(if t.error_count > 0 { "-".into() } else { t.seeders.to_string() }, 5),
        if t.error_count > 0 { theme::DIM } else { theme::GN });
    l.push(rpad(if t.error_count > 0 { "-".into() } else { t.leechers.to_string() }, 5),
        if t.leechers > 0 { theme::GN } else { theme::DIM });
    let sp = if t.downloading {
        format!("DL{}%", t.dl_percent)
    } else if t.up_speed > 0 {
        fb(t.up_speed as u64)
    } else {
        "idle".into()
    };
    l.push(lpad(format!("  {sp}"), 9), if t.downloading || t.up_speed > 0 { theme::YL } else { theme::DIM });
    l.push(lpad(format!(" {}", fb(t.uploaded)), 7), theme::FG);
    let nx = if t.error_count > 0 { "--".into() } else { mmss(t.secs_to_announce) };
    l.push(format!(" {nx}"), theme::DIM);
}

fn v_tor(app: &RatioUpApp, sc: &mut Screen) {
    sc.bordered(|l| {
        l.push(
            format!(" {}{}{}{}{}{}", lpad(" #", 4), lpad("NAME", 22), lpad("STATE", 9), lpad("S", 5), lpad("L", 5), "UPLOAD"),
            theme::DIM,
        );
    });
    for (i, t) in app.snap.rows.iter().enumerate() {
        sc.bordered(|l| {
            l.push(rpad(i + 1, 3), theme::DIM);
            l.push(" ", theme::FG);
            l.push("●", dotcol(t));
            l.push(" ", theme::FG);
            l.push(lpad(&t.name, 21), if i == app.sel { theme::CY } else { theme::FG });
            let st = if t.downloading { format!("DL{}%", t.dl_percent) } else if t.error_count > 0 { "error".into() } else { "seed".into() };
            l.push(lpad(st, 9), theme::DIM);
            l.push(rpad(t.seeders, 5), theme::GN);
            l.push(rpad(t.leechers, 5), if t.leechers > 0 { theme::GN } else { theme::DIM });
            l.push(lpad(format!(" {}", fb(t.uploaded)), 8), theme::FG);
        });
    }
    sc.rule("├", "┤", None);
    sc.bordered(|l| {
        l.push(" ↵ ", theme::CY);
        l.push("detail   ", theme::DIM);
        l.push("p ", theme::CY);
        l.push("pause-row   ", theme::DIM);
        l.push("f ", theme::CY);
        l.push("force   ", theme::DIM);
        l.push("x ", theme::RD);
        l.push("remove", theme::DIM);
    });
    footer(app, sc);
}

fn v_trk(app: &RatioUpApp, sc: &mut Screen) {
    sc.bordered(|l| l.push(" per-torrent trackers (snapshot)", theme::DIM));
    for t in app.snap.rows.iter() {
        sc.bordered(|l| {
            l.push(" ", theme::FG);
            l.push("●", dotcol(t));
            l.push(format!(" {}", lpad(&t.name, 40)), theme::CY);
            l.push(format!("S{} L{}", t.seeders, t.leechers), theme::GN);
        });
    }
    sc.bordered(|l| l.push(" (per-tracker S/L/RTT are in the engine; surfaced here next)", theme::DIM));
    footer(app, sc);
}

fn v_spd(app: &RatioUpApp, sc: &mut Screen) {
    let c = crate::CONFIG.load();
    sc.rule("├", "┤", Some("upload band"));
    setting_num(sc, app, 0, "min upload", &format!("{}/s", fb(c.min_upload_rate as u64)));
    setting_num(sc, app, 1, "max upload", &format!("{}/s", fb(c.max_upload_rate as u64)));
    setting_sel(sc, app, 2, "multiplier", &format!("x{:.2}", crate::torrent::speed_multiplier()));
    sc.rule("├", "┤", Some("download phase"));
    setting_num(sc, app, 3, "min download", &format!("{}/s", fb(c.min_download_rate as u64)));
    setting_num(sc, app, 4, "max download", &format!("{}/s", fb(c.max_download_rate as u64)));
    setting_num(sc, app, 5, "numwant", &c.numwant.unwrap_or(80).to_string());
    sc.rule("├", "┤", Some("total upload (live)"));
    sc.bordered(|l| {
        l.push("  up ", theme::DIM);
        l.push(format!("{}/s", fb(app.snap.total_up_speed)), theme::YL);
        l.push("   Σ ", theme::DIM);
        l.push(fb(app.snap.total_uploaded), theme::GN);
    });
    footer(app, sc);
}

fn v_cli(app: &RatioUpApp, sc: &mut Screen) {
    if let Some(cl) = &app.snap.client {
        kv(sc, "client", &cl.name, theme::FG);
        kv(sc, "peer_id", &cl.peer_id, theme::DIM);
        kv(sc, "user-agent", &cl.user_agent, theme::DIM);
        kv(sc, "key", &cl.key, theme::DIM);
        let c = crate::CONFIG.load();
        kv(sc, "download phase", "on (leech → completed → seed)", theme::GN);
        sc.rule("├", "┤", Some("what the tracker sees"));
        sc.bordered(|l| {
            l.push(
                format!(" GET /announce?peer_id={}&numwant={}&key={}&event=started",
                    cl.peer_id, c.numwant.unwrap_or(80), cl.key),
                theme::DIM,
            );
        });
        sc.bordered(|l| {
            l.push(" k ", theme::CY);
            l.push("re-init client (new key)", theme::DIM);
        });
    }
    footer(app, sc);
}

fn v_sch(app: &RatioUpApp, sc: &mut Screen) {
    sc.bordered(|l| l.push(" seed mode    always  (night/custom: roadmap)", theme::DIM));
    sc.bordered(|l| {
        l.push(" global       ", theme::DIM);
        if app.snap.paused {
            l.push("[ ] paused", theme::RD);
        } else {
            l.push("[x] running", theme::GN);
        }
        l.push("   (p to toggle)", theme::DIM);
    });
    footer(app, sc);
}

fn v_net(app: &RatioUpApp, sc: &mut Screen) {
    let c = crate::CONFIG.load();
    kv(sc, "port", &c.port.to_string(), theme::YL);
    kv(sc, "numwant", &c.numwant.unwrap_or(80).to_string(), theme::YL);
    kv(sc, "torrent dir", &c.torrent_dir.display().to_string(), theme::DIM);
    kv(sc, "pid file", if c.use_pid_file { "on" } else { "off" }, theme::DIM);
    footer(app, sc);
}

fn v_log(app: &RatioUpApp, sc: &mut Screen) {
    sc.bordered(|l| l.push(" live tracing logs print to the launching terminal", theme::DIM));
    sc.bordered(|l| l.push(" (run RatioUp from a terminal to see them, or use TTY mode)", theme::DIM));
    footer(app, sc);
}

fn v_cfg(app: &RatioUpApp, sc: &mut Screen) {
    let c = crate::CONFIG.load();
    sc.rule("├", "┤", Some("config.toml"));
    let kvc = |sc: &mut Screen, k: &str, v: String, col| {
        sc.bordered(|l| {
            l.push(format!(" {} = ", lpad(k, 18)), theme::DIM);
            l.push(v, col);
        });
    };
    kvc(sc, "client", format!("\"{}\"", c.client), theme::GN);
    kvc(sc, "port", c.port.to_string(), theme::YL);
    kvc(sc, "min_upload_rate", c.min_upload_rate.to_string(), theme::YL);
    kvc(sc, "max_upload_rate", c.max_upload_rate.to_string(), theme::YL);
    kvc(sc, "min_download_rate", c.min_download_rate.to_string(), theme::YL);
    kvc(sc, "max_download_rate", c.max_download_rate.to_string(), theme::YL);
    kvc(sc, "numwant", c.numwant.unwrap_or(80).to_string(), theme::YL);
    sc.bordered(|l| {
        l.push(" s ", theme::CY);
        l.push("save config.toml", theme::DIM);
    });
    footer(app, sc);
}

fn footer(app: &RatioUpApp, sc: &mut Screen) {
    let s = &app.snap;
    sc.bordered(|l| {
        l.push(format!(" {} torrents  ", s.rows.len()), theme::FG);
        l.push(format!("↑ {}  ", fb(s.total_uploaded)), theme::GN);
        l.push(format!("up {}/s  ", fb(s.total_up_speed)), theme::YL);
        if s.error_count > 0 {
            l.push(format!("err {}", s.error_count), theme::RD);
        } else {
            l.push("err 0", theme::DIM);
        }
        if s.paused {
            l.push("  [PAUSED]", theme::RD);
        }
    });
}

fn kv(sc: &mut Screen, k: &str, v: &str, col: egui::Color32) {
    sc.bordered(|l| {
        l.push(format!(" {} ", lpad(k, 14)), theme::DIM);
        l.push(v.to_string(), col);
    });
}

// settings rows highlight when selected (app.sel indexes selectable rows in the view)
fn setting_num(sc: &mut Screen, app: &RatioUpApp, idx: usize, label: &str, val: &str) {
    let on = app.sel == idx;
    sc.bordered(|l| {
        l.push(format!(" {} ", lpad(label, 14)), if on { theme::CY } else { theme::DIM });
        l.push("[ ", theme::DIM);
        l.push(lpad(val, 13), theme::YL);
        l.push("]", theme::DIM);
        if on {
            l.push("  ◂ ◂/▸ edit ▸", theme::CY);
        }
    });
}
fn setting_sel(sc: &mut Screen, app: &RatioUpApp, idx: usize, label: &str, val: &str) {
    let on = app.sel == idx;
    sc.bordered(|l| {
        l.push(format!(" {} ", lpad(label, 14)), if on { theme::CY } else { theme::DIM });
        l.push("◂ ", theme::DIM);
        l.push(lpad(val, 13), theme::CY);
        l.push("▸", theme::DIM);
    });
}
