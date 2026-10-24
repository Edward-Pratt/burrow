# Burrow design notes

## Reference

[Tusk](https://github.com/repticJS/tusk) is a Windows-focused Tauri 2 + React app. It includes a PostgreSQL client, SSH terminals, SFTP/FTP, tunnels, RDP, systemd, PM2, Docker, monitoring and an MCP server for Claude. It has no license, so Burrow uses it only as a feature reference and copies none of its code.

## Platform: Linux replacements for Tusk's Windows pieces

| Tusk (Windows) | Burrow (Linux) |
|---|---|
| Windows Credential Manager | Secret Service through the `keyring` crate (gnome-keyring or KWallet) |
| ConPTY; detects PowerShell, cmd and WSL | `portable-pty`; reads shells from `$SHELL` and `/etc/shells` |
| Pageant / Windows OpenSSH agent | `SSH_AUTH_SOCK` |
| Windows toast notifications | D-Bus notifications (`tauri-plugin-notification`) |
| WebView2 | WebKitGTK. Test early on Wayland/NVIDIA; it may need `WEBKIT_DISABLE_DMABUF_RENDERER=1` |
| `.msi` + Tauri updater | AUR PKGBUILD + AppImage. The Tauri updater only covers AppImage |
| RDP | IronRDP or FreeRDP, as a late addon |

## Architecture

### Core

- Window, tabs, sidebar tree (folders of nodes), command palette, settings, theme
- Secrets store (keyring)
- Connection manager: owns SSH sessions, DB connections and PTYs, and hands out opaque handles
- Addon host: loads addons, enforces permissions, routes RPC

### Addons

Each addon is a folder containing `addon.toml`:

```toml
id = "postgres"
name = "PostgreSQL"
version = "0.1.0"
permissions = ["db", "ssh:tunnel"]

[ui]
entry = "ui/index.js"        # ES module, loaded at runtime

[backend]                    # optional
command = "bin/postgres-addon"
protocol = "jsonrpc-stdio"
```

- **UI side:** an ES module that receives a typed host API. It can register sidebar node types, tab types, panels, palette commands and context-menu items.
- **Backend side:** an optional sidecar process that speaks JSON-RPC over stdio (similar to LSP or MCP). Addons can be written in any language, and a crash only takes down the sidecar. WASM (extism) could replace this later if addons need sandboxing.
- **Permissions:** declared in the manifest, approved by the user on install, and enforced by the core on every host call.
- **Secrets:** addons never receive credentials. They ask the core to open a connection and get a handle back. All authenticated I/O goes through the core.

### Built-in addons (dogfood the API)

Built-in features use the same addon API as third-party ones.

1. Local terminal
2. SSH terminal + hosts
3. PostgreSQL client
4. SFTP file panel
5. SSH tunnels
6. Later: systemd, Docker/Podman, PM2, monitoring, MCP bridge, RDP

## Milestones

1. Core shell: Tauri app, tabs, sidebar, palette, settings, keyring
2. Local terminal (xterm.js + portable-pty)
3. SSH hosts + terminal (russh), host key verification, agent auth
4. Addon API v0: manifest, loader, UI registration, sidecar RPC, permissions
5. Postgres addon
6. SFTP, tunnels, then the rest
