//! Playback status and commands, shared by the player, the haptics engine
//! and the remote-control API.

use serde::{Deserialize, Serialize};

/// What the player is doing right now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackStatus {
    /// Location of the open media (path or URL); empty when idle.
    pub location: String,
    pub title: String,
    /// Seconds; 0 when unknown.
    pub duration: f64,
    /// Seconds from the start.
    pub position: f64,
    /// 1.0 is normal speed.
    pub speed: f64,
    pub playing: bool,
    /// Unix time in milliseconds when `position` was sampled, so consumers
    /// can extrapolate between updates.
    pub sampled_at_ms: u64,
}

impl PlaybackStatus {
    /// Position extrapolated to `now_ms`.
    pub fn position_at(&self, now_ms: u64) -> f64 {
        if !self.playing || now_ms <= self.sampled_at_ms {
            return self.position;
        }
        let p = self.position + (now_ms - self.sampled_at_ms) as f64 / 1000.0 * self.speed;
        if self.duration > 0.0 {
            p.min(self.duration)
        } else {
            p
        }
    }
}

/// Requests from outside the player (remote API, web remote, UI).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum PlayerCommand {
    Open { location: String },
    Play,
    Pause,
    TogglePause,
    Seek { position: f64 },
    SeekRelative { delta: f64 },
    SetSpeed { speed: f64 },
    Stop,
    Recenter,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrapolates_while_playing() {
        let s = PlaybackStatus {
            duration: 100.0,
            position: 10.0,
            speed: 2.0,
            playing: true,
            sampled_at_ms: 1000,
            ..Default::default()
        };
        assert_eq!(s.position_at(1500), 11.0);
        assert_eq!(s.position_at(999_000), 100.0);
        let paused = PlaybackStatus {
            playing: false,
            ..s.clone()
        };
        assert_eq!(paused.position_at(5000), 10.0);
    }

    #[test]
    fn command_json() {
        let c: PlayerCommand = serde_json::from_str(r#"{"cmd":"seek","position":12.5}"#).unwrap();
        assert_eq!(c, PlayerCommand::Seek { position: 12.5 });
    }
}
