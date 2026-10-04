#!/usr/bin/env python3
"""Native Linux/X11 smoke test; run under Xvfb with python3-xlib installed.

Only built-in demo data is opened. No credentials, user's SSH configuration,
remote session, or existing windows are touched. Unit tests cover exact layout
and selection semantics; this test covers winit, Blade, clipboard, and closure.
"""
import os
import pathlib
import struct
import subprocess
import time
import zlib

from Xlib import X, XK, display, protocol
from Xlib.ext import xtest

ROOT = pathlib.Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / "target/test-artifacts"
ARTIFACTS.mkdir(parents=True, exist_ok=True)
BINARY = pathlib.Path(os.environ.get("STARCOM_BINARY", ROOT / "target/debug/starcom"))


def wait_until(predicate, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Starcom exited early: {process.returncode}")
        value = predicate()
        if value:
            return value
        time.sleep(0.03)
    raise TimeoutError("desktop operation did not complete")


def pixels(window):
    size = window.get_geometry()
    image = window.get_image(0, 0, size.width, size.height, X.ZPixmap, 0xFFFFFFFF)
    return size.width, size.height, image.data


def save_png(window, path):
    width, height, data = pixels(window)
    # Xvfb is launched with a 24-bit, little-endian TrueColor screen, stored in
    # 32-bit BGRX pixels. Keep this assumption explicit rather than guessing.
    assert len(data) == width * height * 4, "unexpected Xvfb pixel format"
    raw = bytearray()
    for row in range(height):
        raw.append(0)
        for col in range(width):
            i = (row * width + col) * 4
            raw.extend((data[i + 2], data[i + 1], data[i]))

    def chunk(kind, content):
        return struct.pack(">I", len(content)) + kind + content + struct.pack(">I", zlib.crc32(kind + content))

    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def find_demo_word(window):
    """Locate the first cyan terminal word without assuming sidebar geometry."""
    width, height, data = pixels(window)
    rows = []
    # Cyan "Starcom" is the short demo title near the pane's top edge. The
    # sidebar redesign removed the row that used to sit above it, so do not
    # impose a fixed top inset. Shape checks below reject pane/sidebar borders;
    # the width cap keeps a later cyan instruction sentence from winning.
    for y in range(0, min(height, 500)):
        matches = []
        for x in range(width // 2):
            i = (y * width + x) * 4
            blue, green, red = data[i], data[i + 1], data[i + 2]
            if blue >= 140 and green >= 130 and red <= 120 and blue - red >= 40:
                matches.append(x)
        if len(matches) >= 4:
            rows.append((y, min(matches), max(matches), len(matches)))

    bands = []
    for row in rows:
        if not bands or row[0] != bands[-1][-1][0] + 1:
            bands.append([row])
        else:
            bands[-1].append(row)
    band = next(
        (
            band
            for band in bands
            if 4 <= len(band) <= 24
            and sum(row[3] for row in band) >= 16
            and 24 <= max(row[2] for row in band) - min(row[1] for row in band) <= 160
        ),
        None,
    )
    if band is None:
        raise RuntimeError("could not locate cyan demo text")
    x0 = min(row[1] for row in band)
    x1 = max(row[2] for row in band)
    y = (band[0][0] + band[-1][0]) // 2
    return max(0, x0 - 1), y, min(width - 1, x1 + 2)


def move(x, y):
    xtest.fake_input(d, X.MotionNotify, x=x, y=y)
    d.sync()
    time.sleep(0.1)


def mouse(kind):
    xtest.fake_input(d, kind, detail=1)
    d.sync()
    time.sleep(0.1)


with (ARTIFACTS / "desktop.log").open("w") as log:
    process = subprocess.Popen([str(BINARY), "--demo"], cwd=ROOT, stdout=log, stderr=log)
    d = display.Display()
    window = None
    try:
        root = d.screen().root

        def find_window():
            return next((w for w in root.query_tree().children if w.get_wm_name() == "Starcom"), None)

        window = wait_until(find_window)
        window.set_input_focus(X.RevertToParent, X.CurrentTime)
        d.sync()
        # Wait for a populated frame, not just a mapped X11 window.
        wait_until(lambda: len(set(pixels(window)[2])) > 100)
        time.sleep(0.3)
        save_png(window, ARTIFACTS / "desktop-native.png")
        origin = root.translate_coords(window, 0, 0)
        # The demo title says "Starcom". Locate the rendered word so tab chrome
        # or toolbar changes do not invalidate the native test.
        start_x, row_y, end_x = find_demo_word(window)
        move(origin.x + start_x, origin.y + row_y)
        mouse(X.ButtonPress)
        move(origin.x + end_x, origin.y + row_y)
        mouse(X.ButtonRelease)
        # Plain Ctrl-C is a terminal interrupt. The local clipboard shortcut is
        # Ctrl-Shift-C (Cmd-C on macOS, not exercised by this Linux/X11 test).
        ctrl = d.keysym_to_keycode(XK.string_to_keysym("Control_L"))
        shift = d.keysym_to_keycode(XK.string_to_keysym("Shift_L"))
        c = d.keysym_to_keycode(XK.string_to_keysym("c"))
        for kind, code in [
            (X.KeyPress, ctrl),
            (X.KeyPress, shift),
            (X.KeyPress, c),
            (X.KeyRelease, c),
            (X.KeyRelease, shift),
            (X.KeyRelease, ctrl),
        ]:
            xtest.fake_input(d, kind, detail=code)
        d.sync()
        time.sleep(0.2)
        clipboard = d.intern_atom("CLIPBOARD")
        utf8 = d.intern_atom("UTF8_STRING")
        prop = d.intern_atom("STARCOM_SMOKE_CLIPBOARD")
        receiver = root.create_window(0, 0, 1, 1, 0, X.CopyFromParent)
        receiver.convert_selection(clipboard, utf8, prop, X.CurrentTime)
        d.sync()

        def copied():
            while d.pending_events():
                event = d.next_event()
                if event.type == X.SelectionNotify and event.property:
                    return receiver.get_full_property(prop, X.AnyPropertyType).value
            return None

        text = bytes(wait_until(copied)).decode("utf-8")
        assert text == "Starcom", f"unexpected clipboard selection: {text!r}"
        receiver.destroy()
        window.configure(width=1000, height=620)
        d.sync()
        wait_until(lambda: window.get_geometry().width == 1000)
        time.sleep(0.4)
        save_png(window, ARTIFACTS / "desktop-resized.png")
        # Close through the normal window-manager message, not a signal. This
        # reaches the ordered Blade/winit teardown that previously crashed.
        window.send_event(protocol.event.ClientMessage(
            window=window, client_type=d.intern_atom("WM_PROTOCOLS"),
            data=(32, [d.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0])), event_mask=0)
        d.sync()
        assert process.wait(timeout=5) == 0, "window closure failed"
        print("Desktop smoke passed: native render, system clipboard, resize, and clean close.")
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        d.close()
