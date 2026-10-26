# Tasks

## Active

- [ ] **Get a friend testing Burrow** - send them the README's "Trying it" section. If anything fails, ask for the output of `burrow doctor` and for `~/.local/state/burrow/burrowd.log`. Their feedback sets the order of everything below.


## Waiting On

- [ ] **Friend's first test run** - see Active.

## Someday

Next milestones from `docs/DESIGN.md`, in rough order; pick by what the tester uses.

- [ ] **M16: systemd actions and journal** - DESIGN.md milestone 16.
- [ ] **M17: VNC** - DESIGN.md milestone 17. gtk-vnc is GTK 3 only, so this needs an RFB client drawn into a GTK 4 texture.

- [ ] **Shell-command fallback** for servers without the agent - DESIGN.md "Fallback (later)".
- [ ] **Secrets bundle** for sharing a config folder (Argon2id + XChaCha20-Poly1305) - DESIGN.md "Plain-text config", "Sharing". No code yet.
- [ ] **Alert thresholds per host** - Monitoring v2 has fixed rules (disk 90%, memory 10%, a minute unreachable). Per-host numbers would need a settings UI, and should live in the state folder rather than node files, so a shared config can't silence alerts.
- [ ] **Bounded input for sessions** - what a client writes to a terminal goes to its session through an unbounded channel. A client that writes faster than the server reads grows the daemon's memory. Left out of M12.
- [ ] **Window before the daemon answers** - `burrow-gtk` connects to `burrowd`, starting it if needed, before it opens a window, so a slow start shows nothing. A splash would need `App` to start without a `Client`. Left out of M12 as not cheap.
- [ ] **Daemon-replace test flake** - `ipc::start::tests::a_daemon_replaced_at_once_has_still_stopped` failed once on 2026-10-26 with "burrowd is starting; try again" while the machine was busy, then passed four times. May relate to the lock retry added in M12.
- [ ] **Residual tunnel-test flake** - `a_local_tunnel_forwards_to_its_target` in `crates/burrow-core/tests/ssh.rs` failed once in 25 full workspace runs after PR #10 with "could not listen on 127.0.0.1:20019: Address already in use", a port from the allocator in `crates/burrow-core/tests/common/mod.rs`. Not reproduced in 22 further runs, 10 of them logging every port probe with `ss`. Only worth time if CI shows it.

## Done

- [x] ~~M15: containers and PM2~~ (2026-10-26, branch m15-containers-pm2)
- [x] ~~M14: edit remote files~~ (2026-10-26, PR #21 merged)
- [x] ~~M13: terminal essentials~~ (2026-10-26, PR #20 merged)

- [x] ~~M12: architecture and UX review~~ (2026-10-26, PR #19 merged, with the context-menu hang fix in PR #18)
- [x] ~~`stop()` probe can turn away a starting daemon~~ (2026-10-25, branch m12-core-2: a starting daemon retries the lock for about a second)
- [x] ~~Install script with daily updates from main, and an icon~~ (2026-10-25, PR #16 merged)
- [x] ~~`burrow daemon stop` under an open window~~ (2026-10-25, PR #14 merged)
- [x] ~~M10: hosts added in Burrow~~ (2026-10-25, PR #13 merged)
- [x] ~~Review after M9, `burrow doctor`, start-up failure dialog, Arch package, review fixes~~ (2026-10-25, PR #10 merged)
