//! Burrow's GTK4 front end. It is a thin client: every change goes through
//! `burrow_core::Core`, and the window redraws from the core's events.

mod app;
mod dialogs;
mod palette;
mod runtime;
mod settings;
mod sidebar;

use std::path::PathBuf;

use burrow_core::{Core, CoreOptions};

const APP_ID: &str = "dev.burrow.Burrow";

fn main() {
    let options = match core_options() {
        Ok(options) => options,
        Err(e) => fail(&e),
    };
    let config_dir = options.config_dir.clone();
    let core = match Core::open(options) {
        Ok(core) => core,
        Err(e) => fail(&e),
    };

    let app = relm4::RelmApp::new(APP_ID);
    relm4::set_global_css(include_str!("style.css"));
    app.run::<app::App>(app::Init { core, config_dir });
}

/// `BURROW_CONFIG_DIR` points Burrow at another config folder, which keeps
/// development runs away from your real one.
fn core_options() -> burrow_core::Result<CoreOptions> {
    let mut options = CoreOptions::system()?;
    if let Some(dir) = std::env::var_os("BURROW_CONFIG_DIR") {
        options.config_dir = PathBuf::from(dir);
    }
    Ok(options)
}

fn fail(error: &burrow_core::Error) -> ! {
    eprintln!("burrow: {error}");
    std::process::exit(1);
}
