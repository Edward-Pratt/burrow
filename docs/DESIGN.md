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
| `burrow-gtk` | Thin GTK4 client on top of the core. |
| `burrowd` (later) | The core as a user systemd service, so tunnels and monitoring keep running with the window closed. |
| `burrow-cli` (later) | A command-line client. |

The core exposes one API that the UI, addons, the CLI and an MCP bridge all use.

## Platform

| Concern | Choice |
|---|---|
| Secrets | Secret Service through the `keyring` crate (gnome-keyring or KWallet) |
| Local terminal | VTE starts and runs the shell itself; read the shell list from `$SHELL` and `/etc/shells`. `portable-pty` only if the core ever needs a PTY without the UI. |
| SSH | `russh`; agent auth through `SSH_AUTH_SOCK` |
| Notifications | `GNotification` (freedesktop D-Bus, works with mako and swaync) |
| Packaging | AUR PKGBUILD as the main channel, with no self-updater. Not Flatpak: its sandbox blocks host shells, the SSH agent and `~/.ssh`. |
| RDP | IronRDP or FreeRDP, as a late addon |

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
- **Secrets:** addons never receive credentials. They ask the core to open a connection and get an opaque handle back. All authenticated I/O goes through the core.

### Built-in addons (dogfood the API)

Built-in features use the same addon API as third-party ones.

1. Local terminal
2. SSH terminal + hosts
3. PostgreSQL client
4. SFTP file panel
5. SSH tunnels
6. Later: systemd, Docker/Podman, PM2, monitoring, MCP bridge, RDP

## Milestones

1. Workspace with `burrow-core` and `burrow-gtk`: window, tabs, sidebar, palette, settings, keyring
2. Local terminal (VTE)
3. SSH hosts + terminal (russh), host key verification, agent auth
4. Addon protocol v1: manifest, loader, declarative UI, permissions
5. Postgres addon (GtkSourceView editor, GtkColumnView grid)
6. SFTP, tunnels, then the rest
7. `burrowd` + `burrow-cli`
