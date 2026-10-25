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
| `open_terminal` | `node`, `cols`, `rows` | `session { session }` |
| `write_terminal` | `session`, `data` (bytes) | `ok` |
| `resize_terminal` | `session`, `cols`, `rows` | `ok` |
| `answer_prompt` | `session`, `prompt`, `answer` | `ok` |
| `close_session` | `session` | `ok` |
| `list_sessions` | | `sessions { sessions }` |
| `start_tunnel` | `node` | `session { session }` |
| `list_autostart` | | `autostart { nodes }` |
| `set_autostart` | `node`, `enabled`, `expect?` | `ok` |
| `open_files` | `node` | `session { session }` |
| `list_dir` | `session`, `path?` (empty = home) | `dir { path, entries }` |
| `make_dir` | `session`, `path` | `ok` |
| `rename_path` | `session`, `from`, `to` | `ok` |
| `remove_path` | `session`, `path` | `ok` |
| `download` | `session`, `remote`, `local`, `overwrite?` | `transfer { transfer }` |
| `upload` | `session`, `local`, `remote`, `overwrite?` | `transfer { transfer }` |
| `cancel_transfer` | `transfer` | `ok` |
| `list_addons` | | `addons { addons }`, sorted by id |
| `approve_addon` | `id`, `fingerprint` | `ok` |
| `revoke_addon` | `id` | `ok` |
| `open_view` | `node` | `view { view }` |
| `invoke_action` | `addon`, `action`, `node?`, `values?` | `invoked { message }` |

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
| `session_changed` | `session`, `node`, `state` | A session moved on: `connecting { step }`, `connected`, or `closed { error }` (`error` is null when the shell ended normally). `node` is the SSH host of a terminal or file session, and the tunnel node of a tunnel. |
| `transfer_changed` | `transfer`, `session`, `state` | A download or upload moved on: `running { done, total }` (at most a few times a second), `finished`, `failed { error }` or `cancelled`. |
| `autostart_changed` | `nodes` | The tunnels that start with Burrow changed. |
| `prompt_requested` | `session`, `prompt { id, kind, title, message }` | A session waits for `answer_prompt`. |
| `addons_changed` | `addons` | An addon was added, removed, changed, approved, revoked, started or stopped. |
| `view_changed` | `node`, `view` | The addon that owns `node` redrew its view. |

A subscriber that falls more than 256 events behind gets a "lagged" error and should call `list_nodes` again.

## SSH sessions

`open_terminal` on an `ssh-host` node replies at once with a session id. The session then connects in the background and reports each step as `session_changed`.

Terminals, tunnels and file sessions on the same host share one SSH connection, so a passphrase or one-time code is asked for once. The connection closes when its last session does. Session ids grow: a later session has a larger id. `list_sessions` reports each running session's `session`, `node`, `kind` (`terminal`, `tunnel` or `files`) and `state`.

- **Host settings** come from `ssh -G <alias>`, so `Include`, `Match`, `Host` patterns and defaults apply exactly as for `ssh <alias>`. `props.alias` is the alias (the node's name when missing). An alias that is empty, starts with `-`, or has spaces or control characters is refused, because node files can come from a shared folder.
- **Host keys** are checked against the `UserKnownHostsFile` and `GlobalKnownHostsFile` files, following `StrictHostKeyChecking`. A changed or `@revoked` key is always refused. A new key is asked about (`ask`) or added (`accept-new`/`no`), except when the host has known keys of another type, which is always asked about. New keys go into the first user file, hashed if `HashKnownHosts` is on.
- **Signing in** tries agent keys (`IdentityAgent`, configured keys first, only those with `IdentitiesOnly`), then key files (asking for passphrases), then a password, then keyboard-interactive.
- **Saved passwords** are kept in the keyring with the `user@host:port` they were used for (`password-target`), and are only sent there.
- **Not yet supported:** `ProxyJump` and `ProxyCommand` make the session fail with a message, rather than being ignored.

### Prompts

`prompt.kind` is one of:

| `type` | Answer with |
|---|---|
| `confirm` | `{"type": "confirm"}` or `{"type": "cancel"}` |
| `secret` (`can_save`) | `{"type": "text", "value": "…", "save": false}`, or `cancel` |
| `text` | `{"type": "text", "value": "…"}`, or `cancel` |

A prompt left unanswered when the session closes counts as cancelled.

### Output stream

Terminal output is not sent as events: a burst of output would push every subscriber past its event buffer. In-process clients take each session's output with `Core::take_output(session)`, once. It is a bounded stream, so a slow reader slows the SSH channel down instead of losing output. Dropping the stream ends the session. Out-of-process clients will receive the same bytes as notifications (milestone 8).

## SSH tunnels

An `ssh-tunnel` node forwards a port on this computer through an SSH host:

```toml
[props]
via = "6f1c…"                # id of the ssh-host node to go through
type = "local"               # "local" (ssh -L) or "dynamic" (ssh -D, a SOCKS5 proxy)
bind = "127.0.0.1"           # optional; must be a loopback address
port = 5433
target_host = "db.internal"  # "local" only, as the SSH host sees it
target_port = 5432           # "local" only
```

- **`start_tunnel`** runs it as a session, so it reports progress and asks its questions like a terminal does. If the tunnel is already running it returns that session. `close_session` stops it.
- **Checked before binding:** node files can come from a shared folder, so the props are checked before anything listens. A tunnel only listens on a loopback address. The port is bound before signing in, so a port in use fails at once.
- **Edits apply on restart:** a running tunnel keeps the settings it started with.
- **Starting with Burrow** is a choice made on this computer with `set_autostart`. It is kept in `$XDG_STATE_HOME/burrow/autostart.toml`, not in the tunnel's file, so a shared folder can't turn it on. The choice is pinned to the tunnel's settings and its host's alias, and a change to either (by hand or by a `git pull`) withdraws it. A client should pass the props it showed the user as `expect`: if the file changed meanwhile, enabling fails instead of approving settings nobody saw. The process that hosts the core calls `Core::autostart()` once.

## Files (SFTP)

`open_files` on an `ssh-host` node starts an SFTP session for a file panel. Once it reports `connected`, its path commands work. Remote paths are absolute or relative to the home folder.

- **`list_dir`** returns the folder's absolute `path` and its entries, sorted by name. Each entry has a `name`, `kind` (`file`, `dir`, `symlink` or `other`), `size`, and optional `modified` (Unix seconds) and `permissions` (mode bits). Names are sent as the server sent them and can hold control characters, so clients must clean them before showing them. Entries named `.`, `..` or with a `/` are left out.
- **`rename_path`** fails if `to` exists. **`remove_path`** deletes a folder with everything in it, and removes symlinks rather than following them.
- **`download` and `upload`** copy a file or a whole folder in the background and reply with a transfer id at once. `local` is an absolute path naming the copy itself. If the target exists they fail, unless `overwrite` is set: then files are replaced and folders merged.
- **Transfers don't trust names.** A name inside a folder that holds `/`, or is `.` or `..`, fails the transfer rather than writing outside the folder that was picked. Symlinks inside a folder are skipped on both sides, and Burrow won't write through a symlink at the target.
- **No half-written files:** each file is written to a hidden file next to the target and moved into place when complete. A failed or cancelled transfer removes its partial file. Closing the session cancels its transfers. An upload cut off by a lost connection can't clean up, so its hidden `.<name>.burrow-<n>` file stays on the server.

## Addons

An addon is a folder in `$XDG_CONFIG_HOME/burrow/addons/` holding an `addon.toml` and a program. Burrow starts the program when the addon is first used and talks to it over stdin and stdout. The `notes` example in `crates/burrow-addon-notes` is a complete addon in about 200 lines. To try it:

```sh
cargo build -p burrow-addon-notes
mkdir -p ~/.config/burrow/addons/notes
cp target/debug/burrow-addon-notes crates/burrow-addon-notes/addon.toml ~/.config/burrow/addons/notes/
```

### Manifest

```toml
id = "notes"                 # must match the folder's name
name = "Notes"
version = "0.1.0"
api = "1"                    # the API major version it was written for
permissions = ["nodes:read", "nodes:write"]

[backend]
command = "burrow-addon-notes"   # a path inside the folder
args = []
protocol = "jsonrpc-stdio"

[[kinds]]                    # node kinds the addon owns and draws
kind = "note"
label = "Note"
icon = "text-x-generic-symbolic"

[[commands]]                 # palette commands
id = "count"
title = "Count notes"

[[actions]]                  # context-menu actions on other kinds
id = "describe"
title = "Describe host"
kinds = ["ssh-host"]
```

A manifest that fails a check is reported in `list_problems`, and the addon is left out. The checks:
- **Fields:** the `id` matches the folder name. The `api` major is this Burrow's. Every permission is known.
- **Program:** `command` is a relative path to an executable file inside the folder. It is never looked up on `PATH`, and neither `..` nor a symlink may lead out of the folder.
- **Kinds:** `folder` and `ssh-host` belong to Burrow. When two addons claim the same kind, the one whose id sorts first keeps it. The other is left out and reported.

### Approval

An addon never runs until the user approves it. The addons folder can arrive with a dotfiles repo, a shared folder or a `git pull`, so finding an addon there is not consent to run it.

- **What an approval covers:** `list_addons` reports a `fingerprint`, a SHA-256 over the id, permissions, kinds, program path, arguments and every file in the folder, `addon.toml` included. `approve_addon` must repeat it, so the user approves exactly what they were shown. If anything changes, the addon needs approving again (`needs_approval { changed: true }`).
- **When it is checked:** on every rescan (live, through the file watcher) and again just before each start.
- **Where approvals live:** in `$XDG_STATE_HOME/burrow/addons.toml`, outside the config folder, so they do not travel with it.
- **Limits:** a folder over 4096 files or 256 MiB is refused rather than partly pinned. Files outside the folder that the program reads are not covered, and neither is a file swapped in the instant between the check and the program reading it.

**Permissions limit the Burrow API, not the program.** An approved addon is a normal process running as you. It can read your files like any other program, including agent sockets it finds by path. Its permissions only decide what it may ask Burrow to do.

### Where an addon runs

- **Its folder is read-only in practice.** Writing a file there changes the fingerprint and withdraws the approval. So an addon runs in its own data folder, `$XDG_STATE_HOME/burrow/addons/<id>/`, and keeps its files there. `PYTHONPYCACHEPREFIX` points Python's bytecode cache there too.
- **Environment:** cleared, then only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TZ`, `TMPDIR` and `XDG_RUNTIME_DIR` are passed. Burrow adds `BURROW_ADDON_ID`, `BURROW_ADDON_DIR` (its folder), `BURROW_ADDON_DATA` (the data folder) and `BURROW_API`. Pointers to your credentials, such as `SSH_AUTH_SOCK`, are not passed.

### Permissions

Each command an addon sends is checked against this table. Anything not listed is refused, including secrets, sessions, problems and managing addons.

| Command | Needs |
|---|---|
| `version` | nothing |
| `list_nodes`, `get_node` | `nodes:read` |
| `create_node` | `nodes:write`, and the kind must be the addon's own |
| `update_node`, `move_node`, `delete_node` | `nodes:write`, and the node's kind must be the addon's own |

Events reach an addon only with `nodes:read`, and only node events.

### Protocol

JSON-RPC 2.0, one message per line (UTF-8, `\n`-terminated), in both directions. A line over 1 MiB stops the addon. Burrow waits 30 s for each answer.

Burrow sends:

| Method | Params | Result |
|---|---|---|
| `initialize` | `{ api, id }` | anything |
| `open_view` | `{ node }` | a view |
| `invoke` | `{ action, node, values }` | `{ message? }` |
| `event` (notification) | an [event](#events) | |
| `resync` (notification) | | missed events; list again |

The addon sends:

| Method | Params | Result |
|---|---|---|
| `execute` | a [command](#commands) | its response, or an error |
| `view_changed` (notification) | `{ node, view }` for a node of its own kind | |

- **`invoke`:** runs a palette command (`node` is null), a context-menu action, or a button or form from one of the addon's views. Burrow only forwards a palette command or a context action that the manifest declares for that kind. From the addon's own views, any action is forwarded.
- **Lifecycle:** Burrow closes stdin when it is done, and the addon should exit then. An addon that exits or breaks the protocol is restarted on the next use. The delay before that grows from 1 s to 60 s while it keeps failing soon after starting.

### Views

A view is a list of blocks, drawn top to bottom with native widgets:

```json
{"blocks": [
  {"type": "heading", "text": "Shopping"},
  {"type": "text", "text": "milk"},
  {"type": "fields", "fields": [{"label": "Rows", "value": "12"}]},
  {"type": "table", "columns": ["Name", "Size"], "rows": [["a", "1"]]},
  {"type": "form", "action": "save", "submit": "Save", "fields": [
    {"name": "text", "label": "Text", "input": "multiline", "value": "milk"},
    {"name": "pinned", "label": "Pinned", "input": "check", "value": false},
    {"name": "colour", "label": "Colour", "input": "choice", "options": ["red", "blue"], "value": "red"}
  ]},
  {"type": "buttons", "buttons": [{"action": "count", "label": "Count notes"}]}
]}
```

`input` is `text`, `multiline`, `check` or `choice`. Submitting a form sends its `action` with `values`: a bool for `check`, a string for the rest. Tables show their first 500 rows.

## Secrets

Secret values only flow into the core: `set_secret` stores them, and nothing returns them.

- **What the config file holds:** a reference to the keyring entry, for example `password = "keyring:burrow/<node-id>/password"`.
- **Who can read them:** only trusted in-process code, such as the core's own SSH and database connectors, through `Core::resolve_secret`. It is deliberately not a command.
- **Cleanup:** deleting a node also deletes its secrets from the keyring.
- **References are pinned to their owner:** a reference is only followed if it is exactly `keyring:burrow/<this node's id>/<field>`. Config files can come from a shared folder or a `git pull`, so anything else is reported in `list_problems` and ignored. That includes a reference to another app's keyring entry, or a copied file's references to the original node's secrets. Burrow never reads, overwrites or deletes through such a reference.

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
