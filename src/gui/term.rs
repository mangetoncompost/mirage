//! Tiny monospace "terminal" renderer for egui: build a screen as rows of
//! colored segments, then paint them as tight monospace `RichText`. This is how
//! we reproduce the pure-shell mockup (box-drawing borders, aligned columns,
//! `[####----]` bars) inside a native window — no default egui widgets.

use egui::{Color32, RichText};

use super::theme;

/// Inner width of the board in characters (matches the HTML mockup: w = 64).
pub const W: usize = 64;

/// One colored run of text on a line.
pub struct Seg {
    pub text: String,
    pub color: Color32,
}

/// A line = a sequence of colored segments.
#[derive(Default)]
pub struct Line {
    pub segs: Vec<Seg>,
}

impl Line {
    pub fn new() -> Self {
        Self::default()
    }
    /// Append a colored run.
    pub fn push(&mut self, text: impl Into<String>, color: Color32) {
        self.segs.push(Seg { text: text.into(), color });
    }
    /// Visible width (char count across all segments).
    pub fn width(&self) -> usize {
        self.segs.iter().map(|s| s.text.chars().count()).sum()
    }
    /// Pad with spaces (dim) to `n` columns.
    pub fn pad_to(&mut self, n: usize) {
        let w = self.width();
        if w < n {
            self.push(" ".repeat(n - w), theme::DIM);
        }
    }
}

/// A screen is a list of lines, painted top-to-bottom.
#[derive(Default)]
pub struct Screen {
    pub lines: Vec<Line>,
}

impl Screen {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn line(&mut self) -> &mut Line {
        self.lines.push(Line::new());
        self.lines.last_mut().unwrap()
    }
    /// A full-width horizontal rule `├──────┤` style with optional left label.
    pub fn rule(&mut self, left: &str, right: &str, label: Option<&str>) {
        let l = self.line();
        l.push(left.to_string(), theme::LINE);
        match label {
            Some(lbl) => {
                let seg = format!("─ {lbl} ");
                let used = seg.chars().count();
                l.push(seg, theme::LINE);
                if used < W {
                    l.push("─".repeat(W - used), theme::LINE);
                }
            }
            None => {
                l.push("─".repeat(W), theme::LINE);
            }
        }
        l.push(right.to_string(), theme::LINE);
    }
    /// A bordered content line: `│ <content padded to W> │`.
    pub fn bordered(&mut self, build: impl FnOnce(&mut Line)) {
        let mut inner = Line::new();
        build(&mut inner);
        inner.pad_to(W);
        let l = self.line();
        l.push("│", theme::LINE);
        l.segs.extend(inner.segs);
        l.push("│", theme::LINE);
    }

    /// Paint the screen as monospace rows.
    pub fn show(self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for line in self.lines {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for seg in line.segs {
                        ui.label(RichText::new(seg.text).monospace().color(seg.color));
                    }
                });
            }
        });
    }
}

/// Left-pad/truncate to exactly `n` columns (right-aligned numbers).
pub fn rpad(s: impl ToString, n: usize) -> String {
    let s = s.to_string();
    let c = s.chars().count();
    if c >= n {
        s.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect()
    } else {
        format!("{}{}", " ".repeat(n - c), s)
    }
}
/// Right-pad/truncate to exactly `n` columns (left-aligned text).
pub fn lpad(s: impl ToString, n: usize) -> String {
    let s = s.to_string();
    let c = s.chars().count();
    if c >= n {
        s.chars().take(n).collect()
    } else {
        format!("{}{}", s, " ".repeat(n - c))
    }
}

/// ASCII progress bar `[####----]` of inner width `n` cells, into a line.
pub fn bar(line: &mut Line, done: u64, total: u64, n: usize, downloading: bool) {
    let f = if total == 0 { 0 } else { ((n as u64 * done) / total).min(n as u64) as usize };
    line.push("[", theme::DIM);
    line.push("#".repeat(f), if downloading { theme::YL } else { theme::GN });
    line.push("-".repeat(n - f), Color32::from_rgb(0x24, 0x30, 0x24));
    line.push("]", theme::DIM);
}
