//! Native macOS GUI (eframe/egui). The window is a REAL embedded terminal
//! (`egui_term`, alacritty-backed) that PTY-spawns this binary again in `--tty`
//! mode, so it renders the exact crossterm shell dashboard (src/ui) — same 9
//! tabs, same colors, same keys — byte-for-byte. The engine therefore runs in
//! the PTY CHILD process, not here; this process is a thin terminal host. See
//! `term_app` for the details and the `--tty` recursion guard in `main()`.

mod term_app;

/// GUI entry point: run eframe on the main thread hosting the terminal widget.
/// (No background engine here — the PTY child owns the engine.)
pub fn run() {
    // A small terminal-sized window. The embedded terminal lays out the
    // dashboard to whatever cols/rows fit; resizing forwards SIGWINCH to the
    // child so it re-flows, exactly like a real terminal.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 560.0])
            .with_min_inner_size([620.0, 380.0])
            .with_title("RatioUp"),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "RatioUp",
        options,
        Box::new(|cc| Ok(Box::new(term_app::TermApp::new(cc)))),
    );
}
