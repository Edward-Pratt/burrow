//! Addons as clients see them: what one is and may do, its state, and
//! the view it draws for a node, block by block.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// An addon found in the `addons/` folder, as [`Command::ListAddons`](crate::api::Command::ListAddons)
/// reports it. Everything but `state` comes from its `addon.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddonInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    /// What the addon may do through the API, such as `"nodes:read"`.
    pub permissions: Vec<String>,
    /// Node kinds the addon owns. Only it can create or change their nodes,
    /// and it draws their tabs.
    pub kinds: Vec<AddonKind>,
    /// Palette commands.
    pub commands: Vec<AddonCommand>,
    /// Sidebar context-menu actions.
    pub actions: Vec<AddonAction>,
    /// The program that runs, resolved inside the addon's folder.
    pub program: PathBuf,
    /// A hash of everything an approval covers: the id, permissions, kinds,
    /// program path and arguments, and every file in the addon's folder.
    /// [`Command::ApproveAddon`](crate::api::Command::ApproveAddon) must repeat it, so the user approves
    /// exactly what they were shown.
    pub fingerprint: String,
    #[serde(flatten)]
    pub state: AddonState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonKind {
    pub kind: String,
    /// Shown in "New …" menus, such as "PostgreSQL database".
    pub label: String,
    /// A symbolic icon name from the icon theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonCommand {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonAction {
    pub id: String,
    pub title: String,
    /// The node kinds it is offered on.
    pub kinds: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AddonState {
    /// Never approved, or `changed` since it was: other files, permissions
    /// or kinds. It does not run until approved.
    NeedsApproval {
        changed: bool,
    },
    /// Approved. It starts when first used.
    Ready,
    Running,
    /// It stopped by itself or broke the protocol. It is started again on
    /// the next use, after a growing delay.
    Failed {
        error: String,
    },
}

/// What an addon draws for one of its nodes. The client turns the blocks
/// into native widgets, top to bottom.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Heading {
        text: String,
    },
    Text {
        text: String,
    },
    /// Label and value pairs.
    Fields {
        fields: Vec<Field>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// Submitting sends `action` with the field values to the addon.
    Form {
        action: String,
        fields: Vec<FormField>,
        submit: String,
    },
    /// Each button sends its `action` to the addon.
    Buttons {
        buttons: Vec<Button>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// The key of its value in [`Command::InvokeAction`](crate::api::Command::InvokeAction)'s `values`.
    pub name: String,
    pub label: String,
    #[serde(flatten)]
    pub input: FormInput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum FormInput {
    Text {
        #[serde(default)]
        value: String,
    },
    Multiline {
        #[serde(default)]
        value: String,
    },
    Check {
        #[serde(default)]
        value: bool,
    },
    /// One of `options`.
    Choice {
        options: Vec<String>,
        #[serde(default)]
        value: String,
    },
}

/// A form field's value: a bool for `check`, text for the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormValue {
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub action: String,
    pub label: String,
}
