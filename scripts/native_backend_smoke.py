#!/usr/bin/env python3
"""Check native-backend startup failures without accessing a real Linux seat or GPU.

This gate proves feature selection and fail-closed startup only. It does not
simulate DRM, present a frame, or establish native hardware support.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def finish(process):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--default-bin', type=Path, help='Compositor built without the native feature')
    parser.add_argument('--native-bin', type=Path, help='Compositor built with the native feature')
    parser.add_argument('--log-dir', type=Path, default=ROOT / 'target/native-backend-smoke-logs')
    args = parser.parse_args()
    if bool(args.default_bin) != bool(args.native_bin):
        parser.error('--default-bin and --native-bin must be supplied together')
    if args.no_build and not args.default_bin:
        parser.error('--no-build requires --default-bin and --native-bin')
    args.log_dir.mkdir(parents=True, exist_ok=True)
    result_path = args.log_dir / 'results.json'
    result_path.unlink(missing_ok=True)
    evidence = {'hardware_tested': False, 'checks': []}

    def build(arguments, label):
        path = args.log_dir / (label + '.log')
        with path.open('w') as output:
            result = subprocess.run(['cargo', 'build', '-p', 'aura-compositor', '--locked', *arguments],
                                    cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=False)
        require(result.returncode == 0, f'{label} failed; see {path}')

    # Copies live only in the temporary directory, so CI does not upload large binaries.
    # Both variants reuse Cargo's dependency cache; the default binary is restored
    # afterward for the existing nested smoke scripts.
    with tempfile.TemporaryDirectory(prefix='aura-native-smoke-') as directory:
        temporary = Path(directory)
        restore_default = False
        try:
            if args.default_bin:
                default_bin, native_bin = args.default_bin.resolve(), args.native_bin.resolve()
            else:
                metadata = json.loads(subprocess.check_output(
                    ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT))
                binary = Path(metadata['target_directory']) / 'debug/aura-compositor'
                build([], 'build-default')
                default_bin = temporary / 'default-compositor'
                shutil.copy2(binary, default_bin)
                restore_default = True
                build(['--features', 'native'], 'build-native')
                native_bin = temporary / 'native-compositor'
                shutil.copy2(binary, native_bin)
            for path in (default_bin, native_bin):
                require(path.is_file() and os.access(path, os.X_OK), f'Missing executable: {path}')

            def trial(name, executable, *, use_environment=False, device_kind='missing', native=False):
                runtime = temporary / name
                runtime.mkdir(mode=0o700)
                environment = os.environ.copy()
                for key in list(environment):
                    if key.startswith('AURA_') or key in ('DISPLAY', 'WAYLAND_DISPLAY'):
                        environment.pop(key)
                device = runtime / 'requested-card'
                if device_kind == 'regular':
                    device.write_text('This is deliberately not a DRM character device.\n')
                elif device_kind == 'non-drm-character':
                    device = Path('/dev/null')
                    require(stat.S_ISCHR(device.stat().st_mode), '/dev/null is not a character device')
                environment.update(
                    XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(runtime / 'config'),
                    AURA_CONTROL_SOCKET=str(runtime / 'control.sock'),
                    AURA_WAYLAND_DISPLAY='wayland-native-smoke',
                    AURA_DRM_DEVICE=str(device), RUST_LOG='warn',
                )
                argv = [str(executable)]
                if use_environment:
                    environment['AURA_BACKEND'] = 'drm'
                else:
                    argv += ['--backend', 'drm']
                process = subprocess.Popen(argv, cwd=ROOT, env=environment,
                                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                           text=True, start_new_session=True)
                try:
                    timed_out = False
                    try:
                        output, _ = process.communicate(timeout=8)
                    except subprocess.TimeoutExpired:
                        timed_out = True
                        finish(process)
                        output, _ = process.communicate(timeout=2)
                finally:
                    finish(process)
                (args.log_dir / (name + '.log')).write_text(output)
                require(not timed_out, f'{name} did not fail within 8 seconds; see its log')
                require(process.returncode != 0, f'{name} incorrectly reported native startup success')
                require(not (runtime / 'wayland-native-smoke').exists(),
                        f'{name} published a Wayland socket before native initialization completed')
                require(not (runtime / 'control.sock').exists(),
                        f'{name} published a control socket before native initialization completed')
                if native:
                    require('--features native' not in output, f'{name} did not use a native-enabled binary')
                    require(str(device) in output and 'native drm device' in output.lower(),
                            f'{name} did not identify the rejected native device; see its log')
                else:
                    require('requires an aura-compositor build with --features native' in output,
                            f'{name} did not explain how to enable the native backend; see its log')
                evidence['checks'].append({
                    'name': name, 'exit_code': process.returncode,
                    'wayland_published': False, 'control_published': False,
                })

            trial('disabled-cli', default_bin)
            trial('disabled-environment', default_bin, use_environment=True)
            trial('missing-card-cli', native_bin, native=True)
            trial('missing-card-environment', native_bin, native=True, use_environment=True)
            trial('regular-file-card', native_bin, native=True, device_kind='regular')
            trial('non-drm-character-card', native_bin, native=True, device_kind='non-drm-character')
        finally:
            if restore_default:
                build([], 'restore-default')
    result_path.write_text(json.dumps(evidence, indent=2) + '\n')
    print('PASS: native feature selection and missing/non-device startup fail before publishing sockets.')
    print('No seat, KMS presentation, physical input or VT switching was exercised.')
    print(f'Logs and evidence: {args.log_dir}')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        sys.exit(1)
