#!/usr/bin/env python3
"""Disposable nested Wayland test. Captures only its own empty fixture desktop.

Run after `cargo build -p rix-capture`; needs a rebuilt rix-comp with the gated
nested test-input socket. The user's compositor/network/session is untouched.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[3]


def request(path, value):
    if isinstance(value, str):
        value = {"type": value}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(5)
        stream.connect(str(path))
        stream.sendall(json.dumps(value).encode() + b"\n")
        data = b""
        while b"\n" not in data:
            chunk = stream.recv(65536)
            if not chunk or len(data) > 2_000_000:
                raise RuntimeError("Fixture response exceeded bounds")
            data += chunk
    response = json.loads(data.split(b"\n", 1)[0])
    if response.get("status") == "error" or response.get("ok") is False:
        raise RuntimeError(response)
    return response


def wait(condition, label, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = condition()
        if result:
            return result
        time.sleep(0.04)
    raise RuntimeError(f"Timed out: {label}")


def fixture(args):
    base = Path(args.fixture)
    assert os.environ.get("RIX_FILES_PRIVATE_FIXTURE") == str(base)
    assert os.environ.get("DBUS_SESSION_BUS_ADDRESS")
    output = Path(args.output)
    ipc = base / "runtime/rix-ipc.sock"
    seat = base / "runtime/input.sock"
    env = os.environ.copy()
    # Nested shell services must not reach the user's real NetworkManager,
    # UPower, or login manager. The disposable bus deliberately has no system
    # services; the shell can exercise its unavailable-service state safely.
    env["DBUS_SYSTEM_BUS_ADDRESS"] = env["DBUS_SESSION_BUS_ADDRESS"]
    processes = []
    logs = []

    def launch(command, name, environment):
        log = (output / f"{name}.log").open("w")
        logs.append(log)
        process = subprocess.Popen(command, env=environment, stdout=log, stderr=log, start_new_session=True)
        processes.append(process)
        return process

    def event(kind, **fields):
        return request(seat, {"type": kind, **fields})

    def key(code):
        event("key", code=code, pressed=True)
        time.sleep(0.03)
        event("key", code=code, pressed=False)

    def drag(a, b):
        event("motion", x=a[0], y=a[1])
        time.sleep(0.15)
        event("button", code=272, pressed=True)
        time.sleep(0.04)
        event("motion", x=b[0], y=b[1])
        time.sleep(0.04)
        event("button", code=272, pressed=False)

    def windows():
        return request(ipc, "list_windows")["data"]

    def editors():
        return [w for w in windows() if w["app_id"] == "rix-capture"]

    def clipboard_png(name, rect, source=None):
        probes = sorted((REPO / "target/debug/deps").glob("native_clipboard-*"), key=lambda p: p.stat().st_mtime, reverse=True)
        binary = next((p for p in probes if p.is_file() and os.access(p, os.X_OK)), None)
        if not binary:
            raise RuntimeError("Build cargo test -p rix-capture --test native_clipboard --no-run first")
        probe_env = dict(env, RIX_CAPTURE_ISOLATED_WAYLAND="1", RIX_CAPTURE_EXPECTED_RECT=rect, RIX_CAPTURE_CLIPBOARD_PNG=str(output / name))
        if source:
            probe_env["RIX_CAPTURE_EXPECTED_SOURCE"] = str(source)
        result = subprocess.run([binary, "--ignored", "--exact", "capture_png_reaches_an_external_wayland_client_without_resampling", "--nocapture"], env=probe_env, timeout=10, capture_output=True, text=True)
        (output / f"{name}.log").write_text(result.stdout + result.stderr)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return output / name

    def capture_image(name):
        path = output / name
        request(ipc, {"type": "screenshot", "payload": {"path": str(path)}})
        wait(path.is_file, "synthetic screenshot file")

    def selector_ready(offset):
        # A newly spawned GPU selector may take more than half a second to
        # configure. A key pressed before its first buffer reaches the seat
        # arrives as held state on keyboard.enter, not a key-pressed event.
        # Wait for the actual child layer's first attach instead of racing it.
        transcript = (output / "capture.log").read_text()[offset:]
        surface = re.search(r"get_layer_surface\([^\n]*wl_surface#(\d+)", transcript)
        return surface and f"wl_surface#{surface.group(1)}.attach(wl_buffer" in transcript

    try:
        compositor = launch([args.compositor, "--nested", "--socket", "rix-capture-fixture"], "compositor", env)
        wait(lambda: ipc.is_socket() and seat.is_socket() and (base / "runtime/rix-capture-fixture").is_socket(), "private compositor")
        env["WAYLAND_DISPLAY"] = "rix-capture-fixture"
        time.sleep(1.2)  # Initial parent-window configure and panel mapping settle.
        # A stalled DE IPC endpoint must not delay the first editor buffer.
        # Theme updates run in the background; the initial theme comes from the
        # fixture's saved configuration. The old startup waited five seconds.
        stalled_path = base / "runtime/stalled-theme.sock"
        stalled = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        stalled.bind(str(stalled_path))
        stalled.listen(8)
        stalled.settimeout(0.1)
        done = threading.Event()
        held = []
        def hold_connections():
            while not done.is_set():
                try:
                    connection, _ = stalled.accept()
                    held.append(connection)
                except socket.timeout:
                    pass
        thread = threading.Thread(target=hold_connections, daemon=True)
        thread.start()
        try:
            started = time.monotonic()
            early = launch([args.capture, "--editor"], "startup-stalled-ipc",
                           dict(env, RIX_IPC_SOCKET=str(stalled_path)))
            window = wait(lambda: next(iter(editors()), None), "editor first buffer with stalled IPC", timeout=3)
            elapsed = time.monotonic() - started
            assert elapsed < 3, f"Startup blocked for {elapsed:.2f}s"
            assert early.poll() is None and not window["is_minimized"]
            (output / "startup.json").write_text(json.dumps({"stalled_ipc_first_buffer_seconds": elapsed,
                                                              "local_theme": True, "mapped": True}, indent=2))
            early.terminate()
            early.wait(timeout=3)
            wait(lambda: not editors(), "startup fixture closed")
        finally:
            done.set()
            thread.join(timeout=1)
            stalled.close()
            for connection in held:
                connection.close()
        # There are no host applications inside this compositor. This captures
        # its empty desktop and exercises the actual layer-shell selector.
        capture_image("source-desktop.png")
        capture_env = dict(env, WAYLAND_DEBUG="client")
        helper = launch([args.capture, "--area"], "capture", capture_env)
        wait(lambda: selector_ready(0), "initial selector first buffer")
        assert helper.poll() is None
        assert not editors(), "Selecting is an overlay, never a taskbar application"
        capture_image("capture-selector.png")
        drag((250, 210), (920, 610))
        editor = wait(lambda: next(iter(editors()), None), "normal editor after release")
        assert not editor["is_fullscreen"] and not editor["is_maximized"]
        control = base / f"runtime/rix-capture-{helper.pid}.sock"
        wait(control.is_socket, "same PID editor control socket")
        assert control.stat().st_mode & 0o777 == 0o600
        assert helper.poll() is None, "Selector exec must preserve the supervised PID"
        initial_png = clipboard_png("native-clipboard-initial.png", "250,210,670,400", output / "source-desktop.png")
        geometry = next(w for w in event("windows")["windows"] if w["id"] == editor["id"])
        assert geometry["width"] < 1280 and geometry["height"] <= 800
        capture_image("capture-editor-window.png")
        # Drag the server decoration; the editor must behave like an ordinary
        # application rather than a compositor overlay with forced geometry.
        # Fixture geometry is the client body. The server titlebar is above
        # it, so a positive body-relative y would merely click our own header.
        drag((geometry["x"] + 140, geometry["y"] - 15), (geometry["x"] + 200, geometry["y"] + 15))
        moved = wait(lambda: next((w for w in event("windows")["windows"] if w["id"] == editor["id"] and (w["x"], w["y"]) != (geometry["x"], geometry["y"])), None), "decorated editor movement")
        old_clipboard = initial_png.read_bytes()
        transcript_offset = len((output / "capture.log").read_text())
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control_client:
            control_client.sendto(b"area", str(control))
        wait(lambda: not editors(), "editor unmap before new capture")
        wait(lambda: selector_ready(transcript_offset), "cancel selector first buffer")
        key(1)
        restored = wait(lambda: next(iter(editors()), None), "cancel restores existing document")
        assert helper.poll() is None
        unchanged_png = clipboard_png("native-clipboard-cancel.png", "250,210,670,400")
        assert unchanged_png.read_bytes() == old_clipboard, "Escape may not change the clipboard"
        transcript_offset = len((output / "capture.log").read_text())
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control_client:
            control_client.sendto(b"panel", str(control))
        wait(lambda: not editors(), "forwarded next capture")
        wait(lambda: selector_ready(transcript_offset), "recapture selector first buffer")
        drag((310, 230), (870, 570))
        restored = wait(lambda: next(iter(editors()), None), "new selection returns to same editor process")
        assert helper.poll() is None and control.is_socket()
        assert not restored["is_fullscreen"]
        clipboard_png("native-clipboard-recapture.png", "310,230,560,340", output / "source-desktop.png")
        capture_image("capture-editor-recapture.png")
        request(ipc, {"type": "close_window", "payload": restored["id"]})
        wait(lambda: helper.poll() is not None, "ordinary decoration Close exits editor")
        wait(lambda: not control.exists(), "control cleanup")
        (output / "result.json").write_text(json.dumps({"initial": editor, "geometry": geometry, "moved": moved, "restored": restored, "same_pid": helper.pid}, indent=2))
        print("PASS: area overlay → copied native PNG → normal decorated window; same PID; movement; new capture unmap; cancellation preserves clipboard; control forwarding; cleanup", flush=True)
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        for log in logs:
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compositor", default=str(REPO / "target/debug/rix-comp"))
    parser.add_argument("--capture", default=str(REPO / "target/debug/rix-capture"))
    parser.add_argument("--output", default="/tmp/rix-capture-wayland-artifacts")
    parser.add_argument("--fixture", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.fixture:
        fixture(args)
        return
    parent = os.environ.get("WAYLAND_DISPLAY")
    if not parent:
        parser.error("A parent Wayland session is required")
    if not os.path.isabs(parent):
        parent = str(Path(os.environ["XDG_RUNTIME_DIR"]) / parent)
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rix-capture-wayland-") as temporary:
        base = Path(temporary)
        env = os.environ.copy()
        for name, directory in [("HOME", "home"), ("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"), ("XDG_CONFIG_DIRS", "system-config")]:
            path = base / directory
            path.mkdir(mode=0o700)
            env[name] = str(path)
        for name in ["DISPLAY", "WAYLAND_SOCKET", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS", "RIX_THEME_RESOURCES_READY", "RIX_THEME_RESOURCES_DATA", "XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]:
            env.pop(name, None)
        env.update(WAYLAND_DISPLAY=parent, RIX_IPC_SOCKET=str(base / "runtime/rix-ipc.sock"), RIX_NESTED_TEST_INPUT_SOCKET=str(base / "runtime/input.sock"), RIX_FILES_PRIVATE_FIXTURE=str(base), XDG_CURRENT_DESKTOP="RIX", XDG_SESSION_TYPE="wayland", XDG_DATA_DIRS=f"{REPO / 'assets'}:/usr/share", RUST_LOG="info", LANG="ru_RU.UTF-8")
        config = base / "config/rix"
        config.mkdir()
        (config / "display.json").write_text(json.dumps({"width": 1280, "height": 800, "refresh_hz": 60, "flipped": False}))
        (config / "desktop.json").write_text(json.dumps({"language": "en", "autostart": []}))
        (config / "theme.json").write_bytes((REPO / "crates/rix-config/themes/plasma-glass.json").read_bytes())
        command = ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()), "--fixture", str(base), "--compositor", args.compositor, "--capture", args.capture, "--output", str(output)]
        result = subprocess.run(command, env=env, timeout=100)
        raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
