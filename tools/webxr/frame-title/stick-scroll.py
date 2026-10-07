#!/usr/bin/env python3
"""Thumbstick scrolling for Chromium XR on the Steam Frame.

While a controller's laser points at the browser, pushing that controller's
thumbstick scrolls what is under the pointer, like a mouse wheel. launch.sh
starts this next to Chromium; it exits with it.

Steam doesn't do this itself: it turns the pointing hand's stick into a few
weak wheel steps and passes the sticks to Chromium only now and then (as a
virtual Xbox pad, hidden from pages until a button is pressed). So this reads
the sticks straight from SteamVR, as a background OpenVR app, and sends wheel
"button" presses (4/5 up/down, 6/7 left/right) to the browser's X display with
XTEST.

"Pointing at the browser" is the X pointer having moved recently: the laser
moves it all the time (hand tremor alone, ~70 times a second), and it stops
when the laser leaves the browser or a WebXR game hides the laser. The laser
is on the controller whose trigger was pulled last, as SteamVR moves it; the
right one until a trigger is pulled.

Standard library only (ctypes): runs with the Python 3 that SteamOS ships.
"""

import ctypes
import ctypes.util
import os
import sys
import time

POLL_HZ = 90
DEAD_ZONE = 0.25
MAX_STEPS_PER_SECOND = 16   # wheel steps at full deflection (Chromium: 100 px each)
POINTING_WINDOW = 0.35      # seconds since the pointer last moved
IDLE_POLL = 0.05            # seconds between polls while not pointing

OPENVR = "/opt/steamvr/bin/linuxarm64/libopenvr_api.so"
VRApplication_Background = 3
TrackedDeviceClass_Controller = 2
# FnTable:IVRSystem_022 slots.
GET_CONTROLLER_ROLE = 18
GET_TRACKED_DEVICE_CLASS = 19
GET_CONTROLLER_STATE = 33
TrackedControllerRole_RightHand = 2
TRIGGER_BUTTON = 1 << 33  # k_EButton_SteamVR_Trigger


class Axis(ctypes.Structure):
    _pack_ = 4
    _fields_ = [("x", ctypes.c_float), ("y", ctypes.c_float)]


class ControllerState(ctypes.Structure):
    _pack_ = 4  # openvr.h packs to 4 on Linux
    _fields_ = [("packet", ctypes.c_uint32), ("pressed", ctypes.c_uint64),
                ("touched", ctypes.c_uint64), ("axis", Axis * 5)]


def open_steamvr():
    """IVRSystem's GetTrackedDeviceClass, GetControllerRoleForTrackedDeviceIndex
    and GetControllerState, or None."""
    try:
        lib = ctypes.CDLL(OPENVR)
    except OSError:
        return None
    err = ctypes.c_int(0)
    lib.VR_InitInternal2.restype = ctypes.c_uint32
    lib.VR_InitInternal2.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_int, ctypes.c_char_p]
    lib.VR_GetGenericInterface.restype = ctypes.c_void_p
    lib.VR_GetGenericInterface.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_int)]
    lib.VR_InitInternal2(ctypes.byref(err), VRApplication_Background, None)
    if err.value:
        return None
    table = lib.VR_GetGenericInterface(b"FnTable:IVRSystem_022", ctypes.byref(err))
    if not table or err.value:
        return None
    fns = ctypes.cast(table, ctypes.POINTER(ctypes.c_void_p))
    device_class = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_uint32)(fns[GET_TRACKED_DEVICE_CLASS])
    role = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_uint32)(fns[GET_CONTROLLER_ROLE])
    state = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_uint32, ctypes.POINTER(ControllerState),
                             ctypes.c_uint32)(fns[GET_CONTROLLER_STATE])
    return device_class, role, state


class X:
    """The browser's X display: pointer position and XTEST wheel presses."""

    def __init__(self):
        x11 = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        xtst = ctypes.CDLL(ctypes.util.find_library("Xtst") or "libXtst.so.6")
        x11.XOpenDisplay.restype = ctypes.c_void_p
        x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x11.XDefaultRootWindow.restype = ctypes.c_ulong
        x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        x11.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong] + [ctypes.c_void_p] * 7
        x11.XFlush.argtypes = [ctypes.c_void_p]
        xtst.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        self.dpy = x11.XOpenDisplay(None)
        if not self.dpy:
            raise OSError("no X display")
        self.x11, self.xtst = x11, xtst
        self.root = x11.XDefaultRootWindow(self.dpy)

    def pointer(self):
        root, child = ctypes.c_ulong(), ctypes.c_ulong()
        rx, ry, wx, wy, mask = (ctypes.c_int() for _ in range(5))
        self.x11.XQueryPointer(self.dpy, self.root, ctypes.byref(root), ctypes.byref(child),
                               ctypes.byref(rx), ctypes.byref(ry), ctypes.byref(wx),
                               ctypes.byref(wy), ctypes.byref(mask))
        return rx.value, ry.value

    def wheel(self, button):
        self.xtst.XTestFakeButtonEvent(self.dpy, button, 1, 0)
        self.xtst.XTestFakeButtonEvent(self.dpy, button, 0, 0)
        self.x11.XFlush(self.dpy)


def debug(msg):
    if os.environ.get("STICK_SCROLL_DEBUG"):
        print(time.strftime("%H:%M:%S"), msg, flush=True)


def rate(v):
    """Wheel steps per second for a stick axis value in [-1, 1]."""
    a = abs(v)
    if a < DEAD_ZONE:
        return 0.0
    return MAX_STEPS_PER_SECOND * ((a - DEAD_ZONE) / (1 - DEAD_ZONE)) ** 2


class Scroller:
    """Decides the wheel steps from what the controllers and pointer do.

    update() gets the time, the X pointer position and, per controller index,
    (trigger held, stick x, stick y); it returns X wheel buttons to press.
    """

    def __init__(self, tick):
        self.tick = tick
        self.pointing = None      # the controller the laser is on
        self.triggered = set()    # controllers holding their trigger
        self.last_pos, self.moved_at = None, float("-inf")
        self.owed_x = self.owed_y = 0.0  # fractional steps carried over

    def set_default(self, controllers, right):
        """Before any trigger pull the laser is on the right controller."""
        if self.pointing not in controllers:
            self.pointing = right if right in controllers else (controllers or [None])[0]

    def pointing_at_browser(self, now):
        return now - self.moved_at < POINTING_WINDOW

    def update(self, now, pos, controllers):
        if pos != self.last_pos:
            self.last_pos, self.moved_at = pos, now
        for i, (trigger, _, _) in controllers.items():
            if trigger and i not in self.triggered:
                self.pointing = i  # a new pull moves the laser here
                debug(f"trigger on {i}: pointing")
            (self.triggered.add if trigger else self.triggered.discard)(i)
        _, sx, sy = controllers.get(self.pointing, (False, 0.0, 0.0))
        if not self.pointing_at_browser(now) or not (rate(sx) or rate(sy)):
            self.owed_x = self.owed_y = 0.0
            return []
        self.owed_y += rate(sy) * self.tick
        self.owed_x += rate(sx) * self.tick
        buttons = []
        while self.owed_y >= 1:
            buttons.append(4 if sy > 0 else 5)  # stick up scrolls up, like a wheel
            self.owed_y -= 1
        while self.owed_x >= 1:
            buttons.append(7 if sx > 0 else 6)
            self.owed_x -= 1
        if buttons:
            debug(f"scroll with {self.pointing}: stick {sx:.2f},{sy:.2f}")
        return buttons


def main():
    parent = os.getppid()
    vr = open_steamvr()
    if not vr:
        return 0  # no SteamVR: nothing to do
    device_class, role, get_state = vr
    try:
        x = X()
    except OSError:
        return 0
    tick = 1.0 / POLL_HZ
    scroller = Scroller(tick)
    controllers, found_at = [], float("-inf")
    while os.getppid() == parent:  # the launcher went: Chromium has exited
        now = time.monotonic()
        if now - found_at > 5:  # controllers can connect later or reconnect
            controllers = [i for i in range(16) if device_class(i) == TrackedDeviceClass_Controller]
            right = next((i for i in controllers if role(i) == TrackedControllerRole_RightHand), None)
            scroller.set_default(controllers, right)
            found_at = now
        states = {}
        for i in controllers:
            s = ControllerState()
            if get_state(i, ctypes.byref(s), ctypes.sizeof(s)):
                states[i] = (bool(s.pressed & TRIGGER_BUTTON), s.axis[0].x, s.axis[0].y)
        for button in scroller.update(now, x.pointer(), states):
            x.wheel(button)
        # Full rate only while the laser is on the browser; enough to catch
        # trigger pulls otherwise.
        time.sleep(tick if scroller.pointing_at_browser(now) else IDLE_POLL)
    return 0


if __name__ == "__main__":
    sys.exit(main())
