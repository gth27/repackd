use anyhow::{anyhow, Result};

fn main() -> Result<()> {
    // two windows on one library: each saves its own copy, and the last to save drops the other's changes
    let lock_path = repackd::data_file().with_file_name("repackd.lock");
    let _ = std::fs::create_dir_all(lock_path.parent().unwrap_or(&lock_path));
    let lock = std::fs::File::create(&lock_path)?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        rfd::MessageDialog::new().set_title("repackd").set_description("repackd is already open.").show();
        return Ok(());
    }
    let lib = match repackd::Library::load(repackd::data_file(), repackd::games_root()) {
        Ok(l) => l,
        Err(e) => {
            // started from the desktop menu nobody sees stderr; the file is left as it is so nothing is lost
            rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("repackd").set_description(format!("{e:#}\n\nFix or move this file, then start repackd again.")).show();
            return Err(e);
        }
    };
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_title("repackd").with_app_id("repackd").with_inner_size([1000.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("repackd", opts, Box::new(|cc| {
        egui_extras::install_image_loaders(&cc.egui_ctx); // covers and hero art in the library
        Ok(Box::new(repackd::ui::App::new(lib)))
    })).map_err(|e| anyhow!("window: {e}"))
}
