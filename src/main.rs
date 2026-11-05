// No console window on Windows release builds.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod branding;
mod capture;
mod config;
mod editor;
mod h264;
mod icon;
mod library;
mod ocr;
mod overlay;
mod platform;
mod player;
mod recorder;
mod resize;
mod scroll;
mod shortcuts;
mod theme;

use std::sync::Arc;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("rega=info")).init();

    // Used by scripts/bundle-macos.sh to produce the .icns.
    // Used by the packaging scripts: `--write-icon <png|ico> <size>` (an .ico
    // gets every size) and `--write-branding <dir>` (DMG / installer artwork).
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, path, size] = args.as_slice()
        && flag == "--write-icon"
    {
        if path.ends_with(".ico") {
            branding::write_ico(std::path::Path::new(path)).expect("write icon");
        } else {
            let size = size.parse().unwrap_or(1024);
            let (rgba, w, h) = icon::app_icon(size);
            image::RgbaImage::from_raw(w, h, rgba).expect("icon").save(path).expect("write icon");
        }
        return Ok(());
    }
    if let [_, flag, dir] = args.as_slice()
        && flag == "--write-branding"
    {
        branding::write_all(std::path::Path::new(dir)).expect("write branding");
        return Ok(());
    }

    let (rgba, width, height) = icon::app_icon(256);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(config::APP_NAME)
            .with_inner_size([1360.0, 840.0])
            // The labeled top bar needs ~1180 pt; keep room for it.
            .with_min_inner_size([1300.0, 640.0])
            .with_icon(Arc::new(egui::IconData { rgba, width, height })),
        // Start as a menu-bar agent so our window may appear over other apps'
        // full-screen Spaces; the Dock icon is switched on while the main window
        // is open (see `platform::set_dock_icon`).
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        })),
        ..Default::default()
    };
    eframe::run_native(
        config::APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(app::RegaApp::new(cc)))),
    )
}
