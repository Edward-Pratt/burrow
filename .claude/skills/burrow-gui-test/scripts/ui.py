#!/usr/bin/env python3
"""Read and drive burrow-gtk over AT-SPI.

  ui.py dump               visible widgets: [role] 'name' 'text' flags
  ui.py click NAME [N]     press the Nth visible widget named NAME (buttons, menu items)
  ui.py set N TEXT         replace the text of the Nth visible text field
  ui.py select LABEL       select the list row holding a label LABEL (sidebar, file list)

List rows have no AT-SPI action: select them here, or double-click them with x.py.
"""
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

EDITABLE = ("text", "entry", "password text", "spin button")


def app():
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        a = desktop.get_child_at_index(i)
        if a and "burrow" in (a.get_name() or "").lower():
            return a
    sys.exit("burrow-gtk is not running")


def walk(node, depth=0):
    yield node, depth
    try:
        count = node.get_child_count()
    except Exception:
        return
    for i in range(count):
        child = node.get_child_at_index(i)
        if child is not None:
            yield from walk(child, depth + 1)


def visible(node):
    return node.get_state_set().contains(Atspi.StateType.SHOWING)


def text_of(node):
    try:
        t = node.get_text_iface()
        return Atspi.Text.get_text(t, 0, t.get_character_count()) if t else ""
    except Exception:
        return ""


def flags(node):
    states = node.get_state_set()
    out = []
    if not states.contains(Atspi.StateType.SENSITIVE):
        out.append("insensitive")
    if states.contains(Atspi.StateType.CHECKED):
        out.append("checked")
    return " ".join(out)


def find(name, nth=0):
    hits = [
        n for n, _ in walk(app())
        if visible(n) and (n.get_name() == name or text_of(n) == name)
        and n.get_role_name() != "label"
    ]
    if len(hits) <= nth:
        sys.exit(f"no visible widget {name!r} #{nth}")
    return hits[nth]


cmd = sys.argv[1] if len(sys.argv) > 1 else ""
if cmd == "dump":
    for node, depth in walk(app()):
        role, name, text = node.get_role_name(), node.get_name() or "", text_of(node)
        if not visible(node) or (role in ("filler", "panel") and not name and not text):
            continue
        print("  " * depth + f"[{role}] {name!r} {text!r} {flags(node)}")
elif cmd == "click":
    node = find(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 0)
    action = node.get_action_iface()
    if not action or action.get_n_actions() == 0:
        sys.exit(f"{sys.argv[2]!r} has no action; click it with x.py")
    action.do_action(0)
elif cmd == "set":
    fields = [n for n, _ in walk(app()) if visible(n) and n.get_role_name() in EDITABLE]
    field = fields[int(sys.argv[2])]
    length = field.get_text_iface().get_character_count()
    editable = field.get_editable_text_iface()
    editable.delete_text(0, length)
    editable.insert_text(0, sys.argv[3], len(sys.argv[3]))
elif cmd == "select":
    for node, _ in walk(app()):
        if node.get_role_name() == "label" and node.get_name() == sys.argv[2] and visible(node):
            row = node
            while row and row.get_role_name() != "list item":
                row = row.get_parent()
            if row:
                row.get_parent().get_selection_iface().select_child(row.get_index_in_parent())
                break
    else:
        sys.exit(f"no row labelled {sys.argv[2]!r}")
else:
    print(__doc__)
    sys.exit(2)
time.sleep(0.3)
