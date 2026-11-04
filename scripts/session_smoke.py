#!/usr/bin/env python3
"""Run the real supervisor and exercise Dock -> launcher -> actual app execution via X11 input."""
import argparse
import ctypes as C
import ctypes.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from integration_desktop import command, require, wait_for
from smoke_nested import ROOT, start_xvfb, stop, tail

class Input:
    def __init__(self, display):
        self.x=C.CDLL(ctypes.util.find_library('X11') or 'libX11.so.6')
        self.t=C.CDLL(ctypes.util.find_library('Xtst') or 'libXtst.so.6')
        self.x.XOpenDisplay.argtypes=[C.c_char_p];self.x.XOpenDisplay.restype=C.c_void_p
        self.d=self.x.XOpenDisplay(display.encode());require(self.d,'Cannot open test X display')
        self.x.XDefaultRootWindow.argtypes=[C.c_void_p];self.x.XDefaultRootWindow.restype=C.c_ulong
        self.x.XQueryTree.argtypes=[C.c_void_p,C.c_ulong,C.POINTER(C.c_ulong),C.POINTER(C.c_ulong),C.POINTER(C.POINTER(C.c_ulong)),C.POINTER(C.c_uint)]
        self.x.XFetchName.argtypes=[C.c_void_p,C.c_ulong,C.POINTER(C.c_char_p)]
        self.x.XFree.argtypes=[C.c_void_p]
        self.x.XSetInputFocus.argtypes=[C.c_void_p,C.c_ulong,C.c_int,C.c_ulong]
        self.x.XFlush.argtypes=[C.c_void_p]
        self.x.XKeysymToKeycode.argtypes=[C.c_void_p,C.c_ulong];self.x.XKeysymToKeycode.restype=C.c_uint
        self.x.XCloseDisplay.argtypes=[C.c_void_p]
        self.t.XTestFakeKeyEvent.argtypes=[C.c_void_p,C.c_uint,C.c_int,C.c_ulong]
        self.t.XTestFakeButtonEvent.argtypes=[C.c_void_p,C.c_uint,C.c_int,C.c_ulong]
        self.t.XTestFakeMotionEvent.argtypes=[C.c_void_p,C.c_int,C.c_int,C.c_int,C.c_ulong]
        root=self.x.XDefaultRootWindow(self.d)
        parent=C.c_ulong();actual_root=C.c_ulong();children=C.POINTER(C.c_ulong)();count=C.c_uint()
        require(self.x.XQueryTree(self.d,root,C.byref(actual_root),C.byref(parent),C.byref(children),C.byref(count)),'Cannot inspect test windows')
        window=None
        for i in range(count.value):
            name=C.c_char_p()
            if self.x.XFetchName(self.d,children[i],C.byref(name)) and name.value:
                if b'Aura DE' in name.value:window=children[i]
                self.x.XFree(C.cast(name,C.c_void_p))
        if children:self.x.XFree(children)
        require(window is not None,'Aura nested X11 window is missing')
        self.x.XSetInputFocus(self.d,window,2,0);self.x.XFlush(self.d)
    def key(self, symbol):
        code=self.x.XKeysymToKeycode(self.d,symbol)
        require(code!=0,f'No X11 keycode for {symbol}')
        for pressed in [1,0]:self.t.XTestFakeKeyEvent(self.d,code,pressed,0)
        self.x.XFlush(self.d)
    def click(self,x,y):
        self.t.XTestFakeMotionEvent(self.d,0,x,y,0)
        for pressed in [1,0]:self.t.XTestFakeButtonEvent(self.d,1,pressed,0)
        self.x.XFlush(self.d)
    def close(self):self.x.XCloseDisplay(self.d)

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--no-build',action='store_true')
    p.add_argument('--capture',type=Path,help='optional screenshot of this isolated test display (requires ImageMagick import)')
    p.add_argument('--inside-bus',action='store_true',help=argparse.SUPPRESS)
    args=p.parse_args()
    if not os.environ.get('DBUS_SESSION_BUS_ADDRESS'):
        require(not args.inside_bus,'Session bus unavailable')
        return subprocess.run(['dbus-run-session','--',sys.executable,str(Path(__file__).resolve()),*sys.argv[1:],'--inside-bus'],check=False).returncode
    if not args.no_build:subprocess.run(['cargo','build','--workspace','--bins','--locked'],cwd=ROOT,check=True)
    target=Path(json.loads(subprocess.check_output(['cargo','metadata','--no-deps','--locked','--format-version','1'],cwd=ROOT))['target_directory'])/'debug'
    env=os.environ.copy();env['LIBGL_ALWAYS_SOFTWARE']='1';env['RUST_LOG']='info';env['AURA_COMPOSITOR_GLASS']='1'
    # Always isolate X11 so synthetic input can never reach a user's desktop.
    xvfb=start_xvfb(env);session=None;input_=None;logs=ROOT/'target'/'session-smoke-logs';logs.mkdir(parents=True,exist_ok=True)
    log=logs/'session.log'
    try:
        with tempfile.TemporaryDirectory(prefix='aura-session-smoke-') as directory, log.open('w') as output:
            runtime=Path(directory);os.chmod(runtime,0o700)
            data=runtime/'data';applications=data/'applications';applications.mkdir(parents=True)
            marker=runtime/'application-launched'
            (applications/'aura-smoke-marker.desktop').write_text(f'[Desktop Entry]\nType=Application\nName=aurasmokemarker\nExec=/usr/bin/touch {marker}\n')
            env.update(XDG_RUNTIME_DIR=directory,XDG_DATA_HOME=str(data),AURA_CONTROL_SOCKET=str(runtime/'aura-control.sock'),AURA_WINIT_RESOLUTION='1024x640',AURA_OUTPUT_SCALE='1')
            session=subprocess.Popen([str(target/'aura-session'),'--socket','wayland-session-smoke','--no-portal'],cwd=ROOT,env=env,stdout=output,stderr=subprocess.STDOUT)
            control=runtime/'aura-control.sock'
            def status():
                if not control.exists():return {}
                return json.loads(command(control,'status'))
            wait_for(lambda:status().get('mapped_layers')==2,'supervised menu bar and Dock',session,15)
            time.sleep(0.2)
            input_=Input(env['DISPLAY'])
            if args.capture:
                args.capture.parent.mkdir(parents=True,exist_ok=True)
                subprocess.run(['import','-display',env['DISPLAY'],'-window','root',str(args.capture)],check=True,timeout=10)
            # Dock's Apps tile, centered within the 720x112 layer at the bottom.
            input_.click(180,550)
            wait_for(lambda:status().get('mapped_layers')==3,'Dock-launched overlay',session,10)
            time.sleep(0.2)  # Allow the overlay to bind its advertised keyboard capability.
            input_.key(0xff1b)
            wait_for(lambda:status().get('layers')==2,'Escape closing launcher and restoring focus',session,10)
            input_.click(180,550)
            wait_for(lambda:status().get('mapped_layers')==3,'second launcher overlay',session,10)
            time.sleep(0.2)
            for character in 'aurasmokemarker':input_.key(ord(character))
            # Query redraw occurs after compositor-delivered input and a frame callback.
            time.sleep(0.15)
            if args.capture:
                args.capture.parent.mkdir(parents=True,exist_ok=True)
                subprocess.run(['import','-display',env['DISPLAY'],'-window','root',str(args.capture)],check=True,timeout=10)
            input_.key(0xff0d)
            wait_for(marker.exists,'actual desktop-entry application execution',session,10)
            wait_for(lambda:status().get('layers')==2,'launcher exit after successful launch',session,10)
            session.terminate();require(session.wait(timeout=10)==0,'supervisor did not shut down cleanly')
            require(not control.exists(),'control socket leaked after supervisor shutdown')
            require(not (runtime/'wayland-session-smoke').exists(),'Wayland socket leaked after supervisor shutdown')
        print('PASS: real supervisor, Dock pointer input, launcher keyboard/Escape, XDG app execution and shutdown cleanup.')
        print(f'Logs: {logs}')
        return 0
    except Exception as error:
        print(f'FAIL: {error}\n{tail(log)}',file=sys.stderr);return 1
    finally:
        if input_:input_.close()
        stop(session);stop(xvfb)

if __name__=='__main__':sys.exit(main())
