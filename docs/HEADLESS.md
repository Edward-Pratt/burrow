# Burrow on a computer that stays on

The phone app is a client of `burrowd`, over SSH (see [Mobile client](DESIGN.md#mobile-client)), so it needs a computer where the daemon runs all the time and monitors hosts in the background. This is how `warren` is set up. Any Linux computer with systemd and sshd that the phone can reach works the same way.

## Install

Install as on a desktop, with `dist/install.sh`. It installs the window too, and checks it links, so the GTK libraries must be there even if nothing will show it. It ends with `burrow doctor`.

## Keep the daemon running

```sh
sudo loginctl enable-linger $USER    # user services run from boot, without a login
systemctl --user enable --now burrowd.socket burrowd.service
```

The service starts the daemon at boot, so background monitors run from then on. The daemon stays up while it monitors any host, and with none it stops after a minute; the socket starts it again for the next client.

Before enabling them, stop any daemon a client started, or let it stop by itself: `$XDG_RUNTIME_DIR/burrow/burrowd.lock` holds its pid while it runs. When it exits it removes its socket, which by then would be the one systemd listens on.

## A Secret Service

The daemon doesn't start without one, and at boot nobody is there to type a keyring password. On `warren` the login keyring of gnome-keyring has an empty password, which the encrypted disk covers; any process running as the user could read an unlocked keyring anyway. After a reboot, `journalctl --user -u burrowd` shows whether the daemon started.

## Keys for the monitors

A background monitor has nobody to answer a passphrase, so its key has none, and anything running as the user on this computer can use it. Limit what it can do on each server:

- **A server:** log in as a user without sudo, with `restrict` (no terminal, no forwarding) on the key's line. Monitoring only reads `/proc`, the disks and `systemctl`, so it works; service actions need sudo, so they don't.

  ```sh
  sudo useradd --create-home --shell /bin/bash burrow-mon    # its password stays locked
  sudo install -d -m 700 -o burrow-mon -g burrow-mon ~burrow-mon/.ssh
  echo "restrict ssh-ed25519 AAAA… burrow-monitor@warren" |
    sudo install -m 600 -o burrow-mon -g burrow-mon /dev/stdin ~burrow-mon/.ssh/authorized_keys
  sudo restorecon -R ~burrow-mon/.ssh                           # with SELinux
  ```

- **This computer:** monitor it over SSH to `127.0.0.1` as yourself, with `from="127.0.0.1,::1",restrict` on the key's line in your own `authorized_keys`. That gives nothing a process running as you doesn't have already.

Each host gets a block in `~/.ssh/config` naming the key, with `IdentitiesOnly yes`. Take each server's host keys from a computer that already trusts it (`ssh-keygen -F <address>`), rather than accepting whatever answers the first time. For `127.0.0.1`, they are in `/etc/ssh/ssh_host_*_key.pub`.

## Turn on background monitoring

Turning it on, the agent-install consent and the first prompts are answered in Burrow's window, once. On a computer with no screen, run `burrow-gtk` in a remote desktop: on `warren`, its Hyprland session's headless output, served by wayvnc. For each host:

1. **New**, **SSH host**, and **Use a host already in ~/.ssh/config**.
2. Open its monitor, and allow the agent install.
3. Tick **Monitor in the background**.

`burrow call '{"type": "list_sessions"}'` then shows one session with `background: true` for each host.

## Claude on this computer

```sh
claude mcp add --scope user burrow -- ~/.local/bin/burrow mcp serve
burrow mcp grant oracle monitor-read
```

The path is absolute because `~/.local/bin` may not be on the `PATH` Claude starts it with. Grant only what reads (`monitor-read`, `sql-read`) here, and keep write grants on the desktop.

## Updating

The daily timer only checks, and says so in a desktop notification nobody may see, so update over SSH. A daemon with background monitors keeps running its old code, because a new client only replaces a daemon with nothing running in it, so restart it:

```sh
burrow-update
systemctl --user restart burrowd.service
```

The monitors reconnect, and their history on disk stays.

## Check the phone's way in

Before a phone has a key, check the path with a throwaway one: add its line with the `command="…/burrow proxy --role mobile",…` options from [Over SSH](API.md#over-ssh), send `initialize` with `"role": "mobile"` and then `execute` lines, and remove the line afterwards. The answer to `initialize` names the daemon's build, and `list_sessions` shows the background monitors. Asking for another role, or for a shell or a terminal, must be refused.

Connect with `ssh -F /dev/null -o IdentityAgent=none -o IdentitiesOnly=yes -i <key>`. Otherwise ssh can offer a key from your agent or your config first, sign in with that, and give a shell that looks like the forced command failed.
