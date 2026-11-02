//! The window as a whole: the tabs it opens again, its settings, and closing it.

use std::path::PathBuf;

use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::dialogs;
use crate::layout::{self, Layout, SavedTab};
use crate::session;
use crate::settings::Theme;
use crate::terminal;
use crate::update;

use super::tabs::{Panel, TabKey};
use super::{App, Widgets};

use super::message::AppMsg;
use relm4::gtk::gio;
use std::path::Path;
impl App {
    /// Opens the tabs that were open when Burrow last closed, once, when
    /// the nodes they belong to are known. Nodes deleted since are skipped.
    pub(super) fn restore_tabs(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let Some(layout) = self.restore.take() else {
            return;
        };
        let shells = terminal::shells();
        let notebook = widgets.notebook.clone();
        let mut current = None;
        for (i, tab) in layout.tabs.into_iter().enumerate() {
            let before = notebook.n_pages();
            match tab {
                SavedTab::Terminal { shell, folder } => {
                    // A shell no longer installed gives way to the login one.
                    let shell = Some(shell).filter(|shell| shells.contains(shell));
                    let folder = folder.and_then(|folder| layout::safe_folder(&folder));
                    self.open_terminal(shell, folder.as_deref(), widgets, sender);
                }
                SavedTab::Node { node } => self.open_tab_now(node, widgets, sender),
                SavedTab::Shell { node } => self.new_shell(node, None, widgets, sender),
                SavedTab::Files { node } => self.open_files(node, widgets, sender),
                SavedTab::Monitor { node } => self.open_monitor(node, widgets, sender),
            }
            if i == layout.current && notebook.n_pages() > before {
                current = Some(notebook.n_pages() - 1);
            }
        }
        if current.is_some() {
            notebook.set_current_page(current);
        }
        self.save_tabs(widgets);
    }

    /// Writes the open tabs, in the order they are shown, for the next start.
    pub(super) fn save_tabs(&mut self, widgets: &Widgets) {
        if self.restore.is_some() {
            return;
        }
        let notebook = &widgets.notebook;
        let mut layout = Layout::default();
        for i in 0..notebook.n_pages() {
            let Some(page) = notebook.nth_page(Some(i)) else {
                continue;
            };
            let Some((&key, tab)) = self.tabs.iter().find(|(_, tab)| tab.page == page) else {
                continue;
            };
            if notebook.current_page() == Some(i) {
                layout.current = layout.tabs.len();
            }
            layout.tabs.push(match (key, &tab.panel) {
                (_, Panel::Terminal { shell }) => SavedTab::Terminal {
                    shell: shell.clone(),
                    folder: terminal::in_page(&tab.page).and_then(|t| terminal::folder(&t)),
                },
                // A log or an action is for now, not for the next start.
                (TabKey::Shell(..), Panel::Shell(remote)) if remote.run.is_some() => continue,
                (TabKey::Shell(node, _), _) => SavedTab::Shell { node },
                (TabKey::Files(node), _) => SavedTab::Files { node },
                (TabKey::Monitor(node), _) => SavedTab::Monitor { node },
                (TabKey::Node(node), _) => SavedTab::Node { node },
                (TabKey::Terminal(_), _) => continue,
            });
        }
        if let Err(e) = layout.save(&self.state_dir) {
            self.fail(format!("Could not save the open tabs: {e}"));
        }
    }

    /// Closes the window, first asking when that would end terminals: local
    /// shells end with the window, and burrowd closes this client's SSH
    /// sessions when it goes.
    /// Closes the window, asking first if terminals would end. `restart`
    /// is started in its place once it has closed.
    pub(super) fn close_window(&self, window: &gtk::ApplicationWindow, restart: Option<PathBuf>) {
        let close = move |window: &gtk::ApplicationWindow| {
            if let Some(program) = restart {
                update::restart_into(program);
            }
            window.destroy();
        };
        let open = (self.tabs.values())
            .filter(|tab| match &tab.panel {
                Panel::Shell(remote) => matches!(remote.state, session::State::Running(_)),
                Panel::Terminal { .. } => true,
                _ => false,
            })
            .count();
        if open == 0 {
            return close(window);
        }
        let detail = if open == 1 {
            "A terminal is open. Closing Burrow ends it.".to_string()
        } else {
            format!("{open} terminals are open. Closing Burrow ends them.")
        };
        let target = window.clone();
        dialogs::confirm(window, "Close Burrow?", &detail, "Close", move || {
            close(&target)
        });
    }

    pub(super) fn apply_theme(&self) {
        let Some(settings) = gtk::Settings::default() else {
            return;
        };
        match self.settings.theme {
            // Resetting, rather than setting the startup value, hands the
            // setting back to the desktop so later light/dark switches from
            // the portal still apply.
            Theme::System => settings.reset_property("gtk-interface-color-scheme"),
            Theme::Light => {
                settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Light)
            }
            Theme::Dark => settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark),
        }
    }

    pub(super) fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.config_dir) {
            self.fail(format!("Could not save settings: {e}"));
        }
    }

    /// Moves the keyboard focus to the sidebar, showing it if it's hidden.
    pub(super) fn focus_sidebar(&mut self, widgets: &Widgets) {
        if !self.settings.sidebar_visible {
            self.settings.sidebar_visible = true;
            widgets.sidebar_pane.set_visible(true);
            self.save_settings();
        }
        widgets.sidebar.focus();
    }

    pub(super) fn toggle_sidebar(&mut self, widgets: &Widgets) {
        self.settings.sidebar_visible = !self.settings.sidebar_visible;
        widgets
            .sidebar_pane
            .set_visible(self.settings.sidebar_visible);
        self.save_settings();
    }

    /// `None` goes back to the system's monospace font.
    pub(super) fn set_terminal_font(&mut self, font: Option<String>) {
        self.settings.terminal_font = font;
        for tab in self.tabs.values() {
            if let Some(terminal) = terminal::in_page(&tab.page) {
                terminal::set_font(&terminal, self.settings.terminal_font.as_deref());
            }
        }
        self.save_settings();
    }
}

/// Checks for an update and offers to install it; [`AppMsg::Restart`]
/// follows if the user takes it.
pub(super) fn check_for_updates(window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) {
    let sender = sender.clone();
    update::show(window, move |program| {
        sender.input(AppMsg::Restart(program))
    });
}

/// Opens `folder` in the file manager.
pub(super) fn open_folder(window: &gtk::ApplicationWindow, folder: &Path) {
    let folder = gio::File::for_path(folder);
    gtk::FileLauncher::new(Some(&folder)).launch(Some(window), None::<&gio::Cancellable>, |_| {});
}
