# Tasks

## Active

- [ ] **Get a friend testing Burrow** - send them the README's "Trying it" section. If anything fails, ask for the output of `burrow doctor` and for `~/.local/state/burrow/burrowd.log`. Their feedback sets the order of everything below.
- [ ] **M10: hosts added in Burrow** - the biggest first-run gap. "New SSH host" in `crates/burrow-gtk/src/app.rs` (around line 1698) only takes an alias that must already exist in `~/.ssh/config`, so a tester without an ssh config can't add a server from inside Burrow.
  - Design is already in `docs/DESIGN.md`, "OpenSSH config as source of truth": a dialog with hostname, user, port and key, written to `~/.ssh/config.d/burrow.conf`, with the `Include` line added to `~/.ssh/config` after asking once, so `ssh <alias>` in a shell matches Burrow.
  - Nothing in the code writes to the ssh config yet (`grep config.d crates/` finds nothing). Keep `ssh -G` as the only reader.
  - Rules that apply: config is untrusted, so the written file must never be a symlink target, and the alias must pass `ssh::config::check_alias`.

## Waiting On

- [ ] **Friend's first test run** - see Active.

## Someday

Next milestones from `docs/DESIGN.md`, in rough order; pick by what the tester uses.

- [ ] **Monitoring v2** - background sampling, alerts and history on disk (DESIGN.md "Server agent", "Live only, for now"). `GNotification` is in the design and unused anywhere yet, so alerts would be its first use.
- [ ] **Containers and PM2 in burrow-agent** - DESIGN.md "Server agent".
- [ ] **Shell-command fallback** for servers without the agent - DESIGN.md "Fallback (later)".
- [ ] **Secrets bundle** for sharing a config folder (Argon2id + XChaCha20-Poly1305) - DESIGN.md "Plain-text config", "Sharing". No code yet.
- [ ] **`ipc::stop` under an open window** - `burrow daemon stop` reported "burrowd did not stop": the window reconnects and starts a new daemon inside the ten-second wait in `stop()` (`crates/burrow-core/src/ipc/start.rs`), which retakes the lock before `stop` sees it free. Treat a new holder of the lock as the old one having stopped. Seen on 2026-10-25 on the Xvfb rig.
- [ ] **Residual tunnel-test flake** - `a_local_tunnel_forwards_to_its_target` in `crates/burrow-core/tests/ssh.rs` failed once in 25 full workspace runs after PR #10 with "could not listen on 127.0.0.1:20019: Address already in use", a port from the allocator in `crates/burrow-core/tests/common/mod.rs`. Not reproduced in 22 further runs, 10 of them logging every port probe with `ss`. Only worth time if CI shows it.

## Done

- [x] ~~Review after M9, `burrow doctor`, start-up failure dialog, Arch package, review fixes~~ (2026-10-25, PR #10 merged)
