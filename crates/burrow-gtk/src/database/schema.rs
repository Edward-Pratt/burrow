//! The schema tree beside the editor: loading it, drawing it and naming its tables.

use burrow_core::ipc::Client;
use burrow_core::{Command, DbTableKind, NodeId, Response};
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;

use super::{Database, Item, Msg};

impl Database {
    pub(super) fn load_schema(&self, core: &Client, sender: &ComponentSender<App>) {
        let Some(session) = self.session.running() else {
            return;
        };
        self.request(
            Command::ListSchema { session },
            core,
            sender,
            |response| match response {
                Response::Schema { schemas } => Some(Msg::Schema(schemas)),
                _ => None,
            },
        );
    }
}

/// The schemas, their tables and their columns, as a tree. Activating a
/// table shows its first rows.
pub(super) fn schema_tree(
    node: NodeId,
    sender: &ComponentSender<App>,
) -> (gtk::ListView, gio::ListStore) {
    let schemas = gio::ListStore::new::<glib::BoxedAnyObject>();
    let tree = gtk::TreeListModel::new(schemas.clone(), false, false, |item| {
        let item = item.downcast_ref::<glib::BoxedAnyObject>()?;
        let children = gio::ListStore::new::<glib::BoxedAnyObject>();
        match &*item.borrow::<Item>() {
            Item::Schema(schema) => {
                for table in &schema.tables {
                    children.append(&glib::BoxedAnyObject::new(Item::Table {
                        schema: schema.name.clone(),
                        table: table.clone(),
                    }));
                }
            }
            Item::Table { table, .. } if !table.columns.is_empty() => {
                for column in &table.columns {
                    let data_type = if column.nullable {
                        column.data_type.clone()
                    } else {
                        format!("{} not null", column.data_type)
                    };
                    children.append(&glib::BoxedAnyObject::new(Item::Column {
                        name: column.name.clone(),
                        data_type,
                    }));
                }
            }
            _ => return None,
        }
        Some(children.upcast())
    });
    let selection = gtk::SingleSelection::new(Some(tree));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        line.append(&gtk::Image::new());
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&name);
        let detail = gtk::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_hexpand(true);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.add_css_class("dim");
        line.append(&detail);
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&line));
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&expander));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(expander), Some(row)) = (
            item.child().and_downcast::<gtk::TreeExpander>(),
            item.item().and_downcast::<gtk::TreeListRow>(),
        ) else {
            return;
        };
        expander.set_list_row(Some(&row));
        let Some(line) = expander.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let icon = line.first_child().and_downcast::<gtk::Image>();
        let name = icon
            .as_ref()
            .and_then(|i| i.next_sibling())
            .and_downcast::<gtk::Label>();
        let detail = name
            .as_ref()
            .and_then(|n| n.next_sibling())
            .and_downcast::<gtk::Label>();
        let (Some(icon), Some(name), Some(detail), Some(object)) = (
            icon,
            name,
            detail,
            row.item().and_downcast::<glib::BoxedAnyObject>(),
        ) else {
            return;
        };
        let (icon_name, text, extra) = match &*object.borrow::<Item>() {
            Item::Schema(schema) => (
                "folder-symbolic",
                schema.name.clone(),
                tables_count(schema.tables.len()),
            ),
            Item::Table { table, .. } => {
                (table_icon(table.kind), table.name.clone(), String::new())
            }
            Item::Column { name, data_type } => ("", name.clone(), data_type.clone()),
        };
        icon.set_icon_name(Some(icon_name).filter(|i| !i.is_empty()));
        icon.set_visible(!icon_name.is_empty());
        name.set_label(&printable(&text));
        detail.set_label(&printable(&extra));
    });

    let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    view.set_single_click_activate(false);
    view.connect_activate({
        let sender = sender.clone();
        move |_, position| {
            let Some(row) = selection.item(position).and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            let Some(object) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let msg = match &*object.borrow::<Item>() {
                Item::Table { schema, table } => Msg::Browse {
                    schema: schema.clone(),
                    table: table.name.clone(),
                },
                _ => {
                    row.set_expanded(!row.is_expanded());
                    return;
                }
            };
            sender.input(AppMsg::Database { node, msg });
        }
    });
    (view, schemas)
}

pub(super) fn table_icon(kind: DbTableKind) -> &'static str {
    match kind {
        DbTableKind::View | DbTableKind::MaterializedView => "view-reveal-symbolic",
        _ => "x-office-spreadsheet-symbolic",
    }
}

pub(super) fn tables_count(count: usize) -> String {
    match count {
        0 => "empty".into(),
        1 => "1 table".into(),
        n => format!("{n} tables"),
    }
}

/// An SQL identifier, quoted so any name a server sends is taken as a
/// name.
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_whatever_the_server_calls_them() {
        assert_eq!(quote_ident("items"), "\"items\"");
        assert_eq!(
            quote_ident("a\"; drop table x; --"),
            "\"a\"\"; drop table x; --\""
        );
    }
}
