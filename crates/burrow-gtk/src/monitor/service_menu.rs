//! The menu of a service's actions.

use std::rc::Rc;

use burrow_core::{NodeId, Service, ServiceAction};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};

/// Opens the menu of a service's actions, pointing at a widget.
pub(super) type Menu = Rc<dyn Fn(&gtk::Widget, Service)>;

/// The menu opener for a monitor whose page is `page`. The menu hangs off
/// the page, never a row: rows come and go with each list, and a popover
/// among a list's children once hung the window when the list changed
/// while it was open (commit 9fa1e36).
pub(super) fn menu_opener(page: &gtk::Box, node: NodeId, sender: &ComponentSender<App>) -> Menu {
    let page = page.downgrade();
    let sender = sender.clone();
    Rc::new(move |anchor: &gtk::Widget, service: Service| {
        let Some(page) = page.upgrade() else {
            return;
        };
        let popover = crate::app::menu_popover(service_entries(node, &service), &sender);
        popover.set_parent(&page);
        if let Some(bounds) = anchor.compute_bounds(&page) {
            let at = gdk::Rectangle::new(
                bounds.x() as i32,
                bounds.y() as i32,
                bounds.width() as i32,
                bounds.height() as i32,
            );
            popover.set_pointing_to(Some(&at));
        }
        popover.connect_closed(|popover| {
            // Unparented once the signal has returned, not inside it.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    })
}

/// The button at the end of a row that opens its service's menu.
pub(super) fn menu_button(menu: &Menu, service: Service) -> gtk::Button {
    let button = gtk::Button::from_icon_name("view-more-symbolic");
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    button.set_tooltip_text(Some("Actions"));
    let menu = menu.clone();
    button.connect_clicked(move |button| menu(button.upcast_ref(), service.clone()));
    button
}

/// A service's actions, then its log. Those that interrupt it ask first,
/// so they end in "…".
pub(super) fn service_entries(node: NodeId, service: &Service) -> Vec<(String, AppMsg)> {
    let mut entries: Vec<(String, AppMsg)> = (service.actions().iter())
        .map(|&action| {
            let mut label = crate::app::sentence_case(action.verb());
            if confirms(action) {
                label.push('…');
            }
            let message = AppMsg::Control {
                node,
                service: service.clone(),
                action,
            };
            (label, message)
        })
        .collect();
    let log = AppMsg::ShowLogs {
        node,
        service: service.clone(),
    };
    entries.push(("Follow log".into(), log));
    entries
}

/// Whether an action interrupts what the service does, so asks first.
pub fn confirms(action: ServiceAction) -> bool {
    matches!(
        action,
        ServiceAction::Stop | ServiceAction::Restart | ServiceAction::Disable
    )
}
