//! Historical regression fixture; production method bodies are injected unchanged
//! from the pinned Git revision by recording_output_regressions.py.
//!
//! Only external collaborators are fake: the recorder, settings store, output
//! endpoints, SMTC transport, and the lazy-close timer. This is not a full Handy
//! application or an audible-hardware test.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

macro_rules! debug {
    ($($arg:tt)*) => { let _ = format_args!($($arg)*); };
}
macro_rules! error {
    ($($arg:tt)*) => { let _ = format_args!($($arg)*); };
}

const WHISPER_SAMPLE_RATE: usize = 16_000;

#[derive(Clone, Debug)]
enum RecordingState {
    Idle,
    Recording { binding_id: String },
    Stopping,
}

#[derive(Clone, Debug)]
enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

#[derive(Debug, Default, Clone, Copy)]
struct MuteState {
    did_mute: bool,
    prev_muted: Option<bool>,
}

#[derive(Clone)]
struct Settings {
    mute_while_recording: bool,
    lazy_stream_close: bool,
    extra_recording_buffer_ms: u64,
}

fn get_settings(settings: &Settings) -> Settings {
    settings.clone()
}

struct OutputDevices {
    default: usize,
    muted: [bool; 2],
}

static OUTPUT: Mutex<OutputDevices> = Mutex::new(OutputDevices {
    default: 0,
    muted: [false, true],
});

// Model the documented Windows adapter contract: get/set each target the
// default endpoint at call time. Tests use one thread and reset this fixture.
fn get_mute() -> Option<bool> {
    let output = OUTPUT.lock().unwrap();
    Some(output.muted[output.default])
}

fn set_mute(mute: bool) {
    let mut output = OUTPUT.lock().unwrap();
    let target = output.default;
    output.muted[target] = mute;
}

/* PRODUCTION_FREE_FUNCTIONS */

#[derive(Default)]
struct CaptureProbe {
    media_paused: AtomicBool,
    media_paused_when_capture_stopped: AtomicBool,
    capture_stopped: AtomicBool,
}

struct Recorder(Arc<CaptureProbe>);

impl Recorder {
    fn stop(&self) -> Result<Vec<f32>, &'static str> {
        self.0
            .media_paused_when_capture_stopped
            .store(self.0.media_paused.load(Ordering::SeqCst), Ordering::SeqCst);
        self.0.capture_stopped.store(true, Ordering::SeqCst);
        Ok(vec![0.0; WHISPER_SAMPLE_RATE])
    }

    fn close(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
}

struct MediaPause(Arc<CaptureProbe>);

impl MediaPause {
    fn remove(&self) {
        self.0.media_paused.store(false, Ordering::SeqCst);
    }
}

struct AudioRecordingManager {
    app_handle: Settings,
    is_open: Mutex<bool>,
    mute_state: Mutex<MuteState>,
    capture_generation: AtomicU64,
    cancel_generation: AtomicU64,
    state: Mutex<RecordingState>,
    mode: Mutex<MicrophoneMode>,
    recorder: Mutex<Option<Recorder>>,
    is_recording: Mutex<bool>,
    recording_active: AtomicBool,
    media_pause: MediaPause,
    lazy_close_scheduled: AtomicBool,
}

impl AudioRecordingManager {
    // Scheduling is the only substituted lifecycle collaborator: the real
    // 30-second delayed close has intentionally not fired at cancellation.
    fn schedule_lazy_close(&self) {
        self.lazy_close_scheduled.store(true, Ordering::SeqCst);
    }

    /* PRODUCTION_METHODS */
}

fn recording(mode: MicrophoneMode, lazy_close: bool) -> (AudioRecordingManager, Arc<CaptureProbe>) {
    *OUTPUT.lock().unwrap() = OutputDevices {
        default: 0,
        muted: [false, true],
    };
    let probe = Arc::new(CaptureProbe::default());
    probe.media_paused.store(true, Ordering::SeqCst);
    let manager = AudioRecordingManager {
        app_handle: Settings {
            mute_while_recording: true,
            lazy_stream_close: lazy_close,
            extra_recording_buffer_ms: 5,
        },
        is_open: Mutex::new(true),
        mute_state: Mutex::new(MuteState::default()),
        capture_generation: AtomicU64::new(1),
        cancel_generation: AtomicU64::new(0),
        state: Mutex::new(RecordingState::Recording {
            binding_id: "fixture".into(),
        }),
        mode: Mutex::new(mode),
        recorder: Mutex::new(Some(Recorder(probe.clone()))),
        is_recording: Mutex::new(true),
        recording_active: AtomicBool::new(true),
        media_pause: MediaPause(probe.clone()),
        lazy_close_scheduled: AtomicBool::new(false),
    };
    (manager, probe)
}

#[test]
fn immediate_on_demand_cancel_restores_output() {
    let (manager, probe) = recording(MicrophoneMode::OnDemand, false);
    manager.apply_mute();
    assert!(get_mute().unwrap());
    manager.cancel_recording();
    assert!(probe.capture_stopped.load(Ordering::SeqCst));
    assert!(!*manager.is_open.lock().unwrap());
    assert!(!get_mute().unwrap());
}

#[test]
fn user_muted_output_stays_muted() {
    let (manager, _) = recording(MicrophoneMode::AlwaysOn, false);
    set_mute(true);
    manager.apply_mute();
    manager.remove_mute();
    assert!(get_mute().unwrap());
}

#[test]
fn cancel_restores_output_with_always_on_microphone() {
    let (manager, probe) = recording(MicrophoneMode::AlwaysOn, false);
    manager.apply_mute();
    assert!(get_mute().unwrap());
    manager.cancel_recording();
    assert!(probe.capture_stopped.load(Ordering::SeqCst));
    assert!(*manager.is_open.lock().unwrap());
    assert!(
        !get_mute().unwrap(),
        "REGRESSION: cancel left always-on output muted"
    );
}

#[test]
fn cancel_restores_output_before_lazy_microphone_close() {
    let (manager, probe) = recording(MicrophoneMode::OnDemand, true);
    manager.apply_mute();
    manager.cancel_recording();
    assert!(probe.capture_stopped.load(Ordering::SeqCst));
    assert!(manager.lazy_close_scheduled.load(Ordering::SeqCst));
    assert!(*manager.is_open.lock().unwrap());
    assert!(
        !get_mute().unwrap(),
        "REGRESSION: cancel waits for lazy close to unmute"
    );
}

#[test]
fn media_remains_paused_until_capture_stops() {
    let (manager, probe) = recording(MicrophoneMode::AlwaysOn, false);
    manager.apply_mute();
    assert!(manager.stop_recording("fixture", 0).is_some());
    assert!(probe.capture_stopped.load(Ordering::SeqCst));
    assert!(
        probe
            .media_paused_when_capture_stopped
            .load(Ordering::SeqCst),
        "REGRESSION: media resumed before tail capture stopped"
    );
}

#[test]
fn late_readiness_cannot_mute_idle_open_microphone() {
    let (manager, _) = recording(MicrophoneMode::AlwaysOn, false);
    // A worker previously passed its readiness check, but Cancel completes
    // before that worker actually enters apply_mute. No sleeps are needed.
    manager.cancel_recording();
    assert!(!manager.recording_active.load(Ordering::SeqCst));
    assert!(*manager.is_open.lock().unwrap());
    manager.apply_mute();
    assert!(
        !get_mute().unwrap(),
        "REGRESSION: stale readiness muted idle microphone"
    );
}

#[test]
fn restore_targets_the_original_output() {
    let (manager, _) = recording(MicrophoneMode::AlwaysOn, false);
    manager.apply_mute();
    OUTPUT.lock().unwrap().default = 1;
    manager.remove_mute();
    assert_eq!(
        OUTPUT.lock().unwrap().muted,
        [false, true],
        "REGRESSION: mute restoration followed the changed default output"
    );
}
