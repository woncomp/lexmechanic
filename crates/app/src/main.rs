#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::*;

mod assets;
mod document;
mod persistence;
mod runtime;
mod session_table;
mod tabs;
mod workbench;

fn main() {
    let directory = std::env::var_os("LEXMECHANIC_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let directory = directories::ProjectDirs::from("", "", "Lexmechanic")
                .expect("Cannot locate application data directory")
                .data_local_dir()
                .to_owned();
            migrate_legacy_data(&directory);
            directory
        });
    std::fs::create_dir_all(&directory).expect("Cannot create application data directory");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("Lexmechanic.log"))
        .expect("Cannot open application log");
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::sync::Mutex::new(log))
        .init();
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            gpui_kit::open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("Lexmechanic".into()),
                        ..Default::default()
                    }),
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1280.), px(820.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| workbench::Workbench::new(directory, window, cx)),
            )
            .expect("Could not open Lexmechanic window");
            cx.activate(true);
        });
}

/// Carries the index, settings and logs over from the app's former name, once.
fn migrate_legacy_data(directory: &std::path::Path) {
    if directory.exists() {
        return;
    }
    let Some(legacy) = directories::ProjectDirs::from("", "", "AgentChatReader") else {
        return;
    };
    let legacy = legacy.data_local_dir();
    if !legacy.is_dir() {
        return;
    }
    if let Some(parent) = directory.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Left in place on failure: the app then starts with a fresh data directory.
    let _ = std::fs::rename(legacy, directory);
}
