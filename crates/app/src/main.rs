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
    let directory = std::env::var_os("AGENT_CHAT_READER_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("", "", "AgentChatReader")
                .expect("Cannot locate application data directory")
                .data_local_dir()
                .to_owned()
        });
    std::fs::create_dir_all(&directory).expect("Cannot create application data directory");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("AgentChatReader.log"))
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
                        title: Some("AgentChatReader".into()),
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
            .expect("Could not open AgentChatReader window");
            cx.activate(true);
        });
}
