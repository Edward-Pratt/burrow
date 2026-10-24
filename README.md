# Burrow

A Linux-first desktop home base for your servers and databases: terminals, SSH, databases and tunnels in one window, extended through addons.

Inspired by [Tusk](https://github.com/repticJS/tusk). Burrow is written from scratch and shares no code with it.

## Status

Milestones 1 (core) and 2 (GTK shell) in review. See [docs/DESIGN.md](docs/DESIGN.md).

## Goals

- **Linux first.** Built for Arch and Wayland (Hyprland) first, with other distros next. Windows is not a target.
- **Small core, everything else an addon.** The core provides tabs, sidebar, command palette, secrets and the addon API. Features such as the SSH terminal, the Postgres client and SFTP are addons, including the built-in ones.
- **Secrets stay in the core.** Addons work with connection handles. They never see passwords or private keys.
- **Addons describe their UI as data.** They run as separate processes, and the core draws their UI with native widgets.

## Stack

Native Rust + GTK4 (gtk4-rs + relm4). No webview. The core (`burrow-core`) is separate from the UI (`burrow-gtk`).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Addons run as separate processes and may use any license.

## Running it

```sh
cargo run -p burrow-gtk
```

Set `BURROW_CONFIG_DIR=/some/folder` to use a different config folder from `~/.config/burrow`, for example while developing.
