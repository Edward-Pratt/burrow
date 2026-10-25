#!/usr/bin/env python3
"""Send keys and clicks to Xvfb :99 only, through XTest.

  x.py ARG...   each ARG is one of:
    Return, BackSpace, ctrl+l, ctrl+shift+p   a key or combo (X keysym names)
    click:X,Y  dbl:X,Y  right:X,Y              a mouse click at screen pixels
    drag:X1,Y1,X2,Y2                           press, move in steps, release

Input goes to the window under the pointer. It never touches the real
display, so it can't type into the user's own windows.
"""
import ctypes
import sys
import time

x11 = ctypes.CDLL("libX11.so.6")
xtst = ctypes.CDLL("libXtst.so.6")
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XStringToKeysym.restype = ctypes.c_ulong
display = ctypes.c_void_p(x11.XOpenDisplay(b":99"))
if not display.value:
    sys.exit("can't open display :99; run rig.sh start")
MODIFIERS = {"ctrl": "Control_L", "shift": "Shift_L", "alt": "Alt_L"}


def keycode(name):
    sym = x11.XStringToKeysym(MODIFIERS.get(name, name).encode())
    return x11.XKeysymToKeycode(display, ctypes.c_ulong(sym))


if len(sys.argv) < 2:
    print(__doc__)
    sys.exit(2)
def move(px, py):
    xtst.XTestFakeMotionEvent(display, 0, px, py, 0)
    x11.XFlush(display)


for arg in sys.argv[1:]:
    kind, _, at = arg.partition(":")
    if kind == "drag" and at:
        x1, y1, x2, y2 = map(int, at.split(","))
        move(x1, y1)
        time.sleep(0.05)
        xtst.XTestFakeButtonEvent(display, 1, 1, 0)
        x11.XFlush(display)
        time.sleep(0.1)
        # In steps, as a hand would: GTK starts a drag only once the
        # pointer has moved a few pixels with the button down.
        steps = 20
        for i in range(1, steps + 1):
            move(x1 + (x2 - x1) * i // steps, y1 + (y2 - y1) * i // steps)
            time.sleep(0.03)
        time.sleep(0.3)
        xtst.XTestFakeButtonEvent(display, 1, 0, 0)
        x11.XFlush(display)
    elif kind in ("click", "dbl", "right") and at:
        px, py = map(int, at.split(","))
        xtst.XTestFakeMotionEvent(display, 0, px, py, 0)
        x11.XFlush(display)
        time.sleep(0.05)
        button = 3 if kind == "right" else 1
        for _ in range(2 if kind == "dbl" else 1):
            xtst.XTestFakeButtonEvent(display, button, 1, 0)
            xtst.XTestFakeButtonEvent(display, button, 0, 0)
            x11.XFlush(display)
            time.sleep(0.05)
    else:
        codes = [keycode(part) for part in arg.split("+")]
        for code in codes:
            xtst.XTestFakeKeyEvent(display, code, 1, 0)
        for code in reversed(codes):
            xtst.XTestFakeKeyEvent(display, code, 0, 0)
        x11.XFlush(display)
    time.sleep(0.25)
