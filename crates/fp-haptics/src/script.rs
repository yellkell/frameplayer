//! A single-axis script timeline: positions over time.

use serde::{Deserialize, Serialize};

/// One point of a script: be at `pos` at time `at`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// Script time in milliseconds.
    pub at: i64,
    /// Normalised position, `0.0..=1.0` (funscript `pos` / 100).
    pub pos: f32,
}

impl Action {
    /// Creates an action from a time in milliseconds and a normalised
    /// position.
    pub fn new(at: i64, pos: f32) -> Action {
        Action { at, pos }
    }
}

/// Summary numbers for a script. Speeds are in funscript units (0–100) per
/// second, the convention used by OFS and script sites.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptStats {
    /// Number of actions.
    pub action_count: usize,
    /// Time of the last action, milliseconds.
    pub duration_ms: i64,
    /// Distance travelled divided by the time spent moving (segments with no
    /// change of position are left out).
    pub average_speed: f32,
    /// Fastest segment.
    pub max_speed: f32,
}

/// A sorted, de-duplicated list of actions for one axis.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Script {
    actions: Vec<Action>,
}

impl Script {
    /// Builds a script from actions in any order. Positions are clamped to
    /// `0..=1`, non-finite positions are dropped, actions are sorted by time
    /// and, when several share a timestamp, the last one in input order
    /// wins.
    pub fn new(mut actions: Vec<Action>) -> Script {
        actions.retain(|a| a.pos.is_finite());
        for a in &mut actions {
            a.pos = a.pos.clamp(0.0, 1.0);
        }
        // Stable sort keeps input order among equal timestamps.
        actions.sort_by_key(|a| a.at);
        let mut out: Vec<Action> = Vec::with_capacity(actions.len());
        for a in actions {
            match out.last_mut() {
                Some(last) if last.at == a.at => *last = a,
                _ => out.push(a),
            }
        }
        Script { actions: out }
    }

    /// The actions, sorted by time with unique timestamps.
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// Number of actions.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// True when the script has no actions.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Time of the last action in milliseconds (0 for an empty script).
    pub fn duration_ms(&self) -> i64 {
        self.actions.last().map(|a| a.at.max(0)).unwrap_or(0)
    }

    /// Position at script time `t_ms`, linearly interpolated between the
    /// surrounding actions. Before the first action it is the first
    /// action's position, after the last the last one's. `None` when empty.
    pub fn position_at(&self, t_ms: i64) -> Option<f32> {
        let first = self.actions.first()?;
        let i = self.actions.partition_point(|a| a.at <= t_ms);
        if i == 0 {
            return Some(first.pos);
        }
        let a = self.actions[i - 1];
        match self.actions.get(i) {
            None => Some(a.pos),
            Some(b) => {
                let span = (b.at - a.at) as f64;
                let f = ((t_ms - a.at) as f64 / span) as f32;
                Some(a.pos + (b.pos - a.pos) * f)
            }
        }
    }

    /// The first action strictly after `t_ms`, with its index.
    pub fn next_action_after(&self, t_ms: i64) -> Option<(usize, Action)> {
        let i = self.actions.partition_point(|a| a.at <= t_ms);
        self.actions.get(i).map(|a| (i, *a))
    }

    /// The last action at or before `t_ms`, with its index.
    pub fn action_at_or_before(&self, t_ms: i64) -> Option<(usize, Action)> {
        let i = self.actions.partition_point(|a| a.at <= t_ms);
        i.checked_sub(1).map(|j| (j, self.actions[j]))
    }

    /// Speed of the segment that ends at action `index`, in funscript units
    /// per second; 0 for the first action.
    pub fn speed_into(&self, index: usize) -> f32 {
        match (
            index.checked_sub(1).and_then(|p| self.actions.get(p)),
            self.actions.get(index),
        ) {
            (Some(a), Some(b)) => segment_speed(a, b),
            _ => 0.0,
        }
    }

    /// Statistics over the whole script.
    pub fn stats(&self) -> ScriptStats {
        let mut distance = 0.0f64;
        let mut moving_ms = 0i64;
        let mut max_speed = 0.0f32;
        for w in self.actions.windows(2) {
            let d = (w[1].pos - w[0].pos).abs();
            if d > 0.0 {
                distance += f64::from(d) * 100.0;
                moving_ms += w[1].at - w[0].at;
                max_speed = max_speed.max(segment_speed(&w[0], &w[1]));
            }
        }
        let average_speed = if moving_ms > 0 {
            (distance / (moving_ms as f64 / 1000.0)) as f32
        } else {
            0.0
        };
        ScriptStats {
            action_count: self.actions.len(),
            duration_ms: self.duration_ms(),
            average_speed,
            max_speed,
        }
    }

    /// A copy with every position passed through `f` (result clamped to
    /// `0..=1`).
    pub fn map_positions(&self, f: impl Fn(f32) -> f32) -> Script {
        Script {
            actions: self
                .actions
                .iter()
                .map(|a| Action::new(a.at, f(a.pos).clamp(0.0, 1.0)))
                .collect(),
        }
    }

    /// A copy shifted by `offset_ms` (positive moves actions later).
    pub fn shifted(&self, offset_ms: i64) -> Script {
        Script {
            actions: self
                .actions
                .iter()
                .map(|a| Action::new(a.at + offset_ms, a.pos))
                .collect(),
        }
    }

    /// The script as `time_ms,position` lines (position 0–100, integer), the
    /// CSV layout script hosts and The Handy accept. Actions before time 0
    /// are left out.
    pub fn to_csv(&self) -> String {
        let mut s = String::with_capacity(self.actions.len() * 10);
        for a in self.actions.iter().filter(|a| a.at >= 0) {
            s.push_str(&format!("{},{}\n", a.at, (a.pos * 100.0).round() as i32));
        }
        s
    }
}

/// Speed between two actions in funscript units per second (0 when they
/// share a timestamp).
pub(crate) fn segment_speed(a: &Action, b: &Action) -> f32 {
    let dt = (b.at - a.at) as f32 / 1000.0;
    if dt <= 0.0 {
        0.0
    } else {
        (b.pos - a.pos).abs() * 100.0 / dt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(points: &[(i64, f32)]) -> Script {
        Script::new(points.iter().map(|&(t, p)| Action::new(t, p)).collect())
    }

    #[test]
    fn sorts_dedups_and_clamps() {
        let sc = s(&[
            (500, 1.0),
            (0, 0.0),
            (500, 0.5),
            (250, 2.0),
            (100, f32::NAN),
        ]);
        assert_eq!(
            sc.actions(),
            &[
                Action::new(0, 0.0),
                Action::new(250, 1.0),
                Action::new(500, 0.5)
            ]
        );
    }

    #[test]
    fn interpolates_positions() {
        let sc = s(&[(1000, 0.0), (2000, 1.0), (3000, 0.5)]);
        assert_eq!(sc.position_at(0), Some(0.0));
        assert_eq!(sc.position_at(1000), Some(0.0));
        assert_eq!(sc.position_at(1500), Some(0.5));
        assert_eq!(sc.position_at(2000), Some(1.0));
        assert!((sc.position_at(2500).unwrap() - 0.75).abs() < 1e-6);
        assert_eq!(sc.position_at(9999), Some(0.5));
        assert_eq!(Script::default().position_at(0), None);
    }

    #[test]
    fn finds_next_action() {
        let sc = s(&[(1000, 0.0), (2000, 1.0)]);
        assert_eq!(sc.next_action_after(0), Some((0, Action::new(1000, 0.0))));
        assert_eq!(
            sc.next_action_after(1000),
            Some((1, Action::new(2000, 1.0)))
        );
        assert_eq!(
            sc.next_action_after(1999),
            Some((1, Action::new(2000, 1.0)))
        );
        assert_eq!(sc.next_action_after(2000), None);
        assert_eq!(sc.action_at_or_before(999), None);
        assert_eq!(
            sc.action_at_or_before(1500),
            Some((0, Action::new(1000, 0.0)))
        );
    }

    #[test]
    fn computes_stats() {
        // 0→100 in 500 ms (200 u/s), hold 1 s, 100→50 in 1 s (50 u/s).
        let sc = s(&[(0, 0.0), (500, 1.0), (1500, 1.0), (2500, 0.5)]);
        let st = sc.stats();
        assert_eq!(st.action_count, 4);
        assert_eq!(st.duration_ms, 2500);
        assert!((st.max_speed - 200.0).abs() < 1e-3);
        // 150 units over 1.5 s of movement.
        assert!((st.average_speed - 100.0).abs() < 1e-3);
        assert_eq!(Script::default().stats(), ScriptStats::default());
        assert!((sc.speed_into(1) - 200.0).abs() < 1e-3);
        assert_eq!(sc.speed_into(0), 0.0);
    }

    #[test]
    fn csv_output() {
        let sc = s(&[(-100, 0.2), (0, 0.0), (500, 1.0), (750, 0.333)]);
        assert_eq!(sc.to_csv(), "0,0\n500,100\n750,33\n");
    }
}
