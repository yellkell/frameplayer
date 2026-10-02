//! OpenXR session lifecycle as a pure state machine: maps session state
//! changes to the actions the app loop must take.

use openxr as xr;

/// Mirror of `XrSessionState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionState {
    #[default]
    Unknown,
    Idle,
    Ready,
    Synchronized,
    Visible,
    Focused,
    Stopping,
    LossPending,
    Exiting,
}

impl From<xr::SessionState> for SessionState {
    fn from(s: xr::SessionState) -> Self {
        match s {
            xr::SessionState::IDLE => SessionState::Idle,
            xr::SessionState::READY => SessionState::Ready,
            xr::SessionState::SYNCHRONIZED => SessionState::Synchronized,
            xr::SessionState::VISIBLE => SessionState::Visible,
            xr::SessionState::FOCUSED => SessionState::Focused,
            xr::SessionState::STOPPING => SessionState::Stopping,
            xr::SessionState::LOSS_PENDING => SessionState::LossPending,
            xr::SessionState::EXITING => SessionState::Exiting,
            _ => SessionState::Unknown,
        }
    }
}

/// What the loop must do after a state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    None,
    /// Call `xrBeginSession`.
    BeginSession,
    /// Call `xrEndSession`.
    EndSession,
    /// Tear down and quit (EXITING, or LOSS_PENDING / instance loss).
    Exit,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Lifecycle {
    state: SessionState,
    running: bool,
    exit: bool,
}

impl Lifecycle {
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Session has been begun and not yet ended.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The frame loop (wait/begin/end frame) must run.
    pub fn should_run_frame_loop(&self) -> bool {
        self.running
    }

    /// Content is visible to the user (render, but inputs may be inactive).
    pub fn is_visible(&self) -> bool {
        matches!(self.state, SessionState::Visible | SessionState::Focused)
    }

    /// The app has input focus (sync actions only when true).
    pub fn is_focused(&self) -> bool {
        self.state == SessionState::Focused
    }

    pub fn exit_requested(&self) -> bool {
        self.exit
    }

    pub fn on_state_changed(&mut self, new: SessionState) -> LifecycleAction {
        self.state = new;
        match new {
            SessionState::Ready if !self.running => {
                self.running = true;
                LifecycleAction::BeginSession
            }
            SessionState::Stopping if self.running => {
                self.running = false;
                LifecycleAction::EndSession
            }
            SessionState::Exiting | SessionState::LossPending => {
                self.exit = true;
                LifecycleAction::Exit
            }
            _ => LifecycleAction::None,
        }
    }

    /// Instance loss is terminal.
    pub fn on_instance_loss(&mut self) -> LifecycleAction {
        self.exit = true;
        self.running = false;
        LifecycleAction::Exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_lifecycle() {
        let mut l = Lifecycle::default();
        assert_eq!(
            l.on_state_changed(SessionState::Idle),
            LifecycleAction::None
        );
        assert!(!l.should_run_frame_loop());
        assert_eq!(
            l.on_state_changed(SessionState::Ready),
            LifecycleAction::BeginSession
        );
        assert!(l.should_run_frame_loop() && !l.is_visible());
        assert_eq!(
            l.on_state_changed(SessionState::Synchronized),
            LifecycleAction::None
        );
        assert_eq!(
            l.on_state_changed(SessionState::Visible),
            LifecycleAction::None
        );
        assert!(l.is_visible() && !l.is_focused());
        l.on_state_changed(SessionState::Focused);
        assert!(l.is_focused());
        l.on_state_changed(SessionState::Visible);
        l.on_state_changed(SessionState::Synchronized);
        assert_eq!(
            l.on_state_changed(SessionState::Stopping),
            LifecycleAction::EndSession
        );
        assert!(!l.is_running());
        assert_eq!(
            l.on_state_changed(SessionState::Idle),
            LifecycleAction::None
        );
        assert_eq!(
            l.on_state_changed(SessionState::Exiting),
            LifecycleAction::Exit
        );
        assert!(l.exit_requested());
    }

    #[test]
    fn restart_and_loss() {
        let mut l = Lifecycle::default();
        l.on_state_changed(SessionState::Ready);
        // A duplicate READY must not begin twice.
        assert_eq!(
            l.on_state_changed(SessionState::Ready),
            LifecycleAction::None
        );
        l.on_state_changed(SessionState::Stopping);
        assert_eq!(
            l.on_state_changed(SessionState::Ready),
            LifecycleAction::BeginSession
        );
        assert_eq!(
            l.on_state_changed(SessionState::LossPending),
            LifecycleAction::Exit
        );
        let mut m = Lifecycle::default();
        m.on_state_changed(SessionState::Ready);
        assert_eq!(m.on_instance_loss(), LifecycleAction::Exit);
        assert!(!m.is_running());
    }

    #[test]
    fn from_xr() {
        assert_eq!(
            SessionState::from(xr::SessionState::FOCUSED),
            SessionState::Focused
        );
        assert_eq!(
            SessionState::from(xr::SessionState::LOSS_PENDING),
            SessionState::LossPending
        );
    }
}
