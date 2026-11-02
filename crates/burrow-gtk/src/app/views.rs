//! Addon views and actions.

use std::collections::HashMap;
use std::rc::Rc;

use burrow_core::{AddonInfo, Command, NodeId, Response, View};
use relm4::{ComponentSender, gtk};

use crate::addons;
use crate::runtime::runtime;

use super::chrome::send_on_click;
use super::menus::menu_popover;
use super::message::AppMsg;
use super::tabs::{Panel, TabKey};
use super::{App, Widgets};

use super::daemon::{execute, run};
use burrow_core::FormValue;
use std::collections::BTreeMap;
impl App {
    /// Takes a new addon list. Tabs whose addon was approved or revoked are
    /// drawn again.
    pub(super) fn set_addons(
        &mut self,
        addons: Vec<AddonInfo>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let usable = |list: &[AddonInfo], kind: &str| {
            addons::owner(list, kind).map(|a| (a.id.clone(), addons::usable(a)))
        };
        let stale: Vec<NodeId> = (self.tabs.iter())
            .filter(|(_, tab)| matches!(tab.panel, Panel::View(_)))
            .filter_map(|(key, _)| key.node())
            .filter(|id| {
                self.nodes.get(id).is_none_or(|node| {
                    usable(&self.addons, &node.kind) != usable(&addons, &node.kind)
                })
            })
            .collect();
        self.addons = addons;
        for id in stale {
            self.load_view(id, sender);
        }
        widgets
            .new_button
            .set_popover(Some(&menu_popover(self.new_entries(None, ""), sender)));
        widgets.sidebar.set_icons(&self.icons());
    }

    /// The icon for each addon kind.
    pub(super) fn icons(&self) -> HashMap<String, String> {
        self.addons
            .iter()
            .flat_map(|a| &a.kinds)
            .filter_map(|k| Some((k.kind.clone(), k.icon.clone()?)))
            .collect()
    }

    /// The box an addon draws `id`'s view in, if its tab is open.
    pub(super) fn view_page(&self, id: NodeId) -> Option<&gtk::Box> {
        match &self.tabs.get(&TabKey::Node(id))?.panel {
            Panel::View(page) => Some(page),
            _ => None,
        }
    }

    /// Asks the node's addon for its view, or explains why it can't, if
    /// the node's tab shows a view.
    pub(super) fn load_view(&self, id: NodeId, sender: &ComponentSender<Self>) {
        let (Some(page), Some(node)) = (self.view_page(id), self.nodes.get(&id)) else {
            return;
        };
        let Some(addon) = addons::owner(&self.addons, &node.kind) else {
            let message = "The addon for this kind of item is no longer installed.";
            addons::render_message(page, &node.name, message, Vec::new());
            return;
        };
        if !addons::usable(addon) {
            let button = gtk::Button::with_label("Review addon…");
            send_on_click(&button, sender, AppMsg::ReviewAddon(addon.id.clone()));
            let message = format!(
                "This item is drawn by the {} addon, which you have not approved.",
                addon.name
            );
            addons::render_message(page, &node.name, &message, vec![button]);
            return;
        }
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let result = match core.execute(Command::OpenView { node: id }).await {
                Ok(Response::View { view }) => Ok(view),
                Ok(other) => Err(format!("unexpected reply {other:?}")),
                Err(e) => Err(e.to_string()),
            };
            let _ = input.send(AppMsg::ViewLoaded { node: id, result });
        });
    }

    pub(super) fn show_view(
        &self,
        id: NodeId,
        result: Result<View, String>,
        sender: &ComponentSender<Self>,
    ) {
        let (Some(page), Some(node)) = (self.view_page(id), self.nodes.get(&id)) else {
            return;
        };
        let Some(addon) = addons::owner(&self.addons, &node.kind) else {
            return;
        };
        match result {
            Ok(view) => {
                let (addon, sender) = (addon.id.clone(), sender.clone());
                let on_action: addons::OnAction = Rc::new(move |action, values| {
                    sender.input(AppMsg::Invoke {
                        addon: addon.clone(),
                        action,
                        node: Some(id),
                        values,
                    })
                });
                addons::render(page, &view, on_action);
            }
            Err(error) => {
                let button = gtk::Button::with_label("Try again");
                send_on_click(&button, sender, AppMsg::LoadView(id));
                addons::render_message(page, &node.name, &error, vec![button]);
            }
        }
    }

    /// Sends an action to an addon. Its reply, if any, goes to the status
    /// bar.
    pub(super) fn invoke(
        &self,
        addon: String,
        action: String,
        node: Option<NodeId>,
        values: BTreeMap<String, FormValue>,
        sender: &ComponentSender<Self>,
    ) {
        let command = Command::InvokeAction {
            addon,
            action,
            node,
            values,
        };
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let message = match run(&core, command).await {
                Ok(Response::Invoked {
                    message: Some(message),
                }) => AppMsg::Notice(message),
                Ok(_) => return,
                Err(error) => AppMsg::Failed(error),
            };
            let _ = input.send(message);
        });
    }

    pub(super) fn manage_addons(
        &self,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let review = {
            let sender = sender.clone();
            move |id| sender.input(AppMsg::ReviewAddon(id))
        };
        let revoke = {
            let sender = sender.clone();
            move |id| sender.input(AppMsg::RevokeAddon(id))
        };
        let open_folder = {
            let sender = sender.clone();
            move || sender.input(AppMsg::OpenAddonsFolder)
        };
        addons::manage(window, &self.addons, review, revoke, open_folder);
    }

    pub(super) fn review_addon(
        &self,
        id: String,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(addon) = self.addons.iter().find(|a| a.id == id) {
            let command = Command::ApproveAddon {
                id,
                fingerprint: addon.fingerprint.clone(),
            };
            let (core, sender) = (self.core.clone(), sender.clone());
            addons::review(window, addon, move || {
                execute(&core, &sender, command.clone())
            });
        }
    }
}
