# Steam Frame: thumbstick of the laser-pointing controller doesn't scroll desktop game windows

*Bug report for Valve (SteamVR for Linux / Steam Frame). Suggested tracker:
https://github.com/ValveSoftware/SteamVR-for-Linux/issues*

## Summary

On the Steam Frame, when a desktop (non-VR) game window is shown on its panel
and a controller's laser points at it, that controller's thumbstick does not
scroll the window. SteamVR binds the stick to scrolling in laser mode, but the
scroll never reaches the game window (at best a few tiny wheel steps arrive,
usually none). The other controller's thumbstick does reach the game, as
Steam's virtual Xbox pad. Pressing grip switches the pointing controller to
gamepad mode for a moment, during which its stick reaches the game, then Steam
switches it back to the laser on its own.

## Environment

- Steam Frame, SteamOS 0.3.0 (build 20260922.6101926), kernel 6.18.0
- SteamVR 2.17.10 (version.txt 1789606310), driver `frame_controller`
- Steam client build 1791328779
- App: Chromium 157 (arm64), added to the library as a non-Steam game and run
  in Gaming Mode (gamescope, Xwayland), so Steam shows it on its desktop game
  panel (`valve.steam.desktopgame.<appid>`, a dashboard tab with subview layers)

The same applies to any desktop app shown this way; a web browser just makes
it obvious, since scrolling is most of what you do in one.

## Steps to reproduce

1. Add any scrollable desktop app (e.g. a web browser on a long page) to the
   library as a non-Steam game and start it in VR.
2. Point the right controller's laser at the window.
3. Push the right thumbstick up and down.
4. Push the left thumbstick up and down (laser still on the right).
5. Tap grip on the right controller, then push its thumbstick again.

## Expected

Step 3 scrolls the window, the way the stick scrolls Steam's own panels and as
a mouse wheel would on a desktop.

## Actual

- Step 3: nothing, or now and then a few wheel steps of 6-26 px (a mouse wheel
  notch is about 100 px in a browser). Over a 60 s capture with the stick
  pushed repeatedly, the window received 0-8 wheel events.
- Step 4: the left stick scrolls the page, through the Steam virtual Xbox pad
  (`28de:11ff`), when the app reads gamepads.
- Step 5: after the grip tap the right stick arrives through the virtual pad
  (full +/-1.0 on its right-stick axes) for a moment, then stops by itself as
  the controller goes back to laser mode; tapping grip repeatedly gives bursts
  of scrolling.

## What we checked

- SteamVR's compositor bindings for `frame_controller`
  (`vrcompositor_bindings_frame_controller.json`) bind both thumbsticks in
  `/actions/scroll_smooth` (mode `scroll`, `scroll_mode: smooth`) and
  `/actions/scroll_discrete`. So the laser does turn the stick into scroll
  events for the panel's overlay; they just aren't forwarded to the window.
  The panel overlay has `SendVRSmoothScrollEvents` set.
- At the X level (`xinput test-xi2 --root` on the app's display) no wheel
  button events arrive while the pointing stick is pushed.
- While the laser is on the panel, other processes can't read that controller
  either: an overlay app's IVRInput action set (vector2 bound to both
  thumbsticks, priority `k_nActionSetOverlayGlobalPriorityMax`) reports
  `bActive = false` for both hands, and legacy `GetControllerState` from a
  background app returns false. When the laser leaves the panel, both read the
  stick normally. So an app has no way to work around this itself.
- `dashboard.modalGamepadAndLaser = false` (with SteamVR restarted) changes
  nothing here.

## Suggested fix

Forward the panel's `VREvent_ScrollSmooth` / `VREvent_ScrollDiscrete` events to
the desktop game window as wheel input (XI2 smooth scroll valuators, with a
delta comparable to a mouse wheel), or pass the pointing controller's
thumbstick through to the game while the laser is on its panel.
