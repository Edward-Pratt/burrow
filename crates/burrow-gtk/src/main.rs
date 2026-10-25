//! Burrow's GTK4 front end. It is a thin client: every change goes through
//! the core in `burrowd`, and the window redraws from the core's events.

mod addons;
mod app;
mod database;
mod dialogs;
mod files;
mod monitor;
mod palette;
mod remote;
mod runtime;
mod settings;
mod sidebar;
mod terminal;
mod tunnels;

use burrow_core::Paths;
use burrow_core::ipc::{self, Role};

use crate::runtime::runtime;

const APP_ID: &str = "dev.burrow.Burrow";

fn main() {
    // Before GTK or tokio start any threads: this may be the daemon.
    burrow_core::daemon::run_if_asked();
    let config_dir = match Paths::from_env() {
        Ok(paths) => paths.config_dir,
        Err(e) => fail(&e),
    };
    let core = match runtime().block_on(ipc::connect(Role::Gui)) {
        Ok(client) => client,
        Err(e) => fail(&e),
    };

    let app = relm4::RelmApp::new(APP_ID);
    relm4::set_global_css(include_str!("style.css"));
    app.run::<app::App>(app::Init { core, config_dir });
}

fn fail(error: &burrow_core::Error) -> ! {
    eprintln!("burrow: {error}");
    std::process::exit(1);
}
