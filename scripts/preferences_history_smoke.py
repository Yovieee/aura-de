#!/usr/bin/env python3
"""Check persisted preferences and session notification history using actual UI input."""

import argparse
import ctypes as C
import json
import os
from pathlib import Path
import stat
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
    parser.add_argument('--log-dir', type=Path, default=ROOT / 'target/preferences-history-smoke-logs')
    args = parser.parse_args()
    if not args.inside_bus:
        # Always isolate the bus and config, even when the caller has a desktop session.
        with tempfile.TemporaryDirectory(prefix='aura-preferences-bus-') as directory:
            environment = os.environ.copy()
            for variable, child in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'),
                                    ('XDG_STATE_HOME', 'state')]:
                path = Path(directory) / child
                path.mkdir()
                environment[variable] = str(path)
            return subprocess.run(
                ['dbus-run-session', '--', sys.executable, str(Path(__file__).resolve()),
                 *sys.argv[1:], '--inside-bus'], env=environment, check=False,
            ).returncode
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--workspace', '--bins', '--locked'], cwd=ROOT, check=True)
        subprocess.run(['cargo', 'build', '-p', 'aura-shell', '--example', 'history_probe', '--locked'],
                       cwd=ROOT, check=True)
    target = Path(json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT))['target_directory']) / 'debug'
    for path in [target / 'aura-session', target / 'aura-settings', target / 'examples/history_probe']:
        require(path.is_file(), f'Missing {path}; rerun without --no-build')
    args.log_dir.mkdir(parents=True, exist_ok=True)
    result_path = args.log_dir / 'results.json'
    result_path.unlink(missing_ok=True)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith('AURA_CAPTURE_'):
            env.pop(key)
    env.update(LIBGL_ALWAYS_SOFTWARE='1', RUST_LOG='warn', AURA_COMPOSITOR_GLASS='1')
    processes, outputs, logs = [], [], {}
    session = xvfb = input_ = None
    evidence = {}

    def launch(name, argv, extra=None):
        path = args.log_dir / (name + '.log')
        logs[name] = path
        output = path.open('w')
        outputs.append(output)
        process = subprocess.Popen(argv, cwd=ROOT, env=env | (extra or {}),
                                   stdout=output, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    def until(predicate, label, timeout=12):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            require(session is None or session.poll() is None, 'Supervisor exited while waiting for ' + label)
            result = predicate()
            if result:
                return result
            time.sleep(0.025)
        raise RuntimeError('Timed out: ' + label)

    def bus_call(destination, object_path, method, *arguments):
        return subprocess.run(
            ['gdbus', 'call', '--session', '--dest', destination, '--object-path', object_path,
             '--method', method, *arguments], env=env, capture_output=True, text=True, check=True, timeout=3,
        ).stdout.strip()

    def name_owned(name):
        return 'true' in bus_call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                                 'org.freedesktop.DBus.NameHasOwner', name)

    def property_value(name):
        return bus_call('org.aura.Shell1', '/org/aura/Shell1',
                        'org.freedesktop.DBus.Properties.Get', 'org.aura.Shell1', name)

    def bool_property(name):
        value = property_value(name)
        require('<true>' in value or '<false>' in value, f'Missing actual {name} property: {value}')
        return '<true>' in value

    def persistence_error():
        value = property_value('PersistenceError')
        return value not in ("(<''>,)", '(<"">,)')

    try:
        xvfb = start_xvfb(env)
        with tempfile.TemporaryDirectory(prefix='aura-preferences-history-') as directory:
            runtime = Path(directory)
            os.chmod(runtime, 0o700)
            data = runtime / 'data'
            (data / 'applications').mkdir(parents=True)
            # Keep destructive failure fixtures disposable even if this hidden
            # entry point is invoked directly by a test runner.
            for variable, child in [('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'),
                                    ('XDG_STATE_HOME', 'state')]:
                path = runtime / child
                path.mkdir()
                env[variable] = str(path)
            control = runtime / 'control.sock'
            preferences = Path(env['XDG_CONFIG_HOME']) / 'aura/preferences.json'
            socket_name = 'wayland-preferences-history'
            env.update(XDG_RUNTIME_DIR=directory, XDG_DATA_HOME=str(data), XDG_DATA_DIRS=str(data),
                       AURA_CONTROL_SOCKET=str(control), AURA_WINIT_RESOLUTION='1024x640', AURA_OUTPUT_SCALE='1')

            def status():
                return json.loads(command(control, 'status')) if control.exists() else {}

            def layers(count):
                return status().get('mapped_layers') == count

            def start_session(name):
                nonlocal session, input_
                # The host compositor must use X11 after each restart. Only its children
                # get WAYLAND_DISPLAY from the real supervisor.
                session = launch(name, [str(target / 'aura-session'), '--socket', socket_name, '--no-portal'])
                until(lambda: layers(2), 'menu bar and Dock after ' + name)
                until(lambda: name_owned('org.aura.Shell1'), 'actual shell service after ' + name)
                until(lambda: bool_property('PreferencesEnabled'), 'persistent preferences service')
                time.sleep(0.2)
                input_ = Input(env['DISPLAY'])
                x = input_.x
                x.XGetImage.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_uint,
                                       C.c_uint, C.c_ulong, C.c_int]
                x.XGetImage.restype = C.c_void_p
                x.XGetPixel.argtypes = [C.c_void_p, C.c_int, C.c_int]
                x.XGetPixel.restype = C.c_ulong
                x.XDestroyImage.argtypes = [C.c_void_p]

            def stop_session():
                nonlocal session, input_
                if input_:
                    input_.close()
                    input_ = None
                process, session = session, None
                process.terminate()
                require(process.wait(timeout=10) == 0, 'Supervisor did not stop cleanly')
                until(lambda: not name_owned('org.aura.Shell1') and
                      not name_owned('org.freedesktop.Notifications'), 'session bus names released')
                require(not control.exists() and not (runtime / socket_name).exists(),
                        'Session socket leaked across restart')

            def pixels(x, y, width=1, height=1):
                image = input_.x.XGetImage(input_.d, input_.x.XDefaultRootWindow(input_.d), x, y,
                                          width, height, C.c_ulong(-1).value, 2)
                require(image, 'Cannot inspect the actual nested desktop')
                try:
                    return [input_.x.XGetPixel(image, col, row) & 0xffffff
                            for row in range(height) for col in range(width)]
                finally:
                    input_.x.XDestroyImage(image)

            def row_color(index):
                # Read an unlabelled interior pixel, independent of text and focus borders.
                return pixels(666, 118 + 44 * index)[0]

            def history_rows(expected):
                colors = {'active': 0x2a3c51, 'archived': 0x202f40, 'empty': 0x192534}
                return all(row_color(index) == colors[kind] for index, kind in enumerate(expected))

            def open_history(expected):
                require(layers(2), 'History must open from the desktop')
                input_.click(925, 16)
                until(lambda: layers(3), 'history opened with real menu input')
                until(lambda: history_rows(expected), 'actual history rows ' + str(expected))

            def close_overlay():
                input_.click(988, 64)
                until(lambda: layers(2), 'notification overlay closed')

            def open_settings():
                require(layers(2), 'Settings must open from the desktop')
                input_.click(780, 16)
                until(lambda: layers(3), 'settings opened with real menu input')
                time.sleep(0.2)

            def settings_origin():
                current = status()
                return (1024 - 600) // 2, int(current['reserved_top'] +
                    (640 - current['reserved_top'] - current['reserved_bottom'] - 360) / 2)

            def settings_click(x, y):
                origin_x, origin_y = settings_origin()
                input_.click(origin_x + x, origin_y + y)

            def settings_error_visible():
                origin_x, origin_y = settings_origin()
                return 0xffb2b2 in pixels(origin_x + 24, origin_y + 294, 544, 28)

            def close_settings():
                input_.key(0xff1b)
                until(lambda: layers(2), 'settings closed by Escape')

            def saved_profile():
                try:
                    return json.loads(preferences.read_text())
                except (OSError, ValueError):
                    return None

            expected_profile = {'version': 1, 'layout': 'tiling', 'glass_quality': 'ultra',
                                'do_not_disturb': True}

            def restored():
                current = status()
                return current.get('layout') == 'tiling' and current.get('quality') == 'ultra' and \
                    bool_property('DoNotDisturb') and not persistence_error()

            def notification(name, options=()):
                process = launch(name, [str(target / 'examples/history_probe'), '--summary', name,
                                       '--body', 'Real private D-Bus notification: ' + name,
                                       '--hold-after-close-ms', '6000', *options])
                until(lambda: 'HISTORY_READY' in logs[name].read_text(), name + ' real notification ID')
                require(process.poll() is None, name + ' caller exited before interaction')
                time.sleep(0.1)
                return process

            def notified(name, text):
                return text in logs[name].read_text()

            def finish(name, process):
                require(process.wait(timeout=10) == 0, name + ' lifecycle or forbidden late signal check failed')
                require(notified(name, 'HISTORY_FINISHED'), name + ' missed post-close observation')

            start_session('session-save')
            require(not preferences.exists(), 'Fresh isolated session unexpectedly had preferences')
            open_settings()
            settings_click(490, 104)
            until(lambda: status()['layout'] == 'tiling', 'actual settings layout applied')
            settings_click(110, 180)
            until(lambda: status()['quality'] == 'ultra', 'actual settings quality applied')
            settings_click(108, 256)
            until(lambda: bool_property('DoNotDisturb'), 'actual DND applied')
            until(lambda: saved_profile() == expected_profile and not persistence_error(),
                  'complete acknowledged preferences snapshot')
            require(stat.S_IMODE(preferences.stat().st_mode) == 0o600, 'Saved preferences are not private (0600)')
            saved_bytes = preferences.read_bytes()
            close_settings()
            stop_session()
            start_session('session-restored')
            until(restored, 'actual preferences restored in a new session')
            require(preferences.read_bytes() == saved_bytes, 'Restart unexpectedly rewrote saved preferences')
            evidence['preferences_restart'] = {'saved': expected_profile, 'mode': '0600',
                'actual_compositor_and_dnd_restored': True}

            older = notification('older-action')
            newer = notification('newer-dismiss', ['--expect-dismiss'])
            require(layers(2), 'Restored DND did not suppress live cards')
            open_history(['active', 'active', 'empty'])
            input_.key(0xff54)  # Down selects the older row, not the newest global notification.
            input_.key(0xff0d)
            until(lambda: pixels(680, 178)[0] == 0x365d84, 'older notification action card')
            input_.click(739, 186)
            until(lambda: notified('older-action', 'HISTORY_CLOSED'), 'selected older notification action and close')
            require(notified('older-action', 'HISTORY_ACTION') and not notified('newer-dismiss', 'HISTORY_ACTION') and
                    not notified('newer-dismiss', 'HISTORY_CLOSED'), 'History action targeted the wrong live notification')
            until(lambda: layers(2), 'DND restored after manual live card')
            open_history(['active', 'archived', 'empty'])
            input_.click(832, 180)
            until(lambda: pixels(666, 426)[0] == 0x365d84 and row_color(0) == 0x192534,
                  'archived read-only details and Back button')
            input_.click(739, 186)
            input_.click(800, 120)
            time.sleep(0.2)
            require(older.poll() is None and newer.poll() is None and
                    not notified('newer-dismiss', 'HISTORY_ACTION'), 'Archived detail emitted a live action')
            input_.key(0xff1b)
            until(lambda: history_rows(['active', 'archived', 'empty']), 'Escape returns from details to list')
            input_.click(738, 404)
            until(lambda: history_rows(['active', 'empty', 'empty']), 'clear removes only archived rows')
            require(newer.poll() is None and not notified('newer-dismiss', 'HISTORY_CLOSED'),
                    'Clearing history closed an active notification')
            input_.click(832, 136)
            until(lambda: pixels(680, 178)[0] == 0x365d84, 'surviving active notification card')
            input_.click(988, 64)
            until(lambda: notified('newer-dismiss', 'HISTORY_CLOSED'), 'actual active dismissal signal')
            until(lambda: layers(2), 'dismissed live card removed')
            finish('older-action', older)
            finish('newer-dismiss', newer)
            require('reason=2' in logs['newer-dismiss'].read_text() and
                    not notified('newer-dismiss', 'HISTORY_ACTION'), 'Dismiss did not archive without action')
            evidence['history_selection_and_clear'] = {
                'older_live_row_action_only_to_its_owner': True, 'keyboard_down_enter_opens_older_row': True,
                'archived_details_read_only': True, 'escape_returns_from_details_to_list': True,
                'clear_preserves_active_notification': True, 'actual_user_dismissal_reason': 2}

            expiry = notification('expired-history', ['--expect-expired'])
            until(lambda: notified('expired-history', 'HISTORY_CLOSED'), 'actual expiry signal')
            require(layers(2), 'Expiry revealed a card despite restored DND')
            open_history(['archived', 'archived', 'empty'])
            input_.click(832, 136)
            until(lambda: pixels(666, 426)[0] == 0x365d84, 'expired notification read-only details')
            input_.click(739, 186)
            input_.click(800, 120)
            finish('expired-history', expiry)
            require('reason=1' in logs['expired-history'].read_text() and
                    not notified('expired-history', 'HISTORY_ACTION'), 'Expiry history emitted an action')
            close_overlay()
            stop_session()
            start_session('session-history-reset')
            until(restored, 'preferences preserved independently of notification history')
            open_history(['empty'] * 6)
            input_.key(0xff0d)
            input_.click(832, 136)
            time.sleep(0.15)
            require(history_rows(['empty'] * 6), 'Restart retained an actionable history row')
            input_.key(0xff1b)
            until(lambda: layers(2), 'Escape closes history without exiting the shell')
            open_history(['empty'] * 6)
            close_overlay()
            evidence['history_lifetime'] = {'expiry_archived_reason': 1, 'archive_rows_before_restart': 2,
                'archive_empty_after_restart': True, 'preferences_still_restored': True,
                'enter_on_empty_list_is_inert': True, 'escape_closes_overlay_and_shell_reopens_it': True}

            stop_session()
            corrupt = b'{"version":1,"layout": THIS IS NOT JSON\n'
            preferences.write_bytes(corrupt)
            start_session('session-corrupt')
            until(persistence_error, 'malformed saved preferences reported')
            current = status()
            require(current['layout'] == 'floating' and current['quality'] == 'balanced' and
                    not bool_property('DoNotDisturb'), 'Malformed preferences did not retain actual defaults')
            require(preferences.read_bytes() == corrupt, 'Startup destroyed malformed preferences')
            open_settings()
            until(settings_error_visible, 'visible malformed-preferences UI error')
            settings_click(490, 104)
            until(lambda: status()['layout'] == 'tiling', 'session change despite preserved corrupt file')
            until(lambda: persistence_error() and settings_error_visible(), 'corrupt-file save failure stays visible')
            require(preferences.read_bytes() == corrupt, 'UI change silently overwrote malformed preferences')
            close_settings()
            evidence['malformed_preferences'] = {'original_bytes_preserved': True,
                'actual_defaults_used': True, 'visible_startup_and_save_error': True}

            stop_session()
            preferences.write_bytes(saved_bytes)
            start_session('session-write-failure')
            until(restored, 'valid preferences restored before injected filesystem failure')
            open_settings()
            # A directory fails even when tests run as root; permission bits would not.
            preferences.unlink()
            preferences.mkdir()
            settings_click(300, 104)
            until(lambda: status()['layout'] == 'stage-manager', 'actual layout applied before failed persistence')
            until(lambda: persistence_error() and settings_error_visible(), 'visible non-saved settings error')
            require(preferences.is_dir() and not list(preferences.iterdir()),
                    'Failed persistence falsely replaced the injected target directory')
            close_settings()
            stop_session()
            evidence['write_failure'] = {'directory_target_preserved': True,
                'effective_layout': 'stage-manager', 'visible_unsaved_error': True}

        result_path.write_text(json.dumps(evidence, indent=2) + '\n')
        print('PASS: real UI preference saves/restarts, preserved malformed config, visible write failure, '
              'directed notification history actions, read-only archives, clear and session reset.')
        print(f'Logs and evidence: {args.log_dir}')
        return 0
    except Exception as error:
        print(f'FAIL: {error}', file=sys.stderr)
        for name, path in logs.items():
            diagnostic = '\n'.join(line[:500] for line in tail(path).splitlines()[-25:])
            print(f'{name}:\n{diagnostic}', file=sys.stderr)
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
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError, OSError) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        sys.exit(1)
