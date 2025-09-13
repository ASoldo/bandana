mod app;
mod build;
mod build_meta;
mod fs_watcher;
mod preview;
mod project;

use anyhow::Result;

fn main() -> Result<()> {
    let native_options = eframe::NativeOptions::default();
    let _ = eframe::run_native(
        "Bevy Editor",
        native_options,
        Box::new(|cc| Ok(Box::new(app::EditorApp::new(cc)))),
    );
    Ok(())
}
