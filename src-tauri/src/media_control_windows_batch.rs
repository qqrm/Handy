//! Windows session policy, kept independent of WinRT so failures can be tested
//! without live media players (including in the Linux test job).

use log::debug;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackStatus {
    Playing,
    Paused,
    Other,
}

pub(super) trait MediaSession {
    fn source_app_id(&self) -> Result<Option<String>, String>;
    fn playback_status(&self) -> Result<Option<PlaybackStatus>, String>;
    fn pause_enabled(&self) -> Result<bool, String>;
    fn play_enabled(&self) -> Result<bool, String>;
    fn try_pause(&self) -> Result<bool, String>;
    fn try_play(&self) -> Result<bool, String>;
}

/// Retain each concrete session that accepted Pause. App IDs are metadata,
/// never an ownership key: one app may publish several different sessions.
pub(super) fn pause_sessions<S: MediaSession + Clone>(sessions: &[S]) -> Vec<S> {
    let mut paused = Vec::new();
    for session in sessions {
        match pause_session(session) {
            Ok(Some(session)) => paused.push(session),
            Ok(None) => {}
            Err(error) => debug!("Skipping Windows media pause: {error}"),
        }
    }
    paused
}

fn pause_session<S: MediaSession + Clone>(session: &S) -> Result<Option<S>, String> {
    if session.playback_status()? != Some(PlaybackStatus::Playing) || !session.pause_enabled()? {
        return Ok(None);
    }
    // Keep the legacy metadata check, but capture the actual handle before any
    // side effect. No later lookup or app-ID collision can lose its ownership.
    if session.source_app_id()?.is_none_or(|id| id.is_empty()) {
        return Ok(None);
    }
    let owned = session.clone();
    Ok(session.try_pause()?.then_some(owned))
}

/// Restore only the same handles that accepted Pause; never enumerate replacements.
pub(super) fn resume_sessions(paused: &[impl MediaSession]) -> Result<(), String> {
    let mut errors = Vec::new();
    for session in paused {
        if let Err(error) = resume_session(session) {
            debug!("Failed to resume Windows media session: {error}");
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn resume_session(session: &impl MediaSession) -> Result<(), String> {
    if session.playback_status()? != Some(PlaybackStatus::Paused) || !session.play_enabled()? {
        return Ok(());
    }
    if !session.try_play()? {
        debug!("Owned Windows media session declined the play request; ignoring");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[derive(Clone)]
    struct TestSession {
        id: Result<Option<String>, String>,
        status: Rc<Cell<Option<PlaybackStatus>>>,
        failure: Rc<Cell<Option<&'static str>>>,
        pause_enabled: bool,
        play_enabled: bool,
        pause_accepted: bool,
        play_accepted: bool,
        lose_id_after_pause: bool,
        calls: Rc<RefCell<Vec<&'static str>>>,
    }

    impl TestSession {
        fn playing(id: &str) -> Self {
            Self {
                id: Ok(Some(id.to_string())),
                status: Rc::new(Cell::new(Some(PlaybackStatus::Playing))),
                failure: Rc::new(Cell::new(None)),
                pause_enabled: true,
                play_enabled: true,
                pause_accepted: true,
                play_accepted: true,
                lose_id_after_pause: false,
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn call(&self, operation: &'static str) -> Result<(), String> {
            self.calls.borrow_mut().push(operation);
            if self.failure.get() == Some(operation) {
                Err(format!("{operation} failed"))
            } else {
                Ok(())
            }
        }

        fn calls_to(&self, operation: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|&&call| call == operation)
                .count()
        }
    }

    fn ids(sessions: &[TestSession]) -> Vec<String> {
        sessions
            .iter()
            .map(|session| session.id.as_ref().unwrap().clone().unwrap())
            .collect()
    }

    impl MediaSession for TestSession {
        fn source_app_id(&self) -> Result<Option<String>, String> {
            self.call("id")?;
            if self.lose_id_after_pause && self.calls_to("pause") > 0 {
                return Err("ID disappeared after pause".into());
            }
            self.id.clone()
        }

        fn playback_status(&self) -> Result<Option<PlaybackStatus>, String> {
            self.call("status")?;
            Ok(self.status.get())
        }

        fn pause_enabled(&self) -> Result<bool, String> {
            self.call("pause_enabled")?;
            Ok(self.pause_enabled)
        }

        fn play_enabled(&self) -> Result<bool, String> {
            self.call("play_enabled")?;
            Ok(self.play_enabled)
        }

        fn try_pause(&self) -> Result<bool, String> {
            self.call("pause")?;
            if self.pause_accepted {
                self.status.set(Some(PlaybackStatus::Paused));
            }
            Ok(self.pause_accepted)
        }

        fn try_play(&self) -> Result<bool, String> {
            self.call("play")?;
            if self.play_accepted {
                self.status.set(Some(PlaybackStatus::Playing));
            }
            Ok(self.play_accepted)
        }
    }

    #[test]
    fn missing_empty_or_failed_id_never_pauses() {
        for id in [
            Ok(None),
            Ok(Some(String::new())),
            Err("ID lookup failed".into()),
        ] {
            let session = TestSession {
                id,
                ..TestSession::playing("untrackable")
            };
            assert!(pause_sessions(std::slice::from_ref(&session)).is_empty());
            assert_eq!(session.calls_to("pause"), 0);
            assert_eq!(session.status.get(), Some(PlaybackStatus::Playing));
        }
    }

    #[test]
    fn id_is_captured_before_pause_and_never_looked_up_after_it() {
        let session = TestSession {
            lose_id_after_pause: true,
            ..TestSession::playing("player")
        };
        assert_eq!(
            ids(&pause_sessions(std::slice::from_ref(&session))),
            ["player"]
        );
        assert_eq!(
            *session.calls.borrow(),
            ["status", "pause_enabled", "id", "pause"]
        );
    }

    #[test]
    fn later_failures_preserve_earlier_pauses_and_continue_to_later_sessions() {
        for operation in ["status", "pause_enabled", "id", "pause"] {
            let sessions = [
                TestSession::playing("first"),
                TestSession {
                    failure: Rc::new(Cell::new(Some(operation))),
                    ..TestSession::playing("broken")
                },
                TestSession::playing("last"),
            ];
            let paused = pause_sessions(&sessions);
            assert_eq!(ids(&paused), ["first", "last"], "failure in {operation}");
            // Exercise the real recovery policy with the actual partial result.
            let _ = resume_sessions(&paused);
            for index in [0, 2] {
                assert_eq!(
                    sessions[index].calls_to("play"),
                    1,
                    "failure in {operation}"
                );
                assert_eq!(sessions[index].status.get(), Some(PlaybackStatus::Playing));
            }
            assert_eq!(sessions[1].calls_to("play"), 0);
        }
    }

    #[test]
    fn unavailable_sessions_do_not_erase_successful_pauses() {
        let sessions = [
            TestSession::playing("first"),
            TestSession {
                id: Ok(None),
                ..TestSession::playing("missing-id")
            },
            TestSession {
                status: Rc::new(Cell::new(None)),
                ..TestSession::playing("missing-info")
            },
            TestSession::playing("last"),
        ];
        assert_eq!(ids(&pause_sessions(&sessions)), ["first", "last"]);
        assert_eq!(sessions[1].calls_to("pause"), 0);
        assert_eq!(sessions[2].calls_to("pause"), 0);
    }

    #[test]
    fn only_successful_pauses_are_tracked_and_restored() {
        let sessions = [
            TestSession::playing("paused-by-us"),
            TestSession {
                status: Rc::new(Cell::new(Some(PlaybackStatus::Paused))),
                ..TestSession::playing("already-paused")
            },
            TestSession {
                pause_enabled: false,
                ..TestSession::playing("unsupported")
            },
            TestSession {
                pause_accepted: false,
                ..TestSession::playing("declined")
            },
        ];
        let paused = pause_sessions(&sessions);
        assert_eq!(ids(&paused), ["paused-by-us"]);
        // Even if a rejected/unsupported player is later paused by the user,
        // Handy must not start it when the recording ends.
        for session in &sessions {
            session.status.set(Some(PlaybackStatus::Paused));
        }
        resume_sessions(&paused).unwrap();
        assert_eq!(sessions[0].calls_to("play"), 1);
        for session in &sessions[1..] {
            assert_eq!(session.calls_to("play"), 0);
        }
    }

    #[test]
    fn same_app_sessions_are_owned_individually() {
        let sessions = [
            TestSession::playing("same-app"),
            TestSession::playing("same-app"),
        ];
        let paused = pause_sessions(&sessions);
        assert_eq!(ids(&paused), ["same-app", "same-app"]);
        resume_sessions(&paused).unwrap();
        for session in &sessions {
            assert_eq!(session.calls_to("pause"), 1);
            assert_eq!(session.calls_to("play"), 1);
        }
    }

    #[test]
    fn resume_attempts_remaining_sessions_before_reporting_errors() {
        for operation in ["status", "play_enabled", "play"] {
            let sessions = [TestSession::playing("first"), TestSession::playing("last")];
            let paused = pause_sessions(&sessions);
            sessions[0].failure.set(Some(operation));
            let error = resume_sessions(&paused).unwrap_err();
            assert!(error.contains(operation));
            assert_eq!(sessions[1].calls_to("play"), 1, "failure in {operation}");
            assert_eq!(sessions[1].status.get(), Some(PlaybackStatus::Playing));
        }
    }

    #[test]
    fn resume_does_not_start_playing_stopped_unavailable_or_play_disabled_owners() {
        let sessions = [
            TestSession::playing("already-playing"),
            TestSession::playing("stopped"),
            TestSession::playing("missing-info"),
            TestSession {
                play_enabled: false,
                ..TestSession::playing("play-disabled")
            },
        ];
        let paused = pause_sessions(&sessions);
        sessions[0].status.set(Some(PlaybackStatus::Playing));
        sessions[1].status.set(Some(PlaybackStatus::Other));
        sessions[2].status.set(None);
        resume_sessions(&paused).unwrap();
        for session in &sessions {
            assert_eq!(session.calls_to("play"), 0);
        }
    }

    #[test]
    fn prepaused_sibling_with_same_app_id_is_not_resumed() {
        let sessions = [
            TestSession::playing("same-app"),
            TestSession {
                status: Rc::new(Cell::new(Some(PlaybackStatus::Paused))),
                ..TestSession::playing("same-app")
            },
        ];
        let paused = pause_sessions(&sessions);
        assert_eq!(paused.len(), 1);
        resume_sessions(&paused).unwrap();
        assert_eq!(sessions[0].calls_to("play"), 1);
        assert_eq!(sessions[1].calls_to("play"), 0);
    }

    #[test]
    fn replacement_with_same_app_id_cannot_inherit_pause_ownership() {
        let original = TestSession::playing("same-app");
        let paused = pause_sessions(std::slice::from_ref(&original));
        original.status.set(None); // The owned session disappears.
        let replacement = TestSession {
            status: Rc::new(Cell::new(Some(PlaybackStatus::Paused))),
            ..TestSession::playing("same-app")
        };
        resume_sessions(&paused).unwrap();
        assert_eq!(original.calls_to("play"), 0);
        assert_eq!(replacement.calls_to("play"), 0);
        assert_eq!(replacement.status.get(), Some(PlaybackStatus::Paused));
    }

    #[test]
    fn recovery_does_not_need_app_metadata_after_successful_pause() {
        let session = TestSession {
            lose_id_after_pause: true,
            ..TestSession::playing("player")
        };
        let paused = pause_sessions(std::slice::from_ref(&session));
        resume_sessions(&paused).unwrap();
        assert_eq!(session.calls_to("id"), 1);
        assert_eq!(session.calls_to("play"), 1);
    }

    #[test]
    fn declined_play_does_not_prevent_later_resume() {
        let sessions = [
            TestSession {
                play_accepted: false,
                ..TestSession::playing("declined")
            },
            TestSession::playing("last"),
        ];
        let paused = pause_sessions(&sessions);
        resume_sessions(&paused).unwrap();
        assert_eq!(sessions[0].status.get(), Some(PlaybackStatus::Paused));
        assert_eq!(sessions[1].status.get(), Some(PlaybackStatus::Playing));
    }
}
