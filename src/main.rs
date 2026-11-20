// No console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod git;
mod github;
mod highlight;
mod log;
mod ops;
mod patch;
mod store;
mod util;
mod watch;

slint::include_modules!();

use slint::ComponentHandle;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    log::init(&store::data_dir());
    log::info(&format!(
        "Gitust {} starting on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    // Two workers are plenty: git2 work runs on the blocking pool, network is async.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("gitust-worker")
        .enable_all()
        .build()?;

    let store = store::Store::open_default().or_else(|e| {
        log::error(&format!("settings database unavailable ({e:#}); using an in-memory store"));
        store::Store::in_memory()
    })?;

    // Slint's software renderer idles at ~10 MB private memory versus ~90 MB for an
    // OpenGL context, with identical output for this UI. `SLINT_BACKEND=winit-femtovg`
    // switches back to the GPU renderer (e.g. for very large HiDPI windows).
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let _ = slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select();
    }

    let ui = MainWindow::new()?;
    let app = app::App::new(&ui, rt.handle().clone(), store);
    app.bind(&ui);
    // `gitust [path]` opens a repository directly, like `code .`.
    let initial =
        std::env::args_os().nth(1).map(std::path::PathBuf::from).map(|p| std::path::absolute(&p).unwrap_or(p));
    app.restore(&ui, initial);

    ui.show()?;
    ui.global::<AppState>().set_startup_time(format!("{} ms", started.elapsed().as_millis()).into());
    slint::run_event_loop()?;

    drop(ui);
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    Ok(())
}
