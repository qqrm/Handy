use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Weak;
use std::time::Duration;

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
        self.state.lock().unwrap().clone()
    }
    fn is_live(&self) -> MediaResult<bool> {
        Ok(self.live.load(Ordering::SeqCst))
    }
    fn pause(&self) -> MediaResult<bool> {
        self.pause_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.pause_gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if self.pause_result == Ok(true) {
            self.change(Ok(PlaybackState::Paused));
        }
        self.pause_result.clone()
    }
    fn play(&self) -> MediaResult<bool> {
        self.play_calls.fetch_add(1, Ordering::SeqCst);
        if self.play_result == Ok(true) {
            self.change(Ok(PlaybackState::Playing));
        }
        self.play_result.clone()
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
