"""Tests for stick-scroll.py's decisions, with emulated controllers and pointer:
python3 -m unittest test_stick_scroll (in this folder)."""

import importlib.util
import os
import unittest

spec = importlib.util.spec_from_file_location(
    'stick_scroll', os.path.join(os.path.dirname(os.path.abspath(__file__)), 'stick-scroll.py'))
ss = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ss)

RIGHT, LEFT = 1, 2
TICK = 1 / 90


def run(scroller, seconds, controllers, pointer_moves=True, start=0.0):
    """Feeds `seconds` of polls; the pointer jitters (as the laser does) when
    pointer_moves. Returns the wheel buttons pressed."""
    pressed, t, n = [], start, 0
    while t < start + seconds:
        pos = (100 + n % 2, 200) if pointer_moves else (100, 200)
        pressed += scroller.update(t, pos, controllers)
        t += TICK
        n += 1
    return pressed


def sticks(right=(0.0, 0.0), left=(0.0, 0.0), right_trigger=False, left_trigger=False):
    return {RIGHT: (right_trigger, *right), LEFT: (left_trigger, *left)}


class Scroller(unittest.TestCase):
    def setUp(self):
        self.s = ss.Scroller(TICK)
        self.s.set_default([RIGHT, LEFT], RIGHT)

    def test_right_controller_scrolls_by_default(self):
        down = run(self.s, 1.0, sticks(right=(0.0, -1.0)))
        self.assertGreater(len(down), ss.MAX_STEPS_PER_SECOND * 0.8)
        self.assertEqual(set(down), {5})
        up = run(self.s, 1.0, sticks(right=(0.0, 1.0)), start=1.0)
        self.assertEqual(set(up), {4})

    def test_the_other_hands_stick_does_nothing(self):
        self.assertEqual(run(self.s, 1.0, sticks(left=(0.0, -1.0))), [])

    def test_a_trigger_pull_moves_scrolling_to_that_hand(self):
        run(self.s, 0.1, sticks(left_trigger=True))
        run(self.s, 0.1, sticks(), start=0.1)  # released
        self.assertEqual(self.s.pointing, LEFT)
        self.assertEqual(run(self.s, 1.0, sticks(right=(0.0, -1.0)), start=0.2), [])
        self.assertTrue(run(self.s, 1.0, sticks(left=(0.0, -1.0)), start=1.2))
        # Holding the trigger doesn't keep taking it back; a new pull does.
        run(self.s, 0.1, sticks(right_trigger=True), start=2.2)
        self.assertEqual(self.s.pointing, RIGHT)

    def test_no_scrolling_when_the_laser_is_not_on_the_browser(self):
        run(self.s, 0.1, sticks())  # pointer last moved at the start
        self.assertEqual(run(self.s, 1.0, sticks(right=(0.0, -1.0)), pointer_moves=False, start=1.0), [])

    def test_speed_follows_the_stick(self):
        full = len(run(self.s, 1.0, sticks(right=(0.0, -1.0))))
        s2 = ss.Scroller(TICK)
        s2.set_default([RIGHT], RIGHT)
        half = len(run(s2, 1.0, {RIGHT: (False, 0.0, -0.6)}))
        self.assertAlmostEqual(full, ss.MAX_STEPS_PER_SECOND, delta=2)
        self.assertTrue(0 < half < full / 2)

    def test_dead_zone_and_sideways(self):
        self.assertEqual(run(self.s, 1.0, sticks(right=(0.2, -0.2))), [])
        side = run(self.s, 1.0, sticks(right=(1.0, 0.0)))
        self.assertEqual(set(side), {7})

    def test_controllers_reconnecting_keep_the_choice(self):
        run(self.s, 0.1, sticks(left_trigger=True))
        self.s.set_default([RIGHT, LEFT], RIGHT)
        self.assertEqual(self.s.pointing, LEFT)
        self.s.set_default([RIGHT], RIGHT)  # left went away
        self.assertEqual(self.s.pointing, RIGHT)


if __name__ == '__main__':
    unittest.main()
