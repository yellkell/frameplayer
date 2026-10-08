//! The playback clock: media time advancing with wall time at the current
//! speed. Video is the master; audio follows it.

use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy)]
struct State {
    base: f64,
    at: Instant,
    speed: f64,
    running: bool,
}

pub struct Clock {
    s: Mutex<State>,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            s: Mutex::new(State {
                base: 0.0,
                at: Instant::now(),
                speed: 1.0,
                running: false,
            }),
        }
    }
}

impl Clock {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.s.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now(&self) -> f64 {
        let s = *self.lock();
        if s.running {
            s.base + s.at.elapsed().as_secs_f64() * s.speed
        } else {
            s.base
        }
    }

    pub fn running(&self) -> bool {
        self.lock().running
    }

    pub fn speed(&self) -> f64 {
        self.lock().speed
    }

    pub fn set(&self, t: f64) {
        let mut s = self.lock();
        s.base = t;
        s.at = Instant::now();
    }

    fn rebase(s: &mut State) {
        if s.running {
            s.base += s.at.elapsed().as_secs_f64() * s.speed;
        }
        s.at = Instant::now();
    }

    pub fn pause(&self) {
        let mut s = self.lock();
        Self::rebase(&mut s);
        s.running = false;
    }

    pub fn resume(&self) {
        let mut s = self.lock();
        Self::rebase(&mut s);
        s.running = true;
    }

    pub fn set_speed(&self, speed: f64) {
        let mut s = self.lock();
        Self::rebase(&mut s);
        s.speed = speed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn advances_only_when_running() {
        let c = Clock::default();
        c.set(5.0);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(c.now(), 5.0);
        c.resume();
        c.set_speed(2.0);
        std::thread::sleep(Duration::from_millis(50));
        let t = c.now();
        assert!(t > 5.08 && t < 5.4, "{t}");
        c.pause();
        let p = c.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.now(), p);
    }
}
