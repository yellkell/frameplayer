//! The latest playback status, shared between the render thread (which
//! publishes it every frame) and the server threads (which poll it).

use fp_core::{PlaybackStatus, PlayerCommand};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Locks a mutex, carrying on if a panicking thread poisoned it: every
/// value we guard stays consistent between statements.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Latest status plus a change counter.
#[derive(Default)]
pub(crate) struct StatusCell {
    status: Mutex<PlaybackStatus>,
    version: AtomicU64,
}

impl StatusCell {
    /// Stores `s` if it differs, reusing the existing string buffers so the
    /// per-frame call does not allocate.
    pub(crate) fn publish(&self, s: &PlaybackStatus) {
        let mut g = lock(&self.status);
        if *g == *s {
            return;
        }
        // Exhaustive destructuring: a new field in fp-core must be added here.
        let PlaybackStatus {
            location,
            title,
            duration,
            position,
            speed,
            playing,
            sampled_at_ms,
        } = s;
        g.location.clone_from(location);
        g.title.clone_from(title);
        g.duration = *duration;
        g.position = *position;
        g.speed = *speed;
        g.playing = *playing;
        g.sampled_at_ms = *sampled_at_ms;
        drop(g);
        self.version.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn snapshot(&self) -> PlaybackStatus {
        lock(&self.status).clone()
    }

    /// Location of the open media, for deciding whether a DeoVR `path`
    /// means "open something else".
    pub(crate) fn location(&self) -> String {
        lock(&self.status).location.clone()
    }

    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Position jumps larger than this (after extrapolation) count as a seek.
const SEEK_THRESHOLD_S: f64 = 0.75;

/// Whether `cur` differs from what clients last saw (`prev`) in a way they
/// cannot extrapolate: different media, play/pause, speed, duration, title,
/// or a seek. Ordinary playback progress is not a change.
pub fn significant_change(prev: &PlaybackStatus, cur: &PlaybackStatus, now_ms: u64) -> bool {
    prev.location != cur.location
        || prev.title != cur.title
        || prev.playing != cur.playing
        || (prev.speed - cur.speed).abs() > 1e-3
        || (prev.duration - cur.duration).abs() > 0.01
        || (prev.position_at(now_ms) - cur.position_at(now_ms)).abs() > SEEK_THRESHOLD_S
}

/// Decides when a server should push status to its clients: on the first
/// poll, on every [`significant_change`], and every `refresh` while playing.
pub(crate) struct ChangeTracker {
    last: Option<PlaybackStatus>,
    last_version: u64,
    last_sent: Instant,
    refresh: Duration,
}

impl ChangeTracker {
    pub(crate) fn new(refresh: Duration) -> ChangeTracker {
        ChangeTracker {
            last: None,
            last_version: 0,
            last_sent: Instant::now(),
            refresh,
        }
    }

    /// The status to send now, if any. Skips the snapshot entirely while
    /// nothing was published and no periodic refresh is due.
    pub(crate) fn poll(&mut self, cell: &StatusCell, now_ms: u64) -> Option<PlaybackStatus> {
        let refresh_due = self.last_sent.elapsed() >= self.refresh;
        let version = cell.version();
        if let Some(prev) = &self.last {
            let was_playing = prev.playing && !prev.location.is_empty();
            if version == self.last_version && !(was_playing && refresh_due) {
                return None;
            }
        }
        self.last_version = version;
        let snap = cell.snapshot();
        let due = match &self.last {
            None => true,
            Some(prev) => {
                significant_change(prev, &snap, now_ms)
                    || (snap.playing && !snap.location.is_empty() && refresh_due)
            }
        };
        if !due {
            return None;
        }
        self.last = Some(snap.clone());
        self.last_sent = Instant::now();
        Some(snap)
    }
}

/// Longest location accepted from a remote client.
const MAX_LOCATION_LEN: usize = 8192;

/// Rejects commands with values the player should never see from the
/// network: non-finite or negative times, non-positive speeds, empty or
/// oversized locations.
pub(crate) fn command_is_valid(c: &PlayerCommand) -> bool {
    match c {
        PlayerCommand::Open { location } => {
            !location.trim().is_empty() && location.len() <= MAX_LOCATION_LEN
        }
        PlayerCommand::Seek { position } => position.is_finite() && *position >= 0.0,
        PlayerCommand::SeekRelative { delta } => delta.is_finite(),
        PlayerCommand::SetSpeed { speed } => speed.is_finite() && *speed > 0.0 && *speed <= 16.0,
        PlayerCommand::Play
        | PlayerCommand::Pause
        | PlayerCommand::TogglePause
        | PlayerCommand::Stop
        | PlayerCommand::Recenter => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> PlaybackStatus {
        PlaybackStatus {
            location: "/v.mp4".into(),
            title: "V".into(),
            duration: 100.0,
            position: 10.0,
            speed: 1.0,
            playing: true,
            sampled_at_ms: 1_000,
        }
    }

    #[test]
    fn progress_is_not_a_change_but_seek_is() {
        let a = playing();
        let b = PlaybackStatus {
            position: 12.0,
            sampled_at_ms: 3_000,
            ..a.clone()
        };
        assert!(!significant_change(&a, &b, 3_000));
        let seek = PlaybackStatus {
            position: 50.0,
            ..b.clone()
        };
        assert!(significant_change(&a, &seek, 3_000));
        let paused = PlaybackStatus {
            playing: false,
            ..b.clone()
        };
        assert!(significant_change(&a, &paused, 3_000));
        let other = PlaybackStatus {
            location: "/w.mp4".into(),
            ..b
        };
        assert!(significant_change(&a, &other, 3_000));
    }

    #[test]
    fn publish_bumps_version_only_on_difference() {
        let c = StatusCell::default();
        let v0 = c.version();
        c.publish(&PlaybackStatus::default());
        assert_eq!(c.version(), v0);
        c.publish(&playing());
        assert_eq!(c.version(), v0 + 1);
        c.publish(&playing());
        assert_eq!(c.version(), v0 + 1);
        assert_eq!(c.snapshot(), playing());
        assert_eq!(c.location(), "/v.mp4");
    }

    #[test]
    fn tracker_sends_first_changes_and_refreshes() {
        let cell = StatusCell::default();
        let mut t = ChangeTracker::new(Duration::ZERO);
        assert_eq!(t.poll(&cell, 0), Some(PlaybackStatus::default()));
        // Idle and unchanged: nothing, even with a zero refresh interval.
        assert_eq!(t.poll(&cell, 0), None);
        cell.publish(&playing());
        assert_eq!(t.poll(&cell, 1_000), Some(playing()));
        // Playing: refresh due immediately with a zero interval.
        assert_eq!(t.poll(&cell, 1_000), Some(playing()));

        let mut slow = ChangeTracker::new(Duration::from_secs(3600));
        assert!(slow.poll(&cell, 1_000).is_some());
        // Ordinary progress is not sent before the refresh interval...
        cell.publish(&PlaybackStatus {
            position: 11.0,
            sampled_at_ms: 2_000,
            ..playing()
        });
        assert_eq!(slow.poll(&cell, 2_000), None);
        // ...but a pause is.
        cell.publish(&PlaybackStatus {
            playing: false,
            ..playing()
        });
        assert!(slow.poll(&cell, 2_000).is_some_and(|s| !s.playing));
    }

    #[test]
    fn validates_commands() {
        assert!(command_is_valid(&PlayerCommand::Seek { position: 1.0 }));
        assert!(!command_is_valid(&PlayerCommand::Seek { position: -1.0 }));
        assert!(!command_is_valid(&PlayerCommand::Seek {
            position: f64::NAN
        }));
        assert!(!command_is_valid(&PlayerCommand::SetSpeed { speed: 0.0 }));
        assert!(!command_is_valid(&PlayerCommand::Open {
            location: " ".into()
        }));
        assert!(command_is_valid(&PlayerCommand::TogglePause));
    }
}
