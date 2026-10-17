//! Recording-scoped media pause. All platform calls run on one worker, never
//! on the UI thread. A recording generation invalidates delayed readiness work.
use log::debug;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

// Bound both retained provider operations and queued requests. A full queue
// drops optional work; the atomic generation still invalidates stale work.
const MAX_MEDIA_LEASES: usize = 32;
const COMMAND_CAPACITY: usize = 32;
const ACK_DEADLINE: Duration = Duration::from_secs(1);
const FAST_POLL: Duration = Duration::from_millis(20);
const QUARANTINE_POLL: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(3);

#[cfg(windows)]
#[path = "media_pause_windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "media_pause_linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "media_pause_macos.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub(crate) fn request_quicktime_permission() -> MediaResult<bool> {
    platform::request_quicktime_permission()
}

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

/// Poll a submitted request without waiting for its provider. `None` means
/// unresolved, including an uncertain timeout; never translate that to rejection.
/// A terminal false/error means the provider definitively rejected the request.
type MediaOperation = Box<dyn FnMut() -> Option<MediaResult<bool>> + Send>;

#[cfg(any(test, target_os = "linux"))]
fn completed_operation(result: MediaResult<bool>) -> MediaOperation {
    let mut result = Some(result);
    Box::new(move || result.take())
}

/// Media APIs do not identify who issued a command. Once playback leaves our
/// acknowledged pause, ownership is permanently relinquished for this recording.
/// A repeated Pause while already Paused may produce no event and is unknowable.
#[derive(Default)]
struct OwnershipState {
    revision: u64,
    pause_observed: bool,
    lost: bool,
    resumed: bool,
}

#[derive(Default)]
struct PauseOwnership(Mutex<OwnershipState>);

impl PauseOwnership {
    fn observe(&self, state: MediaResult<PlaybackState>) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Self::record(&mut guard, state);
    }

    fn record(guard: &mut OwnershipState, state: MediaResult<PlaybackState>) {
        guard.revision = guard.revision.wrapping_add(1);
        match state {
            Ok(PlaybackState::Paused) => guard.pause_observed = true,
            Ok(PlaybackState::Playing) if !guard.pause_observed => {}
            Ok(PlaybackState::Playing) => {
                guard.lost = true;
                guard.resumed = true;
            }
            _ => guard.lost = true,
        }
    }

    /// Do not replay a Get snapshot after a newer subscription event. Never
    /// hold the ownership lock across IPC: a backend may fence its event stream
    /// before returning. None also prevents a stale snapshot authorizing a
    /// command or releasing an unresolved lease; the next poll can read again.
    fn snapshot(&self, session: &dyn MediaSession) -> Option<MediaResult<PlaybackState>> {
        let revision = self.0.lock().unwrap_or_else(|e| e.into_inner()).revision;
        let state = session.state();
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.revision != revision {
            return None;
        }
        Self::record(&mut guard, state.clone());
        Some(state)
    }

    fn can_resume(&self) -> bool {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.pause_observed && !guard.lost
    }

    fn lost(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).lost
    }

    fn resume_observed(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).resumed
    }
}

/// A subscription is removed when its guard is dropped.
trait SessionWatch: Send {}
trait MediaSession: Send + Sync {
    fn state(&self) -> MediaResult<PlaybackState>;
    /// Creating a request must not wait for asynchronous completion. Initial
    /// errors mean no request was submitted; uncertain effects belong in a
    /// retained operation. Synchronous IPC still depends on provider liveness.
    fn pause(&self) -> MediaResult<MediaOperation>;
    fn play(&self) -> MediaResult<MediaOperation>;
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
    pause: Option<MediaOperation>,
    play: Option<MediaOperation>,
    restoring: bool,
    retiring: bool,
    submitted: Instant,
    quarantined: bool,
}

impl PausedSession {
    /// Advance one lease. Pending operations retain their original object and
    /// watch even after End or the acknowledgement deadline. Returning true
    /// releases the lease. No automatic command is retried.
    fn progress(&mut self) -> bool {
        // Only a definitive disappearance releases a possibly pending effect.
        // An IPC error says nothing about whether the request can still act.
        let live = self.session.is_live();
        if live == Ok(false) {
            return true;
        }
        if let Err(error) = live {
            self.ownership.observe(Err(error));
        }
        let state = self.ownership.snapshot(self.session.as_ref());

        if let Some(operation) = self.pause.as_mut() {
            match operation() {
                None => {
                    self.mark_quarantine();
                    return false;
                }
                Some(Ok(true)) => self.pause = None,
                Some(_) => return true,
            }
        }
        if let Some(operation) = self.play.as_mut() {
            match operation() {
                None => {
                    self.mark_quarantine();
                    return false;
                }
                Some(Ok(true)) => self.play = None,
                Some(_) => {
                    debug!("Media resume rejected; leaving playback to its owner");
                    return true;
                }
            }
        }
        if self.restoring {
            // Play has the same reply/state race as Pause. Do not allow a new
            // recording to pause ahead of the previous delayed resume. Keep
            // the observed transition even if the user has already paused again.
            if self.ownership.resume_observed() || state == Some(Ok(PlaybackState::Other)) {
                return true;
            }
            self.mark_quarantine();
            return false;
        }
        if self.ownership.lost() {
            return true;
        }
        if !self.ownership.can_resume() {
            // A successful method reply is not a Paused acknowledgement. A
            // late state event must remain observable after recording ends.
            self.mark_quarantine();
            return false;
        }
        if self.retiring && state == Some(Ok(PlaybackState::Paused)) {
            match self.session.play() {
                Ok(operation) => {
                    self.play = Some(operation);
                    self.restoring = true;
                    self.submitted = Instant::now();
                    self.quarantined = false;
                }
                Err(error) => {
                    debug!("Media resume could not be submitted: {error}");
                    return true;
                }
            }
        }
        false
    }

    fn mark_quarantine(&mut self) {
        if !self.quarantined && self.submitted.elapsed() >= ACK_DEADLINE {
            self.quarantined = true;
            debug!(
                "Media request remains unconfirmed; retaining its lease and suppressing new pauses"
            );
        }
    }

    fn needs_fast_poll(&self) -> bool {
        !self.quarantined
            && (self.pause.is_some() || self.restoring || !self.ownership.can_resume())
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
    if sessions.len() > MAX_MEDIA_LEASES {
        debug!("Media session limit exceeded; refusing a partial pause batch");
        return Vec::new();
    }
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
            || ownership.snapshot(session.as_ref()) != Some(Ok(PlaybackState::Playing))
            || ownership.lost()
        {
            continue;
        }
        let operation = match session.pause() {
            Ok(operation) => operation,
            Err(_) => continue,
        };
        let _ = ownership.snapshot(session.as_ref());
        paused.push(PausedSession {
            session,
            ownership,
            _watch: watch,
            pause: Some(operation),
            play: None,
            restoring: false,
            retiring: false,
            submitted: Instant::now(),
            quarantined: false,
        });
    }
    paused
}

enum Command {
    Wake,
    Apply(u64),
    #[cfg(test)]
    Barrier(mpsc::Sender<()>),
    Drain(mpsc::Sender<()>),
    #[cfg(test)]
    Park(mpsc::Sender<()>, mpsc::Receiver<()>),
}

struct Sender {
    current: Arc<AtomicU64>,
    commands: mpsc::SyncSender<Command>,
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
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let backend = Arc::new(platform::Backend);
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        let backend = Arc::new(NoMediaBackend);
        Self::with_backend(backend)
    }

    fn with_backend(backend: Arc<dyn MediaBackend>) -> Self {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
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
                let mut requested = false;
                let mut paused = Vec::<PausedSession>::new();
                let mut shutdown: Option<Instant> = None;
                let mut drains = Vec::<(Instant, mpsc::Sender<()>)>::new();
                loop {
                    let interval = if paused.iter().any(PausedSession::needs_fast_poll) {
                        FAST_POLL
                    } else {
                        QUARANTINE_POLL
                    };
                    let command = if shutdown.is_some() {
                        // All senders are gone; recv_timeout would return immediately.
                        std::thread::sleep(interval);
                        None
                    } else {
                        match receiver.recv_timeout(interval) {
                            Ok(command) => Some(command),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                shutdown = Some(Instant::now());
                                None
                            }
                        }
                    };
                    let current = worker_current.load(Ordering::Acquire);
                    if current != generation {
                        for session in &mut paused {
                            session.retiring = true;
                        }
                        generation = current;
                        applied = false;
                        requested = false;
                    }
                    #[cfg(test)]
                    let mut barrier = None;
                    match command {
                        Some(Command::Apply(next)) if next == generation && next != 0 && !applied => {
                            applied = true;
                            requested = true;
                        }
                        // Begin/End are wakeups; the atomic generation is the
                        // source of truth even when the bounded queue is full.
                        Some(Command::Wake) => {}
                        #[cfg(test)]
                        Some(Command::Barrier(done)) => barrier = Some(done),
                        Some(Command::Drain(done)) => {
                            if drains.len() < COMMAND_CAPACITY {
                                drains.push((Instant::now(), done));
                            }
                        }
                        #[cfg(test)]
                        Some(Command::Park(entered, release)) => {
                            entered.send(()).unwrap();
                            release.recv_timeout(Duration::from_secs(5)).unwrap();
                        }
                        _ => {}
                    }
                    // Re-read after a test barrier or synchronous provider IPC.
                    if worker_current.load(Ordering::Acquire) != generation {
                        for session in &mut paused {
                            session.retiring = true;
                        }
                    }
                    paused.retain_mut(|session| !session.progress());
                    // Never overlap a new recording's commands with unresolved
                    // effects from an older recording, including a pending Play.
                    if requested
                        && paused.is_empty()
                        && worker_current.load(Ordering::Acquire) == generation
                    {
                        paused = pause_sessions(backend.as_ref(), &worker_current, generation);
                        requested = false;
                        if worker_current.load(Ordering::Acquire) != generation {
                            for session in &mut paused {
                                session.retiring = true;
                            }
                        }
                        paused.retain_mut(|session| !session.progress());
                    }
                    #[cfg(test)]
                    if let Some(done) = barrier {
                        let _ = done.send(());
                    }
                    drains.retain(|(started, done)| {
                        if paused.is_empty() || started.elapsed() >= EXIT_DEADLINE {
                            if !paused.is_empty() {
                                debug!("Media restoration deadline reached with unresolved provider effects");
                            }
                            let _ = done.send(());
                            false
                        } else {
                            true
                        }
                    });
                    if let Some(started) = shutdown {
                        if paused.is_empty() {
                            break;
                        }
                        if started.elapsed() >= EXIT_DEADLINE {
                            debug!("Unresolved provider effects remain at shutdown; restoration is best effort");
                            break;
                        }
                    }
                }
            })
        {
            debug!("Could not start media control worker: {error}");
        }
        Self(Arc::new(Sender { current, commands }))
    }

    pub(super) fn begin(&self, generation: u64) {
        self.0.current.store(generation, Ordering::Release);
        let _ = self.0.commands.try_send(Command::Wake);
    }

    pub(super) fn apply(&self, generation: u64, enabled: bool, stream_open: bool) {
        if enabled && stream_open {
            let _ = self.0.commands.try_send(Command::Apply(generation));
        }
    }

    pub(super) fn remove(&self) {
        self.0.current.store(0, Ordering::Release);
        let _ = self.0.commands.try_send(Command::Wake);
    }

    pub(super) fn restore_before_exit(&self) {
        self.remove();
        let deadline = Instant::now() + EXIT_DEADLINE;
        let (done, receiver) = mpsc::channel();
        let mut command = Command::Drain(done);
        loop {
            match self.0.commands.try_send(command) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Disconnected(_)) => return,
                Err(mpsc::TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        debug!("Media restoration queue remained full until exit deadline");
                        return;
                    }
                    command = returned;
                    std::thread::sleep(FAST_POLL);
                }
            }
        }
        if receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
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

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
struct NoMediaBackend;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
impl MediaBackend for NoMediaBackend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
#[path = "media_pause_tests.rs"]
mod tests;
