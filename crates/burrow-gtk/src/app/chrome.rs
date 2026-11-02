//! Building the window: header bar, sidebar, tabs, status bar and shortcuts.

use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::sidebar;
use crate::terminal;

use super::menus::menu_popover;
use super::message::AppMsg;
use super::{App, Widgets};

pub(super) fn build(window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) -> Widgets {
    // Header bar.
    let header = gtk::HeaderBar::new();
    let sidebar_button = gtk::Button::from_icon_name("sidebar-show-symbolic");
    sidebar_button.set_tooltip_text(Some("Toggle sidebar (Ctrl+Shift+B)"));
    send_on_click(&sidebar_button, sender, AppMsg::ToggleSidebar);
    header.pack_start(&sidebar_button);

    let new_button = gtk::MenuButton::new();
    new_button.set_icon_name("list-add-symbolic");
    new_button.set_tooltip_text(Some("New"));
    // Filled in once the addons, and their kinds, are known.
    header.pack_start(&new_button);

    let main_menu = gtk::MenuButton::new();
    main_menu.set_icon_name("open-menu-symbolic");
    main_menu.set_popover(Some(&menu_popover(
        vec![
            ("Command palette    Ctrl+Shift+P", AppMsg::ShowPalette),
            ("Reload config", AppMsg::ReloadConfig),
            ("Open config folder", AppMsg::OpenConfigFolder),
            ("Addons…", AppMsg::ManageAddons),
            ("Settings…    Ctrl+,", AppMsg::ShowSettings),
            ("Check for updates…", AppMsg::CheckForUpdates),
        ],
        sender,
    )));
    header.pack_end(&main_menu);
    let palette_button = gtk::Button::from_icon_name("system-search-symbolic");
    palette_button.set_tooltip_text(Some("Command palette (Ctrl+Shift+P)"));
    send_on_click(&palette_button, sender, AppMsg::ShowPalette);
    header.pack_end(&palette_button);
    window.set_titlebar(Some(&header));

    // Sidebar.
    let sidebar = sidebar::Sidebar::new(sender);
    let sidebar_pane = sidebar.pane.clone();

    // Tabs, with a welcome page when none are open.
    let notebook = gtk::Notebook::new();
    notebook.set_scrollable(true);
    // Typing goes straight to a terminal when its tab is chosen. The new
    // page isn't shown yet during the signal, so focus it just after.
    notebook.connect_page_reordered({
        let sender = sender.clone();
        move |_, _, _| sender.input(AppMsg::TabsMoved)
    });
    notebook.connect_switch_page(|_, page, _| {
        if let Some(terminal) = terminal::in_page(page) {
            glib::idle_add_local_once(move || {
                terminal.grab_focus();
            });
        }
    });
    let content = gtk::Stack::new();
    content.set_hexpand(true);
    content.add_named(&welcome(), Some("welcome"));
    content.add_named(&notebook, Some("tabs"));
    content.set_visible_child_name("welcome");

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar_pane));
    paned.set_end_child(Some(&content));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(260);
    paned.set_vexpand(true);

    // Status bar.
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let problems_list = gtk::Label::new(None);
    problems_list.set_xalign(0.0);
    problems_list.set_selectable(true);
    problems_list.set_margin_top(8);
    problems_list.set_margin_bottom(8);
    problems_list.set_margin_start(8);
    problems_list.set_margin_end(8);
    let problems_popover = gtk::Popover::new();
    problems_popover.set_child(Some(&problems_list));
    let problems_button = gtk::MenuButton::new();
    problems_button.add_css_class("flat");
    problems_button.add_css_class("warning");
    problems_button.set_popover(Some(&problems_popover));
    problems_button.set_visible(false);

    let errors_list = gtk::Label::new(None);
    errors_list.set_xalign(0.0);
    errors_list.set_selectable(true);
    errors_list.set_wrap(true);
    errors_list.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    errors_list.set_max_width_chars(72);
    let errors_scroller = gtk::ScrolledWindow::builder()
        .child(&errors_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();
    let clear_errors = gtk::Button::with_label("Clear");
    clear_errors.set_halign(gtk::Align::End);
    send_on_click(&clear_errors, sender, AppMsg::ClearErrors);
    let errors_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    errors_content.set_margin_top(8);
    errors_content.set_margin_bottom(8);
    errors_content.set_margin_start(8);
    errors_content.set_margin_end(8);
    errors_content.append(&errors_scroller);
    errors_content.append(&clear_errors);
    let errors_popover = gtk::Popover::new();
    errors_popover.set_child(Some(&errors_content));
    let errors_button = gtk::MenuButton::new();
    errors_button.add_css_class("flat");
    errors_button.add_css_class("error");
    errors_button.set_tooltip_text(Some("Recent errors"));
    errors_button.set_popover(Some(&errors_popover));
    errors_button.set_visible(false);

    let statusbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    statusbar.add_css_class("statusbar");
    statusbar.append(&status);
    statusbar.append(&errors_button);
    statusbar.append(&problems_button);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&paned);
    root.append(&statusbar);
    window.set_child(Some(&root));

    Widgets {
        sidebar_pane,
        sidebar,
        content,
        notebook,
        new_button,
        status,
        problems_button,
        problems_list,
        errors_button,
        errors_list,
    }
}

pub(super) fn welcome() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_valign(gtk::Align::Center);
    page.set_halign(gtk::Align::Center);
    let title = gtk::Label::new(Some("Burrow"));
    title.add_css_class("welcome-title");
    page.append(&title);
    for (keys, what) in [
        ("Ctrl+Shift+T", "New terminal"),
        ("Ctrl+Shift+P", "Command palette"),
        ("Ctrl+Shift+B", "Toggle sidebar"),
        ("Ctrl+Shift+S", "Focus sidebar"),
        ("Ctrl+Shift+W", "Close tab"),
        ("F2", "Rename the selected item"),
        // Not keys, but the two things the window doesn't show by itself.
        ("+", "Add a host, tunnel or database"),
        ("Right-click", "Open a host's files or monitor"),
    ] {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let key = gtk::Label::new(Some(keys));
        key.add_css_class("kbd");
        key.set_width_chars(12);
        let label = gtk::Label::new(Some(what));
        label.add_css_class("dim");
        row.append(&key);
        row.append(&label);
        page.append(&row);
    }
    page
}

pub(super) fn send_on_click(button: &gtk::Button, sender: &ComponentSender<App>, message: AppMsg) {
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(message.clone()));
}

pub(super) fn add_shortcuts(window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) {
    // Plain Ctrl keys belong to the shell (Ctrl+W deletes a word, Ctrl+R
    // searches history), so Burrow's own use Ctrl+Shift, as other GTK
    // terminals do.
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);
    // Capture runs before the focused widget sees the key. In the default
    // bubble phase a terminal would send these keys to the shell first.
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let shortcuts = [
        ("<Control><Shift>t", AppMsg::NewTerminal(None)),
        ("<Control><Shift>p", AppMsg::ShowPalette),
        ("<Control><Shift>b", AppMsg::ToggleSidebar),
        // A terminal keeps Tab for itself, so this is the way back.
        ("<Control><Shift>s", AppMsg::FocusSidebar),
        ("<Control><Shift>w", AppMsg::CloseCurrentTab),
        ("<Control><Shift>r", AppMsg::ReloadConfig),
        ("<Control>comma", AppMsg::ShowSettings),
        ("<Control>Page_Down", AppMsg::SwitchTab(1)),
        ("<Control>Page_Up", AppMsg::SwitchTab(-1)),
        ("<Control>Tab", AppMsg::SwitchTab(1)),
        // GTK matches this to the ISO_Left_Tab that Shift+Tab sends.
        ("<Control><Shift>Tab", AppMsg::SwitchTab(-1)),
    ];
    for (trigger, message) in shortcuts {
        let sender = sender.clone();
        let action = gtk::CallbackAction::new(move |_, _| {
            sender.input(message.clone());
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(action),
        ));
    }
    window.add_controller(controller);
}
