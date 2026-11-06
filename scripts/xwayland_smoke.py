#!/usr/bin/env python3
"""Exercise real X11 windows, input and lifecycle through Aura's XWayland server."""
import argparse
import ctypes as C
import ctypes.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

from integration_desktop import command, require
from session_smoke import Input
from smoke_nested import ROOT, start_xvfb, stop, tail


class ClientData(C.Union):
    _fields_ = [('bytes', C.c_char * 20), ('shorts', C.c_short * 10), ('longs', C.c_long * 5)]


class ClientMessage(C.Structure):
    _fields_ = [('type', C.c_int), ('serial', C.c_ulong), ('send_event', C.c_int),
                ('display', C.c_void_p), ('window', C.c_ulong), ('message_type', C.c_ulong),
                ('format', C.c_int), ('data', ClientData)]


class InputEvent(C.Structure):
    _fields_ = [('type', C.c_int), ('serial', C.c_ulong), ('send_event', C.c_int),
                ('display', C.c_void_p), ('window', C.c_ulong), ('root', C.c_ulong),
                ('subwindow', C.c_ulong), ('time', C.c_ulong), ('x', C.c_int), ('y', C.c_int),
                ('x_root', C.c_int), ('y_root', C.c_int), ('state', C.c_uint),
                ('detail', C.c_uint), ('same_screen', C.c_int)]


class XEvent(C.Union):
    _fields_ = [('type', C.c_int), ('client', ClientMessage), ('input', InputEvent),
                ('padding', C.c_long * 24)]


class Attributes(C.Structure):
    _fields_ = [('background_pixmap', C.c_ulong), ('background_pixel', C.c_ulong),
                ('border_pixmap', C.c_ulong), ('border_pixel', C.c_ulong),
                ('bit_gravity', C.c_int), ('win_gravity', C.c_int), ('backing_store', C.c_int),
                ('backing_planes', C.c_ulong), ('backing_pixel', C.c_ulong),
                ('save_under', C.c_int), ('event_mask', C.c_long),
                ('do_not_propagate_mask', C.c_long), ('override_redirect', C.c_int),
                ('colormap', C.c_ulong), ('cursor', C.c_ulong)]


class X11:
    def __init__(self, display):
        self.x = C.CDLL(ctypes.util.find_library('X11') or 'libX11.so.6')
        x = self.x
        x.XOpenDisplay.argtypes = [C.c_char_p]; x.XOpenDisplay.restype = C.c_void_p
        x.XDefaultRootWindow.argtypes = [C.c_void_p]; x.XDefaultRootWindow.restype = C.c_ulong
        x.XCreateSimpleWindow.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int,
            C.c_uint, C.c_uint, C.c_uint, C.c_ulong, C.c_ulong]
        x.XCreateSimpleWindow.restype = C.c_ulong
        x.XStoreName.argtypes = [C.c_void_p, C.c_ulong, C.c_char_p]
        x.XSelectInput.argtypes = [C.c_void_p, C.c_ulong, C.c_long]
        x.XInternAtom.argtypes = [C.c_void_p, C.c_char_p, C.c_int]; x.XInternAtom.restype = C.c_ulong
        x.XSetWMProtocols.argtypes = [C.c_void_p, C.c_ulong, C.POINTER(C.c_ulong), C.c_int]
        x.XSetTransientForHint.argtypes = [C.c_void_p, C.c_ulong, C.c_ulong]
        x.XChangeWindowAttributes.argtypes = [C.c_void_p, C.c_ulong, C.c_ulong, C.POINTER(Attributes)]
        x.XChangeProperty.argtypes = [C.c_void_p, C.c_ulong, C.c_ulong, C.c_ulong,
            C.c_int, C.c_int, C.POINTER(C.c_ubyte), C.c_int]
        x.XGetWindowProperty.argtypes = [C.c_void_p, C.c_ulong, C.c_ulong, C.c_long, C.c_long,
            C.c_int, C.c_ulong, C.POINTER(C.c_ulong), C.POINTER(C.c_int), C.POINTER(C.c_ulong),
            C.POINTER(C.c_ulong), C.POINTER(C.POINTER(C.c_ubyte))]
        x.XFree.argtypes = [C.c_void_p]
        for name in ('XMapWindow', 'XUnmapWindow', 'XDestroyWindow', 'XClearWindow'):
            getattr(x, name).argtypes = [C.c_void_p, C.c_ulong]
        x.XResizeWindow.argtypes = [C.c_void_p, C.c_ulong, C.c_uint, C.c_uint]
        x.XFlush.argtypes = [C.c_void_p]
        x.XSync.argtypes = [C.c_void_p, C.c_int]
        x.XCloseDisplay.argtypes = [C.c_void_p]
        x.XConnectionNumber.argtypes = [C.c_void_p]; x.XConnectionNumber.restype = C.c_int
        x.XPending.argtypes = [C.c_void_p]; x.XPending.restype = C.c_int
        x.XNextEvent.argtypes = [C.c_void_p, C.POINTER(XEvent)]
        x.XSendEvent.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_long, C.POINTER(XEvent)]
        x.XGetGeometry.argtypes = [C.c_void_p, C.c_ulong, C.POINTER(C.c_ulong),
            C.POINTER(C.c_int), C.POINTER(C.c_int), C.POINTER(C.c_uint), C.POINTER(C.c_uint),
            C.POINTER(C.c_uint), C.POINTER(C.c_uint)]
        self.d = x.XOpenDisplay(display.encode())
        require(self.d, 'Cannot connect to actual XWayland display ' + display)
        self.root = x.XDefaultRootWindow(self.d)
        self.delete = self.atom('WM_DELETE_WINDOW')
        self.windows = set()
        self.events = []

    def atom(self, name):
        return self.x.XInternAtom(self.d, name.encode(), 0)

    def property32(self, window, name, type_name, values):
        data = (C.c_ulong * len(values))(*values)
        self.x.XChangeProperty(self.d, window, self.atom(name), self.atom(type_name), 32, 0,
                              C.cast(data, C.POINTER(C.c_ubyte)), len(values))

    def read_property32(self, window, name, type_name):
        actual_type, actual_format = C.c_ulong(), C.c_int()
        count, remaining, data = C.c_ulong(), C.c_ulong(), C.POINTER(C.c_ubyte)()
        result = self.x.XGetWindowProperty(self.d, window, self.atom(name), 0, 64, 0,
            self.atom(type_name), C.byref(actual_type), C.byref(actual_format), C.byref(count),
            C.byref(remaining), C.byref(data))
        try:
            require(result == 0 and actual_type.value == self.atom(type_name) and
                    actual_format.value == 32 and not remaining.value, 'Invalid actual X11 property ' + name)
            return list(C.cast(data, C.POINTER(C.c_ulong))[:count.value])
        finally:
            if data:
                self.x.XFree(data)

    def create(self, name, color, position=(40, 80), size=(300, 210), parent=None,
               initial_states=(), csd=False):
        window = self.x.XCreateSimpleWindow(self.d, self.root, *position, *size, 0, 0, color)
        require(window, 'XCreateSimpleWindow failed')
        self.windows.add(window)
        self.x.XStoreName(self.d, window, name.encode())
        self.x.XSelectInput(self.d, window, (1 << 0) | (1 << 1) | (1 << 2) |
                           (1 << 3) | (1 << 15) | (1 << 17) | (1 << 21))
        protocol = C.c_ulong(self.delete)
        self.x.XSetWMProtocols(self.d, window, C.byref(protocol), 1)
        if initial_states:
            atoms = [self.atom(name) for name in initial_states]
            self.property32(window, '_NET_WM_STATE', 'ATOM', atoms)
            require(self.read_property32(window, '_NET_WM_STATE', 'ATOM') == atoms,
                    'X11 initial state property did not exist before mapping')
        if csd:
            # MWM_HINTS_DECORATIONS with decorations=0 asks the WM to omit chrome.
            self.property32(window, '_MOTIF_WM_HINTS', '_MOTIF_WM_HINTS', [2, 0, 0, 0, 0])
            require(self.read_property32(window, '_MOTIF_WM_HINTS', '_MOTIF_WM_HINTS') == [2, 0, 0, 0, 0],
                    'X11 Motif CSD hint did not exist before mapping')
        if parent:
            attributes = Attributes(); attributes.override_redirect = 1
            self.x.XChangeWindowAttributes(self.d, window, 1 << 9, C.byref(attributes))
            self.x.XSetTransientForHint(self.d, window, parent)
        self.x.XMapWindow(self.d, window)
        self.x.XClearWindow(self.d, window)
        self.x.XFlush(self.d)
        return window

    def pump(self):
        while self.x.XPending(self.d):
            event = XEvent()
            self.x.XNextEvent(self.d, C.byref(event))
            record = {'type': event.type, 'window': int(event.input.window)}
            if event.type in (2, 3, 4, 5):
                record['detail'] = event.input.detail
            if event.type == 33:
                record['message_type'] = int(event.client.message_type)
                record['data'] = [int(value) for value in event.client.data.longs]
            self.events.append(record)

    def state(self, window, names, enabled):
        event = XEvent()
        event.client = ClientMessage(type=33, send_event=1, display=self.d, window=window,
            message_type=self.atom('_NET_WM_STATE'), format=32)
        event.client.data.longs[0] = int(enabled)
        for index, name in enumerate(names, 1):
            event.client.data.longs[index] = self.atom(name)
        event.client.data.longs[3] = 1
        require(self.x.XSendEvent(self.d, self.root, 0, (1 << 19) | (1 << 20), C.byref(event)),
                'Cannot send actual X11 window-state request')
        self.x.XFlush(self.d)

    def geometry(self, window):
        root, x, y = C.c_ulong(), C.c_int(), C.c_int()
        width, height, border, depth = C.c_uint(), C.c_uint(), C.c_uint(), C.c_uint()
        require(self.x.XGetGeometry(self.d, window, C.byref(root), C.byref(x), C.byref(y),
            C.byref(width), C.byref(height), C.byref(border), C.byref(depth)), 'XGetGeometry failed')
        return {'x': x.value, 'y': y.value, 'width': width.value, 'height': height.value}

    def destroy(self, window):
        self.x.XDestroyWindow(self.d, window)
        self.windows.remove(window)
        self.x.XFlush(self.d)

    def close(self):
        if self.d:
            for window in self.windows:
                self.x.XDestroyWindow(self.d, window)
            self.windows.clear()
            self.x.XSync(self.d, 0)
            self.x.XCloseDisplay(self.d)
            self.d = None

    def abandon(self):
        # Xlib's default I/O handler exits the process if its server died. On a
        # failing harness path close only our fd, then reap the owned supervisor.
        if self.d:
            os.close(self.x.XConnectionNumber(self.d))
            self.d = None


def fixture_child(marker):
    """An actual X11 app started by the shell's inherited DISPLAY."""
    x11 = X11(os.environ.get('DISPLAY', ''))
    try:
        window = x11.create('AuraX11Launched', 0x39ac78)
        Path(marker).write_text(json.dumps({'display': os.environ.get('DISPLAY'), 'window': window,
            'wayland_display': os.environ.get('WAYLAND_DISPLAY')}) + '\n')
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            x11.pump()
            if any(event['type'] == 33 and event.get('data', [None])[0] == x11.delete
                   for event in x11.events):
                return 0
            time.sleep(0.02)
        raise RuntimeError('Launched X11 fixture did not receive WM_DELETE_WINDOW')
    finally:
        x11.close()


def child_pid(parent, name):
    for process in Path('/proc').iterdir():
        if not process.name.isdigit():
            continue
        try:
            # /proc/PID/task/TID/children is optional in Linux kernels. PPID in
            # stat and the executable identify direct children without it.
            fields = (process / 'stat').read_text().rsplit(')', 1)[1].split()
            if int(fields[1]) == parent and (process / 'exe').readlink().name == name:
                return int(process.name)
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--inside-bus', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--fixture-child', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--log-dir', type=Path, default=ROOT / 'target/xwayland-smoke-logs')
    args = parser.parse_args()
    if args.fixture_child:
        return fixture_child(args.fixture_child)
    if not args.inside_bus:
        return subprocess.run(['dbus-run-session', '--', sys.executable, str(Path(__file__).resolve()),
                               *sys.argv[1:], '--inside-bus'], check=False).returncode
    require(shutil.which('Xwayland'), 'Xwayland is required; install dependencies with --xwayland')
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--workspace', '--bins', '--examples',
                        '--features', 'aura-compositor/xwayland', '--locked'], cwd=ROOT, check=True)
    target = Path(json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked',
                                                    '--format-version', '1'], cwd=ROOT))['target_directory']) / 'debug'
    for binary in ('aura-session', 'aura-compositor', 'aura-shell', 'aura-launcher', 'examples/smoke_client'):
        require((target / binary).is_file(), f'Missing {binary}; rerun without --no-build')
    args.log_dir.mkdir(parents=True, exist_ok=True)
    result_path = args.log_dir / 'results.json'; result_path.unlink(missing_ok=True)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith('AURA_') or key in ('WAYLAND_DISPLAY', 'DISPLAY', 'XAUTHORITY'):
            env.pop(key)
    env.update(LIBGL_ALWAYS_SOFTWARE='1', AURA_COMPOSITOR_GLASS='1', RUST_LOG='info')
    processes, outputs, logs = [], [], {}
    session = xvfb = input_ = x11 = None
    evidence = {}

    def launch(name, argv, extra=None):
        path = args.log_dir / (name + '.log'); logs[name] = path
        output = path.open('w'); outputs.append(output)
        process = subprocess.Popen(argv, cwd=ROOT, env=env | (extra or {}), stdout=output,
                                   stderr=subprocess.STDOUT, umask=0)
        processes.append(process)
        return process

    def until(predicate, label, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            require(session is None or session.poll() is None, 'Supervisor exited while waiting for ' + label)
            if x11:
                x11.pump()
            result = predicate()
            if result:
                return result
            time.sleep(0.025)
        raise RuntimeError('Timed out: ' + label)

    try:
        xvfb = start_xvfb(env)
        host_display = env['DISPLAY']
        host_socket = Path('/tmp/.X11-unix') / ('X' + host_display.removeprefix(':').split('.')[0])
        host_inode = host_socket.stat().st_ino
        with tempfile.TemporaryDirectory(prefix='aura-xwayland-') as directory:
            runtime = Path(directory); os.chmod(runtime, 0o700)
            apps = runtime / 'data/applications'; apps.mkdir(parents=True)
            marker = runtime / 'launched.json'
            def quote(value):
                return '"' + str(value).replace('\\', '\\\\').replace('"', '\\"') + '"'
            (apps / 'aura-x11-probe.desktop').write_text('[Desktop Entry]\nType=Application\n'
                'Name=AuraX11Launcher\nExec=' + ' '.join(map(quote,
                    [sys.executable, Path(__file__).resolve(), '--fixture-child', marker])) + '\n')
            control = runtime / 'control.sock'
            socket_name = 'wayland-xwayland-smoke'
            env.update(XDG_RUNTIME_DIR=directory, XDG_CONFIG_HOME=str(runtime / 'config'),
                XDG_CACHE_HOME=str(runtime / 'cache'), XDG_STATE_HOME=str(runtime / 'state'),
                XDG_DATA_HOME=str(runtime / 'data'), XDG_DATA_DIRS=str(runtime / 'data'),
                AURA_CONTROL_SOCKET=str(control), AURA_WINIT_RESOLUTION='1024x640', AURA_OUTPUT_SCALE='1')
            session = launch('session', [str(target / 'aura-session'), '--socket', socket_name,
                                         '--xwayland', '--no-portal'])
            def status():
                return json.loads(command(control, 'status')) if control.exists() else {}
            def windows():
                return json.loads(command(control, 'windows'))
            def window(title):
                return next((item for item in windows() if item['title'] == title), None)
            until(lambda: status().get('xwayland_ready') and status().get('mapped_layers') == 2,
                  'XWM readiness and supervised shell')
            display = status()['xwayland_display']
            require(display and display != host_display,
                    f'Session inherited the host X11 display: host={host_display!r}, XWayland={display!r}')
            require(host_socket.stat().st_ino == host_inode,
                    'XWayland replaced the existing host X11 filesystem socket')
            x11 = X11(display)
            compositor_pid = until(lambda: child_pid(session.pid, 'aura-compositor'), 'supervised compositor PID')
            xwayland_pid = status().get('xwayland_pid') or until(
                lambda: child_pid(compositor_pid, 'Xwayland'), 'owned XWayland PID')
            x_socket = Path('/tmp/.X11-unix') / ('X' + display.removeprefix(':').split('.')[0])
            require(x_socket.is_socket(), 'Ready XWayland did not publish its actual X11 socket')
            require(stat.S_IMODE(x_socket.stat().st_mode) == 0o600 and x_socket.stat().st_uid == os.geteuid(),
                    'XWayland filesystem socket is not private to the session user')
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as abstract:
                abstract.settimeout(0.25)
                try:
                    abstract.connect('\0' + str(x_socket))
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                else:
                    raise RuntimeError('XWayland published an unrestricted abstract X11 socket')
            initial_max = x11.create('AuraX11InitialMax', 0x36aa78, initial_states=[
                '_NET_WM_STATE_MAXIMIZED_HORZ', '_NET_WM_STATE_MAXIMIZED_VERT'])
            def initial_maximized():
                item = window('AuraX11InitialMax')
                if not item:
                    return False
                current = status(); rect = item['rect']; geometry = x11.geometry(initial_max)
                work_height = 640 - current['reserved_top'] - current['reserved_bottom']
                evidence['_initial_max_diagnostic'] = {'window': item, 'geometry': geometry,
                    'reserved_top': current['reserved_top'], 'reserved_bottom': current['reserved_bottom']}
                return item['maximized'] and rect['x'] == 0 and rect['y'] == current['reserved_top'] and \
                    rect['width'] == 1024 and rect['height'] == work_height and \
                    geometry['width'] == 1024 and geometry['height'] == work_height - 32
            until(initial_maximized, 'pre-map X11 maximized state and actual workarea configure')
            evidence.pop('_initial_max_diagnostic', None)
            x11.destroy(initial_max)
            until(lambda: window('AuraX11InitialMax') is None, 'initial-maximized fixture cleanup')
            for index in range(3):
                transient = x11.create(f'AuraX11Rapid{index}', 0x36aa78)
                x11.x.XUnmapWindow(x11.d, transient)
                x11.destroy(transient)
            x11.x.XSync(x11.d, 0)
            settled = time.monotonic() + 1
            until(lambda: time.monotonic() >= settled and not windows(),
                  'rapid map/unmap/destroy leaves no deferred ghost windows')
            evidence['rapid_lifecycle'] = {'windows': 3, 'no_ghost_windows': True, 'session_alive': True}
            first = x11.create('AuraX11Red', 0xd43d51)
            second = x11.create('AuraX11Blue', 0x397fca, position=(380, 80), csd=True)
            until(lambda: window('AuraX11Red') and window('AuraX11Blue'), 'two managed X11 windows')
            require(all(window(title).get('protocol') == 'x11' for title in ('AuraX11Red', 'AuraX11Blue')),
                    'Actual X11 windows were not registered as X11')
            input_ = Input(host_display)
            x = input_.x
            x.XGetImage.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_uint,
                                   C.c_uint, C.c_ulong, C.c_int]; x.XGetImage.restype = C.c_void_p
            x.XGetPixel.argtypes = [C.c_void_p, C.c_int, C.c_int]; x.XGetPixel.restype = C.c_ulong
            x.XDestroyImage.argtypes = [C.c_void_p]
            def pixel_at(point):
                image = x.XGetImage(input_.d, x.XDefaultRootWindow(input_.d), *point, 1, 1,
                                    C.c_ulong(-1).value, 2)
                require(image, 'Cannot read actual nested output pixels')
                try:
                    return x.XGetPixel(image, 0, 0) & 0xffffff
                finally:
                    x.XDestroyImage(image)
            def center(title):
                rect = window(title)['rect']
                return int(rect['x'] + rect['width'] / 2), int(rect['y'] + rect['height'] / 2)
            def blue_has_no_server_chrome():
                rect = window('AuraX11Blue')['rect']; geometry = x11.geometry(second)
                return rect['height'] == geometry['height'] == 210 and \
                    pixel_at((int(rect['x'] + rect['width'] / 2), int(rect['y'] + 4))) == 0x397fca
            until(blue_has_no_server_chrome, 'pre-map Motif CSD hint omits server chrome in geometry and pixels')
            evidence['initial_state'] = {'pre_map_maximized_uses_workarea': True,
                'motif_csd_has_no_server_chrome': True}
            command(control, 'layout tiling')
            until(lambda: pixel_at(center('AuraX11Red')) == 0xd43d51 and
                  pixel_at(center('AuraX11Blue')) == 0x397fca, 'actual colored X11 buffers composed')
            evidence['real_x11'] = {'private_display': True, 'managed_windows': 2,
                'actual_composed_colors': ['d43d51', '397fca'],
                'host_socket_preserved': True, 'private_socket_mode': '0600',
                'permissive_parent_umask': True, 'abstract_listener_absent': True}

            input_.click(*center('AuraX11Red')); input_.key(ord('a'))
            until(lambda: any(event['type'] == 4 and event['window'] == first for event in x11.events)
                and any(event['type'] == 2 and event['window'] == first for event in x11.events),
                'actual X11 pointer and keyboard events for focused red window')
            require(window('AuraX11Red')['focused'], 'Input did not focus the selected X11 window')
            evidence['input'] = {'pointer_to_selected_x11_window': True, 'keyboard_to_selected_x11_window': True}

            command(control, 'layout floating')
            x11.x.XResizeWindow(x11.d, first, 430, 260); x11.x.XFlush(x11.d)
            until(lambda: x11.geometry(first)['width'] == 430 and x11.geometry(first)['height'] == 260
                  and window('AuraX11Red')['rect']['width'] == 430,
                  'actual client resize request and shared layout geometry')
            x11.state(first, ['_NET_WM_STATE_MAXIMIZED_HORZ', '_NET_WM_STATE_MAXIMIZED_VERT'], True)
            until(lambda: window('AuraX11Red')['maximized'], 'actual X11 maximize request')
            until(lambda: x11.geometry(first)['width'] >= 900, 'maximize configure reached X11 client')
            x11.state(first, ['_NET_WM_STATE_MAXIMIZED_HORZ', '_NET_WM_STATE_MAXIMIZED_VERT'], False)
            until(lambda: not window('AuraX11Red')['maximized'], 'actual X11 unmaximize')
            x11.state(first, ['_NET_WM_STATE_FULLSCREEN'], True)
            until(lambda: window('AuraX11Red')['fullscreen'] and x11.geometry(first)['width'] == 1024
                  and x11.geometry(first)['height'] == 640, 'actual X11 fullscreen geometry')
            until(lambda: pixel_at((510, 320)) == 0xd43d51, 'fullscreen X11 pixels')
            x11.state(first, ['_NET_WM_STATE_FULLSCREEN'], False)
            until(lambda: not window('AuraX11Red')['fullscreen'], 'actual X11 fullscreen exit')
            evidence['state_requests'] = {'client_resize': [430, 260], 'maximized': True,
                'fullscreen_exact_output_geometry': True}

            rect = window('AuraX11Red')['rect']
            popup_position = int(rect['x'] + 60), int(rect['y'] + 60)
            popup_center = popup_position[0] + 36, popup_position[1] + 24
            managed_count = len(windows())
            popup = x11.create('AuraX11Override', 0xc95bce, position=popup_position,
                               size=(72, 48), parent=first)
            until(lambda: pixel_at(popup_center) == 0xc95bce, 'actual override-redirect popup buffer')
            require(len(windows()) == managed_count and window('AuraX11Override') is None,
                    'Override-redirect popup was incorrectly tiled as a managed window')
            input_.click(*popup_center)
            until(lambda: any(event['type'] == 4 and event['window'] == popup for event in x11.events),
                  'actual X11 pointer event to override-redirect popup')
            x11.destroy(popup)
            until(lambda: pixel_at(popup_center) == 0xd43d51, 'override-redirect popup cleanup')
            evidence['override_redirect'] = {'composed_at_requested_geometry': True,
                'outside_managed_layout': True, 'actual_pointer_input': True}

            second_id = window('AuraX11Blue')['id']
            command(control, 'minimize ' + str(second_id))
            until(lambda: window('AuraX11Blue')['minimized'], 'X11 minimize control')
            command(control, 'restore ' + str(second_id))
            until(lambda: not window('AuraX11Blue')['minimized'], 'X11 restore control')
            x11.x.XUnmapWindow(x11.d, second); x11.x.XFlush(x11.d)
            until(lambda: window('AuraX11Blue') is None, 'real X11 unmap')
            x11.x.XMapWindow(x11.d, second); x11.x.XFlush(x11.d)
            until(lambda: window('AuraX11Blue'), 'real X11 remap')
            x11.destroy(second)
            until(lambda: window('AuraX11Blue') is None, 'real X11 destroy')
            evidence['lifecycle'] = {'minimize_restore': True, 'unmap_remap_destroy': True}

            wayland = launch('wayland-client', [str(target / 'examples/smoke_client'), '--idle-on-ready',
                '--title', 'MixedWayland'], {'WAYLAND_DISPLAY': socket_name, 'DISPLAY': display})
            until(lambda: window('MixedWayland') and 'SMOKE_CLIENT_OK' in logs['wayland-client'].read_text(),
                  'mixed Wayland client frame callbacks')
            require(window('MixedWayland').get('protocol') == 'wayland', 'Mixed client protocol was mislabeled')
            command(control, 'layout tiling')
            until(lambda: pixel_at(center('AuraX11Red')) == 0xd43d51, 'X11 pixels coexist with Wayland client')
            command(control, 'close ' + str(window('MixedWayland')['id']))
            require(wayland.wait(timeout=6) == 0, 'Wayland close failed in mixed session')
            until(lambda: window('MixedWayland') is None, 'mixed Wayland client cleanup')
            evidence['mixed_clients'] = True

            # Start the actual desktop-entry fixture from the running shell's Dock.
            time.sleep(0.8)
            input_.click(266, 568)
            until(lambda: marker.exists() and window('AuraX11Launched'), 'X11 app launched by supervised Dock')
            launch_info = json.loads(marker.read_text())
            require(launch_info['display'] == display and launch_info['wayland_display'] == socket_name,
                    'Shell launch inherited a host display instead of its own XWayland')
            command(control, 'close ' + str(window('AuraX11Launched')['id']))
            until(lambda: window('AuraX11Launched') is None, 'launched X11 WM_DELETE cleanup')
            evidence['desktop_launch'] = {'correct_display': True, 'actual_x11_window': True}

            first_id = window('AuraX11Red')['id']
            command(control, 'close ' + str(first_id))
            until(lambda: any(event['type'] == 33 and event['window'] == first
                  and event.get('data', [None])[0] == x11.delete for event in x11.events),
                  'actual WM_DELETE_WINDOW close protocol')
            x11.destroy(first)
            until(lambda: not windows(), 'all real clients destroyed')
            x11.close(); x11 = None
            input_.close(); input_ = None
            session.terminate()
            require(session.wait(timeout=10) == 0, 'Supervisor did not stop cleanly')
            session = None
            require(not control.exists() and not (runtime / socket_name).exists(), 'Wayland/control socket leaked')
            until(lambda: not Path(f'/proc/{xwayland_pid}').exists() and not x_socket.exists(),
                  'owned XWayland process and X11 socket removal')
            evidence['shutdown'] = {'xwayland_reaped': True, 'x11_socket_removed': True,
                'wayland_control_sockets_removed': True}

            session = launch('session-crash', [str(target / 'aura-session'), '--socket', socket_name,
                '--xwayland', '--no-portal'])
            until(lambda: status().get('xwayland_ready') and status().get('mapped_layers') == 2,
                  'second real XWayland session before crash')
            crash_display = status()['xwayland_display']
            crash_compositor = until(lambda: child_pid(session.pid, 'aura-compositor'),
                                     'crash-test compositor PID')
            crash_xwayland = status().get('xwayland_pid') or until(
                lambda: child_pid(crash_compositor, 'Xwayland'), 'crash-test XWayland PID')
            crash_socket = Path('/tmp/.X11-unix') / ('X' + crash_display.removeprefix(':'))
            os.kill(crash_xwayland, signal.SIGTERM)
            stopped_session, session = session, None
            require(stopped_session.wait(timeout=10) != 0,
                    'Unexpected XWayland exit did not fail the required X11 session')
            until(lambda: not control.exists() and not (runtime / socket_name).exists()
                  and not crash_socket.exists() and not Path(f'/proc/{crash_xwayland}').exists(),
                  'failed XWayland session cleanup')
            evidence['crash_cleanup'] = {'supervisor_failed': True, 'xwayland_reaped': True,
                'all_owned_sockets_removed': True}

            empty_path = runtime / 'empty-bin'; empty_path.mkdir()
            failure = launch('session-missing-server', [str(target / 'aura-session'), '--socket', socket_name,
                '--xwayland', '--no-portal', '--startup-timeout', '3'], {'PATH': str(empty_path)})
            require(failure.wait(timeout=10) != 0, 'Missing Xwayland executable falsely reported success')
            failure_log = logs['session-missing-server'].read_text()
            require('Xwayland' in failure_log or 'XWayland' in failure_log,
                    'Missing-server failure did not identify XWayland')
            require(not control.exists() and not (runtime / socket_name).exists(),
                    'Missing Xwayland executable left session sockets behind')
            evidence['missing_executable'] = {'session_failed': True, 'sockets_removed': True}

        result_path.write_text(json.dumps(evidence, indent=2) + '\n')
        print('PASS: real XWayland buffers/input, X11 state/lifecycle, mixed clients, inherited DISPLAY and cleanup.')
        print(f'Logs and evidence: {args.log_dir}')
        return 0
    except Exception as error:
        print('FAIL: ' + str(error), file=sys.stderr)
        if evidence:
            print('Evidence before failure: ' + json.dumps(evidence), file=sys.stderr)
        for name, path in logs.items():
            print(name + ':\n' + '\n'.join(line[:500] for line in tail(path).splitlines()[-25:]),
                  file=sys.stderr)
        return 1
    finally:
        if x11:
            x11.abandon()
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
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print('FAIL: ' + str(error), file=sys.stderr)
        sys.exit(1)
