# Tasks

## Active

- [ ] **Get a friend testing Burrow** - send them the README's "Trying it" section. If anything fails, ask for the output of `burrow doctor` and for `~/.local/state/burrow/burrowd.log`. Their feedback sets the order of everything below.

## Waiting On

- [ ] **Friend's first test run** - see Active.

## Someday

Next milestones from `docs/DESIGN.md`, in rough order; pick by what the tester uses.

- [ ] **Monitoring v2** - background sampling, alerts and history on disk (DESIGN.md "Server agent", "Live only, for now"). `GNotification` is in the design and unused anywhere yet, so alerts would be its first use.
- [ ] **Containers and PM2 in burrow-agent** - DESIGN.md "Server agent".
- [ ] **Shell-command fallback** for servers without the agent - DESIGN.md "Fallback (later)".
- [ ] **Secrets bundle** for sharing a config folder (Argon2id + XChaCha20-Poly1305) - DESIGN.md "Plain-text config", "Sharing". No code yet.
- [ ] **`stop()` probe can turn away a starting daemon** - `free()` in `crates/burrow-core/src/ipc/start.rs` checks the lock by briefly taking a shared lock. A daemon whose `try_lock` lands in that moment logs "another burrowd is already running" and exits, and the window that started it waits out `START_TIMEOUT`. Not seen yet; found while fixing `burrow daemon stop` under an open window.
- [ ] **Residual tunnel-test flake** - `a_local_tunnel_forwards_to_its_target` in `crates/burrow-core/tests/ssh.rs` failed once in 25 full workspace runs after PR #10 with "could not listen on 127.0.0.1:20019: Address already in use", a port from the allocator in `crates/burrow-core/tests/common/mod.rs`. Not reproduced in 22 further runs, 10 of them logging every port probe with `ss`. Only worth time if CI shows it.

## Done

- [x] ~~M10: hosts added in Burrow~~ (2026-10-25, PR #13 merged)
- [x] ~~Review after M9, `burrow doctor`, start-up failure dialog, Arch package, review fixes~~ (2026-10-25, PR #10 merged)
