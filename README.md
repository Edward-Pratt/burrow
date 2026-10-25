# Burrow

A Linux-first desktop home base for your servers and databases: terminals, SSH, databases and tunnels in one window, extended through addons.

Inspired by [Tusk](https://github.com/repticJS/tusk). Burrow is written from scratch and shares no code with it.

## Status

Milestones 1–7 (core, GTK shell, local terminal, SSH, addons, tunnels and SFTP, the PostgreSQL client) are done; milestone 8 (`burrowd`, the `burrow` CLI and the MCP bridge) is in review. See [docs/DESIGN.md](docs/DESIGN.md).

## Goals

- **Linux first.** Built for Arch and Wayland (Hyprland) first, with other distros next. Windows is not a target.
- **Small core, everything else an addon.** The core provides tabs, sidebar, command palette, secrets and the addon API. Features that need secrets, such as SSH, SFTP and the Postgres client, are built into the core. The rest are addons.
- **Secrets stay in the core.** Addons work with connection handles. They never see passwords or private keys.
- **Addons describe their UI as data.** They run as separate processes, and the core draws their UI with native widgets.

## Stack

Native Rust + GTK4 (gtk4-rs + relm4). No webview. The core (`burrow-core`) runs in a daemon of its own (`burrowd`), so tunnels keep running with the window closed. The window (`burrow-gtk`) and the command line (`burrow`) are clients of it.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Addons run as separate processes and may use any license.

## Running it

```sh
cargo run -p burrow-gtk
```

The window starts `burrowd` when none is running, and the daemon stops by itself a minute after nothing uses it. To start it at login instead, so tunnels set to autostart come up without the window, install the units in `dist/systemd/` and run `systemctl --user enable --now burrowd.socket burrowd.service`.

Set `BURROW_CONFIG_DIR=/some/folder` to use a different config folder from `~/.config/burrow`, for example while developing. A run with it set gets a daemon of its own.

## From the command line

```sh
cargo install --path crates/burrow-cli
burrow ls                         # the sidebar
burrow ssh web                    # a terminal
burrow exec web -- uptime         # one command
burrow query app 'select 1'       # SQL, read-only unless --write
burrow tunnel start db-tunnel
burrow call '{"type": "list_sessions"}'
```

## With Claude

`burrow mcp serve` is an MCP server, so an assistant such as Claude can use your hosts and databases, but only where you allow it:

```sh
claude mcp add burrow -- burrow mcp serve
burrow mcp grant app sql-read     # or right-click the node, "Claude access…"
```

Nothing is allowed until you grant it. A grant covers one node and holds only while the node's settings stay as you saw them. Everything the assistant does is logged to `~/.local/state/burrow/mcp-audit.log`. See [docs/API.md](docs/API.md#mcp-bridge) for what each capability allows.
