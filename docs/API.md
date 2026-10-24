# Burrow core API

Every client drives `burrow-core` through one API: the GTK UI, addons, `burrow-cli` and the MCP bridge. In-process clients call `Core::execute(Command)` and `Core::subscribe()`. Out-of-process clients will use the same types as JSON-RPC over a Unix socket (milestone 8).

API version: **1** (`burrow_core::API_VERSION`). Breaking changes bump it.

## Nodes

The sidebar is a tree of nodes.

```json
{
  "id": "6f1c…",                 // UUID, stable across renames and moves
  "parent": "0a2b…",             // null at the top level
  "kind": "ssh-host",            // "folder" or a kind owned by an addon
  "name": "prod-web",
  "color": "#e06c75",            // optional, #rgb or #rrggbb
  "props": { "alias": "prod-web" },  // kind-specific settings
  "secrets": ["password"]        // names of secret fields that are set; never values
}
```

## Commands

Commands are JSON objects tagged by `type`. Each returns a response or an error.

| `type` | Fields | Response |
|---|---|---|
| `version` | | `version { api }` |
| `list_nodes` | | `nodes { nodes }`, sorted by name |
| `get_node` | `id` | `node { node }` |
| `create_node` | `parent?`, `kind`, `name`, `color?`, `props?` | `node { node }` |
| `update_node` | `id`, `patch { name?, color?, props? }` | `node { node }` |
| `move_node` | `id`, `parent?` (null = top level) | `node { node }` |
| `delete_node` | `id`, `recursive?` | `ok` |
| `set_secret` | `id`, `field`, `value` | `node { node }` |
| `clear_secret` | `id`, `field` | `node { node }` |
| `list_problems` | | `problems { problems }` |
| `reload_config` | | `ok` |

Rules:
- **`update_node` patch:** a missing field is left as it is. `"color": null` clears the color. `props` replaces all props.
- **`delete_node` on a folder:** a folder with anything in it needs `"recursive": true`.
- **Secret field names:** lowercase letters, digits, `_` and `-`.
- **Kinds:** lowercase letters, digits, `-` and `.`.

Example:

```json
{"type": "create_node", "kind": "ssh-host", "name": "web", "props": {"alias": "web"}}
```

## Events

Every subscriber receives events. They are sent for changes made through commands and for files edited outside Burrow.

| `type` | Fields | When |
|---|---|---|
| `node_created` | `node` | A node was added. Parents are announced before their children. |
| `node_updated` | `node` | A node changed, including a move to another folder. |
| `node_deleted` | `id` | A node was removed. Children are announced before their parents. |
| `problems_changed` | `problems` | The set of config files that fail to load changed. |

A subscriber that falls more than 256 events behind gets a "lagged" error and should call `list_nodes` again.

## Secrets

Secret values only flow into the core: `set_secret` stores them, and nothing returns them.

- **What the config file holds:** a reference to the keyring entry, for example `password = "keyring:burrow/<node-id>/password"`.
- **Who can read them:** only trusted in-process code, such as the core's own SSH and database connectors, through `Core::resolve_secret`. It is deliberately not a command.
- **Cleanup:** deleting a node also deletes its secrets from the keyring.

## On-disk layout

The tree lives in `$XDG_CONFIG_HOME/burrow/tree/`:

```
tree/
  work/                  # a folder
    _folder.toml         # its id, name and color
    prod-web.toml        # a leaf
  scratch.toml
```

```toml
# prod-web.toml
id = "6f1c…"
kind = "ssh-host"
name = "prod-web"
color = "#e06c75"

[props]
alias = "prod-web"

[secrets]
password = "keyring:burrow/6f1c…/password"
```

- **File names:** a slug of the node's name, numbered on clashes (`web.toml`, `web-2.toml`). They are renamed when the node is renamed.
- **Parent:** a node's parent is the directory it sits in, so moving a file in a file manager moves the node.
- **Hand-written files:** a file without an `id` gets one written back on load, and so does a copied file whose `id` is already taken. A directory without `_folder.toml` gets one.
- **Broken files:** a file that fails to parse is reported in `list_problems` and never overwritten. The rest of the tree still loads.
- **Writes:** Burrow writes every file atomically (a temporary file, then a rename).
- **Live reload:** the file watcher reloads the tree about 250 ms after edits stop and sends events for whatever changed.

### Why ids live in the files

A node's identity is the `id` stored in its file, not its path. Other nodes will refer to it, for example a database connection reached "through" an SSH host. A path-based identity would break those references whenever something is renamed or moved. The cost is that hand-made files need an `id` written into them, which the loader does automatically.
