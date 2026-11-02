//! The notebook's tabs: what each shows, and opening, closing and titling them.

use std::rc::Rc;

use burrow_core::{Command, NodeId, SessionId, TerminalRun};
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::database::Database;
use crate::desktop::Desktop;
use crate::files::Files;
use crate::monitor::Monitor;
use crate::remote::Remote;
use crate::runtime::runtime;
use crate::session;
use crate::tunnels;

use super::chrome::send_on_click;
use super::message::AppMsg;
use super::{App, Widgets};

/// What a tab shows. Node tabs follow their node; terminal tabs belong to
/// no node. Shells and terminals are numbered as they open, and numbers
/// are never reused, so a late reply for a closed tab can't reach a new
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabKey {
    /// A node's own tab. SSH hosts have shells instead.
    Node(NodeId),
    /// A shell on an SSH host. A host can have several.
    Shell(NodeId, u32),
    /// An SSH host's file panel.
    Files(NodeId),
    /// An SSH host's monitor.
    Monitor(NodeId),
    Terminal(u32),
}

impl TabKey {
    pub(super) fn node(self) -> Option<NodeId> {
        match self {
            TabKey::Node(id) | TabKey::Shell(id, _) | TabKey::Files(id) | TabKey::Monitor(id) => {
                Some(id)
            }
            TabKey::Terminal(_) => None,
        }
    }
}

pub(super) struct Tab {
    pub(super) page: gtk::Widget,
    pub(super) title: gtk::Label,
    pub(super) panel: Panel,
}

/// What a tab shows, with the state it keeps.
pub(super) enum Panel {
    /// An SSH host's shell.
    Shell(Remote),
    Files(Files),
    /// Boxed, being much bigger than the other panels.
    Monitor(Box<Monitor>),
    Database(Database),
    /// Boxed, being much bigger than most panels.
    Desktop(Box<Desktop>),
    Tunnel(Rc<tunnels::TunnelPage>),
    /// The box an addon draws its view in.
    View(gtk::Box),
    /// A node Burrow can't open, described by [`fill_page`].
    Placeholder(gtk::Box),
    /// A local shell. VTE runs it, so only its path is kept, to open it
    /// again at the next start.
    Terminal {
        shell: String,
    },
}

impl Panel {
    /// The state of the tab's own session, for panels that run one.
    /// Tunnels run whether or not their tab is open, so theirs isn't.
    pub(super) fn session(&self) -> Option<session::State> {
        match self {
            Panel::Shell(remote) => Some(remote.state),
            Panel::Files(panel) => Some(panel.session.state),
            Panel::Monitor(panel) => Some(panel.session.state),
            Panel::Database(panel) => Some(panel.session.state),
            Panel::Desktop(panel) => Some(panel.session.state),
            Panel::Tunnel(_) | Panel::View(_) | Panel::Placeholder(_) | Panel::Terminal { .. } => {
                None
            }
        }
    }
}

impl App {
    /// Dims the titles of tabs whose session has ended. A shell's tooltip
    /// adds the title its shell set, as a local terminal's tab shows its
    /// shell's, while the shell runs.
    pub(super) fn refresh_tabs(&self) {
        for tab in self.tabs.values() {
            if tab.panel.session() == Some(session::State::Closed) {
                tab.title.add_css_class("dim");
            } else {
                tab.title.remove_css_class("dim");
            }
            if let Panel::Shell(remote) = &tab.panel {
                let label = tab.title.label();
                let tooltip = match &remote.title {
                    Some(title) => Some(format!("{label}\n{title}")),
                    None => title_tooltip(&label).map(str::to_string),
                };
                // Setting it, even to the same text, would redraw it on
                // every burst of output.
                if tab.title.tooltip_text().as_deref() != tooltip.as_deref() {
                    tab.title.set_tooltip_text(tooltip.as_deref());
                }
            }
        }
    }

    /// The tab whose own session is `session`.
    pub(super) fn tab_of(&self, session: SessionId) -> Option<TabKey> {
        let running = Some(session::State::Running(session));
        (self.tabs.iter())
            .find(|(_, tab)| tab.panel.session() == running)
            .map(|(key, _)| *key)
    }

    /// Switches to the tab `key`. Returns whether it is open.
    pub(super) fn show_tab(&self, key: TabKey, widgets: &Widgets) -> bool {
        let Some(tab) = self.tabs.get(&key) else {
            return false;
        };
        if let Some(index) = widgets.notebook.page_num(&tab.page) {
            widgets.notebook.set_current_page(Some(index));
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_tab(
        &mut self,
        key: TabKey,
        page: gtk::Widget,
        panel: Panel,
        icon: &str,
        name: &str,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let title = gtk::Label::new(None);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        set_tab_title(&title, name);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab (Ctrl+Shift+W)"));
        let tab_label = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tab_label.append(&gtk::Image::from_icon_name(icon));
        tab_label.append(&title);
        tab_label.append(&close);

        // Middle-click closes a tab, as in browsers and other terminals.
        let middle_click = gtk::GestureClick::new();
        middle_click.set_button(gdk::BUTTON_MIDDLE);
        middle_click.connect_released({
            let sender = sender.clone();
            move |_, _, _, _| sender.input(AppMsg::CloseTab(key))
        });
        tab_label.add_controller(middle_click);

        let notebook = &widgets.notebook;
        let index = notebook.append_page(&page, Some(&tab_label));
        notebook.set_tab_reorderable(&page, true);
        notebook.set_current_page(Some(index));

        send_on_click(&close, sender, AppMsg::CloseTab(key));
        self.tabs.insert(key, Tab { page, title, panel });
        self.save_tabs(widgets);
    }

    pub(super) fn close_tab(&mut self, key: TabKey, widgets: &Widgets) {
        let Some(tab) = self.tabs.remove(&key) else {
            return;
        };
        close_page(&widgets.notebook, &tab.page);
        self.save_tabs(widgets);
        // A session still starting is closed when its id arrives. Closing
        // a file panel's session stops its transfers too.
        if let Some(session::State::Running(session)) = tab.panel.session() {
            let core = self.core.clone();
            runtime().spawn(async move {
                let _ = core.execute(Command::CloseSession { session }).await;
            });
        }
    }

    /// Closes the tab that is shown.
    pub(super) fn close_current_tab(&mut self, widgets: &Widgets) {
        let current = widgets.notebook.nth_page(widgets.notebook.current_page());
        let key = self
            .tabs
            .iter()
            .find(|(_, tab)| current.as_ref() == Some(&tab.page))
            .map(|(key, _)| *key);
        if let Some(key) = key {
            self.close_tab(key, widgets);
        }
    }
}

pub(super) fn close_page(notebook: &gtk::Notebook, page: &gtk::Widget) {
    if let Some(index) = notebook.page_num(page) {
        notebook.remove_page(Some(index));
    }
}

/// Long titles (a shell's working folder, say) are cut short with "…".
/// Notebooks give tab labels only their minimum width, and an ellipsized
/// label's minimum is just the "…", so the minimum is set to fit the text.
/// A title that is cut short shows in full on hover.
pub(super) fn set_tab_title(label: &gtk::Label, title: &str) {
    label.set_label(title);
    label.set_width_chars(title.chars().count().min(MAX_TITLE_CHARS) as i32);
    label.set_tooltip_text(title_tooltip(title));
}

/// How many characters of a tab's title show before it is cut short.
pub(super) const MAX_TITLE_CHARS: usize = 24;

/// A tab title's tooltip: the title, if it is cut short.
pub(super) fn title_tooltip(title: &str) -> Option<&str> {
    (title.chars().count() > MAX_TITLE_CHARS).then_some(title)
}

/// The title of an SSH host's shell tab: the host for the first, and a
/// number for the rest.
/// "web" for a host's first shell, "web (2)" for the next, and what runs
/// for a tab that runs something else.
pub(super) fn shell_title(host: &str, number: u32, run: Option<&TerminalRun>) -> String {
    match run {
        Some(TerminalRun::Logs { service }) => format!("{host}: {} log", service.name()),
        Some(TerminalRun::Control { service, action }) => {
            format!("{host}: {} {}", action.verb(), service.name())
        }
        None if number == 1 => host.to_string(),
        None => format!("{host} ({number})"),
    }
}

/// The title of an SSH host's monitor tab.
pub(super) fn monitor_title(host: &str) -> String {
    format!("{host} monitor")
}

/// The title of an SSH host's file panel tab.
pub(super) fn files_title(host: &str) -> String {
    format!("{host} files")
}

/// Moves to the next (`1`) or previous (`-1`) tab, wrapping around.
pub(super) fn switch_tab(notebook: &gtk::Notebook, step: i32) {
    let pages = notebook.n_pages() as i32;
    if let Some(current) = notebook.current_page().filter(|_| pages > 0) {
        let next = (current as i32 + step).rem_euclid(pages);
        notebook.set_current_page(Some(next as u32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_core::{Service, ServiceAction, TerminalRun};

    #[test]
    fn shells_after_the_first_are_numbered() {
        assert_eq!(shell_title("web", 1, None), "web");
        assert_eq!(shell_title("web", 2, None), "web (2)");
        let service = Service::Unit {
            name: "nginx.service".into(),
        };
        let logs = TerminalRun::Logs {
            service: service.clone(),
        };
        assert_eq!(shell_title("web", 0, Some(&logs)), "web: nginx.service log");
        let action = ServiceAction::Restart;
        let control = TerminalRun::Control { service, action };
        assert_eq!(
            shell_title("web", 0, Some(&control)),
            "web: restart nginx.service"
        );
    }
}
