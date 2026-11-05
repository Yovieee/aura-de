"""Startup failure paths plus a real X11 client connection; no Rust build needed."""
import ctypes
import ctypes.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import smoke_nested


class XvfbStartupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='aura-xvfb-test-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.bin = self.root / 'bin'; self.bin.mkdir()
        self.logs = self.root / 'logs'

    def fake(self, body):
        executable = self.bin / 'Xvfb'
        executable.write_text(f'#!{sys.executable}\nimport os,sys,time\n' + body)
        executable.chmod(0o700)
        return dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ.get('PATH', ''),
                    DISPLAY=':54321', WAYLAND_DISPLAY='retained-host-display')

    def test_child_exit_preserves_diagnostics_and_requested_environment(self):
        marker = self.root / 'environment-marker'
        env = self.fake("open(os.environ['AURA_XVFB_TEST_MARKER'],'w').write('received')\n"
                        "sys.stderr.write('fixture initialization failed\\n'); sys.exit(7)\n")
        env['AURA_XVFB_TEST_MARKER'] = str(marker)
        with self.assertRaisesRegex(RuntimeError, 'fixture initialization failed') as raised:
            smoke_nested.start_xvfb(env, timeout=1, log_dir=self.logs)
        self.assertIn('exit=7', str(raised.exception))
        self.assertEqual(marker.read_text(), 'received')
        self.assertEqual(env['DISPLAY'], ':54321')
        self.assertEqual(env['WAYLAND_DISPLAY'], 'retained-host-display')
        self.assertIn('fixture initialization failed', next(self.logs.glob('*.log')).read_text())

    def test_silent_startup_is_bounded_and_child_is_reaped(self):
        env = self.fake("sys.stderr.write('still initializing\\n');sys.stderr.flush();time.sleep(30)\n")
        original_popen = subprocess.Popen
        children = []
        def spawn(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            children.append(child)
            self.addCleanup(smoke_nested.stop, child)
            return child
        start = time.monotonic()
        with mock.patch.object(smoke_nested.subprocess, 'Popen', side_effect=spawn):
            with self.assertRaisesRegex(RuntimeError, 'startup deadline'):
                smoke_nested.start_xvfb(env, timeout=0.2, log_dir=self.logs)
        self.assertLess(time.monotonic() - start, 3)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll(), 'failed startup left the server running')

    def test_displayfd_waits_for_complete_line(self):
        read_fd, write_fd = os.pipe()
        process = subprocess.Popen([sys.executable, '-c',
            "import os,sys,time; fd=int(sys.argv[1]); os.write(fd,b'1'); time.sleep(.05); os.write(fd,b'23\\n'); time.sleep(30)",
            str(write_fd)], pass_fds=(write_fd,))
        os.close(write_fd)
        try:
            self.assertEqual(smoke_nested._read_xvfb_display(read_fd, process, time.monotonic() + 2), '123')
        finally:
            os.close(read_fd)
            smoke_nested.stop(process)

    def test_exit_is_detected_even_if_another_writer_keeps_pipe_open(self):
        read_fd, write_fd = os.pipe()
        process = subprocess.Popen([sys.executable, '-c', 'raise SystemExit(9)'])
        start = time.monotonic()
        try:
            with self.assertRaisesRegex(RuntimeError, 'exit 9'):
                smoke_nested._read_xvfb_display(read_fd, process, start + 2)
            self.assertLess(time.monotonic() - start, 1)
        finally:
            os.close(read_fd); os.close(write_fd)
            smoke_nested.stop(process)

    def test_invalid_display_response_stops_server(self):
        env = self.fake("os.write(int(sys.argv[sys.argv.index('-displayfd')+1]),b'invalid\\n');time.sleep(30)\n")
        with self.assertRaisesRegex(RuntimeError, 'invalid display number'):
            smoke_nested.start_xvfb(env, timeout=1, log_dir=self.logs)
        self.assertEqual(env['DISPLAY'], ':54321')

    def test_announced_display_alone_is_not_ready(self):
        display = next(number for number in range(65000, 65535)
                       if not Path(f'/tmp/.X11-unix/X{number}').exists())
        env = self.fake(f"os.write(int(sys.argv[sys.argv.index('-displayfd')+1]),{str(display).encode()!r}+b'\\n');time.sleep(30)\n")
        with self.assertRaisesRegex(RuntimeError, 'X11 setup'):
            smoke_nested.start_xvfb(env, timeout=1, log_dir=self.logs)
        self.assertEqual(env['DISPLAY'], ':54321')

    def test_spawn_failure_closes_both_pipe_descriptors(self):
        env = self.fake('')
        original_pipe = os.pipe
        descriptors = []
        def record_pipe():
            pair = original_pipe(); descriptors.extend(pair); return pair
        with mock.patch.object(smoke_nested.os, 'pipe', side_effect=record_pipe), \
             mock.patch.object(smoke_nested.subprocess, 'Popen', side_effect=OSError('fixture spawn error')):
            with self.assertRaisesRegex(RuntimeError, 'fixture spawn error'):
                smoke_nested.start_xvfb(env, log_dir=self.logs)
        self.assertEqual(len(descriptors), 2)
        for descriptor in descriptors:
            with self.assertRaises(OSError):
                os.fstat(descriptor)

    def test_failed_parent_does_not_leave_a_term_ignoring_helper(self):
        pid_file = self.root / 'helper-pid'
        child = ("import os,signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                 f"open({str(pid_file)!r},'w').write(str(os.getpid()));time.sleep(30)")
        parent = ("import subprocess,sys,time;from pathlib import Path;"
                  f"subprocess.Popen([sys.executable,'-c',{child!r}]);"
                  f"p=Path({str(pid_file)!r});\nwhile not p.exists(): time.sleep(.005)\n")
        process = subprocess.Popen([sys.executable, '-c', parent], start_new_session=True)
        try:
            process.wait(timeout=2)
            smoke_nested._stop_failed_xvfb(process)
            pid = int(pid_file.read_text())
            deadline = time.monotonic() + 1
            while Path(f'/proc/{pid}/stat').exists():
                try:
                    if Path(f'/proc/{pid}/stat').read_text().split()[2] == 'Z':
                        break
                except FileNotFoundError:
                    break
                self.assertLess(time.monotonic(), deadline, 'initialization helper survived cleanup')
                time.sleep(.01)
        finally:
            smoke_nested._stop_failed_xvfb(process)

    def test_real_server_accepts_an_independent_xlib_client(self):
        env = os.environ.copy()
        env['WAYLAND_DISPLAY'] = 'retained-host-display'
        process = smoke_nested.start_xvfb(env)
        try:
            self.assertIsNone(process.poll())
            self.assertNotIn('WAYLAND_DISPLAY', env)
            library = ctypes.CDLL(ctypes.util.find_library('X11') or 'libX11.so.6')
            library.XOpenDisplay.argtypes = [ctypes.c_char_p]
            library.XOpenDisplay.restype = ctypes.c_void_p
            library.XCloseDisplay.argtypes = [ctypes.c_void_p]
            display = library.XOpenDisplay(env['DISPLAY'].encode())
            self.assertTrue(display, 'Xlib could not open the ready virtual display')
            library.XCloseDisplay(display)
        finally:
            smoke_nested.stop(process)


if __name__ == '__main__':
    unittest.main()
