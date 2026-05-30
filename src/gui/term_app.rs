//! The native window as a REAL embedded terminal.
//!
//! Instead of re-drawing the dashboard in egui (which could drift from the
//! actual terminal output), the window hosts an alacritty-backed terminal widget
//! (`egui_term`) and PTY-spawns THIS binary again with `--tty`. The child runs
//! the unmodified crossterm dashboard (src/ui), so the window is the shell
//! dashboard byte-for-byte — same 9 tabs, same colors, same keys.
//!
//! The child takes the CLI/TTY path because `--tty` overrides the bundle/GUI
//! detection in `main()`. It inherits this process's environment, so we set
//! TERM/COLORTERM/LANG here to guarantee truecolor + UTF-8 box-drawing even when
//! launched from a bundle (which has a minimal env).

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use egui_term::{
    BackendSettings, FontSettings, PtyEvent, TerminalBackend, TerminalFont, TerminalView,
};

/// Bundled monospace font with full Box-Drawing / Block-Elements / arrow
/// coverage (JetBrains Mono Nerd Font, OFL). egui's default monospace renders
/// some of these as thin/tofu glyphs; this guarantees the dashboard's borders
/// (╭ ─ ┤ │ █ ░ ↑) look like a real terminal.
const TERM_FONT_NAME: &str = "ratioup-mono";
const TERM_FONT_BYTES: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf");

/// Dense but still comfortable cell size — denser than a GUI font, not as tiny
/// as a packed terminal. On 2× Retina (12.5×2 = 25px) it lands on whole device
/// pixels, so box-drawing stays crisp.
const FONT_SIZE: f32 = 12.5;

/// Register the bundled font as the egui Monospace family (egui_term draws with
/// `FontId::monospace`, so this is what the terminal will use).
fn install_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        TERM_FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_static(TERM_FONT_BYTES)),
    );
    // Front of the Monospace fallback chain so our glyphs win.
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, TERM_FONT_NAME.to_owned());
    ctx.set_fonts(fonts);
}

pub struct TermApp {
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
    font: TerminalFont,
}

impl TermApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_font(&cc.egui_ctx);

        // The child inherits our env; force a capable terminal profile so colors
        // and box-drawing render identically to the user's own terminal.
        // SAFETY: set before the PTY child is spawned; single-threaded at this
        // point (eframe creation), no other thread reads the environment.
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            if std::env::var_os("LANG").is_none() {
                std::env::set_var("LANG", "en_US.UTF-8");
            }
        }

        // Re-launch ourselves in TTY mode inside the PTY.
        let exe = std::env::current_exe()
            .expect("current_exe")
            .to_string_lossy()
            .into_owned();
        let working_directory = std::env::current_dir().ok();

        let (pty_tx, pty_rx) = std::sync::mpsc::channel();
        let backend = TerminalBackend::new(
            0,
            cc.egui_ctx.clone(),
            pty_tx,
            BackendSettings {
                shell: exe,
                args: vec!["--tty".to_string()],
                working_directory,
            },
        )
        .expect("spawn embedded terminal");

        let font = TerminalFont::new(FontSettings {
            font_type: egui::FontId::new(FONT_SIZE, egui::FontFamily::Monospace),
        });

        Self {
            backend,
            pty_rx,
            font,
        }
    }
}

impl eframe::App for TermApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // When the child (the dashboard) exits — e.g. the user pressed `q` — the
        // PTY reports Exit; close the window to match.
        if let Ok((_, PtyEvent::Exit)) = self.pty_rx.try_recv() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let term = TerminalView::new(ui, &mut self.backend)
            .set_focus(true)
            .set_font(self.font.clone())
            .set_size(ui.available_size());
        ui.add(term);
    }
}
