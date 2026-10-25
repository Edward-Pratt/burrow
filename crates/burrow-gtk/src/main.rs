//! Burrow's GTK4 front end. It is a thin client: every change goes through
//! the core in `burrowd`, and the window redraws from the core's events.

mod addons;
mod app;
mod database;
mod dialogs;
mod files;
mod hosts;
mod monitor;
mod palette;
mod remote;
mod runtime;
mod settings;
mod sidebar;
mod terminal;
mod tunnels;

use burrow_core::ipc::{self, Role};
use burrow_core::{Paths, doctor};
use relm4::gtk;
use relm4::gtk::{gio, glib};

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

/// Says why Burrow can't start: on stderr, and in a window when there is a
/// display, since a launcher shows nothing of stderr. The checks that
/// failed say what is missing and how to get it.
fn fail(error: &burrow_core::Error) -> ! {
    let mut text = error.to_string();
    for check in runtime().block_on(doctor::environment()) {
        if check.blocks() {
            let fix = check.fix.unwrap_or_default();
            // The daemon's error often is the check's finding.
            if !text.contains(&check.found) {
                text.push_str(&format!("\n\n{}: {}", check.name, check.found));
            }
            text.push_str(&format!("\n\nFix: {fix}"));
        }
    }
    // Paths from the environment go into it.
    let text: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    eprintln!("burrow: {text}");
    if gtk::init().is_ok() {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Burrow can't start")
            .detail(&text)
            .buttons(["Quit"])
            .build();
        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        dialog.choose(None::<&gtk::Window>, None::<&gio::Cancellable>, move |_| {
            quit.quit()
        });
        main_loop.run();
    }
    std::process::exit(1);
}
