#!/usr/bin/env python3
"""Exercise actual notification actions, desktop settings and layout shortcuts."""
import argparse
import ctypes as C
import json
import os
import socket
from pathlib import Path
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
    parser.add_argument('--inside-bus', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--log-dir', type=Path, default=ROOT / 'target/desktop-controls-smoke-logs')
    args = parser.parse_args()
    if not args.inside_bus:
        with tempfile.TemporaryDirectory(prefix='aura-controls-bus-') as directory:
            env = os.environ.copy()
            for variable, child in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state')]:
                path = Path(directory) / child
                path.mkdir()
                env[variable] = str(path)
            return subprocess.run(['dbus-run-session', '--', sys.executable, str(Path(__file__).resolve()),
                                   *sys.argv[1:], '--inside-bus'], env=env, check=False).returncode
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--workspace', '--bins', '--examples', '--locked'], cwd=ROOT, check=True)
    target = Path(json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT))['target_directory']) / 'debug'
    for path in [target / 'aura-session', target / 'aura-settings', target / 'examples/notification_probe']:
        require(path.is_file(), f'Missing {path}; rerun without --no-build')
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
        with tempfile.TemporaryDirectory(prefix='aura-desktop-controls-') as directory:
            runtime = Path(directory); os.chmod(runtime, 0o700)
            data = runtime / 'data'; (data / 'applications').mkdir(parents=True)
            control = runtime / 'control.sock'
            env.update(XDG_RUNTIME_DIR=directory, XDG_DATA_HOME=str(data), XDG_DATA_DIRS=str(data),
                       AURA_CONTROL_SOCKET=str(control), AURA_WINIT_RESOLUTION='1024x640', AURA_OUTPUT_SCALE='1')
            session = launch('session', [str(target / 'aura-session'), '--socket', 'wayland-desktop-controls', '--no-portal'])
            env['WAYLAND_DISPLAY'] = 'wayland-desktop-controls'

            def status():
                return json.loads(command(control, 'status')) if control.exists() else {}

            def layers(count):
                return status().get('mapped_layers') == count

            def dnd():
                result = subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.aura.Shell1',
                    '--object-path', '/org/aura/Shell1', '--method', 'org.freedesktop.DBus.Properties.Get',
                    'org.aura.Shell1', 'DoNotDisturb'], env=env, capture_output=True, text=True, check=True, timeout=3)
                require('true' in result.stdout or 'false' in result.stdout, 'Missing actual DND property')
                return 'true' in result.stdout

            until(lambda: layers(2), 'menu bar and Dock')
            time.sleep(0.25)
            input_ = Input(env['DISPLAY'])

            def open_settings():
                require(layers(2), 'Settings should open from a clean desktop')
                input_.click(780, 16)
                until(lambda: layers(3), 'settings opened through the menu button')
                time.sleep(0.35)

            def settings_click(x, y):
                current = status()
                origin_x = (1024 - 600) // 2
                origin_y = int(current['reserved_top'] + (640 - current['reserved_top'] - current['reserved_bottom'] - 360) / 2)
                input_.click(origin_x + x, origin_y + y)

            def close_settings(expected_layers=2):
                input_.key(0xff1b)
                until(lambda: layers(expected_layers), 'Escape closing settings')

            def notification(name, options=(), show=True):
                process = launch(name, [str(target / 'examples/notification_probe'), *options])
                until(lambda: 'PROBE_READY' in logs[name].read_text(), 'notification caller received its real ID')
                if show:
                    until(lambda: layers(3), 'notification card buffer')
                    time.sleep(0.15)
                return process

            def completed(name, process, expected):
                require(process.wait(timeout=6) == 0, f'{name} notification lifecycle failed')
                require(expected in logs[name].read_text(), f'{name} missed expected real D-Bus signal')
                until(lambda: layers(2), 'notification role and buffer cleanup')

            # Real settings IPC is checked by compositor state and the D-Bus property.
            open_settings()
            settings_click(490, 104)
            until(lambda: status()['layout'] == 'tiling', 'settings layout acknowledgement')
            settings_click(490, 180)
            until(lambda: status()['quality'] == 'reduced-transparency', 'settings glass quality acknowledgement')
            settings_click(108, 256)
            until(dnd, 'DND enabled through settings')
            close_settings()
            suppressed = notification('dnd', show=False)
            time.sleep(0.35)
            require(layers(2) and suppressed.poll() is None, 'DND did not suppress the card')
            open_settings()
            settings_click(108, 256)
            until(lambda: not dnd(), 'DND disabled through settings')
            until(lambda: layers(4), 'retained notification revealed while settings remains open')
            close_settings(3)
            input_.click(739, 186)
            completed('dnd', suppressed, 'PROBE_ACTION')
            evidence['settings'] = 'actual layout, glass quality and DND state; suppressed notification restored'

            # Tab/Enter exercises the UI keyboard route independently of pointer clicks.
            open_settings()
            input_.key(0xff09); input_.key(0xff0d)
            until(lambda: status()['layout'] == 'stage-manager', 'Tab/Enter applying Stage Manager')
            settings_click(110, 104)
            until(lambda: status()['layout'] == 'floating', 'settings restoring Floating')
            settings_click(300, 180)
            until(lambda: status()['quality'] == 'balanced', 'settings restoring Balanced')
            close_settings()
            evidence['settings_keyboard'] = 'Tab/Enter applies real layout; Escape closes the overlay'

            action = notification('action')
            input_.click(739, 186)
            completed('action', action, 'PROBE_ACTION')
            dismiss = notification('dismiss', ['--expect-dismiss'])
            input_.click(988, 64)
            completed('dismiss', dismiss, 'PROBE_CLOSED')
            evidence['notification_action_dismiss'] = 'directed ActionInvoked plus nonresident close; dismissal reason 2 without action'

            resident = notification('resident', ['--resident'])
            input_.click(739, 186)
            until(lambda: 'PROBE_ACTION' in logs['resident'].read_text(), 'resident action signal')
            time.sleep(0.2)
            require(resident.poll() is None and layers(3), 'Resident notification closed after its action')
            input_.click(988, 64)
            completed('resident', resident, 'PROBE_CLOSED')
            evidence['resident'] = 'action preserves notification until user dismissal'

            many = notification('many', ['--many'])
            input_.click(954, 226); time.sleep(0.15)
            input_.click(954, 226); time.sleep(0.15)
            input_.click(914, 186)
            completed('many', many, 'key=six')
            default = notification('default', ['--default-action'])
            input_.click(800, 120)
            completed('default', default, 'key=default')
            evidence['action_navigation'] = 'all six actions reachable by page controls; default body action works'

            expiry = notification('expiry', ['--expect-expired'], show=False)
            completed('expiry', expiry, 'reason=1')
            evidence['expiry'] = 'real expiry signal removes the notification role'

            def super_key(key):
                modifier = input_.x.XKeysymToKeycode(input_.d, 0xffeb)
                require(modifier != 0, 'Private X server lacks Super key')
                input_.t.XTestFakeKeyEvent(input_.d, modifier, 1, 0)
                input_.key(key)
                input_.t.XTestFakeKeyEvent(input_.d, modifier, 0, 0)
                input_.x.XFlush(input_.d)

            super_key(0xff52)
            until(lambda: status()['layout'] == 'stage-manager', 'Super+Up layout action')
            super_key(0xff54)
            until(lambda: status()['layout'] == 'floating', 'Super+Down layout action')
            super_key(0xff51)
            until(lambda: status()['layout'] == 'tiling', 'Super+Left layout action')
            super_key(0xff53)
            until(lambda: status()['layout'] == 'floating', 'Super+Right layout action')
            input_.key(0xff52); time.sleep(0.2)
            require(status()['layout'] == 'floating', 'Plain arrow unexpectedly changed layout')
            evidence['layout_shortcuts'] = 'actual Super+Arrow input; plain arrow leaves layout unchanged; physical touchpad not exercised'

            # Releases outside the host window must not leave Super stuck in XKB.
            x = input_.x
            modifier = x.XKeysymToKeycode(input_.d, 0xffeb)
            arrow = x.XKeysymToKeycode(input_.d, 0xff53)
            input_.t.XTestFakeKeyEvent(input_.d, modifier, 1, 0)
            input_.t.XTestFakeKeyEvent(input_.d, arrow, 1, 0)
            x.XFlush(input_.d)
            until(lambda: status()['layout'] == 'tiling', 'held Super shortcut')
            x.XSetInputFocus(input_.d, x.XDefaultRootWindow(input_.d), 2, 0)
            x.XFlush(input_.d); time.sleep(0.12)
            input_.t.XTestFakeKeyEvent(input_.d, arrow, 0, 0)
            input_.t.XTestFakeKeyEvent(input_.d, modifier, 0, 0)
            x.XFlush(input_.d); time.sleep(0.12)
            focus = Input(env['DISPLAY']); focus.close()
            time.sleep(0.12)
            input_.key(0xff53); time.sleep(0.15)
            require(status()['layout'] == 'tiling', 'Host focus loss left a modifier or shortcut stuck')
            super_key(0xff54)
            until(lambda: status()['layout'] == 'floating', 'shortcut after host focus return')
            evidence['focus_loss'] = 'release outside the host window clears Super; plain arrow remains plain'

            # An unreachable readback must show an error rather than a committed state.
            missing = launch('settings-unavailable', [str(target / 'aura-settings')],
                             {'AURA_CONTROL_SOCKET': str(runtime / 'missing-control.sock')})
            until(lambda: layers(3), 'settings with missing backend')
            x = input_.x
            x.XGetImage.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_uint, C.c_uint, C.c_ulong, C.c_int]
            x.XGetImage.restype = C.c_void_p
            x.XGetPixel.argtypes = [C.c_void_p, C.c_int, C.c_int]; x.XGetPixel.restype = C.c_ulong
            x.XDestroyImage.argtypes = [C.c_void_p]
            def error_pixels():
                current = status()
                origin_y = int(current['reserved_top'] + (640 - current['reserved_top'] - current['reserved_bottom'] - 360) / 2)
                image = x.XGetImage(input_.d, x.XDefaultRootWindow(input_.d), 236, origin_y + 294,
                                    544, 28, C.c_ulong(-1).value, 2)
                require(image, 'Cannot inspect actual settings error text')
                try:
                    return any(x.XGetPixel(image, col, row) & 0xffffff == 0xffb2b2
                               for row in range(28) for col in range(544))
                finally:
                    x.XDestroyImage(image)
            until(error_pixels, 'visible settings backend error')
            require(missing.poll() is None and status()['layout'] == 'floating', 'Missing backend falsely completed a change')
            input_.key(0xff1b); require(missing.wait(timeout=1) == 0, 'Failed settings UI did not close')
            until(lambda: layers(2), 'failed settings UI cleanup')
            with socket.socket(socket.AF_UNIX) as listener:
                silent_path = runtime / 'silent-control.sock'
                listener.bind(str(silent_path)); listener.listen(1); listener.settimeout(3)
                silent = launch('settings-silent', [str(target / 'aura-settings')],
                                {'AURA_CONTROL_SOCKET': str(silent_path)})
                with listener.accept()[0] as connection:
                    connection.settimeout(1)
                    require(connection.recv(64) == b'status\n', 'Settings did not start a real backend query')
                    until(lambda: layers(3), 'settings with pending backend query')
                    time.sleep(0.15)
                    start = time.monotonic(); input_.key(0xff1b)
                    require(silent.wait(timeout=1) == 0, 'Pending backend I/O blocked Escape')
                    elapsed = time.monotonic() - start
                until(lambda: layers(2), 'pending settings UI cleanup')
            evidence['settings_failures'] = {'visible_backend_error': True, 'pending_query_escape_seconds': round(elapsed, 3)}

            before = status()['rendered_frames']; time.sleep(0.3)
            require(status()['rendered_frames'] == before, 'Controls keep committing unchanged idle frames')
            session.terminate()
            require(session.wait(timeout=10) == 0, 'Supervisor shutdown failed')
            require(not control.exists() and not (runtime / 'wayland-desktop-controls').exists(), 'Session socket leaked')
        (args.log_dir / 'results.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print('PASS: actual notification action/dismiss/resident/pagination signals, desktop settings, DND, keyboard shortcuts and cleanup.')
        print('Physical touchpad gesture input is not exercised by the nested Xvfb backend.')
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
