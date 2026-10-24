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

pub(super) fn pause_sessions(sessions: &[impl MediaSession]) -> Vec<String> {
    let mut paused_ids = Vec::new();
    for session in sessions {
        match pause_session(session) {
            Ok(Some(id)) => {
                if !paused_ids.contains(&id) {
                    paused_ids.push(id);
                }
            }
            Ok(None) => {}
            Err(err) => debug!("Skipping Windows media pause: {err}"),
        }
    }
    // Once any session has been paused, later failures must never discard its ID.
    paused_ids
}

fn pause_session(session: &impl MediaSession) -> Result<Option<String>, String> {
    if session.playback_status()? != Some(PlaybackStatus::Playing) || !session.pause_enabled()? {
        return Ok(None);
    }

    // Resolve everything needed for resume BEFORE the first side effect. There
    // must be no fallible lookup between a successful pause and returning its ID.
    let Some(id) = session.source_app_id()?.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    Ok(session.try_pause()?.then_some(id))
}

pub(super) fn resume_sessions(
    sessions: &[impl MediaSession],
    paused_ids: &[String],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for session in sessions {
        if let Err(err) = resume_session(session, paused_ids) {
            debug!("Failed to resume Windows media session: {err}");
            errors.push(err);
        }
    }

    // Report failures only after every other session has had a chance to resume.
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn resume_session(session: &impl MediaSession, paused_ids: &[String]) -> Result<(), String> {
    let Some(id) = session.source_app_id()? else {
        return Ok(());
    };
    if !paused_ids.contains(&id)
        || session.playback_status()? != Some(PlaybackStatus::Paused)
        || !session.play_enabled()?
    {
        return Ok(());
    }

    if !session.try_play()? {
        debug!("Windows media session '{id}' declined the play request; ignoring");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct TestSession {
        id: Result<Option<String>, String>,
        status: Cell<Option<PlaybackStatus>>,
        failure: Option<&'static str>,
        pause_enabled: bool,
        play_enabled: bool,
        pause_accepted: bool,
        play_accepted: bool,
        lose_id_after_pause: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl TestSession {
        fn playing(id: &str) -> Self {
            Self {
                id: Ok(Some(id.to_string())),
                status: Cell::new(Some(PlaybackStatus::Playing)),
                failure: None,
                pause_enabled: true,
                play_enabled: true,
                pause_accepted: true,
                play_accepted: true,
                lose_id_after_pause: false,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn call(&self, operation: &'static str) -> Result<(), String> {
            self.calls.borrow_mut().push(operation);
            if self.failure == Some(operation) {
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
        assert_eq!(pause_sessions(std::slice::from_ref(&session)), ["player"]);
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
                    failure: Some(operation),
                    ..TestSession::playing("broken")
                },
                TestSession::playing("last"),
            ];
            let paused = pause_sessions(&sessions);
            assert_eq!(paused, ["first", "last"], "failure in {operation}");
            // Exercise the real recovery policy with the actual partial result.
            let _ = resume_sessions(&sessions, &paused);
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
                status: Cell::new(None),
                ..TestSession::playing("missing-info")
            },
            TestSession::playing("last"),
        ];
        assert_eq!(pause_sessions(&sessions), ["first", "last"]);
        assert_eq!(sessions[1].calls_to("pause"), 0);
        assert_eq!(sessions[2].calls_to("pause"), 0);
    }

    #[test]
    fn only_successful_pauses_are_tracked_and_restored() {
        let sessions = [
            TestSession::playing("paused-by-us"),
            TestSession {
                status: Cell::new(Some(PlaybackStatus::Paused)),
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
        assert_eq!(paused, ["paused-by-us"]);
        // Even if a rejected/unsupported player is later paused by the user,
        // Handy must not start it when the recording ends.
        for session in &sessions {
            session.status.set(Some(PlaybackStatus::Paused));
        }
        resume_sessions(&sessions, &paused).unwrap();
        assert_eq!(sessions[0].calls_to("play"), 1);
        for session in &sessions[1..] {
            assert_eq!(session.calls_to("play"), 0);
        }
    }

    #[test]
    fn duplicate_app_ids_are_returned_once() {
        let sessions = [
            TestSession::playing("same-app"),
            TestSession::playing("same-app"),
        ];
        let paused = pause_sessions(&sessions);
        assert_eq!(paused, ["same-app"]);
        resume_sessions(&sessions, &paused).unwrap();
        for session in &sessions {
            assert_eq!(session.calls_to("pause"), 1);
            assert_eq!(session.calls_to("play"), 1);
        }
    }

    #[test]
    fn resume_attempts_remaining_sessions_before_reporting_errors() {
        for operation in ["id", "status", "play_enabled", "play"] {
            let mut sessions = [TestSession::playing("first"), TestSession::playing("last")];
            let paused = pause_sessions(&sessions);
            sessions[0].failure = Some(operation);
            let error = resume_sessions(&sessions, &paused).unwrap_err();
            assert!(error.contains(operation));
            assert_eq!(sessions[1].calls_to("play"), 1, "failure in {operation}");
            assert_eq!(sessions[1].status.get(), Some(PlaybackStatus::Playing));
        }
    }

    #[test]
    fn resume_does_not_start_untracked_playing_stopped_or_unavailable_sessions() {
        let sessions = [
            TestSession {
                status: Cell::new(Some(PlaybackStatus::Paused)),
                ..TestSession::playing("untracked")
            },
            TestSession::playing("already-playing"),
            TestSession {
                status: Cell::new(Some(PlaybackStatus::Other)),
                ..TestSession::playing("stopped")
            },
            TestSession {
                status: Cell::new(None),
                ..TestSession::playing("missing-info")
            },
            TestSession {
                id: Ok(None),
                ..TestSession::playing("missing-id")
            },
            TestSession {
                status: Cell::new(Some(PlaybackStatus::Paused)),
                play_enabled: false,
                ..TestSession::playing("play-disabled")
            },
        ];
        let ids = [
            "already-playing",
            "stopped",
            "missing-info",
            "missing-id",
            "play-disabled",
            "exited",
        ]
        .map(String::from);
        resume_sessions(&sessions, &ids).unwrap();
        for session in &sessions {
            assert_eq!(session.calls_to("play"), 0);
        }
        assert_eq!(sessions[0].calls_to("status"), 0);
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
        resume_sessions(&sessions, &paused).unwrap();
        assert_eq!(sessions[0].status.get(), Some(PlaybackStatus::Paused));
        assert_eq!(sessions[1].status.get(), Some(PlaybackStatus::Playing));
    }
}
