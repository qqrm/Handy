//! Recording-scoped media pause. All platform calls run on one worker, never
//! on the UI thread. A recording generation invalidates delayed readiness work.
use log::debug;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};

#[cfg(windows)]
#[path = "media_pause_windows.rs"]
mod platform;

/// Prevent accidental desktop execution; the hosted VM provides isolation.
/// Environment values are a fail-closed launch check, not a security sandbox.
#[cfg(all(test, windows))]
fn require_isolated_smtc_test_environment() {
    for (name, expected) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Windows"),
        ("HANDY_RUN_ISOLATED_SMTC_TESTS", "1"),
    ] {
        assert!(
            std::env::var(name).is_ok_and(|value| value == expected),
            "Real SMTC tests are restricted to the isolated GitHub-hosted Windows job: \
             {name} must be {expected}. Do not run them on a working desktop or spoof these values."
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackState {
    Playing,
    Paused,
    Other,
}

type MediaResult<T> = Result<T, String>;

/// SMTC does not identify who issued a command. Once playback leaves our
/// acknowledged pause, ownership is permanently relinquished for this recording.
/// A repeated Pause while already Paused may produce no event and is unknowable.
#[derive(Default)]
struct PauseOwnership(Mutex<(bool, bool)>); // (pause observed, ownership lost)

impl PauseOwnership {
    fn observe(&self, state: MediaResult<PlaybackState>) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match state {
            Ok(PlaybackState::Paused) => guard.0 = true,
            Ok(PlaybackState::Playing) if !guard.0 => {}
            _ => guard.1 = true,
        }
    }

    fn can_resume(&self) -> bool {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.0 && !guard.1
    }
}

/// A subscription is removed when its guard is dropped.
trait SessionWatch: Send {}
trait MediaSession: Send + Sync {
    fn state(&self) -> MediaResult<PlaybackState>;
    fn pause(&self) -> MediaResult<bool>;
    fn play(&self) -> MediaResult<bool>;
    fn is_live(&self) -> MediaResult<bool>;
    fn watch(&self, ownership: Arc<PauseOwnership>) -> MediaResult<Box<dyn SessionWatch>>;
}

trait MediaBackend: Send + Sync {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>>;
}

struct PausedSession {
    session: Arc<dyn MediaSession>,
    ownership: Arc<PauseOwnership>,
    _watch: Box<dyn SessionWatch>,
}

impl PausedSession {
    fn resume(self) {
        if self.session.is_live() != Ok(true) {
            return;
        }
        let state = self.session.state();
        self.ownership.observe(state.clone());
        if state == Ok(PlaybackState::Paused)
            && self.ownership.can_resume()
            && self.session.play() != Ok(true)
        {
            debug!("Media session could not resume; leaving playback to its owner");
        }
    }
}

fn pause_sessions(
    backend: &dyn MediaBackend,
    current: &AtomicU64,
    generation: u64,
) -> Vec<PausedSession> {
    let sessions = match backend.sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            debug!("Media session enumeration unavailable: {error}");
            return Vec::new();
        }
    };
    let mut paused = Vec::new();
    for session in sessions {
        if current.load(Ordering::Acquire) != generation {
            break;
        }
        if session.state() != Ok(PlaybackState::Playing) {
            continue;
        }
        let ownership = Arc::new(PauseOwnership::default());
        let Ok(watch) = session.watch(ownership.clone()) else {
            // Do not pause a session whose later user changes we cannot observe.
            continue;
        };
        if current.load(Ordering::Acquire) != generation
            || session.state() != Ok(PlaybackState::Playing)
            || session.pause() != Ok(true)
        {
            continue;
        }
        ownership.observe(session.state());
        paused.push(PausedSession {
            session,
            ownership,
            _watch: watch,
        });
    }
    paused
}

enum Command {
    Begin(u64),
    Apply(u64),
    End(u64),
    Barrier(mpsc::Sender<()>),
    #[cfg(test)]
    Park(mpsc::Sender<()>, mpsc::Receiver<()>),
}

struct Sender {
    current: Arc<AtomicU64>,
    commands: mpsc::Sender<Command>,
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.current.store(0, Ordering::Release);
        // Closing the channel makes the worker restore any remaining leases.
    }
}

#[derive(Clone)]
pub(super) struct MediaPauseController(Arc<Sender>);

impl MediaPauseController {
    pub(super) fn new() -> Self {
        #[cfg(windows)]
        let backend = Arc::new(platform::Backend);
        #[cfg(not(windows))]
        let backend = Arc::new(NoMediaBackend);
        Self::with_backend(backend)
    }

    fn with_backend(backend: Arc<dyn MediaBackend>) -> Self {
        let (commands, receiver) = mpsc::channel();
        let current = Arc::new(AtomicU64::new(0));
        let worker_current = current.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("handy-media-pause".into())
            .spawn(move || {
                #[cfg(windows)]
                let _apartment = match platform::Apartment::new() {
                    Ok(apartment) => apartment,
                    Err(error) => {
                        debug!("Media control apartment unavailable: {error}");
                        return;
                    }
                };
                let mut generation = 0;
                let mut applied = false;
                let mut paused = Vec::<PausedSession>::new();
                let restore = |paused: &mut Vec<PausedSession>| {
                    for session in paused.drain(..) {
                        session.resume();
                    }
                };
                for command in receiver {
                    // Even a superseded Begin/End pair must release an older
                    // recording's sessions before its commands are discarded.
                    if worker_current.load(Ordering::Acquire) != generation {
                        restore(&mut paused);
                    }
                    match command {
                        Command::Begin(next) if worker_current.load(Ordering::Acquire) == next => {
                            restore(&mut paused);
                            generation = next;
                            applied = false;
                        }
                        Command::Apply(next)
                            if next == generation
                                && !applied
                                && worker_current.load(Ordering::Acquire) == next =>
                        {
                            paused = pause_sessions(backend.as_ref(), &worker_current, next);
                            applied = true;
                            // Stop/cancel may have happened during a slow platform call.
                            if worker_current.load(Ordering::Acquire) != next {
                                restore(&mut paused);
                            }
                        }
                        Command::End(ended) if ended == generation => {
                            restore(&mut paused);
                            generation = 0;
                        }
                        Command::Barrier(done) => {
                            let _ = done.send(());
                        }
                        #[cfg(test)]
                        Command::Park(entered, release) => {
                            entered.send(()).unwrap();
                            release
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap();
                        }
                        _ => {}
                    }
                }
                restore(&mut paused);
            })
        {
            debug!("Could not start media control worker: {error}");
        }
        Self(Arc::new(Sender { current, commands }))
    }

    pub(super) fn begin(&self, generation: u64) {
        self.0.current.store(generation, Ordering::Release);
        let _ = self.0.commands.send(Command::Begin(generation));
    }

    pub(super) fn apply(&self, generation: u64, enabled: bool, stream_open: bool) {
        if enabled && stream_open {
            let _ = self.0.commands.send(Command::Apply(generation));
        }
    }

    pub(super) fn remove(&self) {
        let generation = self.0.current.swap(0, Ordering::AcqRel);
        let _ = self.0.commands.send(Command::End(generation));
    }

    pub(super) fn restore_before_exit(&self) {
        self.remove();
        let (done, receiver) = mpsc::channel();
        if self.0.commands.send(Command::Barrier(done)).is_ok()
            && receiver
                .recv_timeout(std::time::Duration::from_secs(3))
                .is_err()
        {
            debug!("Media restoration did not finish before the exit deadline");
        }
    }

    #[cfg(test)]
    fn flush(&self) {
        let (done, receiver) = mpsc::channel();
        self.0.commands.send(Command::Barrier(done)).unwrap();
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}

#[cfg(not(windows))]
struct NoMediaBackend;
#[cfg(not(windows))]
impl MediaBackend for NoMediaBackend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
#[path = "media_pause_tests.rs"]
mod tests;
