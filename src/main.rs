//! discord-native: a lightweight native Discord client.
//!
//! Design goals, in order: functionality, low CPU, low RAM.
//! - egui with the OpenGL (glow) backend, repainting only on input/events.
//! - One network thread with a single-threaded tokio runtime.
//! - zlib-stream gateway compression, lenient zero-copy-ish payload parsing.
//! - Bounded message caches; no images are decoded or kept in memory.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod markdown;
mod model;
mod net;
mod store;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("discord-native")
            .with_app_id("discord-native")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true),
        // No MSAA / depth / stencil buffers: egui doesn't need them.
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };
    eframe::run_native("discord-native", options, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))))
}
