mod annotations;
mod app;
mod pdf;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../logo.png"))
        .expect("embedded application logo must be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Simple PDF Viewer")
            .with_icon(icon)
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([720.0, 480.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "simple-pdf-viewer",
        options,
        Box::new(move |cc| Ok(Box::new(app::PdfViewerApp::new(cc, initial_path)))),
    )
}
