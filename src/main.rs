// No console window on Windows release builds.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod border;
mod branding;
mod capture;
mod config;
mod editor;
mod guide;
mod h264;
mod icon;
mod library;
mod loupe;
mod ocr;
mod open_requests;
mod overlay;
mod platform;
mod player;
mod recorder;
mod resize;
mod scroll;
mod shortcuts;
mod theme;
mod update;

use std::sync::Arc;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("rega=info")).format_timestamp_millis().init();

    log_panics();

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

    // A file to open: a guide double-clicked in Explorer comes as the argument
    // (macOS sends an event instead, see `open_requests`). With Rega already
    // running, that copy takes it and this one is done.
    #[cfg(target_os = "macos")]
    open_requests::mac::install();
    if let Some(file) = std::env::args_os().nth(1).map(std::path::PathBuf::from).filter(|p| p.is_file()) {
        let file = std::path::absolute(&file).unwrap_or(file);
        #[cfg(windows)]
        if let Some(port_file) = open_requests::handoff::port_file() {
            // Started by the user's double-click, this copy may bring a window
            // to the front; pass that on, or the other's would only flash.
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
                let _ = AllowSetForegroundWindow(ASFW_ANY);
            }
            if open_requests::handoff::forward(&port_file, &file) {
                return Ok(());
            }
        }
        open_requests::push(file);
    }
    #[cfg(windows)]
    if let Some(port_file) = open_requests::handoff::port_file()
        && let Err(e) = open_requests::handoff::listen(&port_file, open_requests::push)
    {
        log::warn!("files opened while Rega runs won't reach it: {e}");
    }

    let (rgba, width, height) = icon::app_icon(256);
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut options = eframe::NativeOptions {
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
    #[cfg(windows)]
    prefer_native_renderer(&mut options.wgpu_options);
    eframe::run_native(
        config::APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(app::RegaApp::new(cc)))),
    )
}

/// Windows: draw with DirectX 12 on the GPU that drives the display. By default
/// wgpu takes Vulkan (it is enumerated first) on the most powerful GPU, and on
/// laptops with two GPUs that hands every frame across to the other one, which
/// shows as the whole window blinking blank while it animates. OpenGL stays as a
/// fallback; `WGPU_BACKEND` and `WGPU_POWER_PREF` still override both choices.
#[cfg(windows)]
fn prefer_native_renderer(options: &mut eframe::egui_wgpu::WgpuConfiguration) {
    use eframe::egui_wgpu::{WgpuSetup, wgpu};
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_setup {
        setup.instance_descriptor.backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::DX12 | wgpu::Backends::GL);
        setup.power_preference = wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    }
}

/// Keep what a panic says. A panic while macOS is calling into Rega (a redraw,
/// say) ends as an abort, and the system's crash report has neither the
/// message nor the line. Appended to `crash.log` next to the settings, on top
/// of the usual message on stderr.
fn log_panics() {
    let usual = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = dirs::config_dir().map(|d| d.join(config::APP_NAME)) {
            use std::io::Write;
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                let thread = std::thread::current();
                let _ = writeln!(
                    f,
                    "{} Rega {} on thread {:?}: {info}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    env!("CARGO_PKG_VERSION"),
                    thread.name().unwrap_or("unnamed"),
                );
            }
        }
        usual(info);
    }));
}
