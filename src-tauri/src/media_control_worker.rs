//! Serializes legacy media providers without holding the recording-output lock.
//!
//! Only the current request is retained. Provider calls run outside the request
//! mutex, and each successful pause keeps the resume policy of its recording.
use log::warn;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) trait PlaybackBackend: Send + 'static {
    type Paused: Send + 'static;

    fn pause_playback(&mut self) -> Result<Option<Self::Paused>, String>;
    fn resume_playback(&mut self, paused: Self::Paused) -> Result<(), String>;
}

#[derive(Clone, Copy)]
struct Request {
    generation: u64,
    resume: bool,
    ready: bool,
}

#[derive(Default)]
struct State {
    request: Option<Request>,
    revision: u64,
    settled: u64,
    shutdown: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct Owner {
    shared: Arc<Shared>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.request = None;
        state.shutdown = true;
        state.revision += 1;
        self.shared.changed.notify_all();
    }
}

struct Paused<P> {
    generation: u64,
    resume: bool,
    playback: Option<P>,
}

fn retire<B: PlaybackBackend>(backend: &mut B, paused: &mut Option<Paused<B::Paused>>) {
    if let Some(Paused {
        resume: true,
        playback: Some(playback),
        ..
    }) = paused.take()
    {
        if let Err(error) = backend.resume_playback(playback) {
            warn!("Could not resume media after recording: {error}");
        }
    }
}

#[derive(Clone)]
pub(crate) struct PlaybackWorker(Arc<Owner>);

impl PlaybackWorker {
    pub(crate) fn new<B: PlaybackBackend>(mut backend: B) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("handy-media-control".into())
            .spawn(move || {
                // Retained WinRT session handles stay inside this apartment.
                #[cfg(windows)]
                let _apartment = match super::platform_windows::WinRtGuard::initialize() {
                    Ok(apartment) => apartment,
                    Err(error) => {
                        warn!("Cannot initialize media control worker: {error}");
                        return;
                    }
                };
                let mut seen = 0;
                let mut paused: Option<Paused<B::Paused>> = None;
                loop {
                    let (request, revision, shutdown) = {
                        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                        while state.revision == seen && !state.shutdown {
                            state = worker
                                .changed
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        (state.request, state.revision, state.shutdown)
                    };
                    if paused.as_ref().is_some_and(|owner| {
                        request.is_none_or(|next| next.generation != owner.generation)
                    }) {
                        retire(&mut backend, &mut paused);
                    }
                    if let Some(request) = request.filter(|request| request.ready) {
                        // Resume can block too. Recheck the current generation
                        // before starting another provider operation.
                        let still_current = worker
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .request
                            .is_some_and(|current| {
                                current.generation == request.generation && current.ready
                            });
                        if paused.is_none() && still_current {
                            let playback = match backend.pause_playback() {
                                Ok(playback) => playback,
                                Err(error) => {
                                    warn!("Could not pause media for recording: {error}");
                                    None
                                }
                            };
                            // Even a late result belongs to this request. The
                            // next iteration retires it using its saved policy.
                            paused = Some(Paused {
                                generation: request.generation,
                                resume: request.resume,
                                playback,
                            });
                        }
                    }
                    seen = revision;
                    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.settled = revision;
                    worker.changed.notify_all();
                    if shutdown {
                        break;
                    }
                }
                drop(paused);
                drop(backend);
            })
        {
            warn!("Could not start media control worker: {error}");
        }
        Self(Arc::new(Owner { shared }))
    }

    pub(crate) fn begin(&self, generation: u64, resume: bool) {
        let mut state = self
            .0
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.request = Some(Request {
            generation,
            resume,
            ready: false,
        });
        state.revision += 1;
        self.0.shared.changed.notify_all();
    }

    pub(crate) fn apply(&self, generation: u64) {
        let mut state = self
            .0
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(request) = state.request.as_mut() else {
            return;
        };
        if request.generation != generation || request.ready {
            return;
        }
        request.ready = true;
        state.revision += 1;
        self.0.shared.changed.notify_all();
    }

    pub(crate) fn remove(&self) {
        let mut state = self
            .0
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.request = None;
        state.revision += 1;
        self.0.shared.changed.notify_all();
    }

    pub(crate) fn restore_before_exit(&self) {
        self.remove();
        if !self.wait_until_settled(Duration::from_secs(5)) {
            warn!("Media restoration did not finish before the exit deadline");
        }
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        assert!(self.wait_until_settled(Duration::from_secs(5)));
    }

    fn wait_until_settled(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .0
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let revision = state.revision;
        while state.settled < revision {
            let (next, timed_out) = self
                .0
                .shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            if timed_out.timed_out() && state.settled < revision {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
#[path = "media_control_worker_tests.rs"]
mod tests;
