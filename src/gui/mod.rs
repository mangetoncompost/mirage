//! Native macOS GUI (eframe/egui). eframe owns the main thread (macOS requires
//! UI on the main thread); the async RatioUp engine runs on a background tokio
//! runtime. They share state through the lock-free `control` layer: the GUI
//! reads `control::SNAPSHOT` each frame and sends `control::Cmd`s for mutations.

mod app;
mod theme;
mod views;

use std::path::PathBuf;

/// GUI entry point. Builds a background multi-thread tokio runtime, starts the
/// engine on it, then runs eframe on the main thread.
pub fn run() {
    // Logs go to stderr (visible when launched from a terminal); harmless in .app.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    // Config path: same resolution as the CLI (-c arg, else XDG).
    let config_path: Option<PathBuf> = crate::parse_cli_args().or_else(crate::get_config_from_xdg);

    // Background tokio runtime running the whole engine.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    std::thread::Builder::new()
        .name("ratioup-engine".into())
        .spawn(move || {
            // The runtime is owned by this thread and outlives it via block_on.
            rt.block_on(async move {
                let config = crate::engine::load_config(config_path).await;
                crate::engine::start(config).await;
            });
        })
        .expect("spawn engine thread");

    // eframe on the main thread.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("RatioUp"),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "RatioUp",
        options,
        Box::new(|cc| Ok(Box::new(app::RatioUpApp::new(cc)))),
    );
}
