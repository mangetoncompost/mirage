//! Pure frame rendering: turns a [`Frame`] snapshot into one ready-to-write
//! `String` of ANSI. No I/O, no locks, no globals beyond reading env once for
//! the color/unicode gates. Every line is truncated to the terminal width and
//! followed by a clear-to-EOL so a shorter frame never leaves stale glyphs.

use crate::ui::events::{EventKind, UiEvent};
use crate::ui::snapshot::{Frame, TorrentView};
use crate::ui::view::View;
use crate::utils::{format_bytes, format_bytes_u64};

// --- raw ANSI sequences -----------------------------------------------------
const CLR_EOL: &str = "\x1b[K"; // clear from cursor to end of line
const CLR_BELOW: &str = "\x1b[J"; // clear from cursor to end of screen
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

// --- spinner ----------------------------------------------------------------
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

/// How many feed lines fit on the dashboard after everything else, so the feed
/// pane (padded with blanks) makes the box fill the window exactly.
pub fn feed_capacity(term_h: u16, n_rows: usize, max_visible_rows: usize) -> usize {
    // header: top border + tab strip + sep + client + sep   = 5
    // table:  column header + visible rows + selected-progress
    // feed:   "recent" separator + lines …
    // footer: separator + totals + bottom border            = 3
    let shown_rows = n_rows.min(max_visible_rows) + if n_rows > max_visible_rows { 1 } else { 0 };
    let selected_progress = if n_rows > 0 { 1 } else { 0 };
    let fixed = 5 + (1 + shown_rows + selected_progress) + 1 /* feed sep */ + 3;
    (term_h as usize).saturating_sub(fixed).min(60)
}

/// Maximum torrent rows we draw before collapsing the rest into "(+N more)".
const MAX_VISIBLE_ROWS: usize = 12;

/// The ten tab labels, in order. Index == `View as usize`.
const TAB_LABELS: [&str; 10] = [
    "dash", "tor", "trk", "spd", "cli", "sch", "net", "log", "cfg", "rto",
];

/// Build the whole frame as one ANSI string ready for `draw::paint`.
///
/// `view` selects which of the nine tab bodies to render; `sel` is the
/// highlighted row within list-style views (Dashboard/Torrents/Trackers).
pub fn build_frame(
    f: &Frame,
    width: u16,
    view: View,
    sel: usize,
    overlay: crate::ui::overlay::Overlay,
) -> String {
    let c = Caps::detect();
    let b = box_set(c.utf8);

    // Too-small terminal: don't run the layout math on a degenerate size — show
    // a single centered hint instead of a broken, clipped box.
    if (width as usize) < 40 || f.term_h < 8 {
        let msg = "terminal too small (need 40x8)";
        let mut s = String::new();
        let pad_top = f.term_h / 2;
        for _ in 0..pad_top {
            s.push_str(CLR_EOL);
            s.push_str("\r\n");
        }
        let lpad = (width as usize).saturating_sub(msg.chars().count()) / 2;
        s.push_str(&" ".repeat(lpad));
        s.push_str(&c_warn(&c));
        s.push_str(msg);
        s.push_str(c.reset());
        s.push_str(CLR_EOL);
        s.push_str(CLR_BELOW);
        return s;
    }

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
    // ASCII spinner everywhere: braille (⠋⠙…) is absent from most monospace
    // fonts (incl. the bundled JetBrains Mono), where it renders as a tofu □.
    let spin = SPINNER_A[f.spinner % SPINNER_A.len()];
    let uptime = fmt_hms((f.now - f.started).num_seconds().max(0) as u64);
    // top border with title on the left and spinner+uptime on the right:
    //   ┌─ Mirage ───────────────────── ⠹ 02:14:07 ─┐
    {
        let title = " Mirage ";
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

    // ---- tab strip ----------------------------------------------------------
    // "[1]dash [2]tor … [9]cfg [0]rto" — active label in header color, rest dim.
    // Tab 10 (index 9) uses key "0" not "10" so it stays a single digit. On a
    // narrow terminal fall back to bare digits "[1][2]…[0]".
    {
        let active_idx = view as usize;
        // Label for tab i: digit is (i+1) for tabs 0-8, "0" for tab 9 (Ratio).
        let tab_key = |i: usize| -> String {
            if i == 9 {
                "0".into()
            } else {
                (i + 1).to_string()
            }
        };
        let full_w: usize = TAB_LABELS
            .iter()
            .enumerate()
            .map(|(i, lbl)| dwidth(&format!(" [{}]{} ", tab_key(i), lbl)))
            .sum();
        let mut strip = String::new();
        let avail = inner.saturating_sub(2);
        if full_w <= avail {
            for (i, lbl) in TAB_LABELS.iter().enumerate() {
                let on = i == active_idx;
                let col = if on { c_header(&c) } else { c_dim(&c) };
                strip.push_str(&col);
                if on {
                    strip.push_str(BOLD_if(c.color));
                }
                strip.push_str(&format!(" [{}]{} ", tab_key(i), lbl));
                strip.push_str(c.reset());
            }
        } else {
            for i in 0..TAB_LABELS.len() {
                let on = i == active_idx;
                let col = if on { c_header(&c) } else { c_dim(&c) };
                strip.push_str(&col);
                if on {
                    strip.push_str(BOLD_if(c.color));
                }
                strip.push_str(&format!("[{}]", tab_key(i)));
                strip.push_str(c.reset());
            }
        }
        line(&mut out, &strip, full_w.min(avail));
    }

    // separator under the tab strip
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.ml, b.mr, None));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
        out.push_str("\r\n");
    }

    // ---- view body ----------------------------------------------------------
    // The overlay (if any) replaces the body on any tab. Priority order is
    // enforced in overlay::active() which is called in render_once; build_frame
    // only renders whatever was already resolved there.
    use crate::ui::overlay::Overlay;
    match overlay {
        Overlay::Help => build_help(&mut out, &c, inner, f.term_h, &line),
        Overlay::Palette => build_palette(&mut out, &c, inner, f.term_h, &line),
        Overlay::Detail => build_detail(&mut out, f, &c, &b, inner, &line, &rule),
        Overlay::None => match view {
            View::Dashboard => build_dash(&mut out, f, &c, &b, inner, sel, &line, &rule),
            View::Torrents => build_tor(&mut out, f, &c, &b, inner, sel, &line, &rule),
            View::Trackers => build_trk(&mut out, f, &c, inner, sel, f.term_h, &line),
            View::Speeds => build_spd(&mut out, f, &c, &b, inner, sel, &line, &rule),
            View::Client => build_cli(&mut out, f, &c, &b, inner, &line, &rule),
            View::Schedule => build_sch(&mut out, f, &c, &b, inner, &line, &rule),
            View::Network => build_net(&c, inner, &mut out, &line),
            View::Logs => build_log(&mut out, f, &c, inner, &line),
            View::Config => build_cfg(&c, &b, inner, &mut out, &line, &rule),
            View::Ratio => build_ratio(&mut out, f, &c, inner, &line),
        },
    }

    // ---- fill to the window bottom ------------------------------------------
    // Every view pads with blank bordered rows so the footer + bottom border
    // always anchor to the last terminal row (no empty rows below the box). The
    // footer below is 3 rows (rule + totals + bottom border).
    {
        let emitted = out.matches("\r\n").count();
        let target = f.term_h.saturating_sub(3);
        for _ in emitted..target {
            line(&mut out, "", 0);
        }
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
        let n = f.rows.len();
        let total_up: u64 = f.rows.iter().map(|t| t.uploaded).sum();
        let total_speed: u32 = f.rows.iter().map(|t| t.up_speed).sum();
        let total_err: u32 = f.rows.iter().map(|t| t.error_count as u32).sum();
        let n_marked = f.marked.len();
        let err_span = if total_err > 0 {
            format!("{}errors {}{}", c_err(&c), total_err, c.reset())
        } else {
            format!("{}errors 0{}", c_dim(&c), c.reset())
        };
        let mark_span = if n_marked > 0 {
            format!("   {}✓ {n_marked}{}", c_ok(&c), c.reset())
        } else {
            String::new()
        };
        let plain = format!(
            "{n} torrent{plural}   ↑ total {tot}   up {spd}/s   errors {err}{mark}",
            n = n,
            plural = if n == 1 { "" } else { "s" },
            tot = format_bytes_u64(total_up),
            spd = format_bytes(total_speed),
            err = total_err,
            mark = if n_marked > 0 {
                format!("   ✓ {n_marked}")
            } else {
                String::new()
            },
        );
        let mut txt = format!(
            "{bold}{n}{r} torrent{plural}   {ok}↑ total {tot}{r}   {warn}up {spd}/s{r}   {err}{mark}",
            bold = BOLD_if(c.color),
            r = c.reset(),
            n = n,
            plural = if n == 1 { "" } else { "s" },
            ok = c_ok(&c),
            tot = format_bytes_u64(total_up),
            warn = c_warn(&c),
            spd = format_bytes(total_speed),
            err = err_span,
            mark = mark_span,
        );
        // Right-aligned hint. When celebrating a ratio milestone (F1.3) the
        // hint is replaced by a festive label; the spinner's parity provides the
        // blink — `build_frame` stays deterministic for a given Frame snapshot.
        let (hint, hint_vis): (String, usize) = if f.celebrate && f.spinner.is_multiple_of(2) {
            let lbl = format!("★ {} ★", f.celebrate_label);
            let vis = dwidth(&lbl);
            (format!("{}{lbl}{}", c_warn(&c), c.reset()), vis)
        } else {
            let lbl = "←→ tabs · : cmds · ? help · q quit";
            (lbl.to_string(), dwidth(lbl))
        };
        let avail = inner.saturating_sub(2);
        let used = dwidth(&plain);
        let mut vis = used;
        if f.celebrate && f.spinner.is_multiple_of(2) {
            // Celebration hint: already colored, use as-is if it fits.
            if used + 3 + hint_vis <= avail {
                let gap = avail - used - hint_vis;
                txt.push_str(&" ".repeat(gap));
                txt.push_str(&hint);
                vis = avail;
            }
        } else {
            // Static hint: try progressively shorter versions so at least "? q"
            // stays visible instead of being suppressed entirely at narrow widths.
            let fallbacks: &[(&str, bool)] = &[
                ("←→ tabs · : cmds · ? help · q quit", false),
                (": cmds · ? help · q quit", false),
                ("? help · q quit", false),
                ("? q", false),
            ];
            for &(candidate, _) in fallbacks {
                let cv = dwidth(candidate);
                if used + 3 + cv <= avail {
                    let gap = avail - used - cv;
                    txt.push_str(&" ".repeat(gap));
                    txt.push_str(&c_dim(&c));
                    txt.push_str(candidate);
                    txt.push_str(c.reset());
                    vis = avail;
                    break;
                }
            }
        }
        line(&mut out, &txt, vis.min(avail));
    }

    // Wipe any stale rows from a previously taller frame BEFORE the bottom
    // border, so clearing-below doesn't touch the last row we draw.
    out.push_str(CLR_BELOW);

    // bottom border — the LAST row of the frame. No trailing newline: that would
    // push the cursor onto a row below the box, leaving a blank line (with the
    // cursor) under the footer. Keeping the cursor on the bottom border makes
    // the box occupy exactly `term_h` rows.
    {
        out.push_str(&c_dim(&c));
        out.push_str(&rule(b.bl, b.br, None));
        out.push_str(c.reset());
        out.push_str(CLR_EOL);
    }
    out
}

// ============================================================================
// Per-view body builders. Each emits its rows through the shared `line`/`rule`
// closures handed down from build_frame, so they all keep identical width / box
// / color discipline. `Line` = bordered content row, `Rule` = horizontal rule.
// ============================================================================

type Line<'a> = dyn Fn(&mut String, &str, usize) + 'a;
type Rule<'a> = dyn Fn(&str, &str, Option<&str>) -> String + 'a;

/// Emit a horizontal rule line (with optional label) through the `rule` closure.
fn rule_line(out: &mut String, c: &Caps, b: &Box, rule: &Rule, label: Option<&str>) {
    out.push_str(&c_dim(c));
    out.push_str(&rule(b.ml, b.mr, label));
    out.push_str(c.reset());
    out.push_str(CLR_EOL);
    out.push_str("\r\n");
}

/// A simple "  key   value" row used by several settings-style views.
fn kv_row(out: &mut String, c: &Caps, inner: usize, line: &Line, key: &str, val: &str, col: &str) {
    let txt = format!(
        "{d} {k:<14} {r}{col}{v}{r}",
        d = c_dim(c),
        k = key,
        r = c.reset(),
        col = col,
        v = val,
    );
    let vis = (1 + 14 + 1 + dwidth(val)).min(inner.saturating_sub(2));
    line(out, &txt, vis);
}

// ---- [1] dash : the original dashboard body, kept byte-identical -----------
#[allow(clippy::too_many_arguments)]
fn build_dash(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    sel: usize,
    line: &Line,
    rule: &Rule,
) {
    // client line
    {
        let (content, vis) = match &f.client {
            Some(cl) => {
                let peer = truncate(&cl.peer_id, 22, c.utf8);
                let txt = format!(
                    "{lab}client{r} {bold}{name}{r}   {lab}peer{r} {peer}   {lab}key{r} {key:#010x}",
                    lab = c_dim(c),
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
                let txt = format!("{}waiting for client…{}", c_dim(c), c.reset());
                (txt, dwidth("waiting for client…"))
            }
        };
        line(out, &content, vis);
    }

    // separator
    rule_line(out, c, b, rule, None);

    // ---- torrent table ------------------------------------------------------
    // Header carries the same 2-cell selection gutter as the rows (drawn by
    // emit_row), and is sized against the same table_body_w, so every column
    // lines up exactly under its heading.
    {
        let body_w = table_body_w(inner);
        let bar_w = bar_width(body_w);
        let name_w = name_col(body_w, bar_w);
        let hdr = format!(
            "{gut}{d}{name:<name_w$} {s:>5} {l:>5} {up:>10} {tot:>11} {nxt:>6} {pad}{r}",
            gut = " ".repeat(SEL_GUTTER),
            d = c_dim(c),
            r = c.reset(),
            name = "TORRENT",
            name_w = name_w,
            s = "S",
            l = "L",
            up = "↑ SPEED",
            tot = "UPLOADED",
            nxt = "NEXT",
            pad = " ".repeat(bar_w + 1),
        );
        let vis = SEL_GUTTER + name_w + 1 + 5 + 1 + 5 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
        line(out, &hdr, vis.min(inner.saturating_sub(2)));
    }

    let n = f.rows.len();
    let visible = n.min(MAX_VISIBLE_ROWS);
    for (i, tv) in f.rows.iter().take(visible).enumerate() {
        let (content, vis) = render_torrent_row(tv, c, inner, f.frame_peak_speed);
        emit_row(
            out,
            c,
            inner,
            line,
            &content,
            vis,
            i == sel,
            f.marked.contains(&tv.info_hash),
        );
    }
    if n > MAX_VISIBLE_ROWS {
        let more = format!("{}(+{} more)…{}", c_dim(c), n - MAX_VISIBLE_ROWS, c.reset());
        line(
            out,
            &more,
            dwidth(&format!("(+{} more)…", n - MAX_VISIBLE_ROWS)),
        );
    }

    // ---- feed pane ----------------------------------------------------------
    // The "recent" box extends to the window bottom: render events, then the
    // global fill in build_frame pads the rest as blank bordered rows.
    rule_line(out, c, b, rule, Some("recent"));
    for ev in f.feed.iter() {
        let (content, vis) = render_event_row(ev, c, inner);
        line(out, &content, vis);
    }
    let _ = f.feed_cap;
}

// ---- ? : help overlay (full keymap) ---------------------------------------
fn build_help(out: &mut String, c: &Caps, inner: usize, term_h: usize, line: &Line) {
    // Build into a scratch buffer first, then copy only as many \r\n-delimited
    // lines as fit the body budget (term_h - 3 header rows - 3 footer rows).
    let mut scratch = String::new();
    let line_s = |s: &mut String, content: &str, vis: usize| line(s, content, vis);
    let head = |s: &mut String, t: &str| {
        line_s(
            s,
            &format!("{cy} {t}{r}", cy = c_header(c), t = t, r = c.reset()),
            dwidth(t) + 1,
        );
    };
    let row = |s: &mut String, key: &str, desc: &str| {
        let txt = format!(
            "   {cy}{key:<10}{r}{d}{desc}{r}",
            cy = c_header(c),
            key = key,
            r = c.reset(),
            d = c_dim(c),
            desc = desc
        );
        line_s(
            s,
            &txt,
            (3 + 10 + dwidth(desc)).min(inner.saturating_sub(2)),
        );
    };
    line_s(&mut scratch, "", 0);
    head(&mut scratch, "navigation");
    row(&mut scratch, "1-9 / 0", "jump to tab (0 = ratio graph)");
    row(&mut scratch, "← →", "previous / next tab");
    row(
        &mut scratch,
        "↑ ↓",
        "select row (or upload multiplier off the lists)",
    );
    row(
        &mut scratch,
        "Enter",
        "open detail card for selected torrent",
    );
    row(&mut scratch, "Esc", "back to dashboard (or close overlay)");
    line_s(&mut scratch, "", 0);
    head(&mut scratch, "actions");
    row(&mut scratch, "p", "pause / resume all uploads (global)");
    row(&mut scratch, "r", "resume all uploads");
    row(&mut scratch, "f", "force-announce selected (or all marked)");
    row(
        &mut scratch,
        "x",
        "remove selected (or all marked, announces stopped)",
    );
    row(
        &mut scratch,
        "Space",
        "toggle mark on selected row (multi-select)",
    );
    row(&mut scratch, "a / A", "mark all visible / clear all marks");
    row(&mut scratch, "e", "export snapshot to JSON (+ clipboard)");
    row(
        &mut scratch,
        "+ -",
        "edit selected setting on the Speeds tab",
    );
    line_s(&mut scratch, "", 0);
    head(&mut scratch, "overlays");
    row(
        &mut scratch,
        ":",
        "open command palette (fuzzy search all actions)",
    );
    row(&mut scratch, "i / w", "[detail] info / wire sub-tab");
    line_s(&mut scratch, "", 0);
    head(&mut scratch, "tabs");
    row(
        &mut scratch,
        "k",
        "[cli] re-init the emulated client (new key)",
    );
    row(&mut scratch, "s", "[cfg] save config.toml");
    line_s(&mut scratch, "", 0);
    head(&mut scratch, "session");
    row(&mut scratch, "?", "toggle this help");
    row(
        &mut scratch,
        "q / ^C",
        "quit (announces stopped, saves state)",
    );

    // Clip to fit: header uses 3 rows, footer uses 3 rows — body budget is the rest.
    let budget = term_h.saturating_sub(6);
    for segment in scratch.split_inclusive("\r\n").take(budget) {
        out.push_str(segment);
    }
}

/// Emit a list row with the `SEL_GUTTER`-cell selection gutter at the left:
/// "› " (cyan caret) when selected, else two spaces. `content` is already sized
/// for `table_body_w` (the bordered area minus the gutter), so gutter + content
/// fits the row exactly and the columns line up under the header.
#[allow(clippy::too_many_arguments)]
fn emit_row(
    out: &mut String,
    c: &Caps,
    inner: usize,
    line: &Line,
    content: &str,
    vis: usize,
    selected: bool,
    marked: bool,
) {
    // Gutter is always SEL_GUTTER (2) cells wide.
    // Selection caret: `›` when selected, ` ` otherwise.
    // Mark indicator: replaces the space after the caret with `✓`/`*` when marked.
    let (caret, mark_char) = if c.utf8 { ("›", "✓") } else { (">", "*") };
    let gutter = match (selected, marked) {
        (true, true) => format!(
            "{cy}{caret}{ok}{mark_char}{r}",
            cy = c_header(c),
            ok = c_ok(c),
            r = c.reset(),
        ),
        (true, false) => format!("{}{}  {}", c_header(c), caret, c.reset()),
        (false, true) => format!(" {ok}{mark_char}{r}", ok = c_ok(c), r = c.reset()),
        (false, false) => " ".repeat(SEL_GUTTER),
    };
    let row = format!("{gutter}{content}");
    line(out, &row, (SEL_GUTTER + vis).min(inner.saturating_sub(2)));
}

// ---- [2] tor : full torrent list with state ------------------------------
#[allow(clippy::too_many_arguments)]
fn build_tor(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    sel: usize,
    line: &Line,
    rule: &Rule,
) {
    // column header
    let hdr = format!(
        "{d}  {num:>3} {name:<22}{state:<9}{s:>5}{l:>5} {up}{r}",
        d = c_dim(c),
        r = c.reset(),
        num = "#",
        name = "NAME",
        state = "STATE",
        s = "S",
        l = "L",
        up = "UPLOAD",
    );
    line(out, &hdr, inner.saturating_sub(2));

    // Cap visible rows: header(1) + rule(1) + help(1) + footer(3) = 6 fixed lines
    let visible = f
        .rows
        .len()
        .min(MAX_VISIBLE_ROWS)
        .min(f.term_h.saturating_sub(6));
    for (i, tv) in f.rows.iter().take(visible).enumerate() {
        let dot = dot_span(tv, c);
        let name = truncate(&tv.name, 21, c.utf8);
        let state = if tv.downloading {
            format!("DL{}%", tv.dl_percent)
        } else if tv.error_count > 0 {
            "error".to_string()
        } else {
            "seed".to_string()
        };
        let body = format!(
            "{d}{num:>3}{r} {dot} {name:<21}{d}{state:<9}{r}{ok}{s:>5}{r}{lc}{l:>5}{r}{d} {up:>8}{r}",
            d = c_dim(c),
            r = c.reset(),
            num = i + 1,
            dot = dot,
            name = name,
            state = state,
            ok = c_ok(c),
            s = tv.seeders,
            lc = if tv.leechers > 0 { c_ok(c) } else { c_dim(c) },
            l = tv.leechers,
            up = format_bytes_u64(tv.uploaded),
        );
        let vis = 3 + 1 + 2 + dwidth(&name) + 9 + 5 + 5 + 1 + 8;
        emit_row(
            out,
            c,
            inner,
            line,
            &body,
            vis.min(inner.saturating_sub(2)),
            i == sel,
            f.marked.contains(&tv.info_hash),
        );
    }
    if f.rows.len() > visible {
        let more = format!(
            "{}(+{} more)…{}",
            c_dim(c),
            f.rows.len() - visible,
            c.reset()
        );
        line(
            out,
            &more,
            dwidth(&format!("(+{} more)…", f.rows.len() - visible)),
        );
    }

    rule_line(out, c, b, rule, None);
    let help = format!(
        "{cy}f{r} force   {rd}x{r} remove   {cy}p{r} pause all   {cy}?{r} help",
        cy = c_header(c),
        rd = c_err(c),
        r = c.reset(),
    );
    line(
        out,
        &help,
        dwidth("f force   x remove   p pause all   ? help"),
    );
}

// ---- [3] trk : per-torrent trackers --------------------------------------
fn build_trk(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    inner: usize,
    sel: usize,
    term_h: usize,
    line: &Line,
) {
    let hdr = format!(
        "{d} per-torrent trackers (snapshot){r}",
        d = c_dim(c),
        r = c.reset()
    );
    line(out, &hdr, dwidth(" per-torrent trackers (snapshot)"));
    // header(1) + footer(3) = 4 fixed; cap same as dash
    let visible = f
        .rows
        .len()
        .min(MAX_VISIBLE_ROWS)
        .min(term_h.saturating_sub(4));
    for (i, tv) in f.rows.iter().take(visible).enumerate() {
        let dot = dot_span(tv, c);
        let url = tv
            .urls
            .first()
            .map(|u| u.as_str())
            .unwrap_or("(no tracker)");
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url);
        let name = truncate(&tv.name, 26, c.utf8);
        let body = format!(
            "{dot} {cy}{name:<26}{r} {host}  {ok}S{s} L{l}{r}",
            dot = dot,
            cy = c_header(c),
            name = name,
            r = c.reset(),
            host = host,
            ok = c_ok(c),
            s = tv.seeders,
            l = tv.leechers,
        );
        let vis =
            2 + 26 + 1 + dwidth(host) + 2 + dwidth(&format!("S{} L{}", tv.seeders, tv.leechers));
        emit_row(
            out,
            c,
            inner,
            line,
            &body,
            vis.min(inner.saturating_sub(2)),
            i == sel,
            f.marked.contains(&tv.info_hash),
        );
    }
    if f.rows.len() > visible {
        let more = format!(
            "{}(+{} more)…{}",
            c_dim(c),
            f.rows.len() - visible,
            c.reset()
        );
        line(
            out,
            &more,
            dwidth(&format!("(+{} more)…", f.rows.len() - visible)),
        );
    }
}

// ---- [4] spd : upload/download bands + multiplier -------------------------
#[allow(clippy::too_many_arguments)]
fn build_spd(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    sel: usize,
    line: &Line,
    rule: &Rule,
) {
    let cfg = crate::CONFIG.load();
    let setting = |out: &mut String, idx: usize, label: &str, val: &str, arrows: bool| {
        let on = idx == sel;
        let lc = if on { c_header(c) } else { c_dim(c) };
        let mut txt = format!(
            "{lc} {label:<14} {r}{d}[ {r}{warn}{val:<13}{r}{d}]{r}",
            lc = lc,
            label = label,
            r = c.reset(),
            d = c_dim(c),
            warn = c_warn(c),
            val = val,
        );
        let mut vis = 1 + 14 + 1 + 2 + 13 + 1;
        if on && arrows {
            txt.push_str(&format!(
                "{cy}  +/- edit{r}",
                cy = c_header(c),
                r = c.reset()
            ));
            vis += dwidth("  +/- edit");
        }
        line(out, &txt, vis.min(inner.saturating_sub(2)));
    };
    rule_line(out, c, b, rule, Some("upload band"));
    setting(
        out,
        0,
        "min upload",
        &format!("{}/s", format_bytes(cfg.min_upload_rate)),
        true,
    );
    setting(
        out,
        1,
        "max upload",
        &format!("{}/s", format_bytes(cfg.max_upload_rate)),
        true,
    );
    setting(
        out,
        2,
        "multiplier",
        &format!("x{:.2}", crate::torrent::speed_multiplier()),
        true,
    );
    rule_line(out, c, b, rule, Some("download phase"));
    setting(
        out,
        3,
        "min download",
        &format!("{}/s", format_bytes(cfg.min_download_rate)),
        true,
    );
    setting(
        out,
        4,
        "max download",
        &format!("{}/s", format_bytes(cfg.max_download_rate)),
        true,
    );
    setting(
        out,
        5,
        "numwant",
        &cfg.numwant.unwrap_or(80).to_string(),
        true,
    );
    rule_line(out, c, b, rule, Some("total upload (live)"));
    let total_up: u64 = f.rows.iter().map(|t| t.uploaded).sum();
    let total_speed: u32 = f.rows.iter().map(|t| t.up_speed).sum();
    let txt = format!(
        "{d}  up {r}{warn}{spd}/s{r}{d}   Σ {r}{ok}{tot}{r}",
        d = c_dim(c),
        r = c.reset(),
        warn = c_warn(c),
        spd = format_bytes(total_speed),
        ok = c_ok(c),
        tot = format_bytes_u64(total_up),
    );
    let vis = dwidth(&format!(
        "  up {}/s   Σ {}",
        format_bytes(total_speed),
        format_bytes_u64(total_up)
    ));
    line(out, &txt, vis.min(inner.saturating_sub(2)));
}

// ---- [5] cli : client identity + what the tracker sees --------------------
fn build_cli(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    line: &Line,
    rule: &Rule,
) {
    let cfg = crate::CONFIG.load();
    if let Some(cl) = &f.client {
        kv_row(out, c, inner, line, "client", &cl.name, c.reset());
        kv_row(out, c, inner, line, "peer_id", &cl.peer_id, &c_dim(c));
        kv_row(out, c, inner, line, "user-agent", &cl.user_agent, &c_dim(c));
        kv_row(
            out,
            c,
            inner,
            line,
            "key",
            &format!("{:#010x}", cl.key),
            &c_dim(c),
        );
        kv_row(
            out,
            c,
            inner,
            line,
            "download phase",
            "on (leech → completed → seed)",
            &c_ok(c),
        );
        rule_line(out, c, b, rule, Some("what the tracker sees"));
        let get = format!(
            "{d} GET /announce?peer_id={pid}&numwant={nw}&key={key:#010x}&event=started{r}",
            d = c_dim(c),
            pid = cl.peer_id,
            nw = cfg.numwant.unwrap_or(80),
            key = cl.key,
            r = c.reset(),
        );
        line(out, &get, inner.saturating_sub(2));
        let help = format!(
            "{cy} k {r}{d}re-init client (new key){r}",
            cy = c_header(c),
            d = c_dim(c),
            r = c.reset()
        );
        line(out, &help, dwidth(" k re-init client (new key)"));
    } else {
        line(
            out,
            &format!("{}waiting for client…{}", c_dim(c), c.reset()),
            dwidth("waiting for client…"),
        );
    }
}

// ---- [6] sch : next-announce ledger + global pause (F3.3) ------------------
fn build_sch(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    line: &Line,
    rule: &Rule,
) {
    use crate::torrent::ScheduleReason;

    // Global pause state stays at the top — it gates the whole schedule.
    let paused = crate::control::is_paused();
    let state = if paused {
        format!("{rd}[ ] paused{r}", rd = c_err(c), r = c.reset())
    } else {
        format!("{ok}[x] running{r}", ok = c_ok(c), r = c.reset())
    };
    let txt = format!(
        "{d} global       {r}{state}{d}   (p to toggle){r}",
        d = c_dim(c),
        r = c.reset(),
        state = state,
    );
    let vis = dwidth(" global       ")
        + dwidth(if paused { "[ ] paused" } else { "[x] running" })
        + dwidth("   (p to toggle)");
    line(out, &txt, vis.min(inner.saturating_sub(2)));

    rule_line(out, c, b, rule, Some("next announce"));

    if f.rows.is_empty() {
        line(
            out,
            &format!(
                "{d} (no torrents — add a .torrent to the watch dir){r}",
                d = c_dim(c),
                r = c.reset()
            ),
            dwidth(" (no torrents — add a .torrent to the watch dir)"),
        );
        return;
    }

    // Order by time-to-announce so the next firing is always on top. Skip the
    // busy (mid-announce) placeholder rows — their fields are zeroed sentinels.
    let mut order: Vec<&TorrentView> = f.rows.iter().filter(|t| !t.busy).collect();
    order.sort_by_key(|t| t.secs_to_announce);

    // Column widths: name | MM:SS | bar | reason. Reuse the table helpers so the
    // ledger lines up with the dashboard's notion of "bar width".
    let body = inner.saturating_sub(2);
    let bar_w = bar_width(body).min(10);
    let reason_w = 8; // widest label "interval"
    let nxt_w = 6;
    // name gets whatever is left after the fixed columns + 3 separators.
    let name_w = body
        .saturating_sub(nxt_w + 1 + bar_w + 1 + reason_w + 3)
        .clamp(8, 40);

    for tv in order.iter().take(MAX_VISIBLE_ROWS) {
        let name = truncate(&tv.name, name_w, c.utf8);
        let nxt = fmt_mmss(tv.secs_to_announce);
        // Countdown bar fills as the interval elapses (elapsed / interval).
        let bar = progress_bar(
            tv.interval.saturating_sub(tv.secs_to_announce),
            tv.interval.max(1),
            bar_w,
            c,
        );
        let reason = ScheduleReason::from_u8(tv.schedule_reason).label();
        let row = format!(
            "{d} {name:<name_w$} {warn}{nxt:>nxt_w$}{r} {bar} {d}{reason}{r}",
            d = c_dim(c),
            warn = c_warn(c),
            r = c.reset(),
            name = name,
            name_w = name_w,
            nxt = nxt,
            nxt_w = nxt_w,
            bar = bar,
            reason = reason,
        );
        let vis = 1 + name_w + 1 + nxt_w + 1 + bar_w + 1 + dwidth(reason);
        line(out, &row, vis.min(body));
    }
    let extra = order.len().saturating_sub(MAX_VISIBLE_ROWS);
    if extra > 0 {
        line(
            out,
            &format!("{d} (+{extra} more)…{r}", d = c_dim(c), r = c.reset()),
            dwidth(&format!(" (+{extra} more)…")),
        );
    }
}

// ---- [7] net : network settings -------------------------------------------
fn build_net(c: &Caps, inner: usize, out: &mut String, line: &Line) {
    let cfg = crate::CONFIG.load();
    kv_row(
        out,
        c,
        inner,
        line,
        "port",
        &cfg.port.to_string(),
        &c_warn(c),
    );
    kv_row(
        out,
        c,
        inner,
        line,
        "numwant",
        &cfg.numwant.unwrap_or(80).to_string(),
        &c_warn(c),
    );
    kv_row(
        out,
        c,
        inner,
        line,
        "torrent dir",
        &cfg.torrent_dir.display().to_string(),
        &c_dim(c),
    );
    kv_row(
        out,
        c,
        inner,
        line,
        "pid file",
        if cfg.use_pid_file { "on" } else { "off" },
        &c_dim(c),
    );
}

// ---- [8] log : the in-process event ring (tracing is off in TUI mode) ------
fn build_log(out: &mut String, f: &Frame, c: &Caps, inner: usize, line: &Line) {
    if f.feed.is_empty() {
        line(
            out,
            &format!(
                "{d} (no events yet — they appear here as the engine runs){r}",
                d = c_dim(c),
                r = c.reset()
            ),
            dwidth(" (no events yet — they appear here as the engine runs)"),
        );
    }
    for ev in f.feed.iter() {
        let (content, vis) = render_event_row(ev, c, inner);
        line(out, &content, vis);
    }
}

// ---- [9] cfg : config.toml mirror -----------------------------------------
fn build_cfg(c: &Caps, b: &Box, inner: usize, out: &mut String, line: &Line, rule: &Rule) {
    let cfg = crate::CONFIG.load();
    rule_line(out, c, b, rule, Some("config.toml"));
    let kvc = |out: &mut String, k: &str, v: &str, col: &str| {
        let txt = format!(
            "{d} {k:>18} = {r}{col}{v}{r}",
            d = c_dim(c),
            k = k,
            r = c.reset(),
            col = col,
            v = v,
        );
        let vis = (1 + 18 + 3 + dwidth(v)).min(inner.saturating_sub(2));
        line(out, &txt, vis);
    };
    kvc(out, "client", &format!("\"{}\"", cfg.client), &c_ok(c));
    kvc(out, "port", &cfg.port.to_string(), &c_warn(c));
    kvc(
        out,
        "min_upload_rate",
        &cfg.min_upload_rate.to_string(),
        &c_warn(c),
    );
    kvc(
        out,
        "max_upload_rate",
        &cfg.max_upload_rate.to_string(),
        &c_warn(c),
    );
    kvc(
        out,
        "min_download_rate",
        &cfg.min_download_rate.to_string(),
        &c_warn(c),
    );
    kvc(
        out,
        "max_download_rate",
        &cfg.max_download_rate.to_string(),
        &c_warn(c),
    );
    kvc(
        out,
        "numwant",
        &cfg.numwant.unwrap_or(80).to_string(),
        &c_warn(c),
    );
    let help = format!(
        "{cy} s {r}{d}save config.toml{r}",
        cy = c_header(c),
        d = c_dim(c),
        r = c.reset()
    );
    line(out, &help, dwidth(" s save config.toml"));
}

/// Colored status dot for a torrent (matches render_torrent_row's logic).
fn dot_span(tv: &TorrentView, c: &Caps) -> String {
    if tv.error_count > 0 {
        format!("{}●{}", c_err(c), c.reset())
    } else if tv.downloading {
        format!("{}●{}", c_warn(c), c.reset())
    } else if tv.up_speed > 0 {
        format!("{}●{}", c_ok(c), c.reset())
    } else {
        format!("{}●{}", c_dim(c), c.reset())
    }
}

/// Width of one torrent row's body — the bordered area (`inner-2`) minus the
/// 2-cell selection gutter. Header and rows both size against this so columns
/// line up exactly under the header.
fn table_body_w(inner: usize) -> usize {
    inner.saturating_sub(2).saturating_sub(SEL_GUTTER)
}

/// Cells reserved at the left of every list row for the selection caret.
const SEL_GUTTER: usize = 2;

fn render_torrent_row(
    tv: &crate::ui::snapshot::TorrentView,
    c: &Caps,
    inner: usize,
    peak_speed: u64,
) -> (String, usize) {
    let body_w = table_body_w(inner);
    let bar_w = bar_width(body_w);
    let name_w = name_col(body_w, bar_w);

    if tv.busy {
        let name = "(announcing…)";
        let txt = format!(
            "{d}{name:<name_w$} {s:>5} {l:>5} {up:>10} {tot:>11} {nxt:>6} {bar}{r}",
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
        let vis = name_w + 1 + 5 + 1 + 5 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
        return (txt, vis.min(body_w));
    }

    // The name column total width is `name_w`, and the field starts with a
    // 1-cell dot + 1 space, so the name itself gets `name_w - 2` cells. (Earlier
    // this truncated to `name_w`, making truncated names 2 cells too wide and
    // shoving the S/L/… columns left of the header for short, untruncated names.)
    let name = truncate(&tv.name, name_w.saturating_sub(2), c.utf8);
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
    // While downloading, the bar shows download progress. While seeding it shows
    // a colored SPEED METER (F1.2): this torrent's upload speed against the
    // session-peak single-row speed, green when it's pulling its weight, dim when
    // idle. The next-announce countdown stays available numerically in the NEXT
    // column, so no information is lost by repurposing the bar.
    let bar = if tv.downloading {
        progress_bar(tv.dl_percent as u64, 100, bar_w, c)
    } else {
        meter_bar(tv.up_speed as u64, peak_speed, bar_w, c)
    };

    // Visible-width name column already includes the colored dot (1 cell) + space.
    let name_field = format!("{dot} {name}");
    let name_vis = 2 + dwidth(&name); // dot + space + name
    let pad = name_w.saturating_sub(name_vis);
    let txt = format!(
        "{name_field}{namepad} {s}{sv:>5}{r} {l}{lv:>5}{r} {up}{spd:>10}{r} {tot:>11} {nxt}{nv:>6}{r} {bar}",
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
    let vis = name_w + 1 + 5 + 1 + 5 + 1 + 10 + 1 + 11 + 1 + 6 + 1 + bar_w + 1;
    (txt, vis.min(body_w))
}

fn render_event_row(ev: &UiEvent, c: &Caps, inner: usize) -> (String, usize) {
    let ts = ev.at.format("%H:%M:%S").to_string();
    let glyph = if c.utf8 {
        ev.kind.glyph()
    } else {
        ev.kind.glyph_ascii()
    };
    let col = match ev.kind {
        EventKind::ConnectOk
        | EventKind::PeersUpdated
        | EventKind::Added
        | EventKind::Exported
        | EventKind::GoalReached => c_ok(c),
        EventKind::UploadTick | EventKind::AnnounceSent | EventKind::Milestone => c_warn(c),
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
    // fixed: S(5) L(5) speed(10) uploaded(11) next(6) + 5 separators + bar + 1
    let fixed = 5 + 5 + 10 + 11 + 6 + 5 + bar_w + 1;
    inner.saturating_sub(2).saturating_sub(fixed).clamp(8, 40)
}

/// A bracketed block progress bar: ▕████░░░▏
fn progress_bar(done: u64, total: u64, w: usize, c: &Caps) -> String {
    let filled = (w as u64 * done)
        .checked_div(total)
        .unwrap_or(0)
        .min(w as u64) as usize;
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

/// A bracketed speed meter: like [`progress_bar`] but the fill is `val / max`
/// (a torrent's upload speed against the session-peak summed speed) and the
/// filled glyphs are colored by how hot the row is — green when it's carrying a
/// healthy share of the upload, amber in the mid-band, dim when nearly idle.
/// More upload is *good* for a ratio tool, so a fuller bar is greener, never red.
fn meter_bar(val: u64, max: u64, w: usize, c: &Caps) -> String {
    let max = max.max(1);
    let filled = ((w as u128 * val as u128) / max as u128).min(w as u128) as usize;
    let (lb, rb, full, empty) = if c.utf8 {
        ("▕", "▏", "▆", "░")
    } else {
        ("[", "]", "#", "-")
    };
    // Color by fill fraction (in tenths) so the scale is stable across widths.
    let frac10 = (filled * 10) / w.max(1);
    let fill_col = if frac10 >= 6 {
        c_ok(c) // green: pulling its weight
    } else if frac10 >= 2 {
        c_warn(c) // amber: middling
    } else {
        c_dim(c) // dim: idle / trickle
    };
    let mut s = String::new();
    s.push_str(&c_dim(c));
    s.push_str(lb);
    s.push_str(&fill_col);
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

// ---- [palette] fuzzy command palette overlay (F3.1) -------------------------

/// All palette commands: (label used for matching, display label, key hint).
const PALETTE_CMDS: &[(&str, &str, &str)] = &[
    (
        "force announce selected",
        "force-announce selected torrent",
        "[f]",
    ),
    ("remove selected torrent", "remove selected torrent", "[x]"),
    (
        "pause toggle all uploads",
        "pause / resume all uploads",
        "[p]",
    ),
    ("resume all uploads", "resume all uploads", "[r]"),
    ("reinit client new key", "re-init client (new key)", "[k]"),
    ("export snapshot json", "export snapshot to JSON", "[e]"),
    ("save config toml", "save config.toml", "[s]"),
    ("go to dashboard tab 1", "→ Dashboard tab", "[1]"),
    ("go to torrents tab 2", "→ Torrents tab", "[2]"),
    ("go to trackers tab 3", "→ Trackers tab", "[3]"),
    ("go to speeds tab 4", "→ Speeds tab", "[4]"),
    ("go to client tab 5", "→ Client tab", "[5]"),
    ("go to schedule tab 6", "→ Schedule tab", "[6]"),
    ("go to network tab 7", "→ Network tab", "[7]"),
    ("go to logs tab 8", "→ Logs tab", "[8]"),
    ("go to config tab 9", "→ Config tab", "[9]"),
    ("go to ratio graph tab 0", "→ Ratio graph tab", "[0]"),
    ("help overlay question mark", "toggle help overlay", "[?]"),
    (
        "detail open selected torrent enter",
        "open detail card for selected",
        "[Enter]",
    ),
];

/// How many palette items match the current query (for key-thread navigation).
/// Called from the key thread — reads a Mutex once, acceptable.
pub fn palette_match_count() -> usize {
    let q = crate::ui::overlay::palette_query();
    if q.is_empty() {
        return PALETTE_CMDS.len();
    }
    let ql = q.to_lowercase();
    PALETTE_CMDS
        .iter()
        .filter(|(key, _, _)| key.contains(&*ql))
        .count()
}

/// Execute the Nth visible palette item (called by the key thread on Enter).
pub fn execute_palette_item(idx: usize, selected_hash: Option<[u8; 20]>) {
    use crate::control::{self, Cmd};
    let q = crate::ui::overlay::palette_query();
    let items: Vec<usize> = if q.is_empty() {
        (0..PALETTE_CMDS.len()).collect()
    } else {
        let ql = q.to_lowercase();
        PALETTE_CMDS
            .iter()
            .enumerate()
            .filter(|(_, (key, _, _))| key.contains(&*ql))
            .map(|(i, _)| i)
            .collect()
    };
    let Some(&cmd_idx) = items.get(idx) else {
        return;
    };
    match cmd_idx {
        0 => {
            if let Some(h) = selected_hash {
                control::send(Cmd::ForceAnnounce(h));
            }
        }
        1 => {
            if let Some(h) = selected_hash {
                control::send(Cmd::Remove(h));
            }
        }
        2 => {
            crate::control::toggle_paused();
        }
        3 => {
            crate::control::set_paused(false);
        }
        4 => {
            control::send(Cmd::ReinitClient);
        }
        5 => {
            control::send(Cmd::ExportSnapshot);
        }
        6 => {
            control::send(Cmd::SaveConfig);
        }
        7 => {
            crate::ui::view::set_view(0);
        }
        8 => {
            crate::ui::view::set_view(1);
        }
        9 => {
            crate::ui::view::set_view(2);
        }
        10 => {
            crate::ui::view::set_view(3);
        }
        11 => {
            crate::ui::view::set_view(4);
        }
        12 => {
            crate::ui::view::set_view(5);
        }
        13 => {
            crate::ui::view::set_view(6);
        }
        14 => {
            crate::ui::view::set_view(7);
        }
        15 => {
            crate::ui::view::set_view(8);
        }
        16 => {
            crate::ui::view::set_view(9);
        }
        17 => {
            crate::ui::overlay::toggle_help();
        }
        18 => {
            if let Some(h) = selected_hash {
                crate::ui::overlay::open_detail(h);
            }
        }
        _ => {}
    }
}

fn build_palette(out: &mut String, c: &Caps, inner: usize, term_h: usize, line: &Line) {
    let query = crate::ui::overlay::palette_query();
    let sel = crate::ui::overlay::PALETTE_SEL.load(std::sync::atomic::Ordering::Relaxed);

    // Filter items by substring match (case-insensitive).
    let matches: Vec<(usize, &str, &str)> = if query.is_empty() {
        PALETTE_CMDS
            .iter()
            .enumerate()
            .map(|(i, (_, lbl, hint))| (i, *lbl, *hint))
            .collect()
    } else {
        let ql = query.to_lowercase();
        PALETTE_CMDS
            .iter()
            .enumerate()
            .filter(|(_, (key, _, _))| key.contains(&*ql))
            .map(|(i, (_, lbl, hint))| (i, *lbl, *hint))
            .collect()
    };

    // Header: search bar.
    let prompt = format!(
        "{cy}:{r} {q}{cur}",
        cy = c_header(c),
        r = c.reset(),
        q = query,
        cur = if c.utf8 { "▌" } else { "_" },
    );
    line(out, &prompt, 2 + dwidth(&query) + 1);

    // Separator.
    let sep_char = if c.utf8 { "─" } else { "-" };
    let sep = sep_char.repeat(inner.saturating_sub(4));
    line(
        out,
        &format!("{d} {sep}{r}", d = c_dim(c), r = c.reset()),
        1 + dwidth(&sep),
    );

    let budget = term_h.saturating_sub(6).min(matches.len());
    // Scroll window: keep selected row visible.
    let window_start = if sel >= budget { sel + 1 - budget } else { 0 };

    for (row_idx, (_, lbl, hint)) in matches.iter().enumerate().skip(window_start).take(budget) {
        let is_sel = row_idx == sel;
        let gutter = if is_sel {
            format!("{}❯{} ", c_header(c), c.reset())
        } else {
            "  ".to_string()
        };
        let lbl_col = if is_sel {
            c_ok(c)
        } else {
            c.reset().to_string()
        };
        let row = format!(
            "{g}{lbl_col}{lbl:<32}{r}  {d}{hint}{r}",
            g = gutter,
            lbl = truncate(lbl, 32, c.utf8),
            r = c.reset(),
            d = c_dim(c),
            hint = hint,
        );
        line(out, &row, 2 + dwidth(lbl) + 2 + dwidth(hint));
    }

    if matches.is_empty() {
        line(
            out,
            &format!("{d} (no matches){r}", d = c_dim(c), r = c.reset()),
            dwidth(" (no matches)"),
        );
    }
}

// ---- [detail] per-torrent detail card overlay (F3.2) ------------------------
fn build_detail(
    out: &mut String,
    f: &Frame,
    c: &Caps,
    b: &Box,
    inner: usize,
    line: &Line,
    rule: &Rule,
) {
    let hash = crate::ui::overlay::detail_hash();
    let tv = hash.and_then(|h| f.rows.iter().find(|r| r.info_hash == h));
    match tv {
        None => {
            // Distinguish: no hash stored vs hash present but torrent is busy/removed.
            let msg = if hash.is_some() {
                " torrent announcing — detail will reappear momentarily"
            } else {
                " (no torrent selected — press Enter on a row)"
            };
            line(
                out,
                &format!("{d}{msg}{r}", d = c_dim(c), r = c.reset()),
                dwidth(msg),
            );
            line(
                out,
                &format!("{d} Esc to close{r}", d = c_dim(c), r = c.reset()),
                dwidth(" Esc to close"),
            );
        }
        Some(tv) => {
            // Header: name + sub-tab selector.
            let sub = crate::ui::overlay::DETAIL_SUB.load(std::sync::atomic::Ordering::Relaxed);
            let sub_strip = format!(
                " {i}{info}{r}  {w}{wire}{r} ",
                i = if sub == 0 { c_header(c) } else { c_dim(c) },
                info = "[i]nfo",
                r = c.reset(),
                w = if sub == 1 { c_header(c) } else { c_dim(c) },
                wire = "[w]ire",
            );
            rule_line(
                out,
                c,
                b,
                rule,
                Some(&format!("{}{}{}", tv.name, sub_strip, "")),
            );

            match sub {
                0 => {
                    // Info sub-view: key facts. Bind temporaries so &String lives
                    // long enough across the kv_row call.
                    let up_str = crate::utils::format_bytes_u64(tv.uploaded);
                    let s_str = tv.seeders.to_string();
                    let l_str = tv.leechers.to_string();
                    let nxt_str = fmt_mmss(tv.secs_to_announce);
                    let err_str = tv.error_count.to_string();
                    let err_col = if tv.error_count > 0 {
                        c_err(c)
                    } else {
                        c_dim(c)
                    };
                    let state_str = if tv.downloading {
                        "downloading"
                    } else {
                        "seeding"
                    };
                    let state_col = if tv.downloading { c_warn(c) } else { c_ok(c) };
                    kv_row(out, c, inner, line, "name", &tv.name, c.reset());
                    kv_row(out, c, inner, line, "uploaded", &up_str, &c_ok(c));
                    kv_row(out, c, inner, line, "seeders", &s_str, &c_ok(c));
                    kv_row(out, c, inner, line, "leechers", &l_str, &c_warn(c));
                    kv_row(out, c, inner, line, "next ann.", &nxt_str, &c_dim(c));
                    kv_row(out, c, inner, line, "errors", &err_str, &err_col);
                    kv_row(out, c, inner, line, "state", state_str, &state_col);
                    if tv.downloading {
                        let prog_str = format!("{}%", tv.dl_percent);
                        kv_row(out, c, inner, line, "progress", &prog_str, &c_warn(c));
                    }
                    for url in tv.urls.iter() {
                        let u = truncate(url, inner.saturating_sub(20), c.utf8);
                        line(
                            out,
                            &format!("{d}  {u}{r}", d = c_dim(c), r = c.reset()),
                            2 + dwidth(&u),
                        );
                    }
                }
                _ => {
                    // Wire sub-view — full implementation in F3.2 (needs per-torrent
                    // last-request/response capture in the announcer).
                    line(
                        out,
                        &format!(
                            "{d} wire capture not yet available (coming in F3.2){r}",
                            d = c_dim(c),
                            r = c.reset()
                        ),
                        dwidth(" wire capture not yet available (coming in F3.2)"),
                    );
                }
            }
            // Navigation hint — always visible at the bottom of the card.
            line(
                out,
                &format!(
                    "{d} Esc close · i info · w wire{r}",
                    d = c_dim(c),
                    r = c.reset()
                ),
                dwidth(" Esc close · i info · w wire"),
            );
        }
    }
}

// ---- [0/rto] ratio: cumulative upload graph (F1.1) --------------------------
fn build_ratio(out: &mut String, f: &Frame, c: &Caps, inner: usize, line: &Line) {
    let uptime = (f.now - f.started).num_seconds().max(0) as u64;
    let total_up = f.rows.iter().map(|t| t.uploaded).sum::<u64>();

    if f.up_history.len() < 2 {
        line(
            out,
            &format!(
                "{d} (no history yet — accumulates after the first tick){r}",
                d = c_dim(c),
                r = c.reset()
            ),
            dwidth(" (no history yet — accumulates after the first tick)"),
        );
        return;
    }

    // Graph area: height = available rows - 2 (axes), width = inner - 10 (Y labels).
    let body_h = (f.term_h.saturating_sub(8)).clamp(4, 20);
    let label_w = 9usize; // "12.3 GB  " right-aligned Y axis
    let graph_w = inner.saturating_sub(label_w + 2).max(10);

    // If no bytes have been uploaded yet, show a flat empty graph rather than
    // filling the whole chart at zero (which happens when threshold == 0
    // makes every v >= threshold true).
    let max_up_raw = f.up_history.iter().map(|(_, v)| *v).max().unwrap_or(0);
    if max_up_raw == 0 {
        line(
            out,
            &format!(
                "{d} (no upload yet — graph fills as the session progresses){r}",
                d = c_dim(c),
                r = c.reset()
            ),
            dwidth(" (no upload yet — graph fills as the session progresses)"),
        );
        return;
    }
    let max_up = max_up_raw;
    let span_secs = uptime.max(1);

    // Raster: for each column pick the sample closest to that time offset.
    let cols: Vec<u64> = (0..graph_w)
        .map(|col| {
            let target_secs = (col as u64 * span_secs) / graph_w as u64;
            f.up_history
                .iter()
                .min_by_key(|(s, _)| (*s as u64).abs_diff(target_secs))
                .map(|(_, v)| *v)
                .unwrap_or(0)
        })
        .collect();

    // Draw graph top-to-bottom.
    for row in (0..body_h).rev() {
        let threshold = (max_up * (row as u64 + 1)) / body_h as u64;
        let y_label = if row == body_h - 1 {
            crate::utils::format_bytes_u64(max_up)
        } else if row == 0 {
            "0 B".to_string()
        } else {
            String::new()
        };
        let mut graph_row = String::new();
        graph_row.push_str(&format!(
            "{d}{:>label_w$} {r}",
            y_label,
            d = c_dim(c),
            r = c.reset(),
            label_w = label_w
        ));
        for &v in &cols {
            let (glyph, col) = if v >= threshold {
                (if c.utf8 { "▆" } else { "#" }, c_ok(c))
            } else {
                (" ", c_dim(c))
            };
            graph_row.push_str(&col);
            graph_row.push_str(glyph);
            graph_row.push_str(c.reset());
        }
        let vis = label_w + 1 + graph_w;
        line(out, &graph_row, vis);
    }

    // X axis.
    let axis = {
        let mut s = format!("{d}{:>label_w$} ", "", d = c_dim(c), label_w = label_w);
        for _ in 0..graph_w {
            s.push('─');
        }
        s.push_str(c.reset());
        s
    };
    line(out, &axis, label_w + 1 + graph_w);

    // Summary footer.
    let peak_speed = crate::ui::history::session_peak();
    let summary = format!(
        "{d} session {uptime}  total ↑ {tot}  peak {pk}/s{r}",
        d = c_dim(c),
        r = c.reset(),
        uptime = fmt_hms(uptime),
        tot = crate::utils::format_bytes_u64(total_up),
        pk = crate::utils::format_bytes(peak_speed.min(u32::MAX as u64) as u32),
    );
    let vis = dwidth(&format!(
        " session {}  total ↑ {}  peak {}/s",
        fmt_hms(uptime),
        crate::utils::format_bytes_u64(total_up),
        crate::utils::format_bytes(peak_speed.min(u32::MAX as u64) as u32)
    ));
    line(out, &summary, vis.min(inner.saturating_sub(2)));
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
        let colored = format!(
            "{}{}HELLO{}",
            c_ok(&Caps {
                color: true,
                truecolor: false,
                utf8: true
            }),
            "",
            RESET
        );
        let (out, vis) = clamp_visible(&colored, 3, true);
        assert!(vis <= 3);
        assert!(
            out.ends_with(RESET),
            "truncated colored span must end with RESET: {out:?}"
        );
    }

    #[test]
    fn meter_bar_fill_scales_and_clamps() {
        // Color off + ASCII so the body is plain '#' (full) / '-' (empty)
        // between '[' and ']' — easy to count without parsing ANSI.
        let c = Caps {
            color: false,
            truecolor: false,
            utf8: false,
        };
        let fills = |s: &str| s.matches('#').count();
        // Always the same total width: '[' + w cells + ']'.
        for (val, max, w) in [(0u64, 100u64, 10usize), (50, 100, 10), (100, 100, 10)] {
            let bar = meter_bar(val, max, w, &c);
            assert_eq!(bar.chars().count(), w + 2, "bar must be w+2 chars: {bar:?}");
        }
        assert_eq!(fills(&meter_bar(0, 100, 10, &c)), 0); // idle: empty
        assert_eq!(fills(&meter_bar(50, 100, 10, &c)), 5); // half
        assert_eq!(fills(&meter_bar(100, 100, 10, &c)), 10); // full
        // Over-max never overflows the bar width.
        assert_eq!(fills(&meter_bar(999, 100, 10, &c)), 10);
        // max==0 (no peak yet) must not divide-by-zero; treated as empty.
        let bar = meter_bar(5, 0, 10, &c);
        assert_eq!(bar.chars().count(), 12);
    }
}
