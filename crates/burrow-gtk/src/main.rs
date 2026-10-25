//! Burrow's GTK4 front end. It is a thin client: every change goes through
//! `burrow_core::Core`, and the window redraws from the core's events.

mod addons;
mod app;
mod dialogs;
mod files;
mod palette;
mod remote;
mod runtime;
mod settings;
mod sidebar;
mod terminal;
mod tunnels;

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

/// `BURROW_CONFIG_DIR` points Burrow at another config folder,
/// `BURROW_STATE_DIR` at another state folder (addon approvals), and
/// `BURROW_SSH_CONFIG` at another OpenSSH config (as `ssh -F`), which keeps
/// development runs away from your real ones.
fn core_options() -> burrow_core::Result<CoreOptions> {
    let mut options = CoreOptions::system()?;
    if let Some(dir) = std::env::var_os("BURROW_CONFIG_DIR") {
        options.config_dir = PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("BURROW_STATE_DIR") {
        options.state_dir = PathBuf::from(dir);
    }
    if let Some(file) = std::env::var_os("BURROW_SSH_CONFIG") {
        options.ssh_config = Some(PathBuf::from(file));
    }
    Ok(options)
}

fn fail(error: &burrow_core::Error) -> ! {
    eprintln!("burrow: {error}");
    std::process::exit(1);
}
