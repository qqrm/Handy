use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Weak;
use std::time::Duration;

type Completion = Arc<Mutex<Option<MediaResult<bool>>>>;

struct FakeSession {
    state: Mutex<MediaResult<PlaybackState>>,
    observer: Mutex<Weak<PauseOwnership>>,
    live: AtomicBool,
    pause_result: MediaResult<bool>,
    play_result: MediaResult<bool>,
    watch_ok: bool,
    pause_calls: AtomicUsize,
    play_calls: AtomicUsize,
    pause_gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    events_during_snapshot: Mutex<Vec<MediaResult<PlaybackState>>>,
    pause_ack_during_snapshot: bool,
    defer_pause_state: bool,
    defer_play_state: bool,
    pause_completion: Option<Completion>,
    play_completion: Option<Completion>,
}

impl FakeSession {
    fn new(state: PlaybackState) -> Self {
        Self {
            state: Mutex::new(Ok(state)),
            observer: Mutex::new(Weak::new()),
            live: AtomicBool::new(true),
            pause_result: Ok(true),
            play_result: Ok(true),
            watch_ok: true,
            pause_calls: AtomicUsize::new(0),
            play_calls: AtomicUsize::new(0),
            pause_gate: Mutex::new(None),
            events_during_snapshot: Mutex::new(Vec::new()),
            pause_ack_during_snapshot: false,
            defer_pause_state: false,
            defer_play_state: false,
            pause_completion: None,
            play_completion: None,
        }
    }
    fn change(&self, state: MediaResult<PlaybackState>) {
        *self.state.lock().unwrap() = state.clone();
        if let Some(observer) = self.observer.lock().unwrap().upgrade() {
            observer.observe(state);
        }
    }
    fn pauses(&self) -> usize {
        self.pause_calls.load(Ordering::SeqCst)
    }
    fn plays(&self) -> usize {
        self.play_calls.load(Ordering::SeqCst)
    }
}

struct FakeWatch;
impl SessionWatch for FakeWatch {}
impl MediaSession for FakeSession {
    fn state(&self) -> MediaResult<PlaybackState> {
        let snapshot = self.state.lock().unwrap().clone();
        let events = std::mem::take(&mut *self.events_during_snapshot.lock().unwrap());
        for event in events {
            self.change(event);
        }
        snapshot
    }
    fn is_live(&self) -> MediaResult<bool> {
        Ok(self.live.load(Ordering::SeqCst))
    }
    fn pause(&self) -> MediaResult<MediaOperation> {
        self.pause_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.pause_gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if self.pause_result == Ok(true) {
            if self.pause_ack_during_snapshot {
                *self.events_during_snapshot.lock().unwrap() = vec![Ok(PlaybackState::Paused)];
            } else if !self.defer_pause_state {
                self.change(Ok(PlaybackState::Paused));
            }
        }
        if let Some(completion) = &self.pause_completion {
            let completion = completion.clone();
            Ok(Box::new(move || completion.lock().unwrap().take()))
        } else {
            Ok(completed_operation(self.pause_result.clone()))
        }
    }
    fn play(&self) -> MediaResult<MediaOperation> {
        self.play_calls.fetch_add(1, Ordering::SeqCst);
        if self.play_result == Ok(true) && !self.defer_play_state {
            self.change(Ok(PlaybackState::Playing));
        }
        if let Some(completion) = &self.play_completion {
            let completion = completion.clone();
            Ok(Box::new(move || completion.lock().unwrap().take()))
        } else {
            Ok(completed_operation(self.play_result.clone()))
        }
    }
    fn watch(&self, observer: Arc<PauseOwnership>) -> MediaResult<Box<dyn SessionWatch>> {
        if !self.watch_ok {
            return Err("subscription failed".into());
        }
        *self.observer.lock().unwrap() = Arc::downgrade(&observer);
        Ok(Box::new(FakeWatch))
    }
}

struct FakeBackend(Mutex<MediaResult<Vec<Arc<dyn MediaSession>>>>);
impl MediaBackend for FakeBackend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        self.0.lock().unwrap().clone()
    }
}
fn controller(sessions: &[Arc<FakeSession>]) -> (MediaPauseController, Arc<FakeBackend>) {
    let backend = Arc::new(FakeBackend(Mutex::new(Ok(sessions
        .iter()
        .cloned()
        .map(|s| s as Arc<dyn MediaSession>)
        .collect()))));
    (MediaPauseController::with_backend(backend.clone()), backend)
}
fn start(controller: &MediaPauseController, generation: u64) {
    controller.begin(generation);
    controller.apply(generation, true, true);
    controller.flush();
}
fn stop(controller: &MediaPauseController) {
    controller.remove();
    controller.flush();
}

#[test]
fn disabled_closed_and_unarmed_recordings_do_not_pause() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    c.apply(1, true, true);
    c.begin(1);
    c.apply(1, false, true);
    c.apply(1, true, false);
    stop(&c);
    assert_eq!(session.pauses(), 0);
    assert_eq!(session.plays(), 0);
}

#[test]
fn resumes_only_successfully_paused_original_objects() {
    // App IDs are deliberately absent from this contract: two sessions from
    // one browser remain distinct objects, including one initially paused.
    let playing = Arc::new(FakeSession::new(PlaybackState::Playing));
    let user_paused = Arc::new(FakeSession::new(PlaybackState::Paused));
    let stopped = Arc::new(FakeSession::new(PlaybackState::Other));
    let (c, backend) = controller(&[playing.clone(), user_paused.clone(), stopped.clone()]);
    start(&c, 1);
    let replacement = Arc::new(FakeSession::new(PlaybackState::Paused));
    *backend.0.lock().unwrap() = Ok(vec![replacement.clone()]);
    stop(&c);
    assert_eq!((playing.pauses(), playing.plays()), (1, 1));
    assert_eq!((user_paused.pauses(), user_paused.plays()), (0, 0));
    assert_eq!((stopped.pauses(), stopped.plays()), (0, 0));
    assert_eq!(replacement.plays(), 0);
}

#[test]
fn session_exit_does_not_resume_a_replacement() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    session.live.store(false, Ordering::SeqCst);
    stop(&c);
    assert_eq!(session.plays(), 0);
}

#[test]
fn user_play_then_pause_relinquishes_automatic_resume() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    session.change(Ok(PlaybackState::Playing));
    session.change(Ok(PlaybackState::Paused));
    stop(&c);
    assert_eq!(session.plays(), 0);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
}

#[test]
fn user_resume_stop_and_status_errors_are_respected() {
    for state in [
        Ok(PlaybackState::Playing),
        Ok(PlaybackState::Other),
        Err("disconnected".into()),
    ] {
        let session = Arc::new(FakeSession::new(PlaybackState::Playing));
        let (c, _) = controller(std::slice::from_ref(&session));
        start(&c, 1);
        session.change(state);
        stop(&c);
        assert_eq!(session.plays(), 0);
    }
}

#[test]
fn rejected_or_failed_pause_and_watch_errors_do_not_gain_ownership() {
    for result in [Ok(false), Err("SMTC failure".into())] {
        let session = Arc::new(FakeSession {
            pause_result: result,
            ..FakeSession::new(PlaybackState::Playing)
        });
        let healthy = Arc::new(FakeSession::new(PlaybackState::Playing));
        let (c, _) = controller(&[session.clone(), healthy.clone()]);
        start(&c, 1);
        stop(&c);
        assert_eq!(session.plays(), 0);
        assert_eq!(healthy.plays(), 1);
    }
    let session = Arc::new(FakeSession {
        watch_ok: false,
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    assert_eq!(session.pauses(), 0);
}

#[test]
fn manager_failure_does_not_prevent_the_next_recording() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, backend) = controller(std::slice::from_ref(&session));
    *backend.0.lock().unwrap() = Err("manager unavailable".into());
    start(&c, 1);
    stop(&c);
    *backend.0.lock().unwrap() = Ok(vec![session.clone()]);
    start(&c, 2);
    stop(&c);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
}

#[test]
fn duplicate_ready_stop_and_late_ready_are_idempotent() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    c.apply(1, true, true);
    c.flush();
    stop(&c);
    c.apply(1, true, true);
    stop(&c);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
}

#[test]
fn cancel_during_platform_pause_restores_without_blocking_caller() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *session.pause_gate.lock().unwrap() = Some((entered, released));
    let (c, _) = controller(std::slice::from_ref(&session));
    c.begin(1);
    c.apply(1, true, true);
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    c.remove(); // Must return while the platform call is still blocked.
    release.send(()).unwrap();
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (1, 1));
}

#[test]
fn rapid_restart_finishes_old_restore_before_new_pause() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *session.pause_gate.lock().unwrap() = Some((entered, released));
    let (c, _) = controller(std::slice::from_ref(&session));
    c.begin(1);
    c.apply(1, true, true);
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    c.remove();
    c.begin(2);
    c.apply(1, true, true); // Late first-sample callback from the old recording.
    c.apply(2, true, true);
    release.send(()).unwrap();
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (2, 1));
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    stop(&c);
    assert_eq!(session.plays(), 2);
}

#[test]
fn platform_worker_can_start_and_stop_without_touching_sessions() {
    let c = MediaPauseController::new();
    c.begin(1);
    stop(&c); // No apply, hence no enumeration or pause requests.
}

#[test]
fn superseded_start_and_stop_release_the_previous_recording() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    c.0.commands.send(Command::Park(entered, released)).unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    c.begin(2);
    c.remove();
    release.send(()).unwrap();
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (1, 1));
    assert_eq!(session.state(), Ok(PlaybackState::Playing));
}

#[test]
fn exit_restores_and_failed_play_does_not_block_other_sessions_or_retry() {
    let failed = Arc::new(FakeSession {
        play_result: Err("session no longer accepts commands".into()),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let healthy = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(&[failed.clone(), healthy.clone()]);
    start(&c, 1);
    c.restore_before_exit();
    stop(&c);
    assert_eq!((failed.plays(), healthy.plays()), (1, 1));
    assert_eq!(healthy.state(), Ok(PlaybackState::Playing));
}

#[cfg(windows)]
#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Windows SMTC job"]
fn windows_enumeration_is_read_only() {
    require_isolated_smtc_test_environment();
    let _apartment = platform::Apartment::new().expect("WinRT MTA initialization");
    let sessions = platform::Backend
        .sessions()
        .expect("SMTC session enumeration");
    println!(
        "Read-only SMTC enumeration succeeded: {} sessions",
        sessions.len()
    );
    for session in sessions {
        let _ = session.state();
    }
}

#[test]
fn accepted_pause_with_late_state_after_end_is_restored_once() {
    let session = Arc::new(FakeSession {
        defer_pause_state: true,
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1); // Method accepted, but playback has not changed yet.
    stop(&c);
    assert_eq!((session.pauses(), session.plays()), (1, 0));
    session.change(Ok(PlaybackState::Paused));
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (1, 1));
    c.flush();
    assert_eq!(session.plays(), 1);
}

#[test]
fn post_pause_snapshot_cannot_overwrite_newer_pause_acknowledgement() {
    let session = Arc::new(FakeSession {
        pause_ack_during_snapshot: true,
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    // The Get captures Playing, then the subscription observes our Paused event
    // before that older Get returns (for example, during a D-Bus signal fence).
    start(&c, 1);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    stop(&c);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
}

#[test]
fn progress_snapshot_cannot_overwrite_newer_pause_acknowledgement() {
    let session = Arc::new(FakeSession {
        defer_pause_state: true,
        ..FakeSession::new(PlaybackState::Playing)
    });
    let backend = FakeBackend(Mutex::new(Ok(vec![session.clone()])));
    let mut leases = pause_sessions(&backend, &AtomicU64::new(1), 1);
    assert_eq!(leases.len(), 1);
    leases[0].retiring = true;
    *session.events_during_snapshot.lock().unwrap() = vec![Ok(PlaybackState::Paused)];
    assert!(!leases[0].progress());
    assert!(leases[0].ownership.can_resume());
    assert_eq!(session.plays(), 0, "Do not act on the stale returned state");
    assert!(!leases[0].progress());
    assert_eq!(session.plays(), 1);
    assert!(leases[0].progress());
    assert_eq!(session.state(), Ok(PlaybackState::Playing));
}

#[test]
fn user_override_during_snapshot_still_relinquishes_resume() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let backend = FakeBackend(Mutex::new(Ok(vec![session.clone()])));
    let mut leases = pause_sessions(&backend, &AtomicU64::new(1), 1);
    assert_eq!(leases.len(), 1);
    leases[0].retiring = true;
    *session.events_during_snapshot.lock().unwrap() =
        vec![Ok(PlaybackState::Playing), Ok(PlaybackState::Paused)];
    assert!(leases[0].progress());
    assert_eq!(session.plays(), 0);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
}

#[test]
fn late_successful_pause_completion_is_retained_after_end() {
    let completion = Arc::new(Mutex::new(None));
    let session = Arc::new(FakeSession {
        defer_pause_state: true,
        pause_completion: Some(completion.clone()),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    session.change(Ok(PlaybackState::Paused));
    c.flush();
    assert_eq!(session.plays(), 0); // Do not race the unfinished Pause with Play.
    *completion.lock().unwrap() = Some(Ok(true));
    c.flush();
    assert_eq!(session.plays(), 1);
}

#[test]
fn never_finishing_pause_quarantines_new_recordings_without_blocking_commands() {
    let completion = Arc::new(Mutex::new(None));
    let session = Arc::new(FakeSession {
        defer_pause_state: true,
        pause_completion: Some(completion),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    for generation in 2..50 {
        start(&c, generation);
        stop(&c);
    }
    assert_eq!((session.pauses(), session.plays()), (1, 0));
    session.live.store(false, Ordering::SeqCst);
    c.flush(); // Definitive disappearance releases the quarantined lease.
}

#[test]
fn acknowledgement_deadline_keeps_original_watch_and_pending_operation() {
    let completion = Arc::new(Mutex::new(None));
    let session = Arc::new(FakeSession {
        defer_pause_state: true,
        pause_completion: Some(completion),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let backend = FakeBackend(Mutex::new(Ok(vec![session.clone()])));
    let current = AtomicU64::new(1);
    let mut leases = pause_sessions(&backend, &current, 1);
    assert_eq!(leases.len(), 1);
    leases[0].submitted = Instant::now() - ACK_DEADLINE;
    leases[0].retiring = true;
    assert!(!leases[0].progress());
    assert!(leases[0].quarantined);
    assert!(leases[0].pause.is_some());
    assert!(session.observer.lock().unwrap().upgrade().is_some());
    assert_eq!(session.plays(), 0);
}

#[test]
fn pending_play_finishes_before_replacement_recording_can_pause() {
    let completion = Arc::new(Mutex::new(None));
    let session = Arc::new(FakeSession {
        play_completion: Some(completion.clone()),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    start(&c, 2);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
    *completion.lock().unwrap() = Some(Ok(true));
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (2, 1));
    *completion.lock().unwrap() = Some(Ok(true));
    stop(&c);
}

#[test]
fn accepted_play_with_late_state_blocks_replacement_pause_until_acknowledged() {
    let session = Arc::new(FakeSession {
        defer_play_state: true,
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    start(&c, 2);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    session.change(Ok(PlaybackState::Playing));
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (2, 1));
    stop(&c);
    session.change(Ok(PlaybackState::Playing));
    c.flush();
}

#[test]
fn unknown_play_reply_after_observed_play_stays_quarantined() {
    let session = Arc::new(FakeSession {
        play_completion: Some(Arc::new(Mutex::new(None))),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    stop(&c);
    start(&c, 2);
    assert_eq!((session.pauses(), session.plays()), (1, 1));
    assert_eq!(session.state(), Ok(PlaybackState::Playing));
    session.live.store(false, Ordering::SeqCst);
    stop(&c);
}

#[test]
fn observed_user_override_is_respected_while_pause_completion_is_pending() {
    let completion = Arc::new(Mutex::new(None));
    let session = Arc::new(FakeSession {
        pause_completion: Some(completion.clone()),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    session.change(Ok(PlaybackState::Playing));
    session.change(Ok(PlaybackState::Paused));
    stop(&c);
    *completion.lock().unwrap() = Some(Ok(true));
    c.flush();
    assert_eq!(session.plays(), 0);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
}

#[test]
fn lease_cap_refuses_a_partial_pause_batch() {
    let sessions: Vec<_> = (0..MAX_MEDIA_LEASES + 3)
        .map(|_| Arc::new(FakeSession::new(PlaybackState::Playing)))
        .collect();
    let (c, _) = controller(&sessions);
    start(&c, 1);
    stop(&c);
    for session in sessions {
        assert_eq!((session.pauses(), session.plays()), (0, 0));
    }
}

#[test]
fn unknown_pause_reply_after_observed_pause_stays_quarantined() {
    let session = Arc::new(FakeSession {
        pause_completion: Some(Arc::new(Mutex::new(None))),
        ..FakeSession::new(PlaybackState::Playing)
    });
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1); // Playback changed, but the method result is unknown.
    stop(&c);
    start(&c, 2);
    assert_eq!((session.pauses(), session.plays()), (1, 0));
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    session.live.store(false, Ordering::SeqCst);
    stop(&c);
}

#[test]
fn full_command_queue_cannot_prevent_stop_invalidation() {
    let session = Arc::new(FakeSession::new(PlaybackState::Playing));
    let (c, _) = controller(std::slice::from_ref(&session));
    start(&c, 1);
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    c.0.commands.send(Command::Park(entered, released)).unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..COMMAND_CAPACITY * 2 {
        c.apply(1, true, true); // These calls must not wait for queue capacity.
    }
    c.remove(); // Even if End is dropped, the atomic generation invalidates it.
    release.send(()).unwrap();
    c.flush();
    assert_eq!((session.pauses(), session.plays()), (1, 1));
}
