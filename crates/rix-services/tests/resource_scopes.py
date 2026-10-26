#!/usr/bin/env python3
"""Verify real application cgroups without changing the running desktop.

Requires Linux cgroup v2 and the current user's systemd manager.
Run: python3 crates/rix-services/tests/resource_scopes.py --binary target/release/rix-run
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

CLIENT = """import json, os, pathlib, resource, signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
pathlib.Path(sys.argv[1]).write_text(json.dumps(dict(
    nice=os.getpriority(os.PRIO_PROCESS, 0),
    oom=pathlib.Path('/proc/self/oom_score_adj').read_text().strip(),
    cgroup=pathlib.Path('/proc/self/cgroup').read_text().strip(),
    bus=os.environ['DBUS_SESSION_BUS_ADDRESS'],
    nice_limit=resource.getrlimit(resource.RLIMIT_NICE))))
time.sleep(20)
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    args = parser.parse_args()
    generation = str(900000000 + os.getpid())  # Outside Linux's real PID range.
    scopes = [f'app-rix{generation}-{kind}.scope' for kind in ['foreground', 'background']]
    units = scopes + [f'app-rix{generation}.slice']
    manager_env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=f'unix:path=/run/user/{os.getuid()}/bus')
    private_bus = 'unix:path=/tmp/rix-resource-private-bus-fixture'
    app_env = dict(os.environ, RIX_RESOURCE_POLICY='1', RIX_RESOURCE_GENERATION=generation,
                   DBUS_SESSION_BUS_ADDRESS=private_bus)
    children = []
    inherited_nice = os.getpriority(os.PRIO_PROCESS, 0)

    def properties(unit, *names):
        result = subprocess.check_output(['systemctl', '--user', 'show', unit] +
            [f'--property={name}' for name in names], env=manager_env, text=True, timeout=3)
        return dict(line.split('=', 1) for line in result.splitlines() if '=' in line)

    try:
        with tempfile.TemporaryDirectory(prefix='rix-resource-fixture-') as folder:
            observations = []
            for index, background in enumerate([False, False, True]):
                marker = Path(folder) / f'{index}.json'
                with (Path(folder) / f'{index}.stderr').open('w+') as errors:
                    child = subprocess.Popen([str(args.binary.resolve())] +
                        (['--background'] if background else []) +
                        ['--', sys.executable, '-c', CLIENT, str(marker)],
                        env=app_env, stderr=errors, stdout=subprocess.DEVNULL)
                    children.append(child)
                    deadline = time.monotonic() + 4
                    while not marker.exists() and time.monotonic() < deadline and child.poll() is None:
                        time.sleep(.02)
                    errors.flush()
                    errors.seek(0)
                    stderr = errors.read()
                assert marker.exists(), stderr
                data = json.loads(marker.read_text())
                scope = scopes[int(background)]
                assert data['cgroup'].endswith('/' + scope), (index, data, stderr)
                assert data['nice'] == max(inherited_nice, 10 if background else 0), data
                assert data['oom'] == '0' and data['nice_limit'] == [0, 0], data
                assert data['bus'] == private_bus, data
                observations.append(dict(background=background, nice=data['nice'], scope=scope))
            foreground = properties(scopes[0], 'CPUWeight', 'TimeoutStopUSec', 'SendSIGKILL')
            background = properties(scopes[1], 'CPUWeight', 'IOWeight', 'TimeoutStopUSec', 'SendSIGKILL')
            assert foreground['CPUWeight'] == '100' and background['CPUWeight'] == '25', (foreground, background)
            assert background['IOWeight'] == '25' and foreground['TimeoutStopUSec'] == '2s', (foreground, background)
            assert foreground['SendSIGKILL'] == background['SendSIGKILL'] == 'yes', (foreground, background)
            total = os.sysconf('SC_PHYS_PAGES') * os.sysconf('SC_PAGE_SIZE')
            budget = max(total - min(max(total // 10, 256 * 1024**2), 2 * 1024**3), total // 2)
            assert int(properties(units[2], 'MemoryHigh')['MemoryHigh']) == budget
            start = time.monotonic()
            result = subprocess.run(['systemctl', '--user', 'stop'] + units,
                env=manager_env, capture_output=True, text=True, timeout=8)
            assert result.returncode == 0, result.stderr
            for child in children:
                child.wait(timeout=1)
                assert child.returncode == -signal.SIGKILL, child.returncode
            print(json.dumps(dict(passed=True, applications=observations, memory_high=budget,
                stubborn_processes_killed=True, cleanup_seconds=round(time.monotonic() - start, 2))))
    finally:
        try:
            subprocess.run(['systemctl', '--user', 'stop'] + units, env=manager_env,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=8)
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                    child.wait()


if __name__ == '__main__':
    main()
