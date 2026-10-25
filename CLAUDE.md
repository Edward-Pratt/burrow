# Burrow

A Linux-first desktop app for servers and databases, in Rust + GTK4 (gtk4-rs + relm4, no libadwaita, no webview). Read `docs/DESIGN.md` for the decisions and the milestone plan, and `docs/API.md` for the command/event API.

## Layout

- `crates/burrow-core`: everything that isn't drawing. It owns config, secrets, SSH, tunnels, SFTP and the addon host. Clients drive it only through `Core::execute(Command)` and `Core::subscribe()` (`src/api.rs` is the contract).
  - `src/ssh/`: `connection.rs` shares one connection per host; `session.rs` holds terminals, `tunnel.rs` tunnels, `files.rs` SFTP.
  - `src/addon/`: manifests, approval, JSON-RPC over stdio.
- `crates/burrow-gtk`: a thin client of the core in `burrowd`. State flows one way: user action → `Command` → core `Event` → redraw. Each tab kind has a module: `terminal.rs`/`remote.rs` (terminals), `tunnels.rs`, `files.rs`, `addons.rs`. `app.rs` routes messages and session events between them.
- `crates/burrow-addon-notes`: the example addon, plus the addon integration tests.
- `crates/burrowd`: the daemon binary. The daemon itself is `burrow-core/src/daemon.rs`, and the socket protocol (server, client, finding and starting the daemon, and what each role may run) is `src/ipc/`.
- `crates/burrow-cli`: the `burrow` command, and `burrow mcp serve` in `src/mcp.rs`. What the MCP bridge may do is decided in the daemon (`ipc/server.rs`, `grants.rs`), never in the bridge.

## Checks

CI runs exactly these. Run all three before every commit:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- `crates/burrow-core/tests/ssh.rs` starts a throwaway `sshd` per test. It skips silently if `/usr/bin/sshd` is missing, so check those tests actually ran.
- Put unit tests in the module they test, in a `#[cfg(test)] mod tests`.
- To see a UI change working, use the `burrow-gui-test` skill (`.claude/skills/burrow-gui-test/`). It runs the app on a headless Xvfb display, never on the user's desktop.

## Rules that shape the code

- **Config is untrusted.** Node files can arrive from a dotfiles repo, a shared folder or a `git pull`. Nothing in them may start anything, approve anything or point outside where it should. Examples: tunnels only listen on loopback, and autostart and addon approvals live in `$XDG_STATE_HOME/burrow/`, pinned to a fingerprint of what the user saw.
- **Servers are untrusted.** Check names from a server before they become part of a local path, and strip control characters before showing them. Never write through symlinks.
- **Secrets stay in the core.** Clients and addons never see passwords or keys.
- **API changes:** additive changes keep `API_VERSION` at 1, and breaking ones bump it. Update `docs/API.md` in the same change: the commands table, events table and the rules section.
- **Style:** comments say why, not what, in plain sentences. Match the density of the surrounding code. UI text uses sentence case, with "…" on items that open a dialog.

## Workflow

- One branch per milestone (such as `m6-tunnels-sftp`), with small commits. Write commit messages as an imperative subject, then prose explaining what changed and why. Don't use conventional-commit prefixes.
- Ask before pushing or opening a PR. A milestone PR has these sections: What's new, Trust model, Review fixes, Patterns used and why, Tested, Try it, Not in this PR. See PR #5 or #6.
- The security-guidance plugin reviews each commit and push in the background, and reports later. Fix its findings with a test where you can, or explain why one doesn't apply.

## Running

```sh
cargo run -p burrow-gtk
```

The window is a client of `burrowd`. If no daemon is running, it starts its own binary again with `--burrowd`, so the daemon is always the same build. The daemon stops after a minute with no clients and no sessions. Its log is `burrowd.log` in the state folder.

For dev runs, point Burrow away from the user's real setup:
- `BURROW_CONFIG_DIR` sets the config folder.
- `BURROW_STATE_DIR` sets the folder for approvals and autostart.
- `BURROW_SSH_CONFIG` replaces `~/.ssh/config`, as `ssh -F` does.
- `BURROW_RUNTIME_DIR` sets the daemon's socket folder. If it isn't set but any of the three above is, the run gets a private daemon under `$XDG_RUNTIME_DIR/burrow/dev-<hash>/`, so it never joins the real one. Socket paths max out at 107 bytes, so don't put it in the scratchpad.
- While the daemon runs, its pid is in `burrowd.lock` in the socket folder. After changing core code, stop the old dev daemon, because a running daemon keeps its old code.

The user's agent socket is `/run/user/1000/ssh-agent.socket`. A shell started earlier may not have `SSH_AUTH_SOCK` set.
