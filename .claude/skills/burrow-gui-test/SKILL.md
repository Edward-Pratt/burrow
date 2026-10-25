---
name: burrow-gui-test
description: Run and drive burrow-gtk on a headless X display to check a change in the real app, including SSH terminals, tunnels and the file panel against a throwaway local sshd or (with permission) a real server. Use when asked to test, run, try or screenshot the GTK client, or to confirm a UI change works.
---

# Testing burrow-gtk headless

The app runs on Xvfb `:99`, never on the user's desktop. You read it over AT-SPI, act on it with AT-SPI or XTest, and look at it with screenshots.

Scripts are in `.claude/skills/burrow-gui-test/scripts/`. Set `BURROW_TEST_DIR` to a folder in your scratchpad first. Every Bash call starts fresh, so export it each time, or prefix each command with it.

```sh
export BURROW_TEST_DIR=<scratchpad>/rig
R=.claude/skills/burrow-gui-test/scripts
$R/rig.sh start --sshd        # Xvfb + test sshd, alias "testbox"
# write node files into $BURROW_TEST_DIR/cfg/tree/ (format in docs/API.md, "On-disk layout")
$R/rig.sh app                 # builds and (re)starts burrow-gtk
python3 -I $R/ui.py dump      # what is on screen
$R/rig.sh shot s1             # then Read the png
$R/rig.sh stop                # always, at the end
```

## Driving it

- **Buttons and menu items:** `ui.py click "Label"`. Use the `N` argument to pick among same-named widgets. Labels end with "…" where the UI shows one.
- **Text fields:** `ui.py set N TEXT`, where N counts visible fields top to bottom. In a dialog, the main window's fields come first: in the file panel, 0 is the path bar.
- **Sidebar and file rows:** have no AT-SPI action. Double-click them with `x.py dbl:X,Y`, or `ui.py select LABEL` then use the toolbar. A single click only selects a file-list row.
- **Keys:** `x.py Return`, `x.py ctrl+l`. Input goes to the window under the pointer, so click into the target first (`x.py click:X,Y Return`).
- **Context menu:** `x.py right:X,Y`, then `ui.py click "Browse files"`.
- **Dragging:** `x.py drag:X1,Y1,X2,Y2` presses, moves in steps and releases. Dropping a sidebar row on a folder row moves it there; below the rows moves it to the top level.
- **Dialog buttons named like the dialog:** the dialog's frame matches first, so use `ui.py click Rename 1` or `ui.py click Move 1`.
- **File chooser:** `x.py ctrl+l`, `ui.py set 1 /abs/path/`, then click the accept button ("Upload", "Download here").
- **Layout:** the main window sits at (0,0), 1100×720. Sidebar rows are at y=78, 124, 170 and so on (46 px apart), x≈100; folder arrows at x≈27 plus 14 px per level. Over AT-SPI each row is a `button` named after the node. The file panel's path bar is at y≈129, and its first row at y≈208 with 23 px per row. Take a screenshot when unsure, because layouts shift: a warning line moves the buttons below it down.

## Rules

- **Never send input to the real display.** AT-SPI's `generate_keyboard_event` types into whatever window the user has focused. Use `x.py`, which only talks to `:99`.
- **Never `pkill -f` a pattern** like `burrow-gtk` or `Xvfb :99`: it can match the shell running the command and kill it (exit 143/144). `rig.sh` tracks processes by pid file, and `pgrep -x` is safe.
- **The test sshd logs in as the user.** SFTP starts in `$BURROW_TEST_DIR/home`, so the file panel opens there and a monitor installs burrow-agent under it. Terminals and `exec` still start in the real home folder. Only write inside `$BURROW_TEST_DIR`.
- **A real server needs the user's go-ahead** each time. Agree a scratch folder on it, delete the folder afterwards, and run `rig.sh app --real-ssh`, which uses `~/.ssh/config` and the agent at `/run/user/$UID/ssh-agent.socket`. Burrow can't answer passphrase or host-key prompts for you: the key must already be in the agent, and the host already in `known_hosts`.
- **Check results outside the UI when you can:** probe a tunnel's local port with Python sockets, compare `sha256sum` on both ends of a transfer, and read `$BURROW_TEST_DIR/state/autostart.toml`.
- **Clean up:** `rig.sh stop`, delete files you made on any server, and confirm with `ss -ltn` that no test ports are left listening.

## Known quirks

- The app runs as a client of its own `burrowd`, with its socket in `/run/user/$UID/burrow-rig`. `rig.sh app` restarts it too, so a rebuilt core takes effect, and `rig.sh stop` stops it. To drive the same daemon from the CLI, export the same `BURROW_*` variables the rig sets, including `BURROW_RUNTIME_DIR`.

- Icons not in the Adwaita symbolic set show a broken image. Check the name exists under `/usr/share/icons/Adwaita/symbolic/`.
- A `GtkSpinButton` only takes typed text on `update()`. Read values after calling it.
- `GTK_DEBUG`/`GDK_DEBUG=no-portals` is set by `rig.sh`. Without it, `GtkFileDialog` opens through the desktop portal, on the user's screen.
