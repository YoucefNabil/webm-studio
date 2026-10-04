#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod media;
mod model;
mod render;
mod theme;
mod timeline;
mod video;
#[cfg(test)]
mod selftest;

fn main() -> eframe::Result<()> {
    ffmpeg_next::init().expect("ffmpeg init");
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("WebM Studio").with_inner_size([1500.0, 920.0]).with_min_inner_size([900.0, 600.0]).with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("WebM Studio", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
