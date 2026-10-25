# Burrow core API

Every client drives `burrow-core` through one API: the GTK UI, addons, `burrow-cli` and the MCP bridge. In-process clients call `Core::execute(Command)` and `Core::subscribe()`. Out-of-process clients use the same types as JSON-RPC over a Unix socket; see [Socket](#socket).

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
| `exec` | `node`, `command`, `timeout?` | `exec { result }` |
| `open_files` | `node` | `session { session }` |
| `list_dir` | `session`, `path?` (empty = home) | `dir { path, entries }` |
| `read_file` | `session`, `path`, `limit?` | `file { data, size }` |
| `write_file` | `session`, `path`, `data`, `overwrite?` | `ok` |
| `make_dir` | `session`, `path` | `ok` |
| `rename_path` | `session`, `from`, `to` | `ok` |
| `remove_path` | `session`, `path` | `ok` |
| `download` | `session`, `remote`, `local`, `overwrite?` | `transfer { transfer }` |
| `upload` | `session`, `local`, `remote`, `overwrite?` | `transfer { transfer }` |
| `cancel_transfer` | `transfer` | `ok` |
| `open_database` | `node` | `session { session }` |
| `run_query` | `session`, `sql`, `limit?` | `query { result }` |
| `cancel_query` | `session` | `ok` |
| `set_read_only` | `session`, `read_only` | `ok` |
| `list_schema` | `session` | `schema { schemas }` |
| `export_query` | `session`, `sql`, `path` | `exported { rows }` |
| `list_history` | `node` | `history { entries }`, oldest first |
| `clear_history` | `node` | `ok` |
| `list_grants` | | `grants { grants }` |
| `set_grant` | `node`, `capabilities`, `expect?` | `ok` |
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
| `session_changed` | `session`, `node`, `state` | A session moved on: `connecting { step }`, `connected`, or `closed { error }` (`error` is null when the shell ended normally). `node` is the SSH host of a terminal or file session, the tunnel node of a tunnel, and the database node of a database session. |
| `transfer_changed` | `transfer`, `session`, `state` | A download or upload moved on: `running { done, total }` (at most a few times a second), `finished`, `failed { error }` or `cancelled`. |
| `autostart_changed` | `nodes` | The tunnels that start with Burrow changed. |
| `prompt_requested` | `session`, `prompt { id, kind, title, message }` | A session waits for `answer_prompt`. |
| `prompt_closed` | `session`, `prompt` (the id) | A prompt stopped waiting: a client answered it, or its session ended. A client still showing it should close it. |
| `grants_changed` | `grants` | What the MCP bridge may do changed, including a grant going stale because its node changed. |
| `addons_changed` | `addons` | An addon was added, removed, changed, approved, revoked, started or stopped. |
| `view_changed` | `node`, `view` | The addon that owns `node` redrew its view. |

A subscriber that falls more than 256 events behind gets a "lagged" error and should call `list_nodes` again.

## SSH sessions

`open_terminal` on an `ssh-host` node replies at once with a session id. The session then connects in the background and reports each step as `session_changed`.

Terminals, tunnels and file sessions on the same host share one SSH connection, so a passphrase or one-time code is asked for once. The connection closes when its last session does. Session ids grow: a later session has a larger id. `list_sessions` reports each running session's `session`, `node`, `kind` (`terminal`, `tunnel`, `files`, `database` or `exec`) and `state`.

- **Host settings** come from `ssh -G <alias>`, so `Include`, `Match`, `Host` patterns and defaults apply exactly as for `ssh <alias>`. `props.alias` is the alias (the node's name when missing). An alias that is empty, starts with `-`, or has spaces or control characters is refused, because node files can come from a shared folder.
- **Host keys** are checked against the `UserKnownHostsFile` and `GlobalKnownHostsFile` files, following `StrictHostKeyChecking`. A changed or `@revoked` key is always refused. A new key is asked about (`ask`) or added (`accept-new`/`no`), except when the host has known keys of another type, which is always asked about. New keys go into the first user file, hashed if `HashKnownHosts` is on.
- **Signing in** tries agent keys (`IdentityAgent`, configured keys first, only those with `IdentitiesOnly`), then key files (asking for passphrases), then a password, then keyboard-interactive.
- **Saved passwords** are kept in the keyring with the `user@host:port` they were used for (`password-target`), and are only sent there.
- **Not yet supported:** `ProxyJump` and `ProxyCommand` make the session fail with a message, rather than being ignored.

### Commands without a terminal

`exec` runs one command on an `ssh-host` node, as `ssh host command` does, and replies when it has finished:

```json
{"type": "exec", "result": {"status": 0, "signal": null, "stdout": "…", "stderr": "", "truncated": false, "timed_out": false}}
```

- **Session:** it runs as a session of kind `exec`, on the host's shared connection, so connecting reports `session_changed` and can ask questions like any session. The reply only comes at the end, so a client that wants to answer those prompts watches for `prompt_requested` from a session on that node.
- **Timeout:** `timeout` is in seconds, 60 when missing and at most 3600, and covers connecting too. A command still running then is sent SIGTERM (on servers that take signals), and the reply has `timed_out` and the output so far.
- **Output:** each stream is decoded as UTF-8, with invalid bytes replaced, and cut at 1 MiB (`truncated`).

### Prompts

`prompt.kind` is one of:

| `type` | Answer with |
|---|---|
| `confirm` | `{"type": "confirm"}` or `{"type": "cancel"}` |
| `secret` (`can_save`) | `{"type": "text", "value": "…", "save": false}`, or `cancel` |
| `text` | `{"type": "text", "value": "…"}`, or `cancel` |

A prompt left unanswered when the session closes counts as cancelled. Any client that may run `answer_prompt` can answer any session's prompt, and the first answer wins. Every prompt ends with `prompt_closed`, so the others can close their dialogs.

### Output stream

Terminal output is not sent as events: a burst of output would push every subscriber past its event buffer. In-process clients take each session's output with `Core::take_output(session)`, once. It is a bounded stream, so a slow reader slows the SSH channel down instead of losing output. Dropping the stream ends the session. Socket clients ask for it with `take_output` and receive it as `output` notifications; see [Socket](#socket).

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
- **Starting with Burrow** is a choice made on this computer with `set_autostart`. It is kept in `$XDG_STATE_HOME/burrow/autostart.toml`, not in the tunnel's file, so a shared folder can't turn it on. The choice is pinned to the tunnel's settings and its host's alias, and a change to either (by hand or by a `git pull`) withdraws it. Enabling needs `expect { props, host_name, host_props }`: the tunnel's props and its host's name and props as the user was shown them. If either file changed meanwhile, enabling fails instead of approving settings nobody saw. The process that hosts the core calls `Core::autostart()` once.

## Files (SFTP)

`open_files` on an `ssh-host` node starts an SFTP session for a file panel. Once it reports `connected`, its path commands work. Remote paths are absolute or relative to the home folder.

- **`list_dir`** returns the folder's absolute `path` and its entries, sorted by name. Each entry has a `name`, `kind` (`file`, `dir`, `symlink` or `other`), `size`, and optional `modified` (Unix seconds) and `permissions` (mode bits). Names are sent as the server sent them and can hold control characters, so clients must clean them before showing them. Entries named `.`, `..` or with a `/` are left out.
- **`rename_path`** fails if `to` exists. **`remove_path`** deletes a folder with everything in it, and removes symlinks rather than following them.
- **`download` and `upload`** copy a file or a whole folder in the background and reply with a transfer id at once. `local` is an absolute path naming the copy itself. If the target exists they fail, unless `overwrite` is set: then files are replaced and folders merged.
- **Transfers don't trust names.** A name inside a folder that holds `/`, or is `.` or `..`, fails the transfer rather than writing outside the folder that was picked. Symlinks inside a folder are skipped on both sides, and Burrow won't write through a symlink at the target.
- **Modes:** a copy that replaces a file keeps that file's mode, so a private file stays private. A new copy takes its source's mode without group or other write and without set-id bits. Hidden files are created `0600` until then.
- **No half-written files:** each file is written to a hidden file next to the target and moved into place when complete. A failed or cancelled transfer removes its partial file. Closing the session cancels its transfers. An upload cut off by a lost connection can't clean up, so its hidden `.<name>.burrow-<n>` file stays on the server.

## Databases

A `postgres` node is a PostgreSQL connection. Its props say where the server is and how to reach it:

```toml
[props]
via = "6f1c…"                # optional: id of the ssh-host node to go through
host = "db.internal"         # TCP, as this computer (or the SSH host) sees it
socket = "/run/postgresql"   # or, without host, the socket's folder
port = 5432                  # optional
user = "app"
database = "app"             # optional; the user's name when missing
sslmode = "verify-full"      # optional: disable, require, verify-ca or verify-full
sslrootcert = "/path/ca.pem" # optional; replaces the system's trusted CAs
```

- **Routes:** a server is reached over TCP or through its Unix socket, either from this computer or through an SSH host. Through a host, Burrow opens a channel on the host's shared connection, so nothing listens on this computer. A socket through a host signs in as the SSH user when the server allows peer sign-in.
- **Checked before dialing:** node files can come from a shared folder, so the props are checked first. A socket is named by its folder and port (`<socket>/.s.PGSQL.<port>`), as libpq does, so a file can't aim Burrow at another socket, such as the SSH agent's. Paths must be absolute, without `..` or control characters.
- **TLS:** `sslmode` defaults to `verify-full` for a server reached straight over the network, and to `disable` through an SSH host or a socket, which are private already. `prefer` and `allow` are refused: they quietly fall back to plain text. `verify-ca` checks the chain but not the name, for a server reached by an address its certificate doesn't list. It needs `sslrootcert`, since with the system's CAs any site's certificate would pass. `require` encrypts, but anyone could be at the other end. TLS isn't used over a socket.
- **Saved passwords** are kept in the keyring with the target they were typed for: user, address, database, the SSH host it went through, `sslmode` and `sslrootcert`, each quoted so no name can make one target read as another. A server may ask for the password in clear text, so if any of those change (by hand or by a `git pull`), Burrow asks again instead of sending it.

`open_database` runs as a session, so it reports progress and asks its questions like a terminal does. Once it reports `connected`, the query commands work. A session runs one request at a time; the others wait their turn.

- **Read-only to start with:** a session starts with `default_transaction_read_only` on, so a slip can't change anything. `set_read_only` turns it off or on again. It is a guard against mistakes, not a boundary: a query can run `SET` itself.
- **`run_query`** runs SQL with the simple query protocol and waits for it. Several statements can be sent at once; `result.statements` has one entry per statement that finished, with its `columns`, its `rows` (each value as the server prints it, `null` for NULL) and its `count`. Values longer than 64 KiB are cut short and end with `…`. Values can hold any character, so clients must clean them before showing them. If a statement fails, `result.error` has the server's `message`, `detail`, `hint`, `code` (SQLSTATE) and `position` (in characters from 1), and the statements after it did not run.
- **Row limit:** each statement keeps at most `limit` rows (1000 by default, at most 100 000). Rows nobody reads would still be sent in full, so a statement that returns more is `truncated` and Burrow cancels the query: `result.stopped` is set, and the statements after it did not run. Several statements sent together run as one transaction unless they say otherwise, so stopping one also undoes the others' changes.
- **`cancel_query`** asks the server to stop whatever the session is running, over a second connection along the same route. The query then fails with code `57014`.
- **`list_schema`** returns each schema the user made (not `pg_*` or `information_schema`), with its tables, views, materialized views, foreign and partitioned tables (`kind`), and their columns' `name`, `data_type` and `nullable`.
- **`export_query`** writes every row to `path` as CSV, with a header row for each result. A NULL is an empty field and an empty string is `""`. It runs in a read-only transaction of its own, even on a writable session, and refuses to start while the session has a transaction open, since rolling back at the end would throw that one's work away. The file is written next to `path` under a hidden name and moved into place when complete, so it is private (`0600`) and never half-written.
- **History:** every query run with `run_query` is kept, newest last, up to 200 per node, in `$XDG_STATE_HOME/burrow/history/<node>.json` (`0600`). It stays on this computer because queries can hold data. Queries over 64 KiB are not kept. `clear_history` forgets a node's, and so does deleting the node.

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
- **Kinds:** `folder`, `ssh-host`, `ssh-tunnel` and `postgres` belong to Burrow. When two addons claim the same kind, the one whose id sorts first keeps it. The other is left out and reported.

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

Each command an addon sends is checked against this table. Anything not listed is refused, including secrets, sessions, databases, problems and managing addons.

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

## Socket

`burrowd` serves the API on a Unix socket to clients in other processes: the GTK window, `burrow` (the CLI) and its MCP bridge. Only processes running as the same user may connect; the daemon checks the peer's uid with `SO_PEERCRED`.

JSON-RPC 2.0, one message per line (UTF-8, `\n`-terminated), in both directions. A line over 64 MiB drops the connection.

The client sends:

| Method | Params | Result |
|---|---|---|
| `initialize` | `{ api, role, build }` | `{ api, build, addons_dir }` |
| `execute` | a [command](#commands) | its response, or an error |
| `take_output` | `{ session }` | null; the terminal's output follows as `output` |
| `release_output` (notification) | `{ session }` | ends the session, as dropping the stream does in-process |

The daemon sends:

| Method | Params | When |
|---|---|---|
| `event` (notification) | an [event](#events) | Every event the client's role may see. |
| `resync` (notification) | | The client fell behind and missed events; list again. |
| `output` (notification) | `{ session, data }` | Terminal output, `data` in base64. |
| `output_ended` (notification) | `{ session }` | The terminal has no more output. It always comes after the last `output`. |

- **`initialize` first:** anything else before it is refused. `api` must be the daemon's `API_VERSION`. `build` is the client's version; the daemon's comes back, so a client can tell it is talking to a daemon from before an upgrade.
- **Roles:** `role` is `gui` (the GTK window), `cli` or `mcp` (the MCP bridge). `gui` and `cli` may run every command. `mcp` may only do what the user granted; see [MCP bridge](#mcp-bridge).
- **Sessions belong to their client:** terminals, file sessions and database sessions close when the client that opened them disconnects. Tunnels belong to the daemon and keep running.
- **Output and events:** replies and events are sent before terminal output, so a busy terminal does not hold them up. Terminal output that the client reads slowly slows the session down rather than being dropped.

## MCP bridge

`burrow mcp serve` lets an AI assistant such as Claude use Burrow, as a client with the `mcp` role. It may only do what the user granted, node by node.

| Capability | On | Allows |
|---|---|---|
| `sql-read` | `postgres` | `open_database`, `list_schema`, `cancel_query`, and `run_query` that cannot write |
| `sql-write` | `postgres` | the above, `run_query` that writes, and `set_read_only` |
| `exec` | `ssh-host` | `exec` |
| `files-read` | `ssh-host` | `open_files`, `list_dir`, `read_file` |
| `files-write` | `ssh-host` | the above, `write_file`, `make_dir`, `rename_path`, `remove_path` |
| `tunnel` | `ssh-tunnel` | `start_tunnel`, and `close_session` on that tunnel |

- **Always allowed:** `version`, `list_nodes`, `get_node`, `list_sessions` and `list_grants`. Nodes hold no secrets.
- **Never allowed:** changing nodes, secrets, prompts, autostart, addons, grants, terminals and history. `download`, `upload` and `export_query` are refused because they take a local path. The bridge works with contents instead (`read_file`, `write_file`), so it can't read or overwrite the user's own files.
- **Its own sessions:** session commands only work on sessions the bridge opened itself. Those sessions close when it disconnects.
- **Read-only SQL:** without `sql-write`, every query runs as one statement inside `BEGIN READ ONLY` … `ROLLBACK`. Postgres refuses to prepare more than one statement, so SQL can't change the setting or end the transaction and then write. Some functions still have effects in a read-only transaction, such as `pg_terminate_backend`; for full safety, give the node a database role that can only read.
- **Grants are pinned:** `set_grant` needs `expect`, the node as the user saw it (`name`, `props`, and the same for the node in `props.via`). A grant is kept in `$XDG_STATE_HOME/burrow/mcp.toml` with a fingerprint of those settings. If a node file changes, the grant goes stale (`stale` in `list_grants`) and allows nothing until it is granted again. An empty `capabilities` revokes.
- **Audit log:** every command the bridge sends goes on a line of `$XDG_STATE_HOME/burrow/mcp-audit.log`, with whether it ran and why not. A refused command is logged by its type only, and file contents by their size.
- **Events:** the bridge only hears `node_*` events and `grants_changed`. It follows its own sessions by listing them.
- **Not a wall against the user's own programs:** grants limit what the assistant can do through the bridge. Any process running as the user can connect with another role, so an assistant that can also run shell commands could use `burrow` directly.

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
