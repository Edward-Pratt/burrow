# Burrow

A Linux-first desktop home base for your servers and databases: terminals, SSH, databases and tunnels in one window, extended through addons.

Inspired by [Tusk](https://github.com/repticJS/tusk). Burrow is written from scratch and shares no code with it.

## Status

Milestones 1–11 are done: core, GTK shell, local terminal, SSH, addons, tunnels and SFTP, the PostgreSQL client, `burrowd` with the `burrow` CLI and the MCP bridge, `burrow-agent` with the monitor tab, adding SSH hosts from Burrow, and monitoring in the background with history and alerts. It is early software, in testing with a few people. See [docs/DESIGN.md](docs/DESIGN.md).

## Goals

- **Linux first.** Built for Arch and Wayland (Hyprland) first, with other distros next. Windows is not a target.
- **Small core, everything else an addon.** The core provides tabs, sidebar, command palette, secrets and the addon API. Features that need secrets, such as SSH, SFTP and the Postgres client, are built into the core. The rest are addons.
- **Secrets stay in the core.** Addons work with connection handles. They never see passwords or private keys.
- **Addons describe their UI as data.** They run as separate processes, and the core draws their UI with native widgets.

## Stack

Native Rust + GTK4 (gtk4-rs + relm4). No webview. The core (`burrow-core`) runs in a daemon of its own (`burrowd`), so tunnels keep running with the window closed. The window (`burrow-gtk`) and the command line (`burrow`) are clients of it.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Addons run as separate processes and may use any license.

## Trying it

Burrow needs GTK 4.20 or newer, VTE 4, GtkSourceView 5, OpenSSH and a Secret Service (a keyring daemon such as gnome-keyring, KWallet or KeePassXC). Building it needs Rust 1.93 or newer, best installed with [rustup](https://rustup.rs).

On Arch, the package in `dist/` builds and installs everything: the window, the daemon, the command line, a desktop entry, the systemd units, and the server agent for each musl target you have (`rustup target add x86_64-unknown-linux-musl`, or the `rust-musl` package):

```sh
git clone https://github.com/Edward-Pratt/burrow
cd burrow/dist && makepkg -si
```

To install it for yourself only, in `~/.local`, and keep it up to date with main, run the install script. It needs no root, and adds the same pieces plus a systemd user timer. Once a day the timer fetches main, and when it has moved, builds and installs it in the background, then says so in a desktop notification. Restart Burrow to use the new build.

```sh
git clone https://github.com/Edward-Pratt/burrow
burrow/dist/install.sh
```

The script keeps its own clone in `~/.local/share/burrow/src`. Run `burrow-update` to update now, `burrow-update --branch NAME` to follow another branch, and `journalctl --user -u burrow-update` to see how the last update went. It builds and installs whatever lands on the branch without asking, so anyone who can push there can run code on your computer. To remove it, run `systemctl --user disable --now burrow-update.timer`, then delete `~/.local/bin/{burrow,burrowd,burrow-gtk,burrow-update}`, `~/.local/lib/burrow`, `~/.local/share/burrow/src` and the `burrow*` units in `~/.local/share/systemd/user`.

Anywhere else, build it from the checkout. If a library is missing, `cargo build` names it.

```sh
sudo pacman -S --needed gtk4 vte4 gtksourceview5 openssh gnome-keyring   # Arch
git clone https://github.com/Edward-Pratt/burrow
cd burrow
cargo build --release
./target/release/burrow doctor      # says what is missing, and how to get it
./target/release/burrow-gtk
```

`burrow doctor` checks the pieces Burrow needs at run time: the folders it writes to, OpenSSH, the Secret Service, the SSH agent, the agent builds for servers and the daemon itself. The window runs the same checks when it can't start, and shows them. Monitors need the server agent, built once for each kind of server:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build -p burrow-agent --profile agent --target x86_64-unknown-linux-musl
```

To add a server, pick **New** and then **SSH host**. Burrow saves the host name, user, port and key in `~/.ssh/config.d/burrow.conf`, so `ssh <alias>` in a terminal connects the same way. The first time, it asks before adding the line that reads that file to the top of `~/.ssh/config`. A host already in `~/.ssh/config` can be added by its alias instead.

Burrow finds it in the workspace it runs from, in `lib/burrow/` beside the folder it runs from (`~/.local/lib/burrow/` for the install script), in `/usr/lib/burrow/` or in `$BURROW_AGENT_DIR`. When something goes wrong, `burrowd.log` in `~/.local/state/burrow/` has the daemon's side of it.

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
