# Burrow design notes

## Reference

[Tusk](https://github.com/repticJS/tusk) is a Windows-focused Tauri 2 + React app. It includes a PostgreSQL client, SSH terminals, SFTP/FTP, tunnels, RDP, systemd, PM2, Docker, monitoring and an MCP server for Claude. It has no license, so Burrow uses it only as a feature reference and copies none of its code.

## Decisions

### Native GTK4 UI, no webview

Burrow is a native GTK4 app written in Rust (gtk4-rs + relm4). It does not use Tauri or any webview.

- WebKitGTK, Tauri's Linux webview, is slower than Chromium and has rendering bugs on Wayland and NVIDIA. Tauri's Chromium (CEF) backend is not stable yet and would add about 100 MB.
- GTK4 provides the three hard widgets ready-made:
  - Terminal: VTE (`vte4`), the engine behind GNOME Console and Ptyxis.
  - SQL editor: GtkSourceView 5, with highlighting and completion.
  - Large result sets: GtkColumnView, which only creates widgets for visible rows.
- It is native on Wayland, has built-in support for desktop notifications and file pickers, and works fine on Hyprland.
- Use plain GTK4 with our own CSS stylesheet rather than libadwaita, so the app can match a tiling WM setup instead of looking like GNOME.

Alternatives considered:
- **Iced/libcosmic:** pure Rust and promising, but it has no mature code editor, the terminal widget would have to be built on `alacritty_terminal`, and releases are slow.
- **Qt 6:** the most mature widgets, but calling it from Rust needs awkward C++ glue (cxx-qt).
- **egui, Slint, Dioxus native:** not mature enough for a terminal, a code editor and a large data grid.

### Core separate from UI

Cargo workspace:

| Crate | Role |
|---|---|
| `burrow-core` | Connections, secrets, sessions, addon host, config. No UI code. |
| `burrow-gtk` | Thin GTK4 client of the core in `burrowd`. |
| `burrowd` | The core as a process of its own, so tunnels and monitoring keep running with the window closed. Started by a client, or by a systemd user unit. |
| `burrow-cli` | The `burrow` command-line client, and the MCP bridge (`burrow mcp serve`). |
| `burrow-agent` | A small static program that runs on servers and reports their state over SSH. Its library is the wire format the core parses. |

### One versioned API for every client

The core is driven only through a single command and event API, defined in one place and versioned.

- **Commands** are requests (`connect`, `open_terminal`, `run_query`) and return a result.
- **Events** are what the core pushes to subscribers (`session_state_changed`, `query_progress`, `alert_raised`).
- **Clients:** the GTK UI, addons, `burrow-cli` and the MCP bridge for Claude are all clients of this API. None of them use a private shortcut into the core. Addons speak JSON-RPC over stdio; the UI, the CLI and the MCP bridge speak it over the daemon's Unix socket, each in a role that limits what it may do.
- **Addon protocol:** the addon protocol is a permission-filtered subset of the same API.
- **Testing:** the core is tested headless against Docker containers running `sshd` and Postgres.

### Plain-text config

All non-secret state lives as TOML files in `$XDG_CONFIG_HOME/burrow/` (default `~/.config/burrow/`):

```
~/.config/burrow/
  burrow.toml              # app settings
  tree/                    # sidebar: one folder per sidebar folder
    work/
      _folder.toml         # folder metadata (colour, order)
      api-db.toml          # a connection
      prod-web.toml        # an SSH host's Burrow-only extras
  addons/                  # installed addons
```

- **Secrets:** stored only in the keyring. Config files hold a reference to the keyring entry (`secret = "keyring:burrow/api-db"`), never the secret itself.
- **Dotfiles-friendly:** you can keep the config in a dotfiles repo and review changes with git.
- **Sharing:** sharing a folder means copying its directory. Secrets are added only through an optional encrypted bundle (Argon2id + XChaCha20-Poly1305).
- **Edits from outside Burrow:** the core watches the directory, so edits made in an editor show up live.
- **Runtime state stays out of the config folder:** history, the open-tab layout and caches live in `$XDG_STATE_HOME/burrow/` and `$XDG_CACHE_HOME/burrow/`.

### OpenSSH config as source of truth

- **Host settings:** hosts come from `~/.ssh/config`, including `Include`, `Host` patterns, `Match`, `ProxyJump`, `IdentityFile`, `User` and `Port`. Burrow does not keep its own copy of them.
- **Host keys:** host keys are checked against and added to `~/.ssh/known_hosts`.
- **Burrow-only extras:** colour, folder, start directory and monitoring settings go in Burrow's config, keyed by the `Host` alias.
- **Hosts added in Burrow:** these are written to `~/.ssh/config.d/burrow.conf`, which is pulled in by an `Include` line. Burrow asks once before adding that line, so `ssh myhost` in a shell matches Burrow.

### Server agent

`burrow-agent` is an optional small static binary (musl, x86_64 and aarch64, about 500 KB) that runs on servers. It streams host stats, systemd services, Docker or Podman containers and PM2 apps over the existing SSH connection as JSON lines.

- **Owned by the core, like Postgres:** the core uploads the agent, runs it and parses its output, and the monitor tab is built in. An addon process would need the SSH connection, which only the core holds.
- **Install:** Burrow asks once per host, then uploads it to `~/.local/share/burrow/` on the server, and checks its hash against its own copy every time a monitor starts. The consent lives in the state folder, pinned to the host node, because a node file must never put a program on a server. The MCP bridge can read monitors with a grant, but never installs.
- **Live, or in the background:** a monitor runs while a client has it open, and keeps an hour of samples in memory. A host the user chooses to monitor in the background gets one monitor the daemon runs from when it starts, with or without a window, reconnecting when it drops. That choice lives in the state folder, pinned to the host like the install consent, so a node file can't turn it on. Background monitors keep a week of one-minute averages on disk, by this computer's clock, and raise alerts from fixed rules (a failed service, a disk over 90%, memory under 10%, a host not answering for a minute). Thresholds per host are later.
- **Fallback (later):** servers without the agent will fall back to plain shell commands with parsed output.
- **No exposed ports:** the agent never listens on the network. It only talks over SSH.

## Platform

| Concern | Choice |
|---|---|
| Secrets | Secret Service through the `keyring` crate (gnome-keyring or KWallet) |
| Local terminal | VTE starts and runs the shell itself; read the shell list from `$SHELL` and `/etc/shells`. `portable-pty` only if the core ever needs a PTY without the UI. |
| SSH | `russh`; host settings from `ssh -G <alias>` (OpenSSH resolves `Include`/`Match`/patterns itself); agent auth through `IdentityAgent` / `SSH_AUTH_SOCK`; own `known_hosts` reader and writer (russh's skips markers and wildcards) |
| Paths | XDG base directories (`directories` crate) |
| Notifications | `burrowd` calls `org.freedesktop.Notifications` over D-Bus itself (zbus, already there for the keyring), so alerts show with no window open; works with mako, swaync and GNOME. Not `GNotification`, which needs the window. |
| Packaging | A PKGBUILD for pacman, or `dist/install.sh` for one user in `~/.local`. CI publishes a build of each green push to main (release `build-main`), which the script downloads and checks against its SHA-256, so users don't compile. "Check for updates…" in the window runs the installed script and restarts into the new build; a daily timer only checks and notifies. Updates stay out of the core, so nothing that talks to the daemon, such as the MCP bridge, can start one. Builds come from the newest Arch, and an update whose libraries are missing isn't installed. `--branch` builds from source instead. Not Flatpak: its sandbox blocks host shells, the SSH agent and `~/.ssh`. |
| VNC | The core signs in over a channel on the SSH host's connection, then passes the RFB stream to the window, which draws it with vnc-rs into a GTK texture. Not gtk-vnc, which is GTK 3 only. |
| RDP | IronRDP or FreeRDP, later |

## Addons

Each addon is a folder containing `addon.toml`:

```toml
id = "postgres"
name = "PostgreSQL"
version = "0.1.0"
api = "1"                    # addon protocol major version
permissions = ["db", "ssh:tunnel"]

[backend]
command = "bin/postgres-addon"
protocol = "jsonrpc-stdio"
```

- **Process model:** each addon runs as a sidecar process and speaks JSON-RPC over stdio (similar to LSP or MCP). Addons can be written in any language, and a crash only takes down that addon.
- **Declarative UI:** addons describe their UI as data. They do not draw it. An addon can:
  - register sidebar node types, palette commands and context-menu actions
  - show forms, lists, tables, key-value panels and log streams
  - ask the core to open a terminal, editor or data grid on a connection handle

  The core draws all of this with native widgets. Addons never depend on GTK, so changing toolkit later breaks no addon.
- **Custom UI escape hatch (later):** an optional webview panel for an addon that needs fully custom UI. It loads only when that addon uses it.
- **Permissions:** declared in the manifest, approved by the user on install, and enforced by the core on every call.
- **Approval is pinned:** an approval covers a hash of the manifest's permissions and kinds and of every file in the addon's folder. It is kept in `$XDG_STATE_HOME/burrow/`, not the config folder, because the config folder may be shared or synced. An addon that arrives with a `git pull` never starts by itself, and a changed one asks again. Permissions limit the Burrow API, not the process: an approved addon runs as the user. Burrow does not sandbox it, and the approval dialog says so.
- **Static contributions:** kinds, palette commands and context actions are declared in the manifest, not reported at runtime. Burrow can show them, and check writes against them, without starting the addon, so addons start only when first used.
- **Secrets:** addons never receive credentials. They ask the core to open a connection and get an opaque handle back. All authenticated I/O goes through the core.

### Built-in addons (dogfood the API)

Built-in features use the same addon API as third-party ones.

1. Local terminal
2. SSH terminal + hosts
3. PostgreSQL client
4. SFTP file panel
5. SSH tunnels
6. Since built into the core rather than as addons: systemd, Docker/Podman and PM2 (in the agent), monitoring, the MCP bridge and VNC. Later: RDP

## Milestones

1. Workspace with `burrow-core` and `burrow-gtk`: core API v1, TOML config tree + file watcher, keyring
2. GTK shell: window, tabs, sidebar, palette, settings
3. Local terminal (VTE)
4. SSH: `~/.ssh/config` parsing, `known_hosts`, russh terminal, agent auth
5. Addon protocol v1: manifest, loader, declarative UI, permissions
6. SSH tunnels + SFTP file panel
7. Postgres client in the core, since addons never see secrets (GtkSourceView editor, GtkColumnView grid), then the rest
8. `burrowd` + `burrow-cli` + MCP bridge, with per-node grants for what an assistant may do
9. `burrow-agent` + monitoring addon
10. Hosts added in Burrow: a dialog that writes `~/.ssh/config.d/burrow.conf`
11. Monitoring v2: background monitors, history on disk, alerts and desktop notifications
12. Architecture and UX review: one store for every pinned approval, each command classified in one place, an audit log with rotation, the sidebar on a list model with drag and drop, one panel type per tab, several shells per host, pages that update in place, a read-only file viewer, and an errors list in the status bar
13. Terminal essentials: open tabs restored at the next start, find in a terminal, links that open with Ctrl+click, a terminal font setting and zoom
14. Edit remote files: the file viewer saves back over SFTP, and checks the file didn't change on the server meanwhile
15. Containers and PM2 in `burrow-agent`, shown in the monitor tab, with alerts when one stops
16. Service actions from the monitor tab: systemd units through sudo (a terminal asks for the password when sudo needs one), containers and PM2 apps, and each one's log in a tab
17. VNC desktops in a tab, through an SSH host, with the core signing in so the password stays in the core
