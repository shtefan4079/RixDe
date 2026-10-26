#!/usr/bin/env python3
"""Exercise native Ctrl shortcuts on a private Wayland seat in US/RU/UA layouts.

Build with: cargo test -p rix-files --test ui --no-run
Run with: python3 crates/rix-files/tests/keyboard_wayland.py --ui-test target/debug/deps/ui-HASH
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts/tests"))
from files_wayland import NativeInput, request, stop, wait_for


def inner(args):
    base = Path(os.environ["RIX_KEYBOARD_FIXTURE"])
    output = args.output.resolve()
    report = output / "state.json"
    report.unlink(missing_ok=True)
    ipc = base / "runtime/rix-ipc.sock"
    native = NativeInput(base / "runtime/input.sock")
    processes = []
    logs = []

    def launch(command, name, env):
        log = (output / f"{name}.log").open("w")
        logs.append(log)
        process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, cwd=REPO)
        processes.append(process)
        return process

    def state():
        return json.loads(report.read_text())

    def expect(check, description):
        wait_for(lambda: report.exists() and check(state()), 5, description, [("Fixture", process) for process in processes])

    observations = []
    try:
        env = dict(os.environ)
        display = "rix-keyboard-fixture"
        launch([args.compositor, "--nested", "--socket", display], "compositor", env)
        wait_for(lambda: ipc.is_socket() and native.path.is_socket() and (base / "runtime" / display).is_socket(), 20, "private compositor", [("Compositor", processes[0])])
        layout = request(ipc, "get_layout")["data"]
        layout["keyboard"]["layouts"] = [{"layout": code, "variant": "", "label": code.upper()} for code in ["us", "de", "fr"]]
        request(ipc, {"type": "set_layout", "payload": layout})
        env.update(WAYLAND_DISPLAY=display, RIX_FILES_KEYBOARD_REPORT=str(report))
        launch([args.ui_test, "--ignored", "--exact", "native_keyboard_shortcut_probe", "--nocapture"], "client", env)
        expect(lambda value: True, "keyboard client")
        wait_for(lambda: any(window["app_id"] == "org.rixde.Files.KeyboardFixture" for window in request(ipc, "list_windows")["data"]), 10, "mapped client")
        window = next(window for window in request(ipc, "list_windows")["data"] if window["app_id"] == "org.rixde.Files.KeyboardFixture")
        request(ipc, {"type": "focus_window", "payload": window["id"]})
        for group, code, ordinary in [(0, "us", "a"), (1, "de", "a"), (2, "fr", "q")]:
            request(ipc, {"type": "set_keyboard_group", "payload": group})
            before = len(state()["shortcuts"])
            native.key(30, (29,))  # Ctrl+A on the file view
            expect(lambda value: value["selected"] == 3 and len(value["shortcuts"]) > before, f"{code} select-all")
            for key in [46, 45, 47]:
                native.key(key, (29,))  # Ctrl+C/X/V on files
            expect(lambda value: [item["text"] for item in value["shortcuts"][-3:]] == ["c", "x", "v"], f"{code} file clipboard shortcuts")
            native.key(44, (29,))  # Ctrl+Z reaches filesystem undo.
            expect(lambda value: value["shortcuts"][-1]["text"] == "z" and not value["shortcuts"][-1]["shift"], f"{code} file undo")
            native.key(44, (29, 42))  # Ctrl+Shift+Z reaches filesystem redo.
            expect(lambda value: value["shortcuts"][-1]["text"].lower() == "z" and value["shortcuts"][-1]["shift"], f"{code} file redo")
            native.key(21, (29,))
            expect(lambda value: value["shortcuts"][-1]["text"] == "y", f"{code} alternate redo")
            native.key(33, (29,))  # Ctrl+F
            expect(lambda value: value["shortcuts"][-1]["text"] == "f", f"{code} search focus")
            file_calls = len(state()["shortcuts"])
            native.key(30, (29,))  # Ctrl+A edits search, never selects files
            native.key(30)        # Ordinary typing follows the selected layout
            expect(lambda value: value["search"] == ordinary, f"{code} normal text")
            native.key(44, (29,))
            expect(lambda value: value["search"] == "", f"{code} editor undo")
            native.key(44, (29, 42))
            expect(lambda value: value["search"] == ordinary, f"{code} editor redo")
            native.key(30, (29,))  # Select the newly typed text for copy/cut.
            for key in [46, 45, 47]:
                native.key(key, (29,))
                if key == 45:
                    expect(lambda value: value["search"] == "", f"{code} text cut")
            expect(lambda value: value["search"] == ordinary, f"{code} text paste")
            assert len(state()["shortcuts"]) == file_calls, f"{code}: editor clipboard shortcuts escaped to files"
            native.key(1)  # Escape search
            native.key(38, (29,))  # Ctrl+L
            expect(lambda value: value["shortcuts"][-1]["text"] == "l", f"{code} location focus")
            native.key(30, (29,))
            native.key(30)
            expect(lambda value: value["location"] == ordinary, f"{code} location text selection")
            native.key(1)
            native.key(49, (29, 42))  # Ctrl+Shift+N
            expect(lambda value: value["dialog_kind"] == 1, f"{code} create folder")
            file_calls = len(state()["shortcuts"])
            native.key(30, (29,))
            native.key(30)
            expect(lambda value: value["dialog"] == ordinary, f"{code} modal text selection")
            assert len(state()["shortcuts"]) == file_calls, f"{code}: modal shortcut escaped"
            native.key(1)
            expect(lambda value: value["dialog_kind"] == 0, f"{code} modal dismissal")
            observations.append({"layout": code, "ordinary_text": ordinary, "passed": True})
        (output / "result.json").write_text(json.dumps({"passed": True, "cases": observations, "events": native.events}, indent=2))
        print(json.dumps({"passed": True, "cases": observations}), flush=True)
    finally:
        for process in reversed(processes):
            stop(process)
        for log in logs:
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ui-test", type=Path, required=True)
    parser.add_argument("--compositor", type=Path, default=REPO / "target/release/rix-comp")
    parser.add_argument("--output", type=Path, default=Path("/tmp/rix-files-keyboard-wayland"))
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.ui_test = args.ui_test.resolve()
    args.compositor = args.compositor.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if args.inside:
        inner(args)
        return
    parent = Path(os.environ["WAYLAND_DISPLAY"])
    if not parent.is_absolute():
        parent = Path(os.environ["XDG_RUNTIME_DIR"]) / parent
    with tempfile.TemporaryDirectory(prefix="rix-keyboard-fixture-") as temporary:
        base = Path(temporary)
        env = dict(os.environ)
        for variable, folder in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")]:
            directory = base / folder
            directory.mkdir(mode=0o700)
            env[variable] = str(directory)
        for variable in ["DISPLAY", "WAYLAND_SOCKET", "DBUS_SESSION_BUS_ADDRESS", "DBUS_STARTER_ADDRESS", "DBUS_STARTER_BUS_TYPE", "AT_SPI_BUS_ADDRESS", "XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]:
            env.pop(variable, None)
        env.update(WAYLAND_DISPLAY=str(parent), RIX_KEYBOARD_FIXTURE=str(base), RIX_FILES_PRIVATE_FIXTURE=str(base), RIX_IPC_SOCKET=str(base / "runtime/rix-ipc.sock"), RIX_NESTED_TEST_INPUT_SOCKET=str(base / "runtime/input.sock"), RIX_AUTOSTART="0", RIX_SESSION_BUS_ISOLATED="1", XDG_CURRENT_DESKTOP="RIX", RUST_LOG="info")
        command = ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()), "--inside", "--ui-test", str(args.ui_test), "--compositor", str(args.compositor), "--output", str(args.output)]
        subprocess.run(command, env=env, check=True, timeout=90)


if __name__ == "__main__":
    main()
