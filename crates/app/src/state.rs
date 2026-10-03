//! Top-level navigation state machine (§3.2 app/): Library → Player → Settings.
//!
//! Pure data; the frame loop feeds it [`Nav`] events (from UI actions,
//! controller shortcuts and the remote APIs) and reads [`AppState::screen`]
//! to decide what to draw. Keeping it free of I/O makes the transitions
//! testable and gives a future watch-together sync layer one place to hook in.

/// What occupies the main panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Library,
    /// Video playing; controls may be hidden.
    Player {
        controls_visible: bool,
    },
    PictureAdjust,
    Settings,
    /// Virtual keyboard over whatever was open (search, server address...).
    Keyboard,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Nav {
    OpenItem,
    /// The open video finished or failed; go back where the user came from.
    PlaybackEnded,
    Back,
    ToggleControls,
    /// Any user input while playing (shows controls, resets auto-hide).
    Activity,
    OpenSettings,
    OpenPictureAdjust,
    // The library screen currently embeds its own keyboard; these drive a
    // stand-alone keyboard overlay (server addresses, …).
    #[cfg_attr(not(test), allow(dead_code))]
    OpenKeyboard,
    #[cfg_attr(not(test), allow(dead_code))]
    KeyboardDone,
    /// Seconds since the last activity, polled every frame.
    Idle(f32),
}

/// Controls auto-hide after this long without input while playing.
pub const CONTROLS_AUTOHIDE_S: f32 = 4.0;

#[derive(Debug, Clone)]
pub struct AppState {
    screen: Screen,
    /// Screens to return to on Back; Library is the implicit root.
    stack: Vec<Screen>,
    /// True while a video is loaded (even if another screen covers it).
    pub playing: bool,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            screen: Screen::Library,
            stack: Vec::new(),
            playing: false,
        }
    }
}

impl AppState {
    pub fn screen(&self) -> Screen {
        self.screen
    }

    fn push(&mut self, s: Screen) {
        if self.screen != s {
            self.stack.push(self.screen);
            self.screen = s;
        }
    }

    fn pop(&mut self) {
        self.screen = self.stack.pop().unwrap_or(Screen::Library);
        if let Screen::Player { .. } = self.screen {
            self.screen = Screen::Player {
                controls_visible: true,
            };
        }
    }

    pub fn handle(&mut self, nav: Nav) {
        match (nav, self.screen) {
            (Nav::OpenItem, _) => {
                // Opening from anywhere replaces the player rather than stacking.
                self.stack
                    .retain(|s| !matches!(s, Screen::Player { .. } | Screen::Keyboard));
                if matches!(self.screen, Screen::Player { .. } | Screen::Keyboard) {
                    self.screen = self.stack.pop().unwrap_or(Screen::Library);
                }
                self.playing = true;
                self.push(Screen::Player {
                    controls_visible: true,
                });
            }
            (Nav::PlaybackEnded, _) => {
                self.playing = false;
                self.stack
                    .retain(|s| !matches!(s, Screen::Player { .. } | Screen::PictureAdjust));
                if matches!(self.screen, Screen::Player { .. } | Screen::PictureAdjust) {
                    self.pop();
                }
            }
            (
                Nav::Back,
                Screen::Player {
                    controls_visible: true,
                },
            ) if self.playing => {
                // First Back hides controls; second leaves the player.
                self.screen = Screen::Player {
                    controls_visible: false,
                };
            }
            (Nav::Back, Screen::Player { .. }) => {
                self.playing = false;
                self.pop();
            }
            (Nav::Back, Screen::Library) => {}
            (Nav::Back, _) | (Nav::KeyboardDone, Screen::Keyboard) => self.pop(),
            (Nav::KeyboardDone, _) => {}
            (Nav::ToggleControls, Screen::Player { controls_visible }) => {
                self.screen = Screen::Player {
                    controls_visible: !controls_visible,
                };
            }
            (Nav::Activity, Screen::Player { .. }) => {
                self.screen = Screen::Player {
                    controls_visible: true,
                }
            }
            (
                Nav::Idle(s),
                Screen::Player {
                    controls_visible: true,
                },
            ) if s >= CONTROLS_AUTOHIDE_S => {
                self.screen = Screen::Player {
                    controls_visible: false,
                };
            }
            (Nav::OpenSettings, _) => self.push(Screen::Settings),
            (Nav::OpenPictureAdjust, _) if self.playing => self.push(Screen::PictureAdjust),
            (Nav::OpenKeyboard, _) => self.push(Screen::Keyboard),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_player_back() {
        let mut s = AppState::default();
        s.handle(Nav::OpenItem);
        assert_eq!(
            s.screen(),
            Screen::Player {
                controls_visible: true
            }
        );
        s.handle(Nav::Idle(5.0));
        assert_eq!(
            s.screen(),
            Screen::Player {
                controls_visible: false
            }
        );
        s.handle(Nav::Activity);
        s.handle(Nav::Back);
        assert_eq!(
            s.screen(),
            Screen::Player {
                controls_visible: false
            }
        );
        s.handle(Nav::Back);
        assert_eq!(s.screen(), Screen::Library);
        assert!(!s.playing);
    }

    #[test]
    fn overlays_return_to_player() {
        let mut s = AppState::default();
        s.handle(Nav::OpenItem);
        s.handle(Nav::OpenPictureAdjust);
        assert_eq!(s.screen(), Screen::PictureAdjust);
        s.handle(Nav::OpenKeyboard);
        s.handle(Nav::KeyboardDone);
        assert_eq!(s.screen(), Screen::PictureAdjust);
        s.handle(Nav::Back);
        assert_eq!(
            s.screen(),
            Screen::Player {
                controls_visible: true
            }
        );
        assert!(s.playing);
    }

    #[test]
    fn picture_adjust_requires_video() {
        let mut s = AppState::default();
        s.handle(Nav::OpenPictureAdjust);
        assert_eq!(s.screen(), Screen::Library);
    }

    #[test]
    fn open_item_while_playing_replaces_player() {
        let mut s = AppState::default();
        s.handle(Nav::OpenItem);
        s.handle(Nav::OpenItem);
        s.handle(Nav::Back);
        s.handle(Nav::Back);
        assert_eq!(s.screen(), Screen::Library);
    }

    #[test]
    fn playback_end_from_overlay() {
        let mut s = AppState::default();
        s.handle(Nav::OpenSettings);
        s.handle(Nav::OpenItem);
        s.handle(Nav::OpenPictureAdjust);
        s.handle(Nav::PlaybackEnded);
        assert_eq!(s.screen(), Screen::Settings);
        s.handle(Nav::Back);
        assert_eq!(s.screen(), Screen::Library);
    }
}
