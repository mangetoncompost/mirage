//! Pure frame rendering: turns a [`Frame`] snapshot into one ready-to-write
//! `String` of ANSI. No I/O, no locks, no globals beyond reading env once for
//! the color/unicode gates. Every line is truncated to the terminal width and
//! followed by a clear-to-EOL so a shorter frame never leaves stale glyphs.

use crate::ui::events::{EventKind, UiEvent};
use crate::ui::snapshot::Frame;
use crate::utils::{format_bytes, format_bytes_u64};

// --- raw ANSI sequences -----------------------------------------------------
const CLR_EOL: &str = "\x1b[K"; // clear from cursor to end of line
const CLR_BELOW: &str = "\x1b[J"; // clear from cursor to end of screen
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

// --- spinner ----------------------------------------------------------------
const SPINNER_U: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_A: [&str; 4] = ["|", "/", "-", "\\"];

/// Terminal capabilities, resolved once per frame from the environment.
#[derive(Clone, Copy)]
struct Caps {
    color: bool,
    truecolor: bool,
    utf8: bool,
}

impl Caps {
    fn detect() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none();
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);
        let utf8 = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LANG"))
            .map(|v| {
                let up = v.to_uppercase();
                up.contains("UTF-8") || up.contains("UTF8")
            })
            .unwrap_or(false);
        Caps {
            color,
            truecolor,
            utf8,
        }
    }

    /// Foreground color span (empty when colors are disabled).
    fn fg(&self, rgb: (u8, u8, u8), idx256: u8) -> String {
        if !self.color {
            return String::new();
        }
        if self.truecolor {
            format!("\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2)
        } else {
            format!("\x1b[38;5;{idx256}m")
        }
    }

    fn reset(&self) -> &'static str {
        if self.color { RESET } else { "" }
    }
}

// palette helpers -------------------------------------------------------------
fn c_header(c: &Caps) -> String {
    c.fg((0x4e, 0xc9, 0xff), 45)
} // cyan
fn c_ok(c: &Caps) -> String {
    c.fg((0x4e, 0xc9, 0x6a), 78)
} // green
fn c_warn(c: &Caps) -> String {
    c.fg((0xe5, 0xc0, 0x7b), 179)
} // yellow
fn c_err(c: &Caps) -> String {
    c.fg((0xe0, 0x6c, 0x75), 167)
} // red
fn c_dim(c: &Caps) -> String {
    c.fg((0x80, 0x80, 0x80), 244)
} // gray

/// Display width of a string in terminal cells: skips nothing (no ANSI here),
/// counts most chars as 1 and known wide ranges (emoji) as 2. Used by builders
/// that compose plain (un-escaped) fragments.
fn dwidth(s: &str) -> usize {
    s.chars().map(cell_width).sum()
}

/// Terminal cell width of a single char: 2 for emoji / wide symbols, 0 for
/// zero-width combining marks, 1 otherwise. Deliberately small (no unicode-width
/// dep): covers the glyphs we actually emit (box, braille, arrows, emoji).
fn cell_width(ch: char) -> usize {
    let c = ch as u32;
    if c == 0 {
        return 0;
    }
    // zero-width joiner / variation selectors / combining marks
    if c == 0x200D || (0xFE00..=0xFE0F).contains(&c) || (0x0300..=0x036F).contains(&c) {
        return 0;
    }
    // common wide / emoji blocks (enough for our event glyphs)
    let wide = (0x1100..=0x115F).contains(&c)            // Hangul Jamo
        || (0x2600..=0x27BF).contains(&c)                // Misc symbols + Dingbats (✖ ➕ ➖ ⚠)
        || (0x2B00..=0x2BFF).contains(&c)                // arrows/symbols (⬆)
        || (0x1F000..=0x1FAFF).contains(&c)              // emoji (📡 🌱 🔌)
        || (0x2300..=0x23FF).contains(&c)                // misc technical (some emoji)
        || (0xFE30..=0xFE4F).contains(&c)
        || (0x3000..=0x303F).contains(&c)                // CJK symbols
        || (0x3040..=0x9FFF).contains(&c)                // CJK
        || (0xAC00..=0xD7A3).contains(&c)                // Hangul syllables
        || (0xF900..=0xFAFF).contains(&c)                // CJK compat
        || (0xFF00..=0xFF60).contains(&c)                // fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&c);
    if wide { 2 } else { 1 }
}

/// Measure the *visible* cell width of a string that may contain ANSI escapes,
/// and truncate it to `max` visible cells (preserving escapes, appending RESET
/// if a color was open). Returns (clamped_string, visible_width).
fn clamp_visible(s: &str, max: usize, color: bool) -> (String, usize) {
    let mut out = String::with_capacity(s.len());
    let mut vis = 0usize;
    let mut chars = s.chars().peekable();
    let mut truncated = false;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // copy the whole CSI/escape sequence verbatim (it has 0 width)
            out.push(ch);
            // ESC [ ... <final byte 0x40..0x7e>, or ESC <single>
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if ('\x40'..='\x7e').contains(&c2) {
                        break;
                    }
                }
            } else if let Some(c2) = chars.next() {
                out.push(c2);
            }
            continue;
        }
        let cw = cell_width(ch);
        if vis + cw > max {
            truncated = true;
            break;
        }
        out.push(ch);
        vis += cw;
    }
    if truncated && color {
        out.push_str(RESET);
    }
    (out, vis)
}

/// Truncate `s` to at most `max` display *cells* (not chars), appending a
/// 1-cell ellipsis if cut. Cell-aware so wide glyphs (emoji/CJK) never push the
/// result past `max` — callers rely on `dwidth(truncate(..)) <= max` to keep
/// column widths exact.
fn truncate(s: &str, max: usize, utf8: bool) -> String {
    if dwidth(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve 1 cell for the ellipsis, then take chars by accumulated cell width.
    let budget = max - 1;
    let mut out = String::with_capacity(s.len());
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = cell_width(ch);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(if utf8 { "…" } else { "~" });
    out
}

// box-drawing glyph set ------------------------------------------------------
struct Box {
    h: &'static str,
    v: &'static str,
    tl: &'static str,
    tr: &'static str,
    bl: &'static str,
    br: &'static str,
    ml: &'static str, // tee pointing right (left edge)
    mr: &'static str, // tee pointing left  (right edge)
}

fn box_set(utf8: bool) -> Box {
    if utf8 {
        Box {
            h: "─",
            v: "│",
            tl: "┌",
            tr: "┐",
            bl: "└",
            br: "┘",
            ml: "├",
            mr: "┤",
        }
    } else {
        Box {
            h: "-",
            v: "|",
            tl: "+",
            tr: "+",
            bl: "+",
            br: "+",
            ml: "+",
            mr: "+",
        }
    }
}

/// How many feed lines fit after header + table + footer.
pub fn feed_capacity(term_h: u16, n_rows: usize, max_visible_rows: usize) -> usize {
    // header: top border + title + client + sep        = 4
    // table:  column header + visible rows             = 1 + min(n_rows, cap)
    // feed:   "recent" separator + lines + ...
    // footer: separator + totals + bottom border       = 3
    let shown_rows = n_rows.min(max_visible_rows) + if n_rows > max_visible_rows { 1 } else { 0 };
    let fixed = 4 + (1 + shown_rows) + 1 /* feed sep */ + 3;
    (term_h as usize).saturating_sub(fixed).min(50)
}

/// Maximum torrent rows we draw before collapsing the rest into "(+N more)".
const MAX_VISIBLE_ROWS: usize = 12;

/// Build the whole frame as one ANSI string ready for `draw::paint`.
pub fn build_frame(f: &Frame, width: u16) -> String {
    let c = Caps::detect();
    let b = box_set(c.utf8);
    let w = width.max(20) as usize;
    let inner = w.saturating_sub(2); // space between the two vertical borders

    let mut out = String::with_capacity(w * 24);

    // helper: a horizontal rule with an optional inline label, e.g. "├─ recent ─┤"
    let rule = |left: &str, right: &str, label: Option<&str>| -> String {
        let mut s = String::new();
        s.push_str(left);
        match label {
            Some(lbl) => {
                let lbl = format!("{} {} ", b.h, lbl);
                let lblw = dwidth(&lbl);
                s.push_str(&lbl);
                for _ in 0..inner.saturating_sub(lblw) {
                    s.push_str(b.h);
                }
            }
            None => {
                for _ in 0..inner {
                    s.push_str(b.h);
                }
            }
        }
        s.push_str(right);
        s
    };

    // Bordered content line: "│ <content padded to inner> │". The content
    // carries its own ANSI; we measure its *visible* width ourselves (skipping
    // escapes, counting wide glyphs as 2) so the right border always lands at
    // column `w` no matter how the content was built. The `vis` argument is
    // ignored except as a hint — measuring is authoritative.
    let line = |out: &mut String, content: &str, _vis: usize| {
        let avail = inner.saturating_sub(2); // leading + trailing space
        let (clamped, vis) = clamp_visible(content, avail, c.color);
        out.push_str(&c_dim(&c));
        out.push_str(b.v);
        out.push_str(c.reset());
        out.push(' ');
        out.push_str(&clamped);
        let pad = avail.saturating_sub(vis);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push(' ');
        out.push_str(&c_dim(&c));
        out.push_str(b.v);
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    };

    // ---- header -------------------------------------------------------------
    let spin = if c.utf8 {
        SPINNER_U[f.spinner % SPINNER_U.len()]
    } else {
        SPINNER_A[f.spinner % SPINNER_A.len()]
    };
    let uptime = fmt_hms((f.now - f.started).num_seconds().max(0) as u64);
    // top border with title on the left and spinner+uptime on the right:
    //   ┌─ RatioUp ───────────────────── ⠹ 02:14:07 ─┐
    {
        let title = " RatioUp ";
        // Global upload multiplier (always x1.00 in non-TTY mode). Two decimals
        // so the 0.25/0.50 sub-unity steps read exactly. The fill computation
        // below subtracts dwidth(&right), so the wider segment is auto-accounted.
        let mult = crate::torrent::speed_multiplier();
        let right = format!(" x{mult:.2} {spin} {uptime} ");
        // inner cells consumed by title + right; the rest is horizontal fill.
        let fill = inner.saturating_sub(dwidth(title) + dwidth(&right));
        let mut top = String::new();
        top.push_str(&c_dim(&c));
        top.push_str(b.tl);
        top.push_str(c.reset());
        top.push_str(&c_header(&c));
        top.push_str(BOLD_if(c.color));
        top.push_str(title);
        top.push_str(c.reset());
        top.push_str(&c_dim(&c));
        for _ in 0..fill {
            top.push_str(b.h);
        }
        top.push_str(c.reset());
        top.push_str(&c_header(&c));
        top.push_str(&right);
        top.push_str(c.reset());
        top.push_str(&c_dim(&c));
        top.push_str(b.tr);
        top.push_str(c.reset());
        top.push_str(CLR_EOL);
        top.push_str("\r\n");
        out.push_str(&top);
    }

    // client line
    {
        let (content, vis) = match &f.client {
            Some(cl) => {
                let peer = truncate(&cl.peer_id, 22, c.utf8);
                let txt = format!(
                    "{lab}client{r} {bold}{name}{r}   {lab}peer{r} {peer}   {lab}key{r} {key:#010x}",
                    lab = c_dim(&c),
                    r = c.reset(),
                    bold = BOLD_if(c.color),
                    name = cl.name,
                    peer = peer,
                    key = cl.key,
                );
                let vis = dwidth("client ")
                    + dwidth(&cl.name)
                    + dwidth("   peer ")
                    + dwidth(&peer)
                    + dwidth("   key ")
                    + dwidth(&format!("{:#010x}", cl.key));
                (txt, vis.min(inner.saturating_sub(2)))
            }
            None => {
                let txt = format!("{}waiting for client…{}", c_dim(&c), c.reset());
                (txt, dwidth("waiting for client…"))
            }
        };
        line(&mut out, &content, vis);
    }

    // separator
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.ml, b.mr, None));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    }

    // ---- torrent table ------------------------------------------------------
    // column header
    {
        let bar_w = bar_width(inner);
        let hdr = format!(
            "{d}{name:<name_w$} {s:>4} {l:>4} {up:>10} {tot:>11} {nxt:>6} {pad}{r}",
            d = c_dim(&c),
            r = c.reset(),
            name = "TORRENT",
            name_w = name_col(inner, bar_w),
            s = "S",
            l = "L",
            up = "↑ SPEED",
            tot = "UPLOADED",
            nxt = "NEXT",
            pad = " ".repeat(bar_w + 1),
        );
        let vis = name_col(inner, bar_w) + 1 + 4 + 1 + 4 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
        line(&mut out, &hdr, vis.min(inner.saturating_sub(2)));
    }

    let n = f.rows.len();
    let visible = n.min(MAX_VISIBLE_ROWS);
    for tv in f.rows.iter().take(visible) {
        let (content, vis) = render_torrent_row(tv, &c, inner);
        line(&mut out, &content, vis);
    }
    if n > MAX_VISIBLE_ROWS {
        let more = format!("{}(+{} more)…{}", c_dim(&c), n - MAX_VISIBLE_ROWS, c.reset());
        line(&mut out, &more, dwidth(&format!("(+{} more)…", n - MAX_VISIBLE_ROWS)));
    }

    // ---- feed pane ----------------------------------------------------------
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.ml, b.mr, Some("recent")));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    }
    for ev in f.feed.iter() {
        let (content, vis) = render_event_row(ev, &c, inner);
        line(&mut out, &content, vis);
    }

    // ---- footer -------------------------------------------------------------
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.ml, b.mr, None));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    }
    {
        let total_up: u64 = f.rows.iter().map(|t| t.uploaded).sum();
        let total_speed: u32 = f.rows.iter().map(|t| t.up_speed).sum();
        let total_err: u32 = f.rows.iter().map(|t| t.error_count as u32).sum();
        let err_span = if total_err > 0 {
            format!("{}errors {}{}", c_err(&c), total_err, c.reset())
        } else {
            format!("{}errors 0{}", c_dim(&c), c.reset())
        };
        let plain = format!(
            "{n} torrent{plural}   ↑ total {tot}   up {spd}/s   errors {err}",
            n = n,
            plural = if n == 1 { "" } else { "s" },
            tot = format_bytes_u64(total_up),
            spd = format_bytes(total_speed),
            err = total_err,
        );
        let txt = format!(
            "{bold}{n}{r} torrent{plural}   {ok}↑ total {tot}{r}   {warn}up {spd}/s{r}   {err}",
            bold = BOLD_if(c.color),
            r = c.reset(),
            n = n,
            plural = if n == 1 { "" } else { "s" },
            ok = c_ok(&c),
            tot = format_bytes_u64(total_up),
            warn = c_warn(&c),
            spd = format_bytes(total_speed),
            err = err_span,
        );
        line(&mut out, &txt, dwidth(&plain).min(inner.saturating_sub(2)));
    }

    // bottom border
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.bl, b.br, None));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    }

    // wipe any stale rows from a previously taller frame
    out.push_str(CLR_BELOW);
    out
}

fn render_torrent_row(tv: &crate::ui::snapshot::TorrentView, c: &Caps, inner: usize) -> (String, usize) {
    let bar_w = bar_width(inner);
    let name_w = name_col(inner, bar_w);

    if tv.busy {
        let name = "(announcing…)";
        let txt = format!(
            "{d}{name:<name_w$} {s:>4} {l:>4} {up:>10} {tot:>11} {nxt:>6} {bar}{r}",
            d = c_dim(c),
            r = c.reset(),
            name = name,
            name_w = name_w,
            s = "-",
            l = "-",
            up = "-",
            tot = "-",
            nxt = "-",
            bar = progress_bar(0, 1, bar_w, c),
        );
        let vis = name_w + 1 + 4 + 1 + 4 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
        return (txt, vis.min(inner.saturating_sub(2)));
    }

    let name = truncate(&tv.name, name_w, c.utf8);
    let dot = if tv.error_count > 0 {
        format!("{}●{}", c_err(c), c.reset())
    } else if tv.downloading {
        // Downloading => active (yellow), not idle.
        format!("{}●{}", c_warn(c), c.reset())
    } else if tv.up_speed > 0 {
        format!("{}●{}", c_ok(c), c.reset())
    } else {
        format!("{}●{}", c_dim(c), c.reset())
    };
    let dot_ascii = "* "; // visible width budget for the dot+space when colored we still use 1 cell glyph
    // While downloading, show "DL NN%" instead of an upload speed (upload is 0).
    let speed = if tv.downloading {
        format!("DL {}%", tv.dl_percent)
    } else if tv.up_speed > 0 {
        format!("{}/s", format_bytes(tv.up_speed))
    } else {
        "idle".to_string()
    };
    let nxt = fmt_mmss(tv.secs_to_announce);
    // While downloading, the bar shows download progress; while seeding, it shows
    // the countdown to the next announce.
    let bar = if tv.downloading {
        progress_bar(tv.dl_percent as u64, 100, bar_w, c)
    } else {
        progress_bar(
            tv.interval.saturating_sub(tv.secs_to_announce),
            tv.interval,
            bar_w,
            c,
        )
    };

    // Visible-width name column already includes the colored dot (1 cell) + space.
    let name_field = format!("{dot} {name}");
    let name_vis = 2 + dwidth(&name); // dot + space + name
    let pad = name_w.saturating_sub(name_vis);
    let txt = format!(
        "{name_field}{namepad} {s}{sv:>4}{r} {l}{lv:>4}{r} {up}{spd:>10}{r} {tot:>11} {nxt}{nv:>6}{r} {bar}",
        name_field = name_field,
        namepad = " ".repeat(pad),
        s = c_ok(c),
        sv = tv.seeders,
        l = c_warn(c),
        lv = tv.leechers,
        up = if tv.up_speed > 0 { c_warn(c) } else { c_dim(c) },
        spd = speed,
        r = c.reset(),
        tot = format_bytes_u64(tv.uploaded),
        nxt = c_dim(c),
        nv = nxt,
        bar = bar,
    );
    let _ = dot_ascii;
    let vis = name_w + 1 + 4 + 1 + 4 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
    (txt, vis.min(inner.saturating_sub(2)))
}

fn render_event_row(ev: &UiEvent, c: &Caps, inner: usize) -> (String, usize) {
    let ts = ev.at.format("%H:%M:%S").to_string();
    let glyph = if c.utf8 {
        ev.kind.glyph()
    } else {
        ev.kind.glyph_ascii()
    };
    let col = match ev.kind {
        EventKind::ConnectOk | EventKind::PeersUpdated | EventKind::Added => c_ok(c),
        EventKind::UploadTick | EventKind::AnnounceSent => c_warn(c),
        EventKind::ConnectFail | EventKind::Error => c_err(c),
        EventKind::Removed => c_dim(c),
    };
    let name = truncate(&ev.torrent, 18, c.utf8);
    let head_vis = dwidth(&ts) + 1 + dwidth(glyph) + 1 + dwidth(&name) + 2;
    let avail = inner.saturating_sub(2).saturating_sub(head_vis);
    let msg = truncate(&ev.msg, avail, c.utf8);
    let txt = format!(
        "{d}{ts}{r} {col}{glyph}{r} {name}  {d}{msg}{r}",
        d = c_dim(c),
        r = c.reset(),
        ts = ts,
        col = col,
        glyph = glyph,
        name = name,
        msg = msg,
    );
    let vis = (head_vis + dwidth(&msg)).min(inner.saturating_sub(2));
    (txt, vis)
}

// --- small layout helpers ----------------------------------------------------

/// Width of the progress bar column (between brackets), scaled to terminal.
fn bar_width(inner: usize) -> usize {
    // bar grows with width but stays reasonable
    (inner / 6).clamp(6, 18)
}

/// Width of the torrent-name column given the fixed numeric columns + bar.
fn name_col(inner: usize, bar_w: usize) -> usize {
    // fixed: S(4) L(4) speed(10) uploaded(11) next(6) + 5 separators + bar + 1
    let fixed = 4 + 4 + 10 + 11 + 6 + 5 + bar_w + 1;
    inner.saturating_sub(2).saturating_sub(fixed).clamp(8, 40)
}

/// A bracketed block progress bar: ▕████░░░▏
fn progress_bar(done: u64, total: u64, w: usize, c: &Caps) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((w as u64 * done) / total).min(w as u64) as usize
    };
    let (lb, rb, full, empty) = if c.utf8 {
        ("▕", "▏", "█", "░")
    } else {
        ("[", "]", "#", "-")
    };
    let mut s = String::new();
    s.push_str(&c_dim(c));
    s.push_str(lb);
    s.push_str(&c_ok(c));
    for _ in 0..filled {
        s.push_str(full);
    }
    s.push_str(&c_dim(c));
    for _ in 0..w.saturating_sub(filled) {
        s.push_str(empty);
    }
    s.push_str(rb);
    s.push_str(c.reset());
    s
}

fn fmt_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn fmt_mmss(secs: u64) -> String {
    let m = secs / 60;
    let s = secs % 60;
    if m >= 100 {
        format!("{}h", m / 60)
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[allow(non_snake_case)]
fn BOLD_if(color: bool) -> &'static str {
    if color { BOLD } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_width_basics() {
        assert_eq!(cell_width('a'), 1);
        assert_eq!(cell_width('─'), 1); // box drawing
        assert_eq!(cell_width('⠋'), 1); // braille spinner
        assert_eq!(cell_width('█'), 1); // progress block
        assert_eq!(cell_width('↑'), 1); // up arrow
        assert_eq!(cell_width('📡'), 2); // emoji (announce)
        assert_eq!(cell_width('🌱'), 2); // emoji (peers)
        assert_eq!(cell_width('⬆'), 2); // heavy up arrow emoji
    }

    #[test]
    fn truncate_never_exceeds_max_cells() {
        // ASCII: trivial
        assert_eq!(truncate("hello world", 5, true), "hell…");
        assert!(dwidth(&truncate("hello world", 5, true)) <= 5);
        // wide glyphs must NOT overflow the cell budget
        let s = "🔌🔌🔌test";
        for max in 1..=12 {
            let out = truncate(s, max, true);
            assert!(
                dwidth(&out) <= max,
                "truncate({s:?},{max}) = {out:?} has width {} > {max}",
                dwidth(&out)
            );
        }
        // fits-as-is returns unchanged
        assert_eq!(truncate("abc", 10, true), "abc");
    }

    #[test]
    fn clamp_visible_closes_color_on_truncation() {
        let colored = format!("{}{}HELLO{}", c_ok(&Caps { color: true, truecolor: false, utf8: true }), "", RESET);
        let (out, vis) = clamp_visible(&colored, 3, true);
        assert!(vis <= 3);
        assert!(out.ends_with(RESET), "truncated colored span must end with RESET: {out:?}");
    }
}
