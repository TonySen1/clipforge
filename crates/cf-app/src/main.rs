// Release builds are a GUI app: launching the exe must not flash a console
// window. Debug builds keep the console so `env_logger` output stays visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod export;
mod frame_cache;
mod import;
mod preview;

use eframe::egui;
use eframe::egui::IconData;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Files passed on the command line (e.g. "Open with" or dragging onto the
    // executable) are imported once the window is up.
    let startup_files: Vec<std::path::PathBuf> = std::env::args_os()
        .skip(1)
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ClipForge")
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([960.0, 620.0])
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "ClipForge",
        options,
        Box::new(move |cc| {
            let mut app = app::ClipForgeApp::new(cc);
            for path in startup_files {
                app.queue_import(path);
            }
            Ok(Box::new(app))
        }),
    )
}

fn load_icon() -> IconData {
    const APP_ICON: &[u8] = include_bytes!("../../../assets/icons/icon-256.png");
    match image::load_from_memory(APP_ICON) {
        Ok(image) => {
            let image = image.to_rgba8();
            let (width, height) = image.dimensions();
            IconData {
                rgba: image.into_raw(),
                width,
                height,
            }
        }
        Err(err) => {
            log::warn!("failed to decode window icon: {err}");
            IconData {
                rgba: Vec::new(),
                width: 0,
                height: 0,
            }
        }
    }
}
