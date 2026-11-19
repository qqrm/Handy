//! Owns pause and mute for one capture, independently of microphone stream lifetime.
//! Stop closes readiness immediately, but restoration follows the final captured sample.
use super::media_pause::MediaPauseController;
use super::output_mute::{mute_default_output, MutedOutput};
use log::warn;
use std::sync::{Arc, Mutex};

// Bound retained work if many different outputs disappear before restoration.
// At capacity, refuse another mutation rather than losing an existing owner.
const MAX_MUTE_LEASES: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct OutputOptions {
    pub mute: bool,
    pub pause: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitingReady,
    Applied,
    StopRequested,
    Finishing,
}

struct Recording {
    generation: u64,
    options: OutputOptions,
    phase: Phase,
}

#[derive(Default)]
struct State {
    recording: Option<Recording>,
    // Failed restores retain their original outputs. An unavailable old output
    // must not prevent muting a healthy new default on the next recording.
    muted: Vec<Box<dyn MutedOutput>>,
}

impl State {
    fn restore_mute(&mut self) {
        self.muted.retain_mut(|muted| match muted.restore() {
            Ok(()) => false,
            Err(error) => {
                warn!("Could not restore recording output mute: {error}");
                true
            }
        });
    }
}

trait OutputBackend: Send + Sync {
    fn begin(&self, generation: u64);
    fn mute(&self) -> Result<Option<Box<dyn MutedOutput>>, String>;
    fn pause(&self, generation: u64);
    fn resume(&self);
    fn drain(&self);
}

struct SystemOutputBackend(MediaPauseController);

impl OutputBackend for SystemOutputBackend {
    fn begin(&self, generation: u64) {
        self.0.begin(generation);
    }

    fn mute(&self) -> Result<Option<Box<dyn MutedOutput>>, String> {
        mute_default_output()
    }

    fn pause(&self, generation: u64) {
        self.0.apply(generation, true, true);
    }

    fn resume(&self) {
        self.0.remove();
    }

    fn drain(&self) {
        self.0.restore_before_exit();
    }
}

struct Controller {
    state: Mutex<State>,
    backend: Arc<dyn OutputBackend>,
}

impl Drop for Controller {
    fn drop(&mut self) {
        // Normal stop/cancel/exit already finish capture before this last resort.
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        state.restore_mute();
        self.backend.resume();
    }
}

#[derive(Clone)]
pub(super) struct RecordingOutputController(Arc<Controller>);

impl RecordingOutputController {
    pub(super) fn new() -> Self {
        Self::with_backend(Arc::new(SystemOutputBackend(MediaPauseController::new())))
    }

    fn with_backend(backend: Arc<dyn OutputBackend>) -> Self {
        Self(Arc::new(Controller {
            state: Mutex::new(State::default()),
            backend,
        }))
    }

    /// Settings are a snapshot for this recording. Changes affect the next capture.
    pub(super) fn begin(&self, generation: u64, options: OutputOptions) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.recording.is_some() {
            // The manager must keep Stopping until its capture/cleanup is complete.
            // Refuse an overlap rather than restoring audio into an older capture.
            warn!("Refusing overlapping recording output generation {generation}");
            return;
        }
        // Capture has already started. Retry retained restores only at a later
        // cleanup boundary, never by unmuting an output into this new capture.
        self.0.backend.begin(generation);
        state.recording = Some(Recording {
            generation,
            options,
            phase: Phase::AwaitingReady,
        });
    }

    /// The generation check and the actual mute are serialized with cleanup.
    pub(super) fn ready(&self, generation: u64) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(recording) = state.recording.as_mut() else {
            return;
        };
        if recording.generation != generation || recording.phase != Phase::AwaitingReady {
            return;
        }
        let options = recording.options;
        recording.phase = Phase::Applied;
        if options.mute && state.muted.len() < MAX_MUTE_LEASES {
            match self.0.backend.mute() {
                Ok(Some(muted)) => state.muted.push(muted),
                // Includes an output still muted by a retained lease: keep
                // that owner instead of acquiring a conflicting snapshot.
                Ok(None) => {}
                Err(error) => warn!("Could not mute recording output: {error}"),
            }
        } else if options.mute {
            warn!("Too many pending output restorations; refusing another mute");
        }
        // Pause is independent of mute success; the SMTC worker cannot delay mute.
        if options.pause {
            self.0.backend.pause(generation);
        }
    }

    /// Reject late readiness without releasing protection during capture drain.
    pub(super) fn stop_requested(&self, generation: u64) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = state.recording.as_mut() {
            if recording.generation == generation && recording.phase != Phase::Finishing {
                recording.phase = Phase::StopRequested;
            }
        }
    }

    /// Runs the actual tail/capture stop before restoring output, for Stop and Cancel.
    /// The closure must quiesce capture even when its return value reports an error.
    /// Stale or duplicate finish requests never execute the capture closure.
    pub(super) fn finish_after_capture<T>(
        &self,
        generation: u64,
        capture: impl FnOnce() -> T,
    ) -> Option<T> {
        {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            let recording = state.recording.as_mut()?;
            if recording.generation != generation || recording.phase == Phase::Finishing {
                return None;
            }
            recording.phase = Phase::Finishing;
        }
        // Never hold the output lock across capture drain: Cancel must be able to
        // signal the tail loop, and a backend must not acquire audio-manager locks.
        let finish = FinishOutput {
            controller: self,
            generation,
        };
        let result = capture();
        drop(finish);
        Some(result)
    }

    fn finish(&self, generation: u64) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .recording
            .as_ref()
            .is_some_and(|recording| recording.generation == generation)
        {
            return;
        }
        state.restore_mute();
        self.0.backend.resume();
        state.recording = None;
    }

    /// Stream teardown may run without an active recording, or interrupt one.
    /// It always closes capture before releasing any retained output ownership.
    /// The caller serializes stream teardown with recording start.
    pub(super) fn close_after_capture<T>(&self, capture: impl FnOnce() -> T) -> T {
        let generation = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.recording.as_mut().map(|recording| {
                recording.phase = Phase::Finishing;
                recording.generation
            })
        };
        let result = capture();
        if let Some(generation) = generation {
            self.finish(generation);
        } else {
            self.0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .restore_mute();
        }
        result
    }

    /// Retry a retained endpoint restoration only after capture has been stopped.
    pub(super) fn restore_before_exit(&self) {
        {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.restore_mute();
            state.recording = None;
            self.0.backend.resume();
        }
        self.0.backend.drain();
    }
}

struct FinishOutput<'a> {
    controller: &'a RecordingOutputController,
    generation: u64,
}

impl Drop for FinishOutput<'_> {
    fn drop(&mut self) {
        self.controller.finish(self.generation);
    }
}

#[cfg(test)]
#[path = "recording_output_tests.rs"]
mod tests;
