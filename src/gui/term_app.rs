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

use std::sync::mpsc::Receiver;

use egui_term::{BackendSettings, PtyEvent, TerminalBackend, TerminalView};

pub struct TermApp {
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
}

impl TermApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
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

        Self { backend, pty_rx }
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
            .set_size(ui.available_size());
        ui.add(term);
    }
}
