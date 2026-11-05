#!/usr/bin/env python3
"""Exercise live application discovery, activation and magnified Dock input."""
import argparse
import ctypes as C
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

from integration_desktop import command, require
from session_smoke import Input
from smoke_nested import ROOT, start_xvfb, stop, tail


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--terminal-integration', action='store_true', help='also require and exercise an actual Xfce terminal on the private X display')
    parser.add_argument('--inside-bus', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--log-dir', type=Path, default=ROOT / 'target/launcher-dock-smoke-logs')
    args = parser.parse_args()
    if not args.inside_bus:
        # Services activated by the private bus must also use disposable settings.
        with tempfile.TemporaryDirectory(prefix='aura-launcher-bus-') as directory:
            bus_env = os.environ.copy()
            for variable, child in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state')]:
                path = Path(directory) / child
                path.mkdir()
                bus_env[variable] = str(path)
            return subprocess.run(['dbus-run-session', '--', sys.executable, str(Path(__file__).resolve()),
                                   *sys.argv[1:], '--inside-bus'], env=bus_env, check=False).returncode
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--workspace', '--bins', '--examples', '--locked'], cwd=ROOT, check=True)
    target = Path(json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT))['target_directory']) / 'debug'
    args.log_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith('AURA_CAPTURE_'):
            env.pop(key)
    env.update(LIBGL_ALWAYS_SOFTWARE='1', RUST_LOG='info', AURA_COMPOSITOR_GLASS='1')
    processes, outputs, logs = [], [], {}
    xvfb = session = input_ = None
    evidence = {}

    def launch(name, argv, extra=None):
        path = args.log_dir / (name + '.log')
        logs[name] = path
        output = path.open('w'); outputs.append(output)
        process = subprocess.Popen(argv, cwd=ROOT, env=env | (extra or {}), stdout=output, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    def until(predicate, label, timeout=12):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            require(session is None or session.poll() is None, 'Supervisor exited')
            result = predicate()
            if result:
                return result
            time.sleep(0.025)
        raise RuntimeError('Timed out: ' + label)

    try:
        xvfb = start_xvfb(env)
        with tempfile.TemporaryDirectory(prefix='aura-launcher-dock-') as directory:
            root = Path(directory); os.chmod(root, 0o700)
            data = root / 'data'; apps = data / 'applications'; apps.mkdir(parents=True)
            control = root / 'control.sock'
            recorder = root / 'record.py'
            recorder.write_text('import json,os,sys\nfrom pathlib import Path\n'
                                'Path(sys.argv[1]).write_text(json.dumps({"args":sys.argv[2:],"cwd":os.getcwd()}))\n')
            terminal = root / 'terminal-recorder'
            terminal.write_text('#!/usr/bin/python3\nimport json,os,sys\nfrom pathlib import Path\n'
                                f'Path({str(root / "terminal.json")!r}).write_text(json.dumps(sys.argv[1:]))\n'
                                'assert sys.argv[1]=="--"\nos.execvp(sys.argv[2],sys.argv[2:])\n')
            terminal.chmod(0o700)
            env.update(XDG_RUNTIME_DIR=directory, XDG_DATA_HOME=str(data), XDG_DATA_DIRS=str(data),
                       AURA_CONTROL_SOCKET=str(control), AURA_WINIT_RESOLUTION='1024x640', AURA_OUTPUT_SCALE='1',
                       AURA_TERMINAL=f'"{terminal}" --')

            def entry(filename, name, marker=None, extra=''):
                text = f'[Desktop Entry]\nType=Application\nName={name}\n'
                if marker is not None:
                    text += f'Exec=/usr/bin/python3 "{recorder}" "{marker}"\n'
                (apps / filename).write_text(text + extra)

            dock_marker = root / 'dock.json'
            entry('dock.desktop', 'Dock Probe', dock_marker)
            entry('other.desktop', 'Other Probe', root / 'other.json')
            entry('third.desktop', 'Third Probe', root / 'third.json')
            session = launch('session', [str(target / 'aura-session'), '--socket', 'wayland-launcher-dock', '--no-portal'])
            env['WAYLAND_DISPLAY'] = 'wayland-launcher-dock'

            def status():
                return json.loads(command(control, 'status')) if control.exists() else {}
            until(lambda: status().get('mapped_layers') == 2, 'shell buffers')
            time.sleep(0.8)
            input_ = Input(env['DISPLAY'])
            x = input_.x
            x.XGetImage.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_uint, C.c_uint, C.c_ulong, C.c_int]
            x.XGetImage.restype = C.c_void_p
            x.XGetPixel.argtypes = [C.c_void_p, C.c_int, C.c_int]; x.XGetPixel.restype = C.c_ulong
            x.XDestroyImage.argtypes = [C.c_void_p]

            def first_tile_top():
                image = x.XGetImage(input_.d, x.XDefaultRootWindow(input_.d), 250, 528, 1, 80, C.c_ulong(-1).value, 2)
                require(image, 'Cannot read actual Dock pixels')
                try:
                    return next((528 + y for y in range(80) if x.XGetPixel(image, 0, y) & 0xffffff == 0x28384c), None)
                finally:
                    x.XDestroyImage(image)

            def move(px, py):
                input_.t.XTestFakeMotionEvent(input_.d, 0, px, py, 0); x.XFlush(input_.d)

            move(400, 300)
            idle_top = until(first_tile_top, 'idle Dock pixels')
            move(266, 568)
            hover_top = until(lambda: (top if (top := first_tile_top()) is not None and top < idle_top else None), 'magnified Dock pixels')
            move(400, 300)
            until(lambda: first_tile_top() == idle_top, 'pointer leave restoring Dock')
            move(266, 568)
            until(lambda: first_tile_top() < idle_top, 'second hover')
            input_.click(300, 542)  # Outside the unmagnified first tile, inside the drawn enlarged tile.
            until(dock_marker.exists, 'clicking magnified app target')
            require(not (root / 'other.json').exists(), 'Dock clicked the neighboring app')
            move(400, 300); time.sleep(0.3)
            before = status()['rendered_frames']; time.sleep(0.3)
            require(status()['rendered_frames'] == before, 'Shell continuously redraws while idle')
            evidence['dock'] = {'idle_top': idle_top, 'hover_top': hover_top, 'leave_restored': True,
                                'enlarged_hit_target': 'correct app', 'idle_frames': before}

            def open_launcher(query, extra=None):
                launcher = launch('launcher-' + str(len(processes)), [str(target / 'aura-launcher')], extra)
                until(lambda: status().get('mapped_layers') == 3, 'launcher overlay')
                time.sleep(0.3)
                for letter in query:
                    input_.key(ord(letter))
                time.sleep(0.15)
                return launcher

            def enter_and_wait(launcher, marker):
                input_.key(0xff0d)
                until(marker.exists, 'application result')
                require(launcher.wait(timeout=5) == 0, 'Launcher launch failed')
                until(lambda: status().get('mapped_layers') == 2, 'launcher cleanup')

            hot = root / 'hot.json'; updated = root / 'updated.json'
            launcher = open_launcher('hotprobe')
            entry('hot.desktop', 'HotProbe', hot)
            time.sleep(0.9)
            enter_and_wait(launcher, hot)
            hot.unlink()
            input_.click(320, 568)  # Newly installed app is second alphabetically in the live Dock.
            until(hot.exists, 'live Dock entry without shell restart')
            launcher = open_launcher('hotprobe')
            entry('hot.desktop', 'HotProbe', updated)
            time.sleep(0.9)
            enter_and_wait(launcher, updated)
            updated.unlink()
            launcher = open_launcher('hotprobe')
            (apps / 'hot.desktop').unlink(); time.sleep(0.9)
            input_.key(0xff0d); time.sleep(0.25)
            require(launcher.poll() is None and status()['mapped_layers'] == 3 and not updated.exists(), 'Removed entry remained launchable')
            input_.key(0xff1b); require(launcher.wait(timeout=5) == 0, 'Launcher Escape failed')
            evidence['live_catalog'] = 'install in open launcher, Dock refresh, Exec edit, removal'

            work = root / 'work with spaces'; work.mkdir()
            terminal_marker = root / 'terminal-app.json'
            literal = '$(touch should-not-exist); *'
            entry('terminal.desktop', 'TerminalProbe', None,
                  f'Terminal=true\nPath={work}\nExec=/usr/bin/python3 "{recorder}" "{terminal_marker}" "{literal}"\n')
            launcher = open_launcher('terminalprobe'); time.sleep(0.6)
            enter_and_wait(launcher, terminal_marker)
            record = json.loads(terminal_marker.read_text())
            require(record == {'args': [literal], 'cwd': str(work)}, 'Terminal lost literal argv or working directory')
            require(json.loads((root / 'terminal.json').read_text())[0] == '--', 'Terminal wrapper was bypassed')
            require(not (work / 'should-not-exist').exists(), 'Exec arguments executed through a shell')
            evidence['terminal'] = 'real wrapper process, exact literal argv and cwd'
            if args.terminal_integration:
                require(shutil.which('xfce4-terminal') is not None, 'xfce4-terminal is required for --terminal-integration')
                terminal_marker.unlink()
                launcher = open_launcher('terminalprobe', {'AURA_TERMINAL': 'xfce4-terminal --disable-server -x', 'GDK_BACKEND': 'x11'})
                enter_and_wait(launcher, terminal_marker)
                require(json.loads(terminal_marker.read_text()) == record, 'Actual terminal lost literal arguments or cwd')
                evidence['terminal_emulator'] = 'Xfce terminal executed the command with literal arguments on private X11'
                focus = Input(env['DISPLAY']); focus.close()


            for service, mode in [('Activation', ''), ('Fallback', '--fail')]:
                name = 'org.aura.' + service + 'Probe'
                marker = root / (service + '.json')
                probe = launch('probe-' + service, [str(target / 'examples/application_probe'), name, str(marker)] + ([mode] if mode else []))
                until(lambda: 'APPLICATION_PROBE_READY' in logs['probe-' + service].read_text(), 'D-Bus service ownership')
                fallback = root / 'fallback-exec.json'
                entry(name + '.desktop', service + 'Probe', fallback if mode else None, 'DBusActivatable=true\n')
                launcher = open_launcher(service.lower() + 'probe'); time.sleep(0.6)
                enter_and_wait(launcher, fallback if mode else marker)
                if mode:
                    require(not marker.exists(), 'Failing D-Bus service falsely activated')
                stop(probe)
            evidence['dbus'] = 'actual Activate without Exec, failing Activate uses Exec fallback'

            slow_name = 'org.aura.SlowProbe'
            probe = launch('probe-slow', [str(target / 'examples/application_probe'), slow_name, str(root / 'slow.json'), '--slow'])
            until(lambda: 'APPLICATION_PROBE_READY' in logs['probe-slow'].read_text(), 'slow D-Bus service')
            entry(slow_name + '.desktop', 'AAA SlowProbe', None, 'DBusActivatable=true\n')
            launcher = open_launcher('slowprobe'); time.sleep(0.6)
            input_.key(0xff0d)
            until(lambda: 'APPLICATION_ACTIVATE_STARTED' in logs['probe-slow'].read_text(), 'launcher reached slow D-Bus receiver')
            require(launcher.poll() is None and not (root / 'slow.json').exists(), 'Slow activation did not remain pending')
            start = time.monotonic(); input_.key(0xff1b)
            require(launcher.wait(timeout=1) == 0, 'D-Bus activation blocked launcher input')
            evidence['responsive_activation'] = {'escape_seconds': round(time.monotonic() - start, 3)}
            time.sleep(0.8)
            calls = logs['probe-slow'].read_text().count('APPLICATION_ACTIVATE_STARTED')
            input_.click(266, 568)
            until(lambda: logs['probe-slow'].read_text().count('APPLICATION_ACTIVATE_STARTED') > calls, 'Dock activation reached slow D-Bus receiver')
            start = time.monotonic(); input_.click(310, 16)
            until(lambda: status()['layout'] == 'tiling', 'menu control during slow Dock activation', 1)
            evidence['dock_control_during_activation'] = {'tiling_seconds': round(time.monotonic() - start, 3)}
            input_.click(120, 16)
            until(lambda: status()['layout'] == 'floating', 'restoring layout through menu', 1)
            stop(probe)

            timeout_name = 'org.aura.TimeoutProbe'
            timeout_marker = root / 'timeout-fallback.json'
            probe = launch('probe-timeout', [str(target / 'examples/application_probe'), timeout_name, str(root / 'timeout-activate.json'), '--slow'])
            until(lambda: 'APPLICATION_PROBE_READY' in logs['probe-timeout'].read_text(), 'timeout D-Bus receiver')
            entry(timeout_name + '.desktop', 'TimeoutProbe', timeout_marker, 'DBusActivatable=true\n')
            launcher = open_launcher('timeoutprobe'); time.sleep(0.6)
            start = time.monotonic()
            enter_and_wait(launcher, timeout_marker)
            elapsed = time.monotonic() - start
            require(2.5 <= elapsed < 4.5 and not (root / 'timeout-activate.json').exists(), 'D-Bus timeout did not use the bounded Exec fallback')
            evidence['activation_timeout'] = {'fallback_seconds': round(elapsed, 3)}
            stop(probe)

            window = launch('window', [str(target / 'examples/smoke_client'), '--frames', '2', '--stay', '--idle-on-ready'])
            windows = until(lambda: json.loads(command(control, 'windows')), 'real window')
            window_id = windows[0]['id']; command(control, f'minimize {window_id}')
            time.sleep(0.8); input_.click(190, 610)
            until(lambda: any(w['id'] == window_id and not w['minimized'] for w in json.loads(command(control, 'windows'))), 'Dock restoring window')
            stop(window)
            evidence['window_restore'] = 'actual Dock row click restores minimized Wayland client'
            session.terminate(); require(session.wait(timeout=10) == 0, 'Supervisor cleanup failed')
            require(not control.exists() and not (root / 'wayland-launcher-dock').exists(), 'Session socket leaked')
        (args.log_dir / 'results.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print('PASS: live launcher/Dock updates, terminal argv, D-Bus activation/fallback, responsive input, magnified clicks and window restore.')
        print(f'Logs and evidence: {args.log_dir}')
        return 0
    except Exception as error:
        print(f'FAIL: {error}', file=sys.stderr)
        for name, path in logs.items():
            print(f'{name}:\n{tail(path)}', file=sys.stderr)
        return 1
    finally:
        if input_:
            input_.close()
        for process in reversed(processes):
            stop(process)
        stop(xvfb)
        for output in outputs:
            output.close()


if __name__ == '__main__':
    sys.exit(main())
